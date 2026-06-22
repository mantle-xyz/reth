//! Mantle node type configuration.
//!
//! [`MantleNode`] is a thin wrapper over [`OpNode`] that substitutes the standard
//! [`OpPoolBuilder`] with [`MantlePoolBuilder`], adding Mantle-specific transaction
//! validation on top of the OP stack checks.

use crate::txpool::MantleTransactionValidator;
use mantle_reth_preconf::{
    PreconfAwareValidator, PreconfConfig, PreconfPoolListener, PreconfServiceBuilder, PreconfTxSet,
};
use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
use op_alloy_consensus::OpTxEnvelope;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, PrimitivesTy, TxTy};
use reth_node_builder::{
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, PoolBuilder, PoolBuilderConfigOverrides,
        TxPoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::BasicEngineValidatorBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{
    OpAddOns, OpConsensusBuilder, OpExecutorBuilder, OpFullNodeTypes, OpNetworkBuilder,
    OpNodeTypes, args::RollupArgs, engine::OpEngineTypes,
    node::OpPayloadBuilder as OpNodePayloadBuilder, rpc::OpEthApiBuilder,
};
use reth_optimism_payload_builder::config::{OpDAConfig, OpGasLimitConfig};
use reth_optimism_primitives::OpPrimitives;
use reth_optimism_storage::OpStorage;
use reth_optimism_txpool::{OpPool, OpPooledTransaction, OpPooledTx};
use reth_provider::CanonStateSubscriptions;
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPoolTransaction, Pool, TransactionValidationTaskExecutor,
    blobstore::DiskFileBlobStore,
};
use tracing::info;

use std::sync::Arc;

use reth_optimism_node::{OpEngineApiBuilder, OpEngineValidatorBuilder};

/// Type alias for the Mantle transaction pool.
///
/// Same structure as `OpTransactionPool` but the inner validator chain is:
///
/// ```text
/// PreconfAwareValidator<MantleTransactionValidator<OpTransactionValidator<...>>>
/// ```
///
/// The outermost [`PreconfAwareValidator`] is always present in the type;
/// when preconf is not wired up via [`MantlePoolBuilder::with_preconf`], it
/// holds a default-disabled `PreconfConfig` and an empty `PreconfTxSet` —
/// effectively a no-op layer (both replacement guard and gas-ceiling check
/// short-circuit on the empty / disabled state).
pub type MantleTransactionPool<Client, S, Evm, T = OpPooledTransaction> = OpPool<
    Pool<
        TransactionValidationTaskExecutor<
            PreconfAwareValidator<
                MantleTransactionValidator<
                    reth_optimism_txpool::OpTransactionValidator<Client, T, Evm>,
                >,
            >,
        >,
        CoinbaseTipOrdering<T>,
        S,
    >,
>;

/// Mantle pool builder.
///
/// Wraps [`OpPoolBuilder`] but adds [`MantleTransactionValidator`] to reject:
/// - EIP-155 unprotected transactions (legacy type 0 without `chain_id`)
/// - Legacy `MetaTx` transactions (disabled since `MantleEverest`)
///
/// Mantle does not use OP Stack interop, so supervisor/interop logic is omitted.
#[derive(Debug, Clone)]
pub struct MantlePoolBuilder<T = OpPooledTransaction> {
    pool_config_overrides: PoolBuilderConfigOverrides,
    enable_tx_conditional: bool,
    /// Optional preconfirmation wiring. `None` ⇒ preconf disabled; the
    /// validator chain still includes a `PreconfAwareValidator`, but it
    /// receives a default-disabled config and an empty fifo so its checks
    /// short-circuit. `Some(_)` ⇒ both validator and pool listener are
    /// driven by the provided `cfg` / `fifo`.
    preconf: Option<PreconfWiring>,
    _pd: core::marker::PhantomData<T>,
}

/// Preconf wiring bundle held by [`MantlePoolBuilder`]. Constructed and
/// owned by the preconf service builder; the pool builder only needs the
/// shared handles.
#[derive(Debug, Clone)]
pub struct PreconfWiring {
    /// Runtime preconf configuration; cloned into the validator.
    pub cfg: Arc<PreconfConfig>,
    /// Commitment fifo shared between validator, RPC handler, and builder.
    pub fifo: Arc<PreconfTxSet>,
}

impl<T> Default for MantlePoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            enable_tx_conditional: false,
            preconf: None,
            _pd: core::marker::PhantomData,
        }
    }
}

impl<T> MantlePoolBuilder<T> {
    /// Sets the `enable_tx_conditional` flag.
    pub fn with_enable_tx_conditional(mut self, enable_tx_conditional: bool) -> Self {
        self.enable_tx_conditional = enable_tx_conditional;
        self
    }

    /// Sets the [`PoolBuilderConfigOverrides`].
    pub fn with_pool_config_overrides(
        mut self,
        pool_config_overrides: PoolBuilderConfigOverrides,
    ) -> Self {
        self.pool_config_overrides = pool_config_overrides;
        self
    }

    /// Enable preconfirmation: thread `cfg` and `fifo` into the validator
    /// decoration chain and spawn the pool listener that pushes
    /// whitelisted txs into `fifo`.
    pub fn with_preconf(mut self, cfg: Arc<PreconfConfig>, fifo: Arc<PreconfTxSet>) -> Self {
        self.preconf = Some(PreconfWiring { cfg, fifo });
        self
    }
}

impl<N, T, Evm> PoolBuilder<N, Evm> for MantlePoolBuilder<T>
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec: OpHardforks>>,
    T: EthPoolTransaction<Consensus = TxTy<N::Types>> + OpPooledTx,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<N::Types>> + Clone + 'static,
    // PreconfPoolListener bridges the pool's consensus tx into `OpTxEnvelope`
    // (and drops Deposit / PostExec). This bound makes the conversion path
    // discoverable to the compiler; for any OP-stack `NodePrimitives` it is
    // satisfied by `impl From<OpTransactionSigned> for OpTxEnvelope` in
    // `op-reth/crates/primitives/src/transaction/signed.rs`.
    OpTxEnvelope: From<TxTy<N::Types>>,
{
    type Pool = MantleTransactionPool<N::Provider, DiskFileBlobStore, Evm, T>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<N>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;

        // Resolve preconf wiring once for both the validator decoration
        // and the optional listener spawn below. When no caller has wired
        // up preconf (`with_preconf` not invoked), supply default-disabled
        // handles so the `PreconfAwareValidator` layer becomes a cheap
        // pass-through (empty fifo + disabled cfg).
        let (preconf_cfg, preconf_fifo) = match self.preconf.clone() {
            Some(p) => (p.cfg, p.fifo),
            None => {
                let cfg = Arc::new(PreconfConfig::default());
                let fifo = Arc::new(PreconfTxSet::new(cfg.broadcast_cap));
                (cfg, fifo)
            }
        };
        // Clones moved into the validator-build closure. The listener
        // (spawned later) takes a separate clone of the same Arcs.
        let validator_cfg = preconf_cfg.clone();
        let validator_fifo = preconf_fifo.clone();

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .no_eip4844()
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .kzg_settings(ctx.kzg_settings()?)
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(
                    self.pool_config_overrides
                        .additional_validation_tasks
                        .unwrap_or_else(|| ctx.config().txpool.additional_validation_tasks),
                )
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
                .map(move |validator| {
                    let op_validator = reth_optimism_txpool::OpTransactionValidator::new(validator)
                        .require_l1_data_gas_fee(!ctx.config().dev.dev);
                    let mantle_validator = MantleTransactionValidator::new(op_validator);
                    // `.map` takes `FnMut`; clone the Arcs on every call.
                    PreconfAwareValidator::new(
                        mantle_validator,
                        validator_cfg.clone(),
                        validator_fifo.clone(),
                    )
                });

        let final_pool_config = self.pool_config_overrides.apply(ctx.pool_config());

        let inner_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, final_pool_config.clone());

        // Mantle does not use OP interop — filter is always disabled
        let transaction_pool = OpPool::new(inner_pool, false);

        reth_node_builder::components::spawn_maintenance_tasks(
            ctx,
            transaction_pool.clone(),
            &final_pool_config,
        )?;

        // Spawn the pool listener only when preconf is actually enabled —
        // when `with_preconf` was not called, `preconf_cfg.enabled` is
        // false and the listener would just sit idle filtering every tx
        // against empty whitelists. Skipping the spawn saves a task.
        if preconf_cfg.enabled {
            let listener = PreconfPoolListener::new(
                transaction_pool.clone(),
                preconf_cfg.clone(),
                preconf_fifo.clone(),
            );
            ctx.task_executor().spawn_critical_task("mantle-preconf-pool-listener", listener.run());
            info!(target: "reth::cli", "Mantle preconf pool listener spawned");
        }

        if self.enable_tx_conditional {
            let chain_events = ctx.provider().canonical_state_stream();
            ctx.task_executor().spawn_critical_task(
                "Mantle txpool conditional maintenance task",
                reth_optimism_txpool::maintain::maintain_transaction_pool_conditional_future(
                    transaction_pool.clone(),
                    chain_events,
                ),
            );
        }

        info!(target: "reth::cli", "Mantle transaction pool initialized");

        Ok(transaction_pool)
    }
}

/// Type alias for the Mantle node component builder.
pub type MantleNodeComponentBuilder<N, Payload = OpNodePayloadBuilder> = ComponentsBuilder<
    N,
    MantlePoolBuilder,
    BasicPayloadServiceBuilder<Payload>,
    OpNetworkBuilder,
    OpExecutorBuilder,
    OpConsensusBuilder,
>;

/// Mantle node type configuration.
///
/// A newtype wrapper over [`OpNode`](reth_optimism_node::OpNode) that replaces
/// [`OpPoolBuilder`] with [`MantlePoolBuilder`] to enforce Mantle-specific
/// transaction-pool validation rules.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MantleNode {
    /// Underlying OP node configuration.
    pub op_node: reth_optimism_node::OpNode,
    /// Optional preconfirmation subsystem handle. `None` ⇒ preconf
    /// disabled (default); the node behaves exactly like the
    /// underlying OP node. `Some` ⇒ the validator chain, pool listener,
    /// canonical-state cleaner, and RPC handler all get wired up to
    /// the shared `cfg` / `fifo` held by the builder.
    pub preconf: Option<Arc<PreconfServiceBuilder>>,
}

impl MantleNode {
    /// Creates a new [`MantleNode`] with the given rollup arguments.
    pub fn new(args: RollupArgs) -> Self {
        Self { op_node: reth_optimism_node::OpNode::new(args), preconf: None }
    }

    /// Configure the data availability configuration for the Mantle builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.op_node = self.op_node.with_da_config(da_config);
        self
    }

    /// Configure the gas limit configuration for the Mantle builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: OpGasLimitConfig) -> Self {
        self.op_node = self.op_node.with_gas_limit_config(gas_limit_config);
        self
    }

    /// Enable the preconfirmation subsystem on this node. The provided
    /// builder owns the shared `cfg` / `fifo` handles; the same handles
    /// thread into the validator chain, the pool listener (spawned by
    /// [`MantlePoolBuilder::build_pool`]), the canonical-state handler
    /// (spawned in [`MantleNode::add_ons`]), and the RPC handler
    /// (injected into [`MantleRpcExt`]).
    pub fn with_preconf(mut self, builder: PreconfServiceBuilder) -> Self {
        self.preconf = Some(Arc::new(builder));
        self
    }

    /// Returns the component builder for this Mantle node.
    pub fn components<N>(&self) -> MantleNodeComponentBuilder<N>
    where
        N: FullNodeTypes<Types: OpNodeTypes>,
    {
        let args = &self.op_node.args;

        // Activate pool-side preconf wiring (validator decoration + spawned
        // pool listener inside `build_pool`) if the node was configured
        // for preconf. Otherwise the pool builder stays in its default
        // pass-through state.
        let mut pool_builder =
            MantlePoolBuilder::default().with_enable_tx_conditional(args.enable_tx_conditional);
        if let Some(p) = &self.preconf {
            pool_builder = pool_builder.with_preconf(p.cfg().clone(), p.fifo().clone());
        }

        ComponentsBuilder::default()
            .node_types::<N>()
            .executor(OpExecutorBuilder::default().with_sdm_enabled(args.sdm_enabled))
            .pool(pool_builder)
            .payload(BasicPayloadServiceBuilder::new(
                OpNodePayloadBuilder::new(args.compute_pending_block)
                    .with_da_config(self.op_node.da_config.clone())
                    .with_gas_limit_config(self.op_node.gas_limit_config.clone())
                    .with_sdm_enabled(args.sdm_enabled),
            ))
            .network(OpNetworkBuilder::new(args.disable_txpool_gossip, !args.discovery_v4))
            .consensus(OpConsensusBuilder::default())
    }
}

impl NodeTypes for MantleNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

impl<N> Node<N> for MantleNode
where
    N: FullNodeTypes<Types: OpFullNodeTypes + OpNodeTypes>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        MantlePoolBuilder,
        BasicPayloadServiceBuilder<OpNodePayloadBuilder>,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        let sequencer_url = self.op_node.args.sequencer.clone();
        let preconf = self.preconf.clone();
        let mut add_ons: Self::AddOns = self.op_node.add_ons_builder().build();
        add_ons = add_ons.extend_rpc_modules(move |ctx| {
            // Build SequencerClient if a sequencer URL is configured.
            // SequencerClient::new is async; use block_in_place since we're inside tokio.
            let sequencer_client = sequencer_url
                .map(|url| {
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(reth_optimism_rpc::SequencerClient::new(url))
                    })
                })
                .transpose()
                .map_err(|e| eyre::eyre!("failed to create SequencerClient: {e}"))?;

            // If preconf is enabled, spin up the canonical-state handler and
            // build the RPC handler. The pool listener is already spawned
            // by `MantlePoolBuilder::build_pool` when its `with_preconf`
            // was called during `components()`.
            let preconf_handler: Option<Arc<dyn mantle_reth_rpc_ext::DynPreconfHandler>> =
                preconf.as_ref().map(|svc| {
                    let canon = svc.canon_handler(ctx.node().provider().clone());
                    ctx.node()
                        .task_executor()
                        .spawn_critical_task("mantle-preconf-canon-handler", canon.run());
                    info!(target: "reth::cli", "Mantle preconf canonical-state handler spawned");

                    let handler =
                        svc.rpc_handler(ctx.node().pool().clone(), ctx.node().provider().clone());
                    Arc::new(handler) as Arc<dyn mantle_reth_rpc_ext::DynPreconfHandler>
                });

            let mantle_ext = MantleRpcExt::new(
                ctx.node().provider().clone(),
                Arc::new(ctx.registry.eth_api().clone()),
                sequencer_client,
                preconf_handler,
            );
            ctx.modules.merge_configured(mantle_ext.into_rpc())?;
            info!(target: "reth::cli", "Mantle RPC extensions registered");
            Ok(())
        });
        add_ons
    }
}
