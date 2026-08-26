//! Proofs-history sidecar wiring, shared by the node binary and integration tests.
//!
//! The sidecar has three pieces that must be installed together:
//!
//! - the [`OpProofsExEx`] writer, which mirrors each block's trie updates into the MDBX sidecar,
//! - the `eth_getProof` / `debug_*` RPC overrides, which serve reads from that sidecar,
//! - a metrics task reporting sidecar DB stats ([`spawn_proofs_db_metrics`]).
//!
//! They live here rather than in `main.rs` so the integration tests exercise the same wiring the
//! shipped binary uses. Installing them from the test harness by hand would test the harness, not
//! the binary.
//!
//! Two entry points exist because the binary and the tests hold different builder types:
//! the CLI hands out a [`WithLaunchContext`]-wrapped builder whose inner value is private, while
//! the test harness builds a bare [`NodeBuilderWithComponents`]. Both expose the same
//! `install_exex` / `extend_rpc_modules` methods, so the hook bodies are shared via a macro.

use futures_util::FutureExt;
use jsonrpsee_types::error::ErrorObject;
use reth_db_api::database_metrics::DatabaseMetrics;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::{
    FullNodeTypes, NodeAdapter, NodeBuilderWithComponents, WithLaunchContext,
    components::NodeComponentsBuilder, rpc::RethRpcAddOns,
};
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_node::args::RollupArgs;
use reth_optimism_payload_builder::OpPayloadBuilderAttributes;
use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};
use reth_optimism_rpc::{
    debug::{DebugApiExt, DebugApiOverrideServer},
    eth::proofs::{EthApiExt, EthApiOverrideServer},
};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_tasks::TaskExecutor;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

/// Attaches the ExEx and the RPC overrides to `$builder`.
///
/// Both entry points call this; keeping the body in one place is the point of the module.
macro_rules! install_proofs_history {
    ($builder:expr, $args:expr, $mdbx:expr) => {{
        let storage: OpProofsStorage<Arc<_>> = $mdbx.into();
        let storage_exec = storage.clone();

        let RollupArgs {
            proofs_history_window,
            proofs_history_prune_interval,
            proofs_history_verification_interval,
            ..
        } = *$args;

        $builder
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
                info!(
                    target: "reth::cli",
                    "Installing proofs-history RPC overrides (eth_getProof, debug_*)"
                );
                let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), storage.clone());
                // `Attrs` is a phantom parameter on `DebugApiExt` — nothing in `new()` pins it, so
                // in a generic context it has to be named explicitly for `into_rpc()` to resolve.
                let debug_ext: DebugApiExt<
                    _,
                    _,
                    _,
                    _,
                    OpPayloadBuilderAttributes<OpTransactionSigned>,
                > = DebugApiExt::new(
                    ctx.node().provider().clone(),
                    ctx.registry.eth_api().clone(),
                    storage,
                    ctx.node().task_executor().clone(),
                    ctx.node().evm_config().clone(),
                );
                ctx.modules.replace_configured(api_ext.into_rpc())?;
                ctx.modules.replace_configured(debug_ext.into_rpc())?;
                info!(target: "reth::cli", "Proofs-history RPC overrides installed");
                Ok(())
            })
    }};
}

/// Installs the proofs-history ExEx and RPC overrides on a bare builder (the test-harness path).
///
/// The caller decides how to launch; this only attaches the hooks. Pair it with
/// [`spawn_proofs_db_metrics`] to also report sidecar DB metrics.
pub fn with_proofs_history<T, CB, AO, S>(
    builder: NodeBuilderWithComponents<T, CB, AO>,
    args: &RollupArgs,
    mdbx: Arc<S>,
) -> NodeBuilderWithComponents<T, CB, AO>
where
    T: FullNodeTypes<
            Types: NodeTypes<Primitives = OpPrimitives, ChainSpec: reth_optimism_forks::OpHardforks>,
        >,
    CB: NodeComponentsBuilder<T>,
    <CB::Components as reth_node_builder::components::NodeComponents<T>>::Evm:
        reth_optimism_evm::ConfigurePostExecEvm<Primitives = OpPrimitives>,
    <<CB::Components as reth_node_builder::components::NodeComponents<T>>::Evm as reth_evm::ConfigureEvm>::NextBlockEnvCtx:
        reth_node_api::BuildNextEnv<
            OpPayloadBuilderAttributes<OpTransactionSigned>,
            alloy_consensus::Header,
            <T::Types as NodeTypes>::ChainSpec,
        >,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>, EthApi: FullEthApi>,
    ErrorObject<'static>: From<<AO::EthApi as reth_rpc_eth_api::EthApiTypes>::Error>,
    S: OpProofsStore + DatabaseMetrics + Send + Sync + 'static,
{
    install_proofs_history!(builder, args, mdbx)
}

/// Same as [`with_proofs_history`], for a builder that already carries a launch context — the
/// path the shipped `op-reth` binary takes.
///
/// [`WithLaunchContext`] keeps its inner builder private, so it cannot be unwrapped and passed to
/// [`with_proofs_history`]; it forwards `install_exex` / `extend_rpc_modules` under the same
/// names, which is why both can share one macro body.
pub fn with_proofs_history_launch_ctx<T, CB, AO, S>(
    builder: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    args: &RollupArgs,
    mdbx: Arc<S>,
) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
where
    T: FullNodeTypes<
            Types: NodeTypes<Primitives = OpPrimitives, ChainSpec: reth_optimism_forks::OpHardforks>,
        >,
    CB: NodeComponentsBuilder<T>,
    <CB::Components as reth_node_builder::components::NodeComponents<T>>::Evm:
        reth_optimism_evm::ConfigurePostExecEvm<Primitives = OpPrimitives>,
    <<CB::Components as reth_node_builder::components::NodeComponents<T>>::Evm as reth_evm::ConfigureEvm>::NextBlockEnvCtx:
        reth_node_api::BuildNextEnv<
            OpPayloadBuilderAttributes<OpTransactionSigned>,
            alloy_consensus::Header,
            <T::Types as NodeTypes>::ChainSpec,
        >,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>, EthApi: FullEthApi>,
    ErrorObject<'static>: From<<AO::EthApi as reth_rpc_eth_api::EthApiTypes>::Error>,
    S: OpProofsStore + DatabaseMetrics + Send + Sync + 'static,
{
    install_proofs_history!(builder, args, mdbx)
}

/// Spawns a task that periodically reports metrics for the proofs DB.
pub fn spawn_proofs_db_metrics<S>(
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
