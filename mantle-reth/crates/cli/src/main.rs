#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use eyre::ErrReport;
use futures_util::FutureExt;
use mantle_reth_cli::{
    MantleArgs, MantleChainSpecParser, MantleNode, seed_blockchain_tree_metrics,
};
use mantle_reth_preconf::PreconfServiceBuilder;
use reth_db::DatabaseEnv;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_builder::{FullNodeComponents, NodeBuilder, WithLaunchContext};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_node::args::{ProofsStorageVersion, RollupArgs};
use reth_optimism_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore,
    db::{MdbxProofsStorage, MdbxProofsStorageV2},
};
use reth_tasks::TaskExecutor;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "_rjem_malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

fn main() {
    reth_cli_util::sigsegv_handler::install();
    mantle_reth_cli::version::init_mantle_version();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    if let Err(err) = reth_optimism_cli::Cli::<MantleChainSpecParser, MantleArgs>::parse().run(
        async move |builder, args| {
            info!(target: "reth::cli", "Launching Mantle node");
            let mut node = MantleNode::new(args.rollup.clone());
            match args.preconf.into_config() {
                Some(cfg) => {
                    let all = cfg.all_preconfs;
                    let journal = cfg.journal_path.clone();
                    let svc = PreconfServiceBuilder::from_config(cfg)
                        .await
                        .map_err(|e| eyre::eyre!("preconf service init: {e}"))?;
                    node = node.with_preconf(svc);
                    info!(
                        target: "reth::cli",
                        "Mantle preconf ENABLED (all_preconfs={all}, journal={journal:?})",
                    );
                }
                None => {
                    info!(
                        target: "reth::cli",
                        "Mantle preconf DISABLED (pass --preconf.enable to opt in)",
                    );
                }
            }
            launch_node(builder, node, args.rollup).await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Launches the node, installing the proofs-history sidecar when `--proofs-history` is set.
///
/// Without the flag this is the plain launch path. With it, the ExEx (write side), the
/// `eth_getProof` / `debug_*` RPC overrides (read side), and the storage metrics task are
/// wired up against a shared MDBX sidecar handle.
async fn launch_node(
    builder: WithLaunchContext<NodeBuilder<DatabaseEnv, OpChainSpec>>,
    node: MantleNode,
    args: RollupArgs,
) -> eyre::Result<(), ErrReport> {
    if !args.proofs_history {
        let handle = builder
            .node(node)
            .on_node_started(|full_node| {
                seed_blockchain_tree_metrics(&full_node.provider);
                Ok(())
            })
            .launch()
            .await?;
        return handle.node_exit_future.await;
    }

    let path = args
        .proofs_history_storage_path
        .clone()
        .ok_or_else(|| eyre::eyre!("--proofs-history.storage-path is required"))?;

    match args.proofs_history_storage_version {
        ProofsStorageVersion::V1 => {
            info!(target: "reth::cli", "Using on-disk storage for proofs history (v1)");
            let mdbx = Arc::new(
                MdbxProofsStorage::new(&path)
                    .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
            );
            launch_with_proof_history(builder, node, args, mdbx).await
        }
        ProofsStorageVersion::V2 => {
            info!(target: "reth::cli", "Using on-disk storage for proofs history (v2)");
            let mdbx = Arc::new(
                MdbxProofsStorageV2::new(&path)
                    .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorageV2: {e}"))?,
            );
            launch_with_proof_history(builder, node, args, mdbx).await
        }
    }
}

/// Installs the ExEx, RPC overrides, and metrics hook for proof history, then launches the node.
async fn launch_with_proof_history<S>(
    builder: WithLaunchContext<NodeBuilder<DatabaseEnv, OpChainSpec>>,
    node: MantleNode,
    args: RollupArgs,
    mdbx: Arc<S>,
) -> eyre::Result<(), ErrReport>
where
    S: OpProofsStore + DatabaseMetrics + Send + Sync + 'static,
{
    let storage: OpProofsStorage<Arc<S>> = mdbx.clone().into();
    let storage_exec = storage.clone();

    let RollupArgs {
        proofs_history_window,
        proofs_history_prune_interval,
        proofs_history_verification_interval,
        ..
    } = args;

    let handle = builder
        .node(node)
        .on_node_started(move |full_node| {
            seed_blockchain_tree_metrics(&full_node.provider);
            spawn_proofs_db_metrics(
                full_node.task_executor,
                mdbx,
                full_node.config.metrics.push_gateway_interval,
            );
            Ok(())
        })
        .install_exex("proofs-history", async move |exex_context| {
            Ok(OpProofsExEx::builder(exex_context, storage_exec)
                .with_proofs_history_window(proofs_history_window)
                .with_proofs_history_prune_interval(proofs_history_prune_interval)
                .with_verification_interval(proofs_history_verification_interval)
                .build()
                .run()
                .boxed())
        })
        .extend_rpc_modules(move |ctx| {
            info!(target: "reth::cli", "Installing proofs-history RPC overrides (eth_getProof, debug_executePayload)");
            let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), storage.clone());
            let debug_ext = DebugApiExt::new(
                ctx.node().provider().clone(),
                ctx.registry.eth_api().clone(),
                storage,
                ctx.node().task_executor().clone(),
                ctx.node().evm_config().clone(),
            );
            let eth_replaced = ctx.modules.replace_configured(api_ext.into_rpc())?;
            let debug_replaced = ctx.modules.replace_configured(debug_ext.into_rpc())?;
            info!(target: "reth::cli", eth_replaced, debug_replaced, "Proofs-history RPC overrides installed");
            Ok(())
        })
        .launch()
        .await?;

    handle.node_exit_future.await
}

/// Spawns a task that periodically reports metrics for the proofs DB.
fn spawn_proofs_db_metrics<S>(
    executor: TaskExecutor,
    storage: Arc<S>,
    metrics_report_interval: Duration,
) where
    S: DatabaseMetrics + Send + Sync + 'static,
{
    executor.spawn_critical_task("op-proofs-storage-metrics", async move {
        info!(
            target: "reth::cli",
            ?metrics_report_interval,
            "Starting op-proofs-storage metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}
