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
    state::{AccountOverride, EvmOverrides, StateOverride},
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

    /// Applies user overrides to specified account fields and storage slots after the snapshot.
    pub fn overrides(
        &self,
        state: Option<StateOverride>,
        block: Option<Box<BlockOverrides>>,
    ) -> EvmOverrides {
        let mut merged_state = self.state.clone();
        for (address, account) in state.unwrap_or_default() {
            merge_account_override(merged_state.entry(address).or_default(), account);
        }
        let mut block = block.unwrap_or_default();
        let mut hashes = self.block_hashes.clone();
        // The first simulated block can query the preview tip. Pin its published
        // hash (zero while unsealed), even if canonical history advances later.
        hashes.insert(self.header.number, self.header.hash());
        hashes.extend(block.block_hash.take().unwrap_or_default());
        block.block_hash = Some(hashes);
        EvmOverrides::new(Some(merged_state), Some(block))
    }
}

/// User fields win even when zero or empty; unspecified fields retain preview values.
fn merge_account_override(preview: &mut AccountOverride, user: AccountOverride) {
    let AccountOverride { balance, nonce, code, state, state_diff, move_precompile_to } = user;
    if balance.is_some() {
        preview.balance = balance;
    }
    if nonce.is_some() {
        preview.nonce = nonce;
    }
    if code.is_some() {
        preview.code = code;
    }
    if move_precompile_to.is_some() {
        preview.move_precompile_to = move_precompile_to;
    }
    match (state, state_diff) {
        (Some(state), diff) => {
            // A full replacement discards preview storage. Keep an invalid request's diff
            // alongside it so the standard executor still rejects state + stateDiff.
            preview.state = Some(state);
            preview.state_diff = diff;
        }
        (None, Some(diff)) => {
            // Retain a full replacement's zero defaults when patching its slots.
            if let Some(state) = preview.state.as_mut() {
                state.extend(diff);
            } else {
                preview.state_diff.get_or_insert_with(Default::default).extend(diff);
            }
        }
        (None, None) => {}
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

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;

    fn context(account: AccountOverride) -> PendingExecutionContext {
        PendingExecutionContext {
            base_block: B256::ZERO.into(),
            header: SealedHeader::seal_slow(Header::default()),
            state: StateOverride::from_iter([(Address::ZERO, account)]),
            l1_block_info: Default::default(),
            block_hashes: Default::default(),
        }
    }

    #[test]
    fn partial_account_overrides_preserve_unspecified_preview_fields() {
        let preview = AccountOverride {
            balance: Some(U256::from(100)),
            nonce: Some(7),
            code: Some(vec![0x00].into()),
            state_diff: Some([(B256::ZERO, B256::with_last_byte(9))].into_iter().collect()),
            ..Default::default()
        };
        let context = context(preview.clone());
        for user in [
            AccountOverride::default(),
            AccountOverride { balance: Some(U256::ZERO), ..Default::default() },
            AccountOverride { nonce: Some(0), ..Default::default() },
            AccountOverride { code: Some(Default::default()), ..Default::default() },
            AccountOverride {
                move_precompile_to: Some(Address::repeat_byte(1)),
                ..Default::default()
            },
        ] {
            let overrides = context
                .overrides(Some(StateOverride::from_iter([(Address::ZERO, user.clone())])), None)
                .state
                .unwrap();
            let account = &overrides[&Address::ZERO];
            assert_eq!(account.balance, user.balance.or(Some(U256::from(100))));
            assert_eq!(account.nonce, user.nonce.or(Some(7)));
            assert_eq!(account.code, user.code.or(Some(vec![0x00].into())));
            assert_eq!(account.state_diff, preview.state_diff);
            assert_eq!(account.move_precompile_to, user.move_precompile_to);
        }
        assert_eq!(context.state[&Address::ZERO], preview, "requests must not mutate the snapshot");
    }

    #[test]
    fn storage_overrides_replace_or_patch_preview_storage() {
        use alloy_evm::overrides::apply_state_overrides;
        use revm::{Database, database::EmptyDB};

        let context = context(AccountOverride {
            state_diff: Some(
                [
                    (B256::ZERO, B256::with_last_byte(7)),
                    (B256::with_last_byte(1), B256::with_last_byte(8)),
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        });
        let slots = [(B256::ZERO, B256::ZERO), (B256::with_last_byte(2), B256::with_last_byte(9))];
        for (user, expected) in [
            (AccountOverride::default().with_state_diff(slots), [0u64, 8, 9, 42]),
            (AccountOverride::default().with_state(slots), [0, 0, 9, 0]),
            (AccountOverride::default().with_state([]), [0, 0, 0, 0]),
            (AccountOverride::default().with_state_diff([]), [7, 8, 0, 42]),
        ] {
            let mut db = revm::database::State::builder().with_database(EmptyDB::default()).build();
            apply_state_overrides(
                StateOverride::from_iter([(
                    Address::ZERO,
                    AccountOverride::default()
                        .with_nonce(1)
                        .with_state_diff([(B256::with_last_byte(3), B256::with_last_byte(42))]),
                )]),
                &mut db,
            )
            .unwrap();
            let overrides = context
                .overrides(Some(StateOverride::from_iter([(Address::ZERO, user)])), None)
                .state
                .unwrap();
            apply_state_overrides(overrides, &mut db).unwrap();
            for (slot, value) in expected.into_iter().enumerate() {
                assert_eq!(db.storage(Address::ZERO, U256::from(slot)).unwrap(), U256::from(value));
            }
        }
    }

    #[test]
    fn storage_diff_preserves_a_full_preview_storage_replacement() {
        let context = context(AccountOverride::default().with_state([
            (B256::ZERO, B256::with_last_byte(7)),
            (B256::with_last_byte(1), B256::with_last_byte(8)),
        ]));
        let overrides = context
            .overrides(
                Some(StateOverride::from_iter([(
                    Address::ZERO,
                    AccountOverride::default().with_state_diff([(B256::ZERO, B256::ZERO)]),
                )])),
                None,
            )
            .state
            .unwrap();
        let account = &overrides[&Address::ZERO];
        assert_eq!(
            account.state,
            Some(
                [(B256::ZERO, B256::ZERO), (B256::with_last_byte(1), B256::with_last_byte(8)),]
                    .into_iter()
                    .collect()
            )
        );
        assert!(account.state_diff.is_none());
    }

    #[test]
    fn conflicting_user_storage_overrides_still_fail_validation() {
        use alloy_evm::overrides::{StateOverrideError, apply_state_overrides};
        use revm::database::EmptyDB;

        let context = context(AccountOverride::default());
        let user = AccountOverride::default().with_state([]).with_state_diff([]);
        let overrides = context
            .overrides(Some(StateOverride::from_iter([(Address::ZERO, user)])), None)
            .state
            .unwrap();
        let mut db = revm::database::State::builder().with_database(EmptyDB::default()).build();
        assert!(matches!(
            apply_state_overrides(overrides, &mut db),
            Err(StateOverrideError::BothStateAndStateDiff(Address::ZERO))
        ));
    }
}
