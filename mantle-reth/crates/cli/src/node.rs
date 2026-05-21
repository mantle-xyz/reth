//! Mantle node type configuration.
//!
//! [`MantleNode`] is a thin wrapper over [`OpNode`] that substitutes the standard
//! [`OpPoolBuilder`] with [`MantlePoolBuilder`], adding Mantle-specific transaction
//! validation on top of the OP stack checks.

use crate::txpool::MantleTransactionValidator;
use mantle_reth_rpc_ext::{MantleEthApiExtServer, MantleRpcExt};
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
/// Same structure as `OpTransactionPool` but the inner validator is wrapped in
/// [`MantleTransactionValidator`].
pub type MantleTransactionPool<Client, S, Evm, T = OpPooledTransaction> = OpPool<
    Pool<
        TransactionValidationTaskExecutor<
            MantleTransactionValidator<
                reth_optimism_txpool::OpTransactionValidator<Client, T, Evm>,
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
    _pd: core::marker::PhantomData<T>,
}

impl<T> Default for MantlePoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            enable_tx_conditional: false,
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
}

impl<N, T, Evm> PoolBuilder<N, Evm> for MantlePoolBuilder<T>
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec: OpHardforks>>,
    T: EthPoolTransaction<Consensus = TxTy<N::Types>> + OpPooledTx,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<N::Types>> + Clone + 'static,
{
    type Pool = MantleTransactionPool<N::Provider, DiskFileBlobStore, Evm, T>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<N>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;
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
                .map(|validator| {
                    let op_validator = reth_optimism_txpool::OpTransactionValidator::new(validator)
                        .require_l1_data_gas_fee(!ctx.config().dev.dev);
                    MantleTransactionValidator::new(op_validator)
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
}

impl MantleNode {
    /// Creates a new [`MantleNode`] with the given rollup arguments.
    pub fn new(args: RollupArgs) -> Self {
        Self { op_node: reth_optimism_node::OpNode::new(args) }
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

    /// Returns the component builder for this Mantle node.
    pub fn components<N>(&self) -> MantleNodeComponentBuilder<N>
    where
        N: FullNodeTypes<Types: OpNodeTypes>,
    {
        let args = &self.op_node.args;
        ComponentsBuilder::default()
            .node_types::<N>()
            .executor(OpExecutorBuilder::default().with_sdm_enabled(args.sdm_enabled))
            .pool(
                MantlePoolBuilder::default().with_enable_tx_conditional(args.enable_tx_conditional),
            )
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

            let mantle_ext = MantleRpcExt::new(
                ctx.node().provider().clone(),
                Arc::new(ctx.registry.eth_api().clone()),
                sequencer_client,
            );
            ctx.modules.merge_configured(mantle_ext.into_rpc())?;
            info!(target: "reth::cli", "Mantle RPC extensions registered");
            Ok(())
        });
        add_ons
    }
}
