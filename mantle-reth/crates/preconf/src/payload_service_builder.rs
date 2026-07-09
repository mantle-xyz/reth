//! Reth `PayloadServiceBuilder` impl for mantle's preconf payload
//! pipeline.
//!
//! Plugs into the cli's `ComponentsBuilder::payload(...)` slot the
//! same way the upstream [`BasicPayloadServiceBuilder`] does, but
//! constructs a [`PreconfPayloadJobGenerator`] + spawns
//! [`PayloadBuilderService`] driving it (rather than upstream's sync
//! [`BasicPayloadJobGenerator`]).
//!
//! **Naming disambiguation**: this module hosts
//! [`MantlePreconfServiceBuilder`], which implements reth's
//! [`PayloadServiceBuilder`] trait — a **reth node-builder plumbing
//! type** activated by the components builder. Do not confuse it with
//! [`crate::service_builder::PreconfServiceBuilder`], which is the
//! **application-level owner** of `Arc<PreconfConfig>` /
//! `Arc<PreconfTxSet>` / `Option<Arc<PreconfJournal>>` shared handles.
//! The two collaborate: the app-level builder is constructed first at
//! cli startup and its `cfg` / `fifo` flow into this reth-facing
//! builder via [`MantlePreconfServiceBuilder::new`].
//!
//! **OP-stack specific** — the generator hardcodes
//! `OpPayloadAttrs` (RPC) → `OpPayloadBuilderAttributes<N::SignedTx>`
//! (builder) conversion, mirroring upstream's `convert_build_args`.
//! `N` (`NodePrimitives`) stays generic so this works with both
//! `OpPrimitives` and any future mantle-specific OP-derived primitives.
//!
//! [`BasicPayloadServiceBuilder`]: reth_node_builder::components::BasicPayloadServiceBuilder
//! [`BasicPayloadJobGenerator`]: reth_basic_payload_builder::BasicPayloadJobGenerator
//! [`PayloadBuilderService`]: reth_payload_builder::PayloadBuilderService

use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::TxEnvelope;
use reth_chain_state::CanonStateSubscriptions;
use reth_node_api::{FullNodeTypes, NodeTypes, PayloadTypes};
use reth_node_builder::{
    BuilderContext,
    components::{BasicPayloadServiceBuilder, PayloadServiceBuilder},
};
use reth_optimism_evm::ConfigurePostExecEvm;
use reth_optimism_node::{OpBuiltPayload, node::OpPayloadBuilder};
use reth_optimism_payload_builder::{
    OpPayloadAttrs, OpPayloadBuilderAttributes, OpPayloadPrimitives, config::OpBuilderConfig,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_payload_primitives::BuildNextEnv;
use reth_primitives_traits::{HeaderTy, TxTy};
use reth_storage_api::BlockReaderIdExt;

use crate::{
    PreconfConfig, PreconfServiceBuilder, PreconfTxSet,
    builder::{
        payload_builder::PreconfPayloadBuilder,
        payload_job_generator::PreconfPayloadJobGenerator,
    },
};

/// [`PayloadServiceBuilder`] that wires the mantle preconf-aware
/// payload builder + generator into reth's payload service.
///
/// Holds the shared preconf state (config + fifo) that the cli
/// constructed once at startup, plus the OP builder settings
/// ([`OpBuilderConfig`]: DA / gas-limit / sdm-enable). On
/// [`Self::spawn_payload_builder_service`] it assembles a
/// [`PreconfPayloadBuilder`] from the runtime-provided pool / provider
/// / evm-config, wraps it in a [`PreconfPayloadJobGenerator`], and
/// spawns [`PayloadBuilderService`] via the task executor.
///
/// `N` is bound on the struct because the
/// [`PreconfPayloadJobGenerator`]'s associated `Job` type names
/// `OpBuiltPayload<N>` concretely. For mantle's OP-stack target, pick
/// `N = OpPrimitives`.
pub struct MantlePreconfServiceBuilder<N> {
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
    /// OP builder settings (DA limits, max gas per tx, sdm-enable, ...).
    builder_config: OpBuilderConfig,
    /// Optional handle to the application-level service builder. The
    /// wire-event publisher lives on `svc` and is populated by
    /// [`crate::PreconfServiceBuilder::start`] — which runs during
    /// [`MantlePoolBuilder::build_pool`], **before** reth invokes
    /// [`Self::spawn_payload_builder_service`]. So this indirection
    /// lets us read the publisher lazily at spawn time, after `start`
    /// has run. `None` when preconf is disabled on the node.
    svc: Option<Arc<PreconfServiceBuilder>>,
    /// `fn() -> N` marker so the struct is `Send + Sync` without
    /// constraining `N` itself.
    _pd: PhantomData<fn() -> N>,
}

impl<N> MantlePreconfServiceBuilder<N> {
    /// Construct a new service builder bound to shared preconf state
    /// and OP builder settings. `svc` is optional; when `Some`, the
    /// spawned payload builder pulls the wire-event publisher lazily
    /// via [`PreconfServiceBuilder::event_publisher`] at spawn time.
    pub const fn new(
        cfg: Arc<PreconfConfig>,
        fifo: Arc<PreconfTxSet>,
        builder_config: OpBuilderConfig,
        svc: Option<Arc<PreconfServiceBuilder>>,
    ) -> Self {
        Self { cfg, fifo, builder_config, svc, _pd: PhantomData }
    }
}

impl<N> std::fmt::Debug for MantlePreconfServiceBuilder<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MantlePreconfServiceBuilder")
            .field("cfg", &self.cfg)
            .field("fifo", &self.fifo)
            .field("builder_config", &self.builder_config)
            .finish_non_exhaustive()
    }
}

impl<Node, Pool, EvmConfig, N> PayloadServiceBuilder<Node, Pool, EvmConfig>
    for MantlePreconfServiceBuilder<N>
where
    // Reth node-builder bounds. The engine's PayloadAttributes is the
    // RPC variant (`OpPayloadAttrs`); our generator unwraps to the
    // inner `OpPayloadAttributes` and converts to builder-variant
    // (`OpPayloadBuilderAttributes<N::SignedTx>`) before calling
    // `build_payload`.
    Node: FullNodeTypes<
            Types: NodeTypes<
                Primitives = N,
                Payload: PayloadTypes<
                    BuiltPayload = OpBuiltPayload<N>,
                    PayloadAttributes = OpPayloadAttrs,
                >,
            >,
        >,
    Node::Provider: BlockReaderIdExt<Header = HeaderTy<N>>
        + reth_chainspec::ChainSpecProvider<ChainSpec: reth_optimism_forks::OpHardforks>
        + reth_storage_api::StateProviderFactory
        + CanonStateSubscriptions<Primitives = N>
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
    <Node::Provider as reth_chainspec::ChainSpecProvider>::ChainSpec:
        reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_optimism_txpool::OpPooledTx<Consensus = N::SignedTx>,
        > + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
    EvmConfig: ConfigurePostExecEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<
                OpPayloadBuilderAttributes<TxTy<N>>,
                HeaderTy<N>,
                <Node::Provider as reth_chainspec::ChainSpecProvider>::ChainSpec,
            >,
        > + Clone
        + Send
        + 'static,
    N: OpPayloadPrimitives,
    N::SignedTx: From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>>
        + TryFrom<TxEnvelope>,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: EvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        let Self { cfg, fifo, builder_config, svc, _pd } = self;

        // Rollback safety — when `--preconf.enable` is absent, `cfg` is
        // `PreconfConfig::default()` with `enabled: false`. In that case
        // the fork's `PreconfPayloadBuilder` (select! loop, sweep-ticker
        // gas quota, fifo carryover replay, ...) is bypassed entirely
        // and reth's upstream `BasicPayloadServiceBuilder<OpPayloadBuilder>`
        // is spawned instead. This gives operators a clean rollback path
        // if a fatal preconf-fork bug is discovered in production —
        // omitting `--preconf.enable` yields the same payload service
        // as vanilla op-reth, no code revert needed.
        if !cfg.enabled {
            tracing::info!(
                target: "mantle::preconf::payload",
                "preconf disabled — delegating to upstream OP payload builder",
            );
            let upstream = OpPayloadBuilder::new(false)
                .with_da_config(builder_config.da_config.clone())
                .with_gas_limit_config(builder_config.gas_limit_config.clone())
                .with_sdm_enabled(builder_config.sdm_enabled);
            return BasicPayloadServiceBuilder::new(upstream)
                .spawn_payload_builder_service(ctx, pool, evm_config)
                .await;
        }

        // Read the publisher lazily — `start` populates it during
        // `build_pool`, which runs before this method.
        let publisher = svc.as_ref().and_then(|s| s.event_publisher());

        let builder = PreconfPayloadBuilder::new(
            pool,
            ctx.provider().clone(),
            evm_config,
            builder_config,
            cfg,
            fifo,
            publisher,
        );

        let generator: PreconfPayloadJobGenerator<Pool, Node::Provider, EvmConfig, N> =
            PreconfPayloadJobGenerator::new(builder);

        let (payload_service, payload_service_handle) =
            PayloadBuilderService::new(generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical_task("mantle preconf payload builder service", payload_service);

        Ok(payload_service_handle)
    }
}
