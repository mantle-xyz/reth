//! Aggregated pending block state assembled from flashblocks.

use std::{collections::HashMap as StdHashMap, sync::Arc};

use alloy_consensus::{Header, Sealed, TxReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, B256, BlockNumber, TxHash, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::{
    BlockTransactions, Filter, Header as RpcHeader, Log, Withdrawal, state::StateOverride,
};
use arc_swap::Guard;
use imbl::{HashMap, Vector};
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use op_revm::{L1BlockInfo as PendingL1BlockInfo, OpHaltReason};
use reth_revm::db::BundleState;
use reth_rpc_eth_api::RpcBlock;
use revm::{context_interface::result::ExecutionResult, state::EvmState};

use crate::{BuildError, PendingBlocksAPI, StateProcessorError, TransactionWithLogs};

/// Builder for [`PendingBlocks`].
#[derive(Debug)]
pub struct PendingBlocksBuilder {
    flashblocks: Vector<MantleFlashblockPayload>,
    headers: Vec<Sealed<Header>>,
    latest_flashblock_tx_start: Option<usize>,
    latest_block_base: Option<OpFlashblockPayloadBase>,
    latest_block_l1_block_info: Option<PendingL1BlockInfo>,
    latest_block_transaction_count: Option<usize>,
    latest_block_cumulative_gas_used: Option<u64>,
    latest_block_next_log_index: Option<usize>,

    transactions: Vector<Transaction>,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_position: HashMap<B256, (BlockNumber, usize)>,
    next_position_per_block: StdHashMap<BlockNumber, usize>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,
    transaction_results: HashMap<B256, ExecutionResult<OpHaltReason>>,
    execution_times: HashMap<B256, u128>,

    bundle_state: Option<Arc<BundleState>>,

    deferred_error: Option<BuildError>,
}

impl Default for PendingBlocksBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingBlocksBuilder {
    /// Creates a new empty builder.
    pub fn new() -> Self {
        Self {
            flashblocks: Vector::new(),
            headers: Vec::new(),
            latest_flashblock_tx_start: None,
            latest_block_base: None,
            latest_block_l1_block_info: None,
            latest_block_transaction_count: None,
            latest_block_cumulative_gas_used: None,
            latest_block_next_log_index: None,
            transactions: Vector::new(),
            account_balances: HashMap::new(),
            transaction_count: HashMap::new(),
            transaction_receipts: HashMap::new(),
            transactions_by_hash: HashMap::new(),
            transaction_position: HashMap::new(),
            next_position_per_block: StdHashMap::new(),
            transaction_state: HashMap::new(),
            transaction_senders: HashMap::new(),
            transaction_results: HashMap::new(),
            execution_times: HashMap::new(),
            state_overrides: None,
            bundle_state: None,
            deferred_error: None,
        }
    }

    /// Creates a builder pre-populated from an existing snapshot.
    pub fn from_previous(pending_blocks: &PendingBlocks) -> Self {
        Self {
            flashblocks: pending_blocks.flashblocks.clone(),
            headers: vec![
                pending_blocks.earliest_header.clone(),
                pending_blocks.latest_header.clone(),
            ],
            latest_flashblock_tx_start: Some(pending_blocks.latest_flashblock_tx_start),
            latest_block_base: Some(pending_blocks.latest_block_base.clone()),
            latest_block_l1_block_info: Some(pending_blocks.latest_block_l1_block_info.clone()),
            latest_block_transaction_count: Some(pending_blocks.latest_block_transaction_count),
            latest_block_cumulative_gas_used: Some(pending_blocks.latest_block_cumulative_gas_used),
            latest_block_next_log_index: Some(pending_blocks.latest_block_next_log_index),
            transactions: pending_blocks.transactions.clone(),
            account_balances: pending_blocks.account_balances.clone(),
            transaction_count: pending_blocks.transaction_count.clone(),
            transaction_receipts: pending_blocks.transaction_receipts.clone(),
            transactions_by_hash: pending_blocks.transactions_by_hash.clone(),
            transaction_position: pending_blocks.transaction_position.clone(),
            next_position_per_block: pending_blocks.next_position_per_block.clone(),
            transaction_state: pending_blocks.transaction_state.clone(),
            transaction_senders: pending_blocks.transaction_senders.clone(),
            state_overrides: pending_blocks.state_overrides.clone(),
            transaction_results: pending_blocks.transaction_results.clone(),
            execution_times: pending_blocks.execution_times.clone(),
            bundle_state: Some(Arc::clone(&pending_blocks.bundle_state)),
            deferred_error: None,
        }
    }

    /// Adds flashblocks to the builder.
    #[inline]
    pub fn with_flashblocks(
        &mut self,
        flashblocks: impl IntoIterator<Item = MantleFlashblockPayload>,
    ) -> &Self {
        self.flashblocks.extend(flashblocks);
        self
    }

    /// Adds a header to the builder.
    #[inline]
    pub fn with_header(&mut self, header: Sealed<Header>) -> &Self {
        self.headers.push(header);
        self
    }

    /// Replaces the latest header in the builder.
    #[inline]
    pub fn replace_latest_header(&mut self, header: Sealed<Header>) -> &Self {
        if let Some(latest) = self.headers.last_mut() {
            *latest = header;
        } else {
            self.headers.push(header);
        }
        self
    }

    /// Stores metadata needed to append more transactions to the latest block
    /// without replaying it.
    #[inline]
    pub fn with_latest_block_context(
        &mut self,
        latest_flashblock_tx_start: usize,
        latest_block_base: OpFlashblockPayloadBase,
        latest_block_l1_block_info: PendingL1BlockInfo,
        latest_block_transaction_count: usize,
        latest_block_cumulative_gas_used: u64,
        latest_block_next_log_index: usize,
    ) -> &Self {
        self.latest_flashblock_tx_start = Some(latest_flashblock_tx_start);
        self.latest_block_base = Some(latest_block_base);
        self.latest_block_l1_block_info = Some(latest_block_l1_block_info);
        self.latest_block_transaction_count = Some(latest_block_transaction_count);
        self.latest_block_cumulative_gas_used = Some(latest_block_cumulative_gas_used);
        self.latest_block_next_log_index = Some(latest_block_next_log_index);
        self
    }

    /// Stores a transaction in the builder.
    ///
    /// A duplicate `tx_hash` is recorded as a deferred [`BuildError::DuplicateTransaction`]
    /// and surfaced from [`Self::build`], rather than overwriting the position index.
    #[inline]
    pub fn with_transaction(&mut self, transaction: Transaction) -> &Self {
        let tx_hash = transaction.tx_hash();
        if self.transaction_position.contains_key(&tx_hash) {
            self.deferred_error.get_or_insert(BuildError::DuplicateTransaction { tx_hash });
            return self;
        }
        let block_number = transaction.block_number.unwrap_or(0);
        let position = self.next_position_per_block.entry(block_number).or_insert(0);
        self.transaction_position.insert(tx_hash, (block_number, *position));
        *position += 1;
        self.transactions_by_hash.insert(tx_hash, transaction.clone());
        self.transactions.push_back(transaction);
        self
    }

    /// Stores the EVM state changes produced by a transaction.
    #[inline]
    pub fn with_transaction_state(&mut self, hash: B256, state: EvmState) -> &Self {
        self.transaction_state.insert(hash, state);
        self
    }

    /// Records the sender of a transaction.
    #[inline]
    pub fn with_transaction_sender(&mut self, hash: B256, sender: Address) -> &Self {
        self.transaction_senders.insert(hash, sender);
        self
    }

    /// Increments the pending nonce for an account.
    #[inline]
    pub fn increment_nonce(&mut self, sender: Address) -> &Self {
        let current_count = self.transaction_count.get(&sender).copied().unwrap_or(U256::ZERO);
        _ = self.transaction_count.insert(sender, current_count + U256::from(1));
        self
    }

    /// Stores the receipt for a transaction.
    #[inline]
    pub fn with_receipt(&mut self, hash: B256, receipt: OpTransactionReceipt) -> &Self {
        self.transaction_receipts.insert(hash, receipt);
        self
    }

    /// Records the balance of an account after execution.
    #[inline]
    pub fn with_account_balance(&mut self, address: Address, balance: U256) -> &Self {
        self.account_balances.insert(address, balance);
        self
    }

    /// Sets state overrides for the pending blocks.
    #[inline]
    pub fn with_state_overrides(&mut self, state_overrides: StateOverride) -> &Self {
        self.state_overrides = Some(state_overrides);
        self
    }

    /// Sets the accumulated bundle state.
    #[inline]
    pub fn with_bundle_state(&mut self, bundle_state: BundleState) -> &Self {
        self.bundle_state = Some(Arc::new(bundle_state));
        self
    }

    /// Stores the execution result for a transaction.
    #[inline]
    pub fn with_transaction_result(
        &mut self,
        hash: B256,
        result: ExecutionResult<OpHaltReason>,
    ) -> &Self {
        self.transaction_results.insert(hash, result);
        self
    }

    /// Stores per-transaction EVM execution time.
    #[inline]
    pub fn with_execution_time(&mut self, hash: B256, time_us: u128) -> &Self {
        self.execution_times.insert(hash, time_us);
        self
    }

    /// Builds the pending blocks.
    pub fn build(self) -> Result<PendingBlocks, StateProcessorError> {
        if let Some(err) = self.deferred_error {
            return Err(err.into());
        }

        let earliest_header = self.headers.first().cloned().ok_or(BuildError::MissingHeaders)?;
        let latest_header = self.headers.last().cloned().ok_or(BuildError::MissingHeaders)?;

        let latest_flashblock_index =
            self.flashblocks.last().map(|fb| fb.index).ok_or(BuildError::NoFlashblocks)?;
        let latest_block_base = self
            .latest_block_base
            .clone()
            .or_else(|| {
                self.flashblocks.iter().rev().find_map(|flashblock| flashblock.base.clone())
            })
            .ok_or(BuildError::MissingHeaders)?;
        let latest_block_l1_block_info =
            self.latest_block_l1_block_info.clone().unwrap_or_default();
        let latest_block_transaction_count =
            self.latest_block_transaction_count.unwrap_or_else(|| {
                self.transactions
                    .iter()
                    .filter(|tx| tx.block_number.unwrap_or_default() == latest_header.number)
                    .count()
            });
        let latest_block_cumulative_gas_used =
            self.latest_block_cumulative_gas_used.unwrap_or_else(|| {
                self.transactions
                    .iter()
                    .filter(|tx| tx.block_number.unwrap_or_default() == latest_header.number)
                    .filter_map(|tx| self.transaction_receipts.get(&tx.tx_hash()))
                    .last()
                    .map(|receipt| receipt.inner.inner.cumulative_gas_used())
                    .unwrap_or_default()
            });
        let latest_block_next_log_index = self.latest_block_next_log_index.unwrap_or_else(|| {
            self.transactions
                .iter()
                .filter(|tx| tx.block_number.unwrap_or_default() == latest_header.number)
                .filter_map(|tx| self.transaction_receipts.get(&tx.tx_hash()))
                .map(|receipt| receipt.inner.logs().len())
                .sum()
        });
        let latest_flashblock_tx_start = self.latest_flashblock_tx_start.unwrap_or_else(|| {
            let latest_flashblock_tx_count = self
                .flashblocks
                .last()
                .map(|flashblock| flashblock.diff.transactions.len())
                .unwrap_or_default();
            if latest_flashblock_tx_count == 0 {
                0
            } else {
                self.transactions.len().saturating_sub(latest_flashblock_tx_count)
            }
        });

        for transaction in &self.transactions {
            let tx_hash = transaction.tx_hash();
            if !self.transaction_receipts.contains_key(&tx_hash) {
                return Err(BuildError::MissingReceipt { tx_hash }.into());
            }
        }

        Ok(PendingBlocks {
            earliest_header,
            latest_header,
            latest_flashblock_index,
            latest_flashblock_tx_start,
            latest_block_base,
            latest_block_l1_block_info,
            latest_block_transaction_count,
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
            flashblocks: self.flashblocks,
            transactions: self.transactions,
            account_balances: self.account_balances,
            transaction_count: self.transaction_count,
            transaction_receipts: self.transaction_receipts,
            transactions_by_hash: self.transactions_by_hash,
            transaction_position: self.transaction_position,
            next_position_per_block: self.next_position_per_block,
            transaction_state: self.transaction_state,
            transaction_senders: self.transaction_senders,
            state_overrides: self.state_overrides,
            bundle_state: self.bundle_state.unwrap_or_default(),
            transaction_results: self.transaction_results,
            execution_times: self.execution_times,
        })
    }
}

/// Aggregated pending block state from flashblocks.
#[derive(Debug, Clone)]
pub struct PendingBlocks {
    earliest_header: Sealed<Header>,
    latest_header: Sealed<Header>,
    latest_flashblock_index: u64,
    latest_flashblock_tx_start: usize,
    latest_block_base: OpFlashblockPayloadBase,
    latest_block_l1_block_info: PendingL1BlockInfo,
    latest_block_transaction_count: usize,
    latest_block_cumulative_gas_used: u64,
    latest_block_next_log_index: usize,
    flashblocks: Vector<MantleFlashblockPayload>,
    transactions: Vector<Transaction>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
    transactions_by_hash: HashMap<B256, Transaction>,
    transaction_position: HashMap<B256, (BlockNumber, usize)>,
    next_position_per_block: StdHashMap<BlockNumber, usize>,
    transaction_state: HashMap<B256, EvmState>,
    transaction_senders: HashMap<B256, Address>,
    state_overrides: Option<StateOverride>,
    transaction_results: HashMap<B256, ExecutionResult<OpHaltReason>>,
    execution_times: HashMap<B256, u128>,

    bundle_state: Arc<BundleState>,
}

impl PendingBlocks {
    fn transaction_with_logs(
        transaction: &Transaction,
        receipt: &OpTransactionReceipt,
    ) -> TransactionWithLogs {
        TransactionWithLogs {
            transaction: transaction.clone(),
            logs: receipt.inner.logs().to_vec(),
            gas_used: receipt.inner.gas_used,
            status: receipt.inner.inner.status_or_post_state(),
            cumulative_gas_used: receipt.inner.inner.cumulative_gas_used(),
            contract_address: receipt.inner.contract_address,
            logs_bloom: receipt.inner.inner.logs_bloom,
        }
    }

    /// Returns the latest block number in the pending state.
    #[inline]
    pub fn latest_block_number(&self) -> BlockNumber {
        self.latest_header.number
    }

    /// Returns the canonical block number (the block before pending).
    #[inline]
    pub fn canonical_block_number(&self) -> BlockNumberOrTag {
        BlockNumberOrTag::Number(self.earliest_header.number - 1)
    }

    /// Returns the earliest block number in the pending state.
    #[inline]
    pub fn earliest_block_number(&self) -> BlockNumber {
        self.earliest_header.number
    }

    /// Returns the payload ID for the current build attempt.
    #[inline]
    pub fn payload_id(&self) -> PayloadId {
        self.flashblocks.iter().next().map(|fb| fb.payload_id).unwrap_or_default()
    }

    /// Returns the index of the latest flashblock.
    #[inline]
    pub const fn latest_flashblock_index(&self) -> u64 {
        self.latest_flashblock_index
    }

    /// Returns the start offset of the latest flashblock's transactions.
    #[inline]
    pub const fn latest_flashblock_tx_start(&self) -> usize {
        self.latest_flashblock_tx_start
    }

    /// Returns the base payload for the latest pending block.
    #[inline]
    pub const fn latest_block_base(&self) -> &OpFlashblockPayloadBase {
        &self.latest_block_base
    }

    /// Returns the cached L1 block info for the latest pending block.
    #[inline]
    pub const fn latest_block_l1_block_info(&self) -> &PendingL1BlockInfo {
        &self.latest_block_l1_block_info
    }

    /// Returns the current transaction count for the latest pending block.
    #[inline]
    pub const fn latest_block_transaction_count(&self) -> usize {
        self.latest_block_transaction_count
    }

    /// Returns the cumulative gas used after the latest transaction in the latest block.
    #[inline]
    pub const fn latest_block_cumulative_gas_used(&self) -> u64 {
        self.latest_block_cumulative_gas_used
    }

    /// Returns the next log index for the latest pending block.
    #[inline]
    pub const fn latest_block_next_log_index(&self) -> usize {
        self.latest_block_next_log_index
    }

    /// Returns the latest header.
    #[inline]
    pub fn latest_header(&self) -> Sealed<Header> {
        self.latest_header.clone()
    }

    /// Returns the parent hash of the earliest pending block.
    ///
    /// Consumers reusing cached execution results must verify their incoming
    /// `parent_block_hash` matches this value: during a reorg or failover two
    /// different parent hashes can share the same block number.
    #[inline]
    pub fn parent_hash(&self) -> B256 {
        self.earliest_header.parent_hash
    }

    /// Returns all flashblocks.
    pub fn get_flashblocks(&self) -> Vec<MantleFlashblockPayload> {
        self.flashblocks.iter().cloned().collect()
    }

    /// Returns only the flashblocks for the latest pending block.
    pub fn latest_block_flashblocks(&self) -> Vec<MantleFlashblockPayload> {
        let latest_block = self.latest_block_number();
        self.flashblocks
            .iter()
            .filter(|flashblock| flashblock.metadata.block_number == latest_block)
            .cloned()
            .collect()
    }

    /// Returns the EVM state for a transaction.
    pub fn get_transaction_state(&self, hash: &B256) -> Option<EvmState> {
        self.transaction_state.get(hash).cloned()
    }

    /// Returns the sender of a transaction.
    pub fn get_transaction_sender(&self, tx_hash: &B256) -> Option<Address> {
        self.transaction_senders.get(tx_hash).copied()
    }

    /// Returns a shared reference to the bundle state.
    pub fn get_bundle_state(&self) -> Arc<BundleState> {
        Arc::clone(&self.bundle_state)
    }

    /// Returns all transactions for a specific block number.
    pub fn get_transactions_for_block(
        &self,
        block_number: BlockNumber,
    ) -> impl Iterator<Item = &Transaction> {
        self.transactions.iter().filter(move |tx| tx.block_number.unwrap_or(0) == block_number)
    }

    fn get_withdrawals(&self) -> Vec<Withdrawal> {
        self.flashblocks.iter().flat_map(|fb| fb.diff.withdrawals.clone()).collect()
    }

    /// Returns the latest block, optionally with full transaction details.
    pub fn get_latest_block(&self, full: bool) -> RpcBlock<Optimism> {
        let header = self.latest_header();
        let block_number = header.number;
        let block_transactions: Vec<Transaction> =
            self.get_transactions_for_block(block_number).cloned().collect();

        let transactions = if full {
            BlockTransactions::Full(block_transactions)
        } else {
            let tx_hashes: Vec<B256> = block_transactions.iter().map(|tx| tx.tx_hash()).collect();
            BlockTransactions::Hashes(tx_hashes)
        };

        RpcBlock::<Optimism> {
            header: RpcHeader::from_consensus(header, None, None),
            transactions,
            uncles: Vec::new(),
            withdrawals: Some(self.get_withdrawals().into()),
        }
    }

    /// Returns the receipt for a transaction.
    pub fn get_receipt(&self, tx_hash: TxHash) -> Option<&OpTransactionReceipt> {
        self.transaction_receipts.get(&tx_hash)
    }

    /// Returns the execution result for a transaction.
    pub fn get_transaction_result(&self, tx_hash: &B256) -> Option<&ExecutionResult<OpHaltReason>> {
        self.transaction_results.get(tx_hash)
    }

    /// Returns the per-transaction EVM execution time in microseconds.
    pub fn get_execution_time(&self, tx_hash: &B256) -> Option<u128> {
        self.execution_times.get(tx_hash).copied()
    }

    /// Returns a transaction by its hash.
    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<&Transaction> {
        self.transactions_by_hash.get(&tx_hash)
    }

    /// Returns true if the transaction hash is in the pending blocks.
    pub fn has_transaction_hash(&self, tx_hash: &B256) -> bool {
        self.transactions_by_hash.contains_key(tx_hash)
    }

    /// Returns the per-block position (0-indexed) of a transaction within `block_number`.
    pub fn transaction_position(&self, block_number: BlockNumber, tx_hash: &B256) -> Option<usize> {
        self.transaction_position
            .get(tx_hash)
            .and_then(|&(bn, pos)| (bn == block_number).then_some(pos))
    }

    /// Returns the transaction count for an address in pending state.
    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count.get(&address).copied().unwrap_or(U256::ZERO)
    }

    /// Returns the balance for an address in pending state.
    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).copied()
    }

    /// Returns the state overrides for the pending state.
    pub fn get_state_overrides(&self) -> Option<StateOverride> {
        self.state_overrides.clone()
    }

    /// Returns logs matching the filter from pending state.
    pub fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        let mut logs = Vec::new();

        for tx in &self.transactions {
            if let Some(receipt) = self.transaction_receipts.get(&tx.tx_hash()) {
                for log in receipt.inner.logs() {
                    if filter.matches(log.as_ref()) {
                        logs.push(log.clone());
                    }
                }
            }
        }

        logs
    }

    /// Returns all pending transactions from flashblocks.
    pub fn get_pending_transactions(&self) -> Vec<Transaction> {
        self.transactions.iter().cloned().collect()
    }

    /// Returns the total number of pending transactions across all tracked blocks.
    #[inline]
    pub fn pending_transaction_count(&self) -> usize {
        self.transactions.len()
    }

    /// Returns all pending transactions with their associated logs.
    pub fn get_pending_transactions_with_logs(&self) -> Vec<TransactionWithLogs> {
        self.transactions
            .iter()
            .filter_map(|tx| {
                self.transaction_receipts
                    .get(&tx.tx_hash())
                    .map(|receipt| Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns the hashes of all pending transactions from flashblocks.
    pub fn get_pending_transaction_hashes(&self) -> Vec<B256> {
        self.transactions.iter().map(|tx| tx.tx_hash()).collect()
    }

    const fn previous_flashblocks_tx_count(&self) -> usize {
        self.latest_flashblock_tx_start
    }

    /// Returns logs matching the filter from only the latest flashblock (delta).
    pub fn get_latest_flashblock_logs(&self, filter: &Filter) -> Vec<Log> {
        let prev_count = self.previous_flashblocks_tx_count();
        let mut logs = Vec::new();

        for tx in self.transactions.iter().skip(prev_count) {
            if let Some(receipt) = self.transaction_receipts.get(&tx.tx_hash()) {
                for log in receipt.inner.logs() {
                    if filter.matches(log.as_ref()) {
                        logs.push(log.clone());
                    }
                }
            }
        }

        logs
    }

    /// Returns transactions with their logs from only the latest flashblock (delta).
    pub fn get_latest_flashblock_transactions_with_logs(&self) -> Vec<TransactionWithLogs> {
        let prev_count = self.previous_flashblocks_tx_count();

        self.transactions
            .iter()
            .skip(prev_count)
            .filter_map(|tx| {
                self.transaction_receipts
                    .get(&tx.tx_hash())
                    .map(|receipt| Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns latest-flashblock transactions where at least one log matches the filter.
    ///
    /// When a transaction matches, all of its logs are returned.
    pub fn get_latest_flashblock_transactions_with_logs_filtered(
        &self,
        filter: &Filter,
    ) -> Vec<TransactionWithLogs> {
        let prev_count = self.previous_flashblocks_tx_count();

        self.transactions
            .iter()
            .skip(prev_count)
            .filter_map(|tx| {
                let receipt = self.transaction_receipts.get(&tx.tx_hash())?;
                let logs = receipt.inner.logs();

                let has_match = logs.iter().any(|log| filter.matches(log.as_ref()));
                if !has_match {
                    return None;
                }

                Some(Self::transaction_with_logs(tx, receipt))
            })
            .collect()
    }

    /// Returns the hashes of transactions from only the latest flashblock (delta).
    pub fn get_latest_flashblock_transaction_hashes(&self) -> Vec<B256> {
        let prev_count = self.previous_flashblocks_tx_count();
        self.transactions.iter().skip(prev_count).map(|tx| tx.tx_hash()).collect()
    }
}

impl PendingBlocksAPI for Guard<Option<Arc<PendingBlocks>>> {
    fn get_canonical_block_number(&self) -> BlockNumberOrTag {
        self.as_ref().map(|pb| pb.canonical_block_number()).unwrap_or(BlockNumberOrTag::Latest)
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.as_ref().map(|pb| pb.get_transaction_count(address)).unwrap_or(U256::ZERO)
    }

    fn get_block(&self, full: bool) -> Option<serde_json::Value> {
        self.as_ref().and_then(|pb| serde_json::to_value(pb.get_latest_block(full)).ok())
    }

    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<serde_json::Value> {
        self.as_ref()
            .and_then(|pb| pb.get_receipt(tx_hash))
            .and_then(|receipt| serde_json::to_value(receipt).ok())
    }

    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<serde_json::Value> {
        self.as_ref()
            .and_then(|pb| pb.get_transaction_by_hash(tx_hash))
            .and_then(|tx| serde_json::to_value(tx).ok())
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.as_ref().and_then(|pb| pb.get_balance(address))
    }

    fn get_state_overrides(&self) -> Option<StateOverride> {
        self.as_ref().and_then(|pb| pb.get_state_overrides())
    }

    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log> {
        self.as_ref().map(|pb| pb.get_pending_logs(filter)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom, Signed};
    use alloy_primitives::{Bloom, Bytes, Log as PrimitiveLog, LogData, Signature, TxKind};
    use alloy_rpc_types_eth::TransactionReceipt;
    use mantle_reth_flashblocks_types::MantleFlashblockMetadata;
    use op_alloy_consensus::{OpReceipt, OpTxEnvelope};
    use op_alloy_rpc_types::L1BlockInfo;
    use op_alloy_rpc_types_engine::{OpFlashblockPayloadBase, OpFlashblockPayloadDelta};
    use revm::state::AccountInfo;
    use revm_database::states::BundleBuilder;

    use super::*;

    fn test_sender() -> Address {
        Address::repeat_byte(0x01)
    }

    fn test_flashblock() -> MantleFlashblockPayload {
        test_flashblock_for_block(1)
    }

    fn test_flashblock_for_block(block_number: u64) -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(OpFlashblockPayloadBase {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: OpFlashblockPayloadDelta {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: MantleFlashblockMetadata {
                block_number,
                ..MantleFlashblockMetadata::default()
            },
        }
    }

    fn test_legacy_transaction() -> Transaction {
        test_transaction_with_hash(B256::ZERO)
    }

    fn test_transaction_with_hash(hash: B256) -> Transaction {
        test_transaction_with_hash_in_block(hash, 1)
    }

    fn test_transaction_with_hash_in_block(hash: B256, block_number: u64) -> Transaction {
        let legacy = alloy_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let envelope =
            OpTxEnvelope::Legacy(Signed::new_unchecked(legacy, Signature::test_signature(), hash));
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    envelope,
                    Address::ZERO,
                ),
                block_hash: Some(B256::ZERO),
                block_number: Some(block_number),
                block_timestamp: None,
                transaction_index: Some(0),
                effective_gas_price: Some(1_000_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn receipt_with_logs(
        tx_hash: B256,
        block_number: u64,
        logs: Vec<Log>,
        cumulative_gas_used: u64,
    ) -> OpTransactionReceipt {
        OpTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: OpReceipt::Legacy(Receipt {
                        status: Eip658Value::Eip658(true),
                        cumulative_gas_used,
                        logs,
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(block_number),
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: L1BlockInfo::default(),
            op_gas_refund: None,
        }
    }

    fn make_log(
        tx_hash: B256,
        block_number: u64,
        address: Address,
        topics: Vec<B256>,
        log_index: u64,
    ) -> Log {
        Log {
            inner: PrimitiveLog { address, data: LogData::new_unchecked(topics, Bytes::new()) },
            block_hash: Some(B256::ZERO),
            block_number: Some(block_number),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(0),
            log_index: Some(log_index),
            removed: false,
        }
    }

    fn test_receipt(tx_hash: B256) -> OpTransactionReceipt {
        receipt_with_logs(tx_hash, 1, vec![], 21_000)
    }

    fn test_receipt_with_log(tx_hash: B256, log_address: Address) -> OpTransactionReceipt {
        receipt_with_logs(tx_hash, 1, vec![make_log(tx_hash, 1, log_address, vec![], 0)], 21_000)
    }

    fn test_receipt_with_log_and_topic(
        tx_hash: B256,
        log_address: Address,
        topic0: B256,
    ) -> OpTransactionReceipt {
        receipt_with_logs(
            tx_hash,
            1,
            vec![make_log(tx_hash, 1, log_address, vec![topic0], 0)],
            21_000,
        )
    }

    fn test_receipt_for_block_with_log_count(
        tx_hash: B256,
        block_number: u64,
        log_address: Address,
        log_count: usize,
        cumulative_gas_used: u64,
    ) -> OpTransactionReceipt {
        let logs = (0..log_count)
            .map(|i| make_log(tx_hash, block_number, log_address, vec![], i as u64))
            .collect();
        receipt_with_logs(tx_hash, block_number, logs, cumulative_gas_used)
    }

    fn header_at(number: u64) -> Sealed<Header> {
        Sealed::new_unchecked(Header { number, ..Default::default() }, B256::ZERO)
    }

    fn build_pending_blocks_with_logs(entries: &[(B256, Address)]) -> PendingBlocks {
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));

        for &(hash, addr) in entries {
            builder.with_transaction(test_transaction_with_hash(hash));
            builder.with_receipt(hash, test_receipt_with_log(hash, addr));
        }

        builder.build().expect("build should succeed")
    }

    fn build_pending_blocks_with_topics(entries: &[(B256, Address, B256)]) -> PendingBlocks {
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));

        for &(hash, addr, topic) in entries {
            builder.with_transaction(test_transaction_with_hash(hash));
            builder.with_receipt(hash, test_receipt_with_log_and_topic(hash, addr, topic));
        }

        builder.build().expect("build should succeed")
    }

    #[test]
    fn from_previous_preserves_bundle_state() {
        let tx_hash = B256::with_last_byte(0xAA);
        let sender = test_sender();
        let bundle_state = BundleBuilder::new(0..=0)
            .state_address(sender)
            .state_present_account_info(sender, AccountInfo::default())
            .build();

        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(test_transaction_with_hash(tx_hash));
        builder.with_receipt(tx_hash, test_receipt_with_log(tx_hash, sender));
        builder.with_bundle_state(bundle_state);

        let pending_blocks = builder.build().expect("build should succeed");
        let next_builder = PendingBlocksBuilder::from_previous(&pending_blocks);
        let next_pending_blocks = next_builder.build().expect("build from previous should succeed");

        assert!(
            next_pending_blocks.get_bundle_state().account(&sender).is_some(),
            "bundle_state should be preserved when cloning a pending snapshot"
        );
    }

    #[test]
    fn from_previous_preserves_next_position_per_block() {
        let tx_hash_a = B256::with_last_byte(0xAA);
        let tx_hash_b = B256::with_last_byte(0xBB);
        let tx_hash_c = B256::with_last_byte(0xCC);

        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(test_transaction_with_hash(tx_hash_a));
        builder.with_receipt(tx_hash_a, test_receipt_with_log(tx_hash_a, test_sender()));
        builder.with_transaction(test_transaction_with_hash(tx_hash_b));
        builder.with_receipt(tx_hash_b, test_receipt_with_log(tx_hash_b, test_sender()));

        let pending_blocks = builder.build().expect("build should succeed");

        let mut next_builder = PendingBlocksBuilder::from_previous(&pending_blocks);
        next_builder.with_transaction(test_transaction_with_hash(tx_hash_c));
        next_builder.with_receipt(tx_hash_c, test_receipt_with_log(tx_hash_c, test_sender()));
        let next_pending_blocks =
            next_builder.build().expect("build from previous should preserve positions");

        assert_eq!(next_pending_blocks.transaction_position(1, &tx_hash_c), Some(2));
    }

    #[test]
    fn build_rejects_duplicate_transaction() {
        let tx = test_legacy_transaction();
        let tx_hash = tx.tx_hash();
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(tx.clone());
        builder.with_transaction(tx);
        builder.with_transaction_sender(tx_hash, test_sender());
        builder.with_transaction_state(tx_hash, Default::default());
        builder.with_receipt(tx_hash, test_receipt(tx_hash));

        let err = builder.build().expect_err("build should fail on duplicate tx");
        assert_eq!(err, StateProcessorError::Build(BuildError::DuplicateTransaction { tx_hash }));
    }

    #[test]
    fn build_rejects_transaction_without_receipt() {
        let tx = test_legacy_transaction();
        let tx_hash = tx.tx_hash();
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(tx);
        builder.with_transaction_sender(tx_hash, test_sender());

        let err = builder.build().expect_err("build should fail without a receipt");
        assert_eq!(err, StateProcessorError::Build(BuildError::MissingReceipt { tx_hash }));
    }

    #[test]
    fn build_fallbacks_use_latest_block_values_only() {
        let tx_hash_1 = B256::with_last_byte(0xA1);
        let tx_hash_2 = B256::with_last_byte(0xB2);

        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([test_flashblock_for_block(1), test_flashblock_for_block(2)]);
        builder.with_header(header_at(1));
        builder.with_header(header_at(2));
        builder.with_transaction(test_transaction_with_hash_in_block(tx_hash_1, 1));
        builder.with_receipt(
            tx_hash_1,
            test_receipt_for_block_with_log_count(tx_hash_1, 1, test_sender(), 2, 21_000),
        );
        builder.with_transaction(test_transaction_with_hash_in_block(tx_hash_2, 2));
        builder.with_receipt(
            tx_hash_2,
            test_receipt_for_block_with_log_count(tx_hash_2, 2, test_sender(), 1, 42_000),
        );

        let pending_blocks = builder.build().expect("build should succeed without latest context");

        assert_eq!(pending_blocks.latest_block_base().block_number, 2);
        assert_eq!(pending_blocks.latest_block_transaction_count(), 1);
        assert_eq!(pending_blocks.latest_block_cumulative_gas_used(), 42_000);
        assert_eq!(pending_blocks.latest_block_next_log_index(), 1);
    }

    #[test]
    fn get_pending_logs_returns_logs_in_transaction_order() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_a = Address::with_last_byte(0x0A);
        let addr_b = Address::with_last_byte(0x0B);
        let addr_c = Address::with_last_byte(0x0C);

        let pending =
            build_pending_blocks_with_logs(&[(hash_a, addr_a), (hash_b, addr_b), (hash_c, addr_c)]);

        let logs = pending.get_pending_logs(&Filter::default());

        assert_eq!(logs.len(), 3, "should return one log per transaction");
        assert_eq!(logs[0].address(), addr_a);
        assert_eq!(logs[1].address(), addr_b);
        assert_eq!(logs[2].address(), addr_c);
    }

    #[test]
    fn filtered_transactions_returns_only_matching_by_address() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_a = Address::with_last_byte(0x0A);
        let addr_b = Address::with_last_byte(0x0B);
        let addr_c = Address::with_last_byte(0x0C);

        let pending =
            build_pending_blocks_with_logs(&[(hash_a, addr_a), (hash_b, addr_b), (hash_c, addr_c)]);

        let filter = Filter::new().address(addr_b);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_b);
        assert_eq!(txs[0].logs.len(), 1);
        assert_eq!(txs[0].logs[0].address(), addr_b);
    }

    #[test]
    fn filtered_transactions_returns_only_matching_by_topic0() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);

        let addr = Address::with_last_byte(0x01);
        let topic_transfer = B256::with_last_byte(0x01);
        let topic_approval = B256::with_last_byte(0x02);

        let pending = build_pending_blocks_with_topics(&[
            (hash_a, addr, topic_transfer),
            (hash_b, addr, topic_approval),
        ]);

        let filter = Filter::new().event_signature(topic_transfer);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_a);
    }

    #[test]
    fn filtered_transactions_returns_all_logs_when_any_matches() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_match = Address::with_last_byte(0x0A);
        let addr_other = Address::with_last_byte(0x0B);

        let receipt = receipt_with_logs(
            hash_a,
            1,
            vec![
                make_log(hash_a, 1, addr_match, vec![], 0),
                make_log(hash_a, 1, addr_other, vec![], 1),
            ],
            42_000,
        );

        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(test_transaction_with_hash(hash_a));
        builder.with_receipt(hash_a, receipt);
        let pending = builder.build().expect("build should succeed");

        let filter = Filter::new().address(addr_match);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].logs.len(), 2, "should return ALL logs, not just matching");
        assert_eq!(txs[0].logs[0].address(), addr_match);
        assert_eq!(txs[0].logs[1].address(), addr_other);
    }

    #[test]
    fn filtered_transactions_returns_none_when_no_match() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);
        let addr_unrelated = Address::with_last_byte(0xFF);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let filter = Filter::new().address(addr_unrelated);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert!(txs.is_empty());
    }

    #[test]
    fn filtered_transactions_populates_gas_used() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let filter = Filter::new().address(addr_a);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].gas_used, 21_000);
    }

    #[test]
    fn unfiltered_transactions_populates_gas_used() {
        let hash_a = B256::with_last_byte(0xAA);
        let addr_a = Address::with_last_byte(0x0A);

        let pending = build_pending_blocks_with_logs(&[(hash_a, addr_a)]);

        let txs = pending.get_latest_flashblock_transactions_with_logs();

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].gas_used, 21_000);
    }

    #[test]
    fn unfiltered_transactions_populate_receipt_fields() {
        let tx_hash = B256::with_last_byte(0xAA);
        let log_address = Address::with_last_byte(0x0A);
        let contract_address = Address::with_last_byte(0x0B);
        let logs_bloom: Bloom = [0x22; 256].into();

        let mut receipt = receipt_with_logs(
            tx_hash,
            1,
            vec![make_log(tx_hash, 1, log_address, vec![], 0)],
            42_000,
        );
        receipt.inner.inner.logs_bloom = logs_bloom;
        receipt.inner.contract_address = Some(contract_address);

        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([test_flashblock()]);
        builder.with_header(header_at(0));
        builder.with_transaction(test_transaction_with_hash(tx_hash));
        builder.with_receipt(tx_hash, receipt);
        let pending = builder.build().expect("build should succeed");

        let txs = pending.get_latest_flashblock_transactions_with_logs();

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].status, Eip658Value::Eip658(true));
        assert_eq!(txs[0].cumulative_gas_used, 42_000);
        assert_eq!(txs[0].contract_address, Some(contract_address));
        assert_eq!(txs[0].logs_bloom, logs_bloom);
    }

    #[test]
    fn filtered_transactions_with_combined_address_and_topic() {
        let hash_a = B256::with_last_byte(0xAA);
        let hash_b = B256::with_last_byte(0xBB);
        let hash_c = B256::with_last_byte(0xCC);

        let addr_usdc = Address::with_last_byte(0x0A);
        let addr_weth = Address::with_last_byte(0x0B);
        let topic_transfer = B256::with_last_byte(0x01);
        let topic_approval = B256::with_last_byte(0x02);

        let pending = build_pending_blocks_with_topics(&[
            (hash_a, addr_usdc, topic_transfer),
            (hash_b, addr_usdc, topic_approval),
            (hash_c, addr_weth, topic_transfer),
        ]);

        let filter = Filter::new().address(addr_usdc).event_signature(topic_transfer);
        let txs = pending.get_latest_flashblock_transactions_with_logs_filtered(&filter);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].transaction.tx_hash(), hash_a);
    }
}
