//! Request-scoped execution against one published consumer snapshot.
//!
//! State remains anchored to the canonical parent hash. Only the execution header
//! is substituted, so the standard call and simulation implementations can derive
//! the correct EVM configuration without publishing a synthetic pending block.

use std::{collections::BTreeMap, sync::Arc};

use alloy_consensus::Header;
use alloy_eips::BlockId;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{
    BlockOverrides,
    state::{EvmOverrides, StateOverride, StateOverridesBuilder},
};
use reth_evm::EvmEnvFor;
use reth_primitives_traits::{Block, BlockTy, NodePrimitives, RecoveredBlock, SealedHeader};
use reth_revm::database::EvmStateProvider;
use reth_rpc_eth_api::{
    EthApiTypes, RpcNodeCore, RpcNodeCoreExt, RpcTxReq,
    helpers::{
        Call, EthCall, LoadBlock, LoadPendingBlock, LoadState, SpawnBlocking,
        estimate::EstimateCall, pending_block::PendingEnvBuilder,
    },
};
use reth_rpc_eth_types::{EthStateCache, PendingBlock, builder::config::PendingBlockKind};
use reth_tasks::{
    Runtime,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use tokio::sync::{Mutex, Semaphore};

use crate::PendingBlocks;

/// State and execution header captured by a single read of the consumer snapshot.
#[derive(Debug, Clone)]
pub struct PendingExecutionContext {
    /// Canonical parent of the earliest preview block, pinned by hash.
    pub base_block: BlockId,
    /// Latest preview header. Its hash is zero until the producer seals the block.
    pub header: SealedHeader<Header>,
    /// Cumulative state changes from the canonical parent through the preview tip.
    pub state: StateOverride,
    /// L1 attributes for the preview tip, used by Mantle estimation fee checks.
    pub l1_block_info: op_revm::L1BlockInfo,
    /// Published parent hashes for preview blocks, as used by consumer replay.
    pub block_hashes: BTreeMap<u64, B256>,
}

impl PendingExecutionContext {
    pub(crate) fn from_pending(pending: &PendingBlocks) -> Self {
        let block_hashes = pending.block_hash_overrides();
        Self {
            base_block: pending.parent_hash().into(),
            header: {
                let header = pending.latest_header();
                SealedHeader::new(header.inner().clone(), header.hash())
            },
            state: pending.get_state_overrides().unwrap_or_default(),
            l1_block_info: pending.latest_block_l1_block_info().clone(),
            block_hashes,
        }
    }

    /// Applies user overrides after the snapshot; user block hashes win per height.
    pub fn overrides(
        &self,
        state: Option<StateOverride>,
        block: Option<Box<BlockOverrides>>,
    ) -> EvmOverrides {
        let state = StateOverridesBuilder::new(self.state.clone())
            .extend(state.unwrap_or_default())
            .build();
        let mut block = block.unwrap_or_default();
        let mut hashes = self.block_hashes.clone();
        // The first simulated block can query the preview tip. Pin its published
        // hash (zero while unsealed), even if canonical history advances later.
        hashes.insert(self.header.number, self.header.hash());
        hashes.extend(block.block_hash.take().unwrap_or_default());
        block.block_hash = Some(hashes);
        EvmOverrides::new(Some(state), Some(block))
    }
}

/// A private view of the Eth API for a single consumer request.
///
/// Delegates execution, scheduling, limits and conversion to the node's API.
/// `evm_env_at` supplies the preview header for calls; `recovered_block` supplies
/// it as the parent for the standard simulator, which executes subsequent blocks.
#[derive(Debug, Clone)]
pub struct PendingEthApi<Eth> {
    inner: Eth,
    context: Arc<PendingExecutionContext>,
}

impl<Eth> PendingEthApi<Eth> {
    /// Captures the execution context for the lifetime of this request.
    pub fn new(inner: Eth, context: PendingExecutionContext) -> Self {
        Self { inner, context: Arc::new(context) }
    }
}

impl<Eth: EthCall> PendingEthApi<Eth>
where
    <Eth::Provider as reth_chainspec::ChainSpecProvider>::ChainSpec:
        reth_optimism_forks::OpHardforks,
{
    /// Reads fee attributes and the current token ratio from the captured preview state.
    pub async fn l1_block_info(&self) -> Result<op_revm::L1BlockInfo, Eth::Error> {
        use alloy_evm::overrides::apply_state_overrides;
        use reth_rpc_eth_api::FromEthApiError;
        use reth_rpc_eth_types::EthApiError;
        use revm::Database;

        let context = Arc::clone(&self.context);
        self.inner
            .spawn_with_state_at_block(context.base_block, move |_, mut db| {
                apply_state_overrides(context.state.clone(), &mut db)
                    .map_err(Eth::Error::from_eth_err)?;
                let mut info = context.l1_block_info.clone();
                info.token_ratio = db
                    .storage(
                        op_revm::constants::GAS_ORACLE_CONTRACT,
                        op_revm::constants::TOKEN_RATIO_SLOT,
                    )
                    .map_err(EthApiError::from)
                    .map_err(Eth::Error::from_eth_err)?;
                Ok(info)
            })
            .await
    }

    /// Retains Mantle's post-estimation L1/operator fee check using preview state.
    pub async fn check_estimate_funds(
        &self,
        request: alloy_rpc_types_eth::TransactionRequest,
        estimate: U256,
        overrides: Option<StateOverride>,
    ) -> Result<(), Eth::Error> {
        use alloy_consensus::{SignableTransaction, TxEip1559};
        use alloy_eips::Encodable2718;
        use alloy_evm::overrides::apply_state_overrides;
        use reth_chainspec::ChainSpecProvider;
        use reth_optimism_forks::OpHardforks;
        use reth_rpc_eth_api::FromEthApiError;
        use reth_rpc_eth_types::EthApiError;
        use revm::Database;

        let fee_cap = request.max_fee_per_gas.or(request.gas_price).unwrap_or_default();
        let Some(from) = request.from.filter(|_| fee_cap != 0) else { return Ok(()) };
        let context = Arc::clone(&self.context);
        self.inner
            .spawn_with_state_at_block(context.base_block, move |this, mut db| {
                let chain_spec = this.provider().chain_spec();
                if !chain_spec.is_mantle() ||
                    !chain_spec.is_mantle_arsia_active_at_timestamp(context.header.timestamp)
                {
                    return Ok(());
                }
                apply_state_overrides(overrides.unwrap_or_default(), &mut db)
                    .map_err(Eth::Error::from_eth_err)?;
                let balance = db
                    .basic(from)
                    .map_err(EthApiError::from)
                    .map_err(Eth::Error::from_eth_err)?
                    .map(|account| account.balance)
                    .unwrap_or_default();
                let mut l1_info = context.l1_block_info.clone();
                l1_info.token_ratio = db
                    .storage(
                        op_revm::constants::GAS_ORACLE_CONTRACT,
                        op_revm::constants::TOKEN_RATIO_SLOT,
                    )
                    .map_err(EthApiError::from)
                    .map_err(Eth::Error::from_eth_err)?;
                let gas_limit = u64::try_from(estimate).unwrap_or(u64::MAX);
                // Same unsigned fee envelope as OpEthApi's Arsia funds check.
                let envelope = TxEip1559 {
                    chain_id: 0,
                    nonce: request.nonce.unwrap_or_default(),
                    gas_limit,
                    max_fee_per_gas: fee_cap,
                    max_priority_fee_per_gas: request
                        .gas_price
                        .or(request.max_priority_fee_per_gas)
                        .unwrap_or_default(),
                    to: alloy_primitives::TxKind::Create,
                    value: request.value.unwrap_or_default(),
                    input: request.input.input().cloned().unwrap_or_default(),
                    ..Default::default()
                }
                .into_signed(alloy_primitives::Signature::new(U256::ZERO, U256::ZERO, false))
                .encoded_2718();
                mantle_reth_eth_api::mantle_arsia_check_funds(
                    &mantle_reth_eth_api::ArsiaFundsCheck {
                        gas_limit,
                        fee_cap: U256::from(fee_cap),
                        value: request.value.unwrap_or_default(),
                        from_balance: balance,
                        l1_block_info: &l1_info,
                        tx_envelope: &envelope,
                        chain_spec: chain_spec.as_ref(),
                        timestamp: context.header.timestamp,
                    },
                )
                .map_err(|error| {
                    Eth::Error::from_eth_err(EthApiError::InvalidParams(error.to_string()))
                })
            })
            .await
    }
}

impl<Eth: EthApiTypes> EthApiTypes for PendingEthApi<Eth> {
    type Error = Eth::Error;
    type NetworkTypes = Eth::NetworkTypes;
    type RpcConvert = Eth::RpcConvert;
    fn converter(&self) -> &Self::RpcConvert {
        self.inner.converter()
    }
}

impl<Eth: RpcNodeCore> RpcNodeCore for PendingEthApi<Eth> {
    type Primitives = Eth::Primitives;
    type Provider = Eth::Provider;
    type Pool = Eth::Pool;
    type Evm = Eth::Evm;
    type Network = Eth::Network;
    fn pool(&self) -> &Self::Pool {
        self.inner.pool()
    }
    fn evm_config(&self) -> &Self::Evm {
        self.inner.evm_config()
    }
    fn network(&self) -> &Self::Network {
        self.inner.network()
    }
    fn provider(&self) -> &Self::Provider {
        self.inner.provider()
    }
}

impl<Eth: RpcNodeCoreExt> RpcNodeCoreExt for PendingEthApi<Eth> {
    fn cache(&self) -> &EthStateCache<Self::Primitives> {
        self.inner.cache()
    }
}

impl<Eth: SpawnBlocking> SpawnBlocking for PendingEthApi<Eth> {
    fn io_task_spawner(&self) -> &Runtime {
        self.inner.io_task_spawner()
    }
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.tracing_task_pool()
    }
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.tracing_task_guard()
    }
    fn blocking_io_task_guard(&self) -> &Arc<Semaphore> {
        self.inner.blocking_io_task_guard()
    }
}

impl<Eth: LoadPendingBlock> LoadPendingBlock for PendingEthApi<Eth> {
    fn pending_block(&self) -> &Mutex<Option<PendingBlock<Self::Primitives>>> {
        self.inner.pending_block()
    }
    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<Self::Evm> {
        self.inner.pending_env_builder()
    }
    fn pending_block_kind(&self) -> PendingBlockKind {
        self.inner.pending_block_kind()
    }
}

impl<Eth: EthCall> LoadState for PendingEthApi<Eth>
where
    Eth::Primitives: NodePrimitives<BlockHeader = Header>,
{
    async fn evm_env_at(
        &self,
        at: BlockId,
    ) -> Result<(EvmEnvFor<Self::Evm>, BlockId), Self::Error> {
        if at == self.context.base_block {
            Ok((self.inner.evm_env_for_header(&self.context.header)?, at))
        } else {
            self.inner.evm_env_at(at).await
        }
    }
}

impl<Eth: EthCall> LoadBlock for PendingEthApi<Eth>
where
    Eth::Primitives: NodePrimitives<BlockHeader = Header>,
{
    async fn recovered_block(
        &self,
        at: BlockId,
    ) -> Result<Option<Arc<RecoveredBlock<BlockTy<Self::Primitives>>>>, Self::Error> {
        if at == self.context.base_block {
            // The simulator only consumes this header; state is loaded separately at `at`.
            let block = BlockTy::<Self::Primitives>::new_sealed(
                self.context.header.clone(),
                Default::default(),
            );
            Ok(Some(Arc::new(RecoveredBlock::new_sealed(block, vec![]))))
        } else {
            self.inner.recovered_block(at).await
        }
    }
}

impl<Eth: EthCall> Call for PendingEthApi<Eth>
where
    Eth::Primitives: NodePrimitives<BlockHeader = Header>,
{
    fn call_gas_limit(&self) -> u64 {
        self.inner.call_gas_limit()
    }
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.compute_state_root_for_eth_simulate()
    }
    fn evm_memory_limit(&self) -> u64 {
        self.inner.evm_memory_limit()
    }
}

impl<Eth: EthCall> EstimateCall for PendingEthApi<Eth>
where
    Eth::Primitives: NodePrimitives<BlockHeader = Header>,
{
    fn estimate_gas_with<S: EvmStateProvider>(
        &self,
        env: EvmEnvFor<Self::Evm>,
        request: RpcTxReq<Self::NetworkTypes>,
        state: S,
        overrides: EvmOverrides,
    ) -> Result<U256, Self::Error> {
        // Keep Mantle's gas estimator, including its fee-cap based allowance checks.
        self.inner.estimate_gas_with(env, request, state, overrides)
    }
}

impl<Eth: EthCall> EthCall for PendingEthApi<Eth> where
    Eth::Primitives: NodePrimitives<BlockHeader = Header>
{
}
