//! Flashblocks state processor.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard},
    time::Instant,
};

use alloy_consensus::{
    Block, BlockBody, Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, Decodable2718};
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, BlockNumber};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use op_alloy_consensus::OpTxEnvelope;
use rayon::prelude::*;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use revm_database::states::bundle_state::BundleRetention;
use tokio::sync::{Mutex, broadcast::Sender, mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error, instrument, warn};

use crate::{
    AssembledBlock, BlockAssembler, ExecutionError, FlashblockCache, PendingBlocks,
    PendingBlocksBuilder, PendingStateBuilder, ProviderError, Result, StateProcessorError,
    metrics::Metrics,
    validation::{
        CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
        ReorgDetector, SequenceValidationResult,
    },
};

type PendingExecutionDb = State<StateProviderDatabase<StateProviderBox>>;

#[derive(Debug)]
struct LivePendingState {
    db: PendingExecutionDb,
    state_overrides: StateOverride,
}

/// Messages consumed by the state processor.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StateUpdate {
    /// New canonical block to reconcile against pending state.
    ///
    /// The optional acknowledgement fires once reconciliation and any cached
    /// replay finish. Tests use it to sequence delivery without sleeping.
    Canonical(RecoveredBlock<OpBlock>, Option<oneshot::Sender<()>>),
    /// Incoming flashblock payload to extend pending state.
    ///
    /// The optional acknowledgement fires once processing finishes, whatever the
    /// outcome. Tests use it to sequence injections without sleeping.
    Flashblock(MantleFlashblockPayload, Option<oneshot::Sender<()>>),
}

/// Processes flashblocks and canonical blocks to keep pending state updated.
#[derive(Debug)]
pub struct StateProcessor<Client> {
    rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    max_trailing_depth: u64,
    max_leading_depth: u64,
    client: Client,
    sender: Sender<Arc<PendingBlocks>>,
    cache: Arc<Mutex<FlashblockCache>>,
    live_state: StdMutex<Option<LivePendingState>>,
}

impl<Client> StateProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    fn lock_live_state(&self) -> StdMutexGuard<'_, Option<LivePendingState>> {
        self.live_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_live_state(&self) {
        *self.lock_live_state() = None;
    }

    fn set_live_state(&self, db: PendingExecutionDb, state_overrides: StateOverride) {
        *self.lock_live_state() = Some(LivePendingState { db, state_overrides });
    }

    fn publish_pending_blocks(
        &self,
        mut pending_blocks_builder: PendingBlocksBuilder,
        mut db: PendingExecutionDb,
        state_overrides: StateOverride,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        db.merge_transitions(BundleRetention::Reverts);
        pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
        pending_blocks_builder.with_state_overrides(state_overrides.clone());

        let pending_blocks = Arc::new(pending_blocks_builder.build()?);
        self.set_live_state(db, state_overrides);

        Ok(Some(pending_blocks))
    }

    /// Creates a new state processor wired to the provided channels and state.
    pub fn new(
        client: Client,
        pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
        max_trailing_depth: u64,
        max_leading_depth: u64,
        max_cache_ahead_blocks: u64,
        rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
        sender: Sender<Arc<PendingBlocks>>,
    ) -> Self {
        let best = client.best_block_number().unwrap_or(0);
        let cache = FlashblockCache::new(best, max_cache_ahead_blocks);

        Self {
            pending_blocks,
            client,
            max_trailing_depth,
            max_leading_depth,
            rx,
            sender,
            cache: Arc::new(Mutex::new(cache)),
            live_state: StdMutex::new(None),
        }
    }

    /// Processes updates from the queue until the channel closes.
    pub async fn start(&self) {
        while let Some(update) = self.rx.lock().await.recv().await {
            let prev_pending_blocks = self.pending_blocks.load_full();
            match update {
                StateUpdate::Canonical(block, ack) => {
                    debug!(message = "processing canonical block", block_number = block.number);
                    match self.process_canonical_block(prev_pending_blocks, &block) {
                        Ok(new_pending_blocks) => {
                            self.pending_blocks.swap(new_pending_blocks);

                            let mut cache = self.cache.lock().await;
                            cache.update_canonical(block.number);
                            let cached = cache.drain(block.number + 1);
                            drop(cache);

                            if !cached.is_empty() {
                                debug!(
                                    message = "replaying cached flashblocks after canonical block",
                                    canonical_block = block.number,
                                    cached_count = cached.len(),
                                );
                                for flashblock in cached {
                                    let fb_prev = self.pending_blocks.load_full();
                                    self.apply_flashblock(fb_prev, flashblock).await;
                                }
                            }
                        }
                        Err(e) => {
                            error!(message = "could not process canonical block", error = %e);
                        }
                    }
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
                StateUpdate::Flashblock(flashblock, ack) => {
                    debug!(
                        message = "processing flashblock",
                        block_number = flashblock.metadata.block_number,
                        flashblock_index = flashblock.index
                    );
                    self.apply_flashblock(prev_pending_blocks, flashblock).await;
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
            }
        }
    }

    /// Rejects a flashblock that would push the overlay too far ahead of canonical.
    ///
    /// Placed on the flashblock path rather than in `reconcile`: a stalled canonical
    /// produces no `StateUpdate::Canonical`, so a check there would never run.
    fn exceeds_leading_depth(&self, flashblock: &MantleFlashblockPayload) -> bool {
        let Ok(canonical) = self.client.best_block_number() else { return false };
        let leading = flashblock.metadata.block_number.saturating_sub(canonical);
        leading > self.max_leading_depth
    }

    async fn apply_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: MantleFlashblockPayload,
    ) {
        if self.exceeds_leading_depth(&flashblock) {
            Metrics::leading_depth_exceeded().increment(1);
            warn!(
                message = "flashblock leads canonical beyond the configured depth, dropping",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                max_leading_depth = self.max_leading_depth,
            );
            return;
        }

        let start_time = Instant::now();
        match self.process_flashblock(prev_pending_blocks, &flashblock) {
            Ok(new_pending_blocks) => {
                if let Some(ref pb) = new_pending_blocks {
                    _ = self.sender.send(Arc::clone(pb));
                }
                self.pending_blocks.swap(new_pending_blocks);
                Metrics::block_processing_duration().record(start_time.elapsed());
            }
            Err(e) => {
                match e {
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader {
                        ..
                    }) => {
                        let inserted = self.cache.lock().await.insert(flashblock);
                        if inserted {
                            debug!(message = "cached flashblock pending canonical block", error = %e);
                            return;
                        }
                    }
                    StateProcessorError::MissingFirstFlashblock |
                    StateProcessorError::NonConsecutiveBlock { .. } => {
                        // A slice that cannot extend the current overlay is still a
                        // valid restart point when it carries a base payload, so cache
                        // it instead of dropping it; higher indices are only useful
                        // once their predecessor is cached.
                        let mut cache = self.cache.lock().await;
                        let resumable = flashblock.index == 0 ||
                            cache.has_flashblock(
                                flashblock.metadata.block_number,
                                flashblock.index - 1,
                            );
                        if resumable {
                            let block_number = flashblock.metadata.block_number;
                            let index = flashblock.index;
                            if cache.insert(flashblock) {
                                debug!(
                                    message = "cached flashblock pending its predecessor",
                                    block_number, index
                                );
                            }
                        }
                        return;
                    }
                    _ => {}
                }

                if !matches!(
                    e,
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader { .. })
                ) {
                    error!(message = "could not process flashblock", error = %e);
                    Metrics::block_processing_error().increment(1);
                }
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(block_number = block.number))]
    fn process_canonical_block(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        block: &RecoveredBlock<OpBlock>,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                self.clear_live_state();
                return Ok(None);
            }
        };

        let mut flashblocks = pending_blocks.get_flashblocks();
        let num_flashblocks_for_canon =
            flashblocks.iter().filter(|fb| fb.metadata.block_number == block.number).count();
        Metrics::flashblocks_in_block().record(num_flashblocks_for_canon as f64);
        Metrics::pending_snapshot_height().set(pending_blocks.latest_block_number() as f64);

        let tracked_txns = pending_blocks.get_transactions_for_block(block.number);
        let tracked_txn_hashes: Vec<_> = tracked_txns.map(|tx| tx.tx_hash()).collect();
        let block_txn_hashes: Vec<_> =
            block.body().transactions.iter().map(|tx| *tx.hash()).collect();

        let reorg_result = ReorgDetector::detect(&tracked_txn_hashes, &block_txn_hashes);
        let reorg_detected = reorg_result.is_reorg();

        let strategy = CanonicalBlockReconciler::reconcile(
            Some(pending_blocks.earliest_block_number()),
            Some(pending_blocks.latest_block_number()),
            block.number,
            self.max_trailing_depth,
            reorg_detected,
        );

        match strategy {
            ReconciliationStrategy::CatchUp => {
                debug!(
                    message = "pending snapshot cleared because canonical caught up",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    canonical_block = block.number,
                );
                Metrics::pending_clear_catchup().increment(1);
                Metrics::pending_snapshot_fb_index()
                    .set(pending_blocks.latest_flashblock_index() as f64);
                self.clear_live_state();
                Ok(None)
            }
            ReconciliationStrategy::HandleReorg => {
                warn!(
                    message = "reorg detected, recomputing pending flashblocks going ahead of reorg",
                    tracked_txn_hashes = ?tracked_txn_hashes,
                    block_txn_hashes = ?block_txn_hashes,
                );
                Metrics::pending_clear_reorg().increment(1);

                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks)
            }
            ReconciliationStrategy::DepthLimitExceeded { depth, max_depth } => {
                debug!(
                    message = "pending blocks depth exceeds max depth, resetting pending blocks",
                    pending_blocks_depth = depth,
                    max_depth = max_depth,
                );

                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks)
            }
            ReconciliationStrategy::Continue => {
                debug!(
                    message = "canonical block behind latest pending block, continuing with existing pending state",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    earliest_pending_block = pending_blocks.earliest_block_number(),
                    canonical_block = block.number,
                    pending_txns_for_block = ?tracked_txn_hashes.len(),
                    canonical_txns_for_block = ?block_txn_hashes.len(),
                );
                // Deliberately not retained: dropping older flashblocks would lose
                // the `earliest` block number that the trailing-depth guard measures.
                self.build_pending_state(prev_pending_blocks, &flashblocks)
            }
            ReconciliationStrategy::NoPendingState => {
                debug!(message = "no pending state to update with canonical block, skipping");
                Ok(None)
            }
        }
    }

    fn process_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: &MantleFlashblockPayload,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                if flashblock.index == 0 {
                    return self.build_pending_state(None, std::slice::from_ref(flashblock));
                }

                return Err(StateProcessorError::MissingFirstFlashblock);
            }
        };

        let validation_result = FlashblockSequenceValidator::validate(
            pending_blocks.latest_block_number(),
            pending_blocks.latest_flashblock_index(),
            flashblock.metadata.block_number,
            flashblock.index,
            flashblock.metadata.prev_flashblock_id,
        );

        match validation_result {
            SequenceValidationResult::NextInSequence => {
                self.build_pending_state_for_same_block(pending_blocks, flashblock)
            }
            SequenceValidationResult::FirstOfNextBlock => {
                self.build_pending_state_for_next_block(pending_blocks, flashblock)
            }
            SequenceValidationResult::Duplicate => {
                Metrics::unexpected_block_order().increment(1);
                warn!(
                    message = "received duplicate flashblock for current block, ignoring",
                    curr_block = %pending_blocks.latest_block_number(),
                    flashblock_index = %flashblock.index,
                );
                Ok(prev_pending_blocks)
            }
            SequenceValidationResult::InvalidNewBlockIndex { block_number, index } => {
                Metrics::unexpected_block_order().increment(1);
                // The slice is not applicable to this overlay, but it invalidates
                // nothing either: leaving both the snapshot and the live state
                // alone keeps them paired, so a slice that does continue the
                // tracked sequence still executes incrementally.
                warn!(
                    message = "received flashblock for a non-consecutive block, leaving pending state untouched",
                    curr_block = %pending_blocks.latest_block_number(),
                    new_block = %block_number,
                    new_flashblock_index = %index,
                );
                Err(StateProcessorError::NonConsecutiveBlock {
                    block_number,
                    index,
                    tracked_block_number: pending_blocks.latest_block_number(),
                })
            }
            SequenceValidationResult::NonSequentialGap { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    expected_flashblock_index = %expected,
                    actual_flashblock_index = %actual,
                    "received non-sequential flashblock index for current block"
                );
                self.clear_live_state();
                Ok(None)
            }
            SequenceValidationResult::NonSequentialPredecessor { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    curr_flashblock_index = %pending_blocks.latest_flashblock_index(),
                    new_block = %flashblock.metadata.block_number,
                    new_flashblock_index = %flashblock.index,
                    expected_prev_block = %expected.block_number,
                    expected_prev_index = %expected.index,
                    actual_prev_block = %actual.block_number,
                    actual_prev_index = %actual.index,
                    "received flashblock with non-sequential predecessor link"
                );
                self.clear_live_state();
                Ok(None)
            }
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_same_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &MantleFlashblockPayload,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let latest_block_base = prev_pending_blocks.latest_block_base().clone();
        let latest_block_l1_block_info = prev_pending_blocks.latest_block_l1_block_info().clone();
        let latest_flashblock_tx_start = prev_pending_blocks.pending_transaction_count();

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "same_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let latest_header = prev_pending_blocks.latest_header();
        let mut latest_block_flashblocks = prev_pending_blocks.latest_block_flashblocks();
        latest_block_flashblocks.push(flashblock.clone());
        let latest_block_header =
            BlockAssembler::refresh_same_block_header(&latest_header, &latest_block_flashblocks)?;

        db.block_hashes.insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let evm_env = evm_config
            .evm_env(&latest_header)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let previous_block_transaction_count = prev_pending_blocks.latest_block_transaction_count();
        let pending_block = Block {
            header: Header {
                parent_hash: latest_block_base.parent_hash,
                number: latest_block_base.block_number,
                timestamp: latest_block_base.timestamp,
                gas_limit: latest_block_base.gas_limit,
                base_fee_per_gas: Some(latest_block_base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: BlockBody {
                transactions: flashblock
                    .diff
                    .transactions
                    .iter()
                    .map(|tx| OpTxEnvelope::decode_2718_exact(tx.as_ref()))
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?,
                ..Default::default()
            },
        };
        let latest_block_transaction_count =
            previous_block_transaction_count + pending_block.body.transactions.len();
        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(OpTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(OpTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        Metrics::sender_recovery_duration().record(recovery_start.elapsed());

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.replace_latest_header(latest_block_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            latest_block_l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder.set_execution_offsets(
            prev_pending_blocks.latest_block_cumulative_gas_used(),
            prev_pending_blocks.latest_block_next_log_index(),
        );

        for (offset, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();
            let idx = previous_block_transaction_count + offset;
            Self::execute_into_builder(
                &mut pending_state_builder,
                &mut pending_blocks_builder,
                idx,
                tx_hash,
                transaction,
                sender,
            )?;
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            latest_flashblock_tx_start,
            latest_block_base,
            latest_block_l1_block_info,
            latest_block_transaction_count,
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );
        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_next_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &MantleFlashblockPayload,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let Some(base) = flashblock.base.clone() else {
            return Err(StateProcessorError::MissingFirstFlashblock);
        };

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "next_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let previous_header = prev_pending_blocks.latest_header();
        let current_block = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
        let l1_block_info = current_block.l1_block_info()?;
        let AssembledBlock { block: assembled_block, header: assembled_header, .. } = current_block;
        let pending_block = Block {
            header: Header {
                parent_hash: base.parent_hash,
                number: base.block_number,
                timestamp: base.timestamp,
                gas_limit: base.gas_limit,
                base_fee_per_gas: Some(base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: assembled_block.body,
        };

        db.block_hashes.insert(base.block_number - 1, base.parent_hash);

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let block_env_attributes = OpNextBlockEnvAttributes::from(base.clone());
        let evm_env = evm_config
            .next_evm_env(&previous_header, &block_env_attributes)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(OpTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(OpTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        Metrics::sender_recovery_duration().record(recovery_start.elapsed());

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.with_header(assembled_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder
            .apply_pre_execution_changes(base.parent_hash, Some(base.parent_beacon_block_root))?;

        for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();
            Self::execute_into_builder(
                &mut pending_state_builder,
                &mut pending_blocks_builder,
                idx,
                tx_hash,
                transaction,
                sender,
            )?;
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            prev_pending_blocks.pending_transaction_count(),
            base,
            l1_block_info,
            flashblock.diff.transactions.len(),
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(level = "debug", skip_all, fields(num_flashblocks = flashblocks.len()))]
    fn build_pending_state(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblocks: &[MantleFlashblockPayload],
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let mut flashblocks_per_block =
            BTreeMap::<BlockNumber, Vec<MantleFlashblockPayload>>::new();
        for flashblock in flashblocks {
            flashblocks_per_block
                .entry(flashblock.metadata.block_number)
                .or_default()
                .push(flashblock.clone());
        }

        let Some(&earliest_block_number) = flashblocks_per_block.keys().min() else {
            return Ok(None);
        };
        let canonical_block = earliest_block_number - 1;
        let mut last_block_header = self
            .client
            .header_by_number(canonical_block)
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?
            .ok_or(ProviderError::MissingCanonicalHeader { block_number: canonical_block })?;

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let state_provider = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?;
        let state_provider_db = StateProviderDatabase::new(state_provider);
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        let mut db = State::builder().with_database(state_provider_db).with_bundle_update().build();

        let mut state_overrides =
            prev_pending_blocks.as_ref().map_or_else(StateOverride::default, |pending_blocks| {
                pending_blocks.get_state_overrides().unwrap_or_default()
            });

        let mut total_transaction_count = 0usize;
        for (_block_number, flashblocks) in flashblocks_per_block {
            let assembled = BlockAssembler::assemble(&flashblocks)?;
            let latest_flashblock_tx_count =
                flashblocks.last().map(|latest| latest.diff.transactions.len()).unwrap_or_default();
            let latest_block_base = assembled.base.clone();

            pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
            pending_blocks_builder.with_header(assembled.header.clone());

            let l1_block_info = assembled.l1_block_info()?;
            let latest_block_l1_block_info = l1_block_info.clone();
            let latest_block_transaction_count = assembled.block.body.transactions.len();

            let block_env_attributes = OpNextBlockEnvAttributes::from(assembled.base.clone());

            db.block_hashes
                .insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

            let evm_env = evm_config
                .next_evm_env(&last_block_header, &block_env_attributes)
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
            let evm = evm_config.evm_with_env(db, evm_env);

            let recovery_start = Instant::now();
            let txs_with_senders: Vec<(OpTxEnvelope, Address)> = assembled
                .block
                .body
                .transactions
                .par_iter()
                .cloned()
                .map(|tx| -> Result<(OpTxEnvelope, Address)> {
                    let tx_hash = tx.tx_hash();
                    let sender = match prev_pending_blocks
                        .as_ref()
                        .and_then(|p| p.get_transaction_sender(&tx_hash))
                    {
                        Some(cached) => cached,
                        None => tx.recover_signer()?,
                    };
                    Ok((tx, sender))
                })
                .collect::<Result<_>>()?;
            Metrics::sender_recovery_duration().record(recovery_start.elapsed());

            let block_header = assembled.block.header.clone();
            let parent_block_hash = assembled.base.parent_hash;
            let parent_beacon_block_root = Some(assembled.base.parent_beacon_block_root);

            let mut pending_state_builder = PendingStateBuilder::new(
                self.client.chain_spec(),
                evm,
                assembled.block,
                prev_pending_blocks.clone(),
                l1_block_info,
                state_overrides,
            );

            pending_state_builder
                .apply_pre_execution_changes(parent_block_hash, parent_beacon_block_root)?;

            for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
                let tx_hash = transaction.tx_hash();
                Self::execute_into_builder(
                    &mut pending_state_builder,
                    &mut pending_blocks_builder,
                    idx,
                    tx_hash,
                    transaction,
                    sender,
                )?;
            }

            let latest_flashblock_tx_start = total_transaction_count
                .saturating_add(latest_block_transaction_count)
                .saturating_sub(latest_flashblock_tx_count);
            pending_blocks_builder.with_latest_block_context(
                latest_flashblock_tx_start,
                latest_block_base,
                latest_block_l1_block_info,
                latest_block_transaction_count,
                pending_state_builder.cumulative_gas_used(),
                pending_state_builder.next_log_index(),
            );
            total_transaction_count += latest_block_transaction_count;

            (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
            last_block_header = block_header;
        }

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }
}

impl<Client> StateProcessor<Client> {
    fn execute_into_builder<E, ChainSpec>(
        pending_state_builder: &mut PendingStateBuilder<E, ChainSpec>,
        pending_blocks_builder: &mut PendingBlocksBuilder,
        idx: usize,
        tx_hash: alloy_primitives::B256,
        transaction: OpTxEnvelope,
        sender: Address,
    ) -> Result<()>
    where
        E: reth_evm::Evm<HaltReason = op_revm::OpHaltReason>,
        E::DB: revm::Database + revm::DatabaseCommit,
        E::Tx: reth_evm::FromRecoveredTx<OpTxEnvelope>,
        ChainSpec: OpHardforks + EthChainSpec + Clone,
    {
        pending_blocks_builder.with_transaction_sender(tx_hash, sender);
        pending_blocks_builder.increment_nonce(sender);

        let executed_transaction = pending_state_builder
            .execute_transaction(idx, Recovered::new_unchecked(transaction, sender))?;

        if let Some(time_us) = executed_transaction.execution_time_us {
            pending_blocks_builder.with_execution_time(tx_hash, time_us);
        }

        for (address, account) in &executed_transaction.state {
            if account.is_touched() {
                pending_blocks_builder.with_account_balance(*address, account.info.balance);
            }
        }

        pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
        pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
        pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
        pending_blocks_builder.with_transaction_result(tx_hash, executed_transaction.result);

        Ok(())
    }
}
