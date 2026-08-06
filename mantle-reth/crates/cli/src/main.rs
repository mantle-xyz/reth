#![allow(missing_docs, rustdoc::missing_crate_level_docs)]

use clap::Parser;
use eyre::ErrReport;
use mantle_reth_cli::{
    MantleArgs, MantleChainSpecParser, MantleNode, proofs_history::with_proofs_history_launch_ctx,
    seed_blockchain_tree_metrics, spawn_proofs_db_metrics,
};
use mantle_reth_preconf::PreconfServiceBuilder;
use reth_db::DatabaseEnv;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::args::{ProofsStorageVersion, RollupArgs};
use reth_optimism_trie::{
    OpProofsStore,
    db::{MdbxProofsStorage, MdbxProofsStorageV2},
};
use std::sync::Arc;
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
    let metrics_mdbx = mdbx.clone();

    let builder = builder.node(node).on_node_started(move |full_node| {
        seed_blockchain_tree_metrics(&full_node.provider);
        spawn_proofs_db_metrics(
            full_node.task_executor,
            metrics_mdbx,
            full_node.config.metrics.push_gateway_interval,
        );
        Ok(())
    });

    let handle = with_proofs_history_launch_ctx(builder, &args, mdbx).launch().await?;

    handle.node_exit_future.await
}
