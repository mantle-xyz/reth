//! Flashblocks state processor.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use alloy_consensus::{
    Block, BlockBody, Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, Decodable2718};
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, B256, BlockNumber};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use mantle_reth_flashblocks_types::{FlashblockId, MantleFlashblockPayload};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use op_revm::OpSpecId;
use rayon::prelude::*;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::{ConfigureEvm, EvmEnv};
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use revm_database::states::bundle_state::BundleRetention;
use tokio::sync::{Mutex, broadcast::Sender, mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error, info, instrument, warn};

use crate::{
    AssembledBlock, BlockAssembler, ExecutionError, FlashblockCache, PendingBlocks,
    PendingBlocksBuilder, PendingStateBuilder, ProtocolError, ProviderError, Result,
    StateProcessorError,
    metrics::Metrics,
    validation::{
        CanonicalBlockReconciler, FlashblockSequenceValidator, IncomingBuild,
        ReconciliationStrategy, ReorgDetector, SequenceValidationResult, base_links_to_overlay,
        slice_follows_by_content,
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

/// The consuming end of the update queue, paired with the depth its producers keep.
///
/// The two travel together: taking an update is what makes the depth drop, so an
/// owner that held only the receiver could not keep the count honest.
#[derive(Debug)]
pub struct QueueConsumer {
    rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
    depth: Arc<AtomicUsize>,
}

impl QueueConsumer {
    /// Pairs a receiver with the depth counter its senders increment.
    pub const fn new(
        rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
        depth: Arc<AtomicUsize>,
    ) -> Self {
        Self { rx, depth }
    }

    /// Takes the next update, reporting how many remain queued behind it.
    async fn recv(&self) -> Option<(StateUpdate, usize)> {
        let update = self.rx.lock().await.recv().await?;
        let waiting = self.depth.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        Some((update, waiting))
    }
}

/// Processes flashblocks and canonical blocks to keep pending state updated.
#[derive(Debug)]
pub struct StateProcessor<Client> {
    queue: QueueConsumer,
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

        // Every slice freezes a copy so the live database can keep mutating. The cost
        // grows with the overlay, so it is worth watching rather than assuming.
        let clone_started = Instant::now();
        let bundle_state = db.bundle_state.clone();
        Metrics::bundle_state_clone_duration().record(clone_started.elapsed());
        Metrics::bundle_state_clone_size().record(bundle_state.size_hint() as f64);

        pending_blocks_builder.with_bundle_state(bundle_state);
        pending_blocks_builder.with_state_overrides(state_overrides.clone());

        let pending_blocks = Arc::new(pending_blocks_builder.build()?);

        // Deliberately read here rather than taking the loop's value: this guard
        // exists for the window between that read and now, so a fresher tip is the
        // whole point.
        let tip_at_publish = self.canonical_tip();
        if !self.extends_canonical_tip(&pending_blocks, tip_at_publish) {
            debug!(
                message = "built snapshot no longer leads the canonical tip, discarding",
                canonical_tip = ?tip_at_publish,
                latest_pending_block = pending_blocks.latest_block_number(),
            );
            Metrics::pending_drop_not_ahead().increment(1);
            self.clear_live_state();
            return Ok(None);
        }

        if !self.base_is_canonical(&pending_blocks) {
            Metrics::pending_drop_base_mismatch().increment(1);
            self.clear_live_state();
            return Ok(None);
        }

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
        queue: QueueConsumer,
        sender: Sender<Arc<PendingBlocks>>,
    ) -> Self {
        let best = client.best_block_number().unwrap_or(0);
        let cache = FlashblockCache::new(best, max_cache_ahead_blocks);

        Self {
            pending_blocks,
            client,
            max_trailing_depth,
            max_leading_depth,
            queue,
            sender,
            cache: Arc::new(Mutex::new(cache)),
            live_state: StdMutex::new(None),
        }
    }

    /// Processes updates from the queue until the channel closes.
    pub async fn start(&self) {
        while let Some((update, waiting)) = self.queue.recv().await {
            Metrics::queue_depth().set(waiting as f64);

            // Read once per update and threaded down to every guard below it:
            // re-reading would let them disagree within one iteration, which shows up
            // as self-contradicting logs rather than as a wrong decision.
            let tip = self.canonical_tip();
            let prev_pending_blocks = self.load_pending_or_drop_stranded(tip);
            match update {
                StateUpdate::Canonical(block, ack) => {
                    debug!(message = "processing canonical block", block_number = block.number);
                    let reconciled =
                        match self.process_canonical_block(prev_pending_blocks, &block, tip) {
                            Err(
                                e @ StateProcessorError::Protocol(
                                    ProtocolError::ParentHashMismatch { .. },
                                ),
                            ) => {
                                self.drop_on_parent_hash_mismatch(&e);
                                Ok(None)
                            }
                            other => other,
                        };
                    match reconciled {
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
                                    let fb_prev = self.load_pending_or_drop_stranded(tip);
                                    self.apply_flashblock(fb_prev, flashblock, tip).await;
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
                    self.apply_flashblock(prev_pending_blocks, flashblock, tip).await;
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
    fn exceeds_leading_depth(
        &self,
        flashblock: &MantleFlashblockPayload,
        tip: Option<BlockNumber>,
    ) -> bool {
        // An unknown tip admits the slice: refusing work we cannot justify refusing
        // would stall the overlay on a transient provider error.
        let Some(tip) = tip else { return false };
        let leading = flashblock.metadata.block_number.saturating_sub(tip);
        leading > self.max_leading_depth
    }

    /// The node's canonical tip, or `None` when the provider cannot answer.
    ///
    /// Callers decide what `None` means for them: admitting a slice is the safe
    /// default on the flashblock path, keeping the snapshot is the safe default on
    /// the guard paths. Nothing here chooses for them.
    fn canonical_tip(&self) -> Option<BlockNumber> {
        match self.client.best_block_number() {
            Ok(tip) => Some(tip),
            Err(e) => {
                warn!(message = "could not read the canonical tip", error = %e);
                None
            }
        }
    }

    /// Returns `true` when the chain has already canonicalised this slice's block.
    ///
    /// Executing it cannot produce a publishable snapshot, and doing the work anyway
    /// is what lets a backlog sustain itself: superseded payloads occupy the single
    /// consumer while fresh ones wait behind them.
    fn is_superseded(
        &self,
        flashblock: &MantleFlashblockPayload,
        tip: Option<BlockNumber>,
    ) -> bool {
        tip.is_some_and(|tip| flashblock.metadata.block_number <= tip)
    }

    /// Returns `true` when the snapshot still extends past the chain tip.
    ///
    /// A snapshot whose latest block is already canonical holds nothing pending, so
    /// publishing it would serve `pending` from a base the chain has passed.
    ///
    /// Only a race reaches this: every reconciliation branch that rebuilds has
    /// already established `latest > tip`, but the chain can move while the rebuild
    /// runs. An unknown tip keeps the snapshot.
    fn extends_canonical_tip(
        &self,
        pending_blocks: &PendingBlocks,
        tip: Option<BlockNumber>,
    ) -> bool {
        tip.is_none_or(|tip| pending_blocks.latest_block_number() > tip)
    }

    /// Fails when the chain has the parent height under a different hash.
    ///
    /// Earlier than `base_is_canonical` and per block: executing on the wrong
    /// parent is never useful, and only the overlay's earliest block is checked at
    /// publish. A parent the chain does not have yet, or a provider error, is left
    /// to the other guards.
    fn ensure_canonical_parent(&self, block_number: BlockNumber, declared: B256) -> Result<()> {
        let Some(parent_number) = block_number.checked_sub(1) else { return Ok(()) };
        match self.client.block_hash(parent_number) {
            Ok(Some(canonical)) if canonical != declared => {
                Err(ProtocolError::ParentHashMismatch { block_number, declared, canonical }.into())
            }
            _ => Ok(()),
        }
    }

    /// Drops the overlay after a base payload named a non-canonical parent.
    fn drop_on_parent_hash_mismatch(&self, error: &StateProcessorError) {
        Metrics::parent_hash_mismatch().increment(1);
        warn!(message = "base payload is not built on the canonical parent, dropping the overlay", %error);
        self.clear_live_state();
        self.pending_blocks.swap(None);
    }

    /// Returns `true` when the snapshot's base block is still the canonical block
    /// at that height.
    ///
    /// The parent-hash and predecessor checks judge a block before it runs; this
    /// one catches a snapshot whose base the chain replaced after it was built —
    /// a reorg or a failover putting a different block at the same height.
    ///
    /// Read on every publish rather than driven by canonical notifications: a head
    /// rollback whose target has left the engine's in-memory tree emits none, and a
    /// guard waiting to be told would never run for that shape.
    ///
    /// Fails closed — a base that cannot be confirmed is treated as a bad one. This
    /// is a publish-time check, not a continuous invariant: a snapshot that already
    /// published and is later invalidated keeps being served until the next publish
    /// attempt runs this check again.
    fn base_is_canonical(&self, pending_blocks: &PendingBlocks) -> bool {
        // Saturating to 0 would compare against genesis and always mismatch, logging
        // a reorg that did not happen. There is no flashblock for block 0, so this is
        // unreachable; the assert catches a violation instead of misdiagnosing it.
        debug_assert!(
            pending_blocks.earliest_block_number() >= 1,
            "pending overlay's earliest block cannot be genesis"
        );
        let base_number = pending_blocks.earliest_block_number().saturating_sub(1);
        let declared = pending_blocks.parent_hash();

        match self.client.block_hash(base_number) {
            Ok(Some(canonical)) if canonical == declared => true,
            Ok(Some(canonical)) => {
                warn!(
                    message = "pending snapshot is anchored to a block that is no longer canonical",
                    base_number,
                    declared = %declared,
                    canonical = %canonical,
                );
                false
            }
            Ok(None) => {
                warn!(
                    message = "pending snapshot is anchored to a height the chain no longer has",
                    base_number,
                    declared = %declared,
                );
                false
            }
            Err(err) => {
                warn!(
                    message = "could not confirm the pending snapshot's base block",
                    base_number,
                    declared = %declared,
                    %err,
                );
                false
            }
        }
    }

    /// Returns `true` when the snapshot has fallen too far behind to be worth keeping.
    ///
    /// Measured from `latest`, not `earliest`: `PendingBlocksBuilder::from_previous`
    /// carries the earliest header forward unchanged, so `tip - earliest` is the
    /// snapshot's width, which the trailing-depth limit already governs.
    ///
    /// Reuses `max_trailing_depth` deliberately. Since `earliest <= latest`, this
    /// bound is strictly weaker than the one `DepthLimitExceeded` applies, so it can
    /// only fire where reconciliation would already have rebuilt — it just does not
    /// wait for a notification that may never come.
    fn is_stranded_behind_tip(
        &self,
        pending_blocks: &PendingBlocks,
        tip: Option<BlockNumber>,
    ) -> bool {
        tip.is_some_and(|tip| {
            tip.saturating_sub(pending_blocks.latest_block_number()) > self.max_trailing_depth
        })
    }

    /// Loads the published snapshot, dropping it first if it is stranded behind the tip.
    ///
    /// Every other staleness guard runs inside `process_canonical_block`, so a stream
    /// that delivers slices but no canonical notifications would otherwise leave the
    /// overlay anchored wherever it was, indefinitely.
    fn load_pending_or_drop_stranded(
        &self,
        tip: Option<BlockNumber>,
    ) -> Option<Arc<PendingBlocks>> {
        let pending_blocks = self.pending_blocks.load_full()?;

        if !self.is_stranded_behind_tip(&pending_blocks, tip) {
            return Some(pending_blocks);
        }

        debug!(
            message = "pending snapshot stranded behind the canonical tip, dropping",
            canonical_tip = ?tip,
            latest_pending_block = pending_blocks.latest_block_number(),
            max_trailing_depth = self.max_trailing_depth,
        );
        Metrics::pending_drop_stranded().increment(1);
        self.clear_live_state();
        self.pending_blocks.store(None);

        None
    }

    /// Canonical height to reconcile a notification against.
    ///
    /// Canonical notifications and slices share one queue, so under backlog the
    /// notification being handled can sit far below the chain. Reconciling against it
    /// alone evaluates every guard in a stale frame: the overlay keeps trailing the
    /// real tip while `CatchUp` and the depth limit both read as satisfied. Taking the
    /// larger of the two keeps the guards anchored to the chain. Falls back to the
    /// notification when the provider cannot answer, which is what the pre-tip
    /// behaviour was.
    fn effective_canonical_number(notified: BlockNumber, tip: Option<BlockNumber>) -> BlockNumber {
        tip.map_or(notified, |tip| notified.max(tip))
    }

    async fn apply_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: MantleFlashblockPayload,
        tip: Option<BlockNumber>,
    ) {
        if self.is_superseded(&flashblock, tip) {
            debug!(
                message = "flashblock is for an already canonical block, skipping",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                canonical_tip = ?tip,
            );
            Metrics::flashblock_superseded().increment(1);
            return;
        }

        if self.exceeds_leading_depth(&flashblock, tip) {
            // Leading depth bounds published previews, not the early-arrival
            // cache. Preserve restartable slices within its independent window.
            let block_number = flashblock.metadata.block_number;
            let index = flashblock.index;
            let mut cache = self.cache.lock().await;
            let resumable = index == 0 || cache.has_flashblock(block_number, index - 1);
            if resumable && cache.is_cacheable(block_number) {
                cache.insert(flashblock);
                debug!(
                    message = "cached flashblock beyond the overlay leading depth",
                    block_number, index,
                );
                return;
            }
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
        // Kept to tell "the overlay moved" from "the slice was ignored": the
        // `Duplicate` arm hands the previous snapshot straight back, and
        // re-broadcasting it makes subscribers replay a batch of transactions
        // they have already seen. The published value is a *delta* —
        // `get_latest_flashblock_logs` skips the first
        // `latest_flashblock_tx_start`, a field frozen when the snapshot was
        // built — so sending the same `Arc` twice recomputes the same
        // non-empty slice rather than an empty one.
        let previous = prev_pending_blocks.clone();
        match self.process_flashblock(prev_pending_blocks, &flashblock) {
            Ok(new_pending_blocks) => {
                if let Some(ref pb) = new_pending_blocks &&
                    !previous.as_ref().is_some_and(|old| Arc::ptr_eq(old, pb))
                {
                    _ = self.sender.send(Arc::clone(pb));
                }
                self.pending_blocks.swap(new_pending_blocks);
                Metrics::block_processing_duration().record(start_time.elapsed());
            }
            Err(e) => {
                match e {
                    StateProcessorError::Protocol(ProtocolError::StalePredecessor) => {
                        Metrics::stale_predecessor().increment(1);
                        warn!(
                            message = "base slice does not link to the overlay's parent; restarting from the canonical parent",
                            block = %flashblock.metadata.block_number,
                        );
                        // Fail closed: the overlay this slice refused to extend is not served.
                        self.clear_live_state();
                        self.pending_blocks.swap(None);
                        // The same path as a fresh start, cached if the parent has not landed.
                        match self.process_flashblock(None, &flashblock) {
                            Ok(new_pending_blocks) => {
                                if let Some(ref pb) = new_pending_blocks {
                                    _ = self.sender.send(Arc::clone(pb));
                                }
                                self.pending_blocks.swap(new_pending_blocks);
                                Metrics::block_processing_duration().record(start_time.elapsed());
                            }
                            Err(StateProcessorError::Provider(
                                ProviderError::MissingCanonicalHeader { .. },
                            )) => {
                                self.cache.lock().await.insert(flashblock);
                            }
                            Err(
                                e @ StateProcessorError::Protocol(
                                    ProtocolError::ParentHashMismatch { .. },
                                ),
                            ) => self.drop_on_parent_hash_mismatch(&e),
                            Err(e) => {
                                error!(
                                    message = "could not process flashblock from the canonical parent",
                                    error = %e,
                                );
                                Metrics::block_processing_error().increment(1);
                            }
                        }
                        return;
                    }
                    StateProcessorError::Protocol(ProtocolError::ParentHashMismatch { .. }) => {
                        // Fail closed and do not cache: the parent is canonical already,
                        // so a replay would hit the same mismatch.
                        self.drop_on_parent_hash_mismatch(&e);
                        return;
                    }
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
        tip: Option<BlockNumber>,
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
            Self::effective_canonical_number(block.number, tip),
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

        // A base slice whose parent block is in the overlay extends it only when it
        // names that block's last slice by position and content. Anything else goes
        // to the canonical parent instead.
        if flashblock.index == 0 &&
            let Some(parent_number) = flashblock.metadata.block_number.checked_sub(1) &&
            let Some(parent_last) = pending_blocks.last_flashblock_of(parent_number) &&
            !base_links_to_overlay(
                (
                    FlashblockId { block_number: parent_number, index: parent_last.index },
                    parent_last.diff.block_hash,
                ),
                flashblock.metadata.prev_flashblock_id,
                flashblock.metadata.prev_flashblock_hash,
            )
        {
            return Err(ProtocolError::StalePredecessor.into());
        }

        // The next slice of the tracked build must follow the overlay's latest slice by
        // content. Only the next one: a duplicate names the slice before the latest,
        // and gaps and other builds are the validator's to judge.
        if flashblock.index > 0 &&
            flashblock.payload_id == pending_blocks.payload_id() &&
            flashblock.metadata.block_number == pending_blocks.latest_block_number() &&
            flashblock.index == pending_blocks.latest_flashblock_index().saturating_add(1) &&
            let Some(latest) =
                pending_blocks.last_flashblock_of(pending_blocks.latest_block_number()) &&
            !slice_follows_by_content(
                latest.diff.block_hash,
                flashblock.metadata.prev_flashblock_hash,
            )
        {
            Metrics::slice_content_mismatch().increment(1);
            warn!(
                message = "slice does not follow the overlay's latest slice by content; dropping the overlay",
                block = %flashblock.metadata.block_number,
                index = %flashblock.index,
            );
            self.clear_live_state();
            return Ok(None);
        }

        let incoming_build = if flashblock.payload_id == pending_blocks.payload_id() {
            IncomingBuild::Same
        } else if flashblock.base.is_some() {
            IncomingBuild::OtherWithBase
        } else {
            IncomingBuild::OtherWithoutBase
        };

        let validation_result = FlashblockSequenceValidator::validate(
            pending_blocks.latest_block_number(),
            pending_blocks.latest_flashblock_index(),
            flashblock.metadata.block_number,
            flashblock.index,
            flashblock.metadata.prev_flashblock_id,
            incoming_build,
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
            SequenceValidationResult::RebuiltBlock => {
                // Not `unexpected_block_order`: a second build of a height is
                // something the sequencer does on purpose, and counting it
                // there would drown the stream faults that counter is read for.
                Metrics::rebuilt_block().increment(1);
                info!(
                    message = "the sequencer started this block again; rebuilding from its new base slice",
                    curr_block = %pending_blocks.latest_block_number(),
                    curr_flashblock_index = %pending_blocks.latest_flashblock_index(),
                );
                // The overlay is replaced rather than extended, so the live
                // state it was carrying belongs to the abandoned attempt.
                self.clear_live_state();
                // Only the rebuilt block is dropped. The blocks under it are
                // usually not canonical yet — the overlay is what runs ahead of
                // the chain — so rebuilding from canonical state alone could
                // not reach the rebuilt block's parent.
                let mut flashblocks = pending_blocks.get_flashblocks();
                flashblocks
                    .retain(|fb| fb.metadata.block_number < flashblock.metadata.block_number);
                flashblocks.push(flashblock.clone());
                self.build_pending_state(None, &flashblocks)
            }
            SequenceValidationResult::ForeignBuild => {
                // Its own counter, not `unexpected_block_order`: the stream is
                // not faulty, it is a correct stream of a build this node never
                // saw the start of. What it reports is that a resume handed
                // over a replacement's continuation without its base — the one
                // signal that situation otherwise leaves.
                Metrics::foreign_build_slice().increment(1);
                warn!(
                    message = "received a slice from another build of the tracked block; dropping the overlay rather than splicing",
                    block = %flashblock.metadata.block_number,
                    new_flashblock_index = %flashblock.index,
                    curr_flashblock_index = %pending_blocks.latest_flashblock_index(),
                );
                self.clear_live_state();
                Ok(None)
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

        // Taken through a temporary guard: the rebuild in the fallback below writes the
        // live state back, so holding the guard across it deadlocks this thread.
        //
        // Taken, and on every failure below **not put back**. That is the
        // point: each fallible step from here owns the `db`, so a failure
        // leaves it holding half a transaction's writes. Returning that to the
        // slot would hand the next slice a state no block ever had. The cost
        // of dropping it is one full rebuild — `build_pending_state` replays
        // the overlay from the canonical parent, the same path a cold start
        // takes — and `state_overrides` survives it because replaying the same
        // slices yields the same values (last writer wins per address/slot).
        let live_state = self.lock_live_state().take();
        let Some(LivePendingState { mut db, state_overrides }) = live_state else {
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
        self.ensure_canonical_parent(base.block_number, base.parent_hash)?;

        // Taken through a temporary guard: the rebuild in the fallback below writes the
        // live state back, so holding the guard across it deadlocks this thread.
        //
        // Taken, and on every failure below **not put back**. That is the
        // point: each fallible step from here owns the `db`, so a failure
        // leaves it holding half a transaction's writes. Returning that to the
        // slot would hand the next slice a state no block ever had. The cost
        // of dropping it is one full rebuild — `build_pending_state` replays
        // the overlay from the canonical parent, the same path a cold start
        // takes — and `state_overrides` survives it because replaying the same
        // slices yields the same values (last writer wins per address/slot).
        let live_state = self.lock_live_state().take();
        let Some(LivePendingState { mut db, state_overrides }) = live_state else {
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
        let evm_env = next_block_evm_env(&evm_config, &previous_header, &base)?;
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
        let canonical_block =
            earliest_block_number.checked_sub(1).ok_or(ProtocolError::GenesisFlashblock)?;
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
            self.ensure_canonical_parent(assembled.base.block_number, assembled.base.parent_hash)?;
            let latest_flashblock_tx_count =
                flashblocks.last().map(|latest| latest.diff.transactions.len()).unwrap_or_default();
            let latest_block_base = assembled.base.clone();

            pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
            pending_blocks_builder.with_header(assembled.header.clone());

            let l1_block_info = assembled.l1_block_info()?;
            let latest_block_l1_block_info = l1_block_info.clone();
            let latest_block_transaction_count = assembled.block.body.transactions.len();

            db.block_hashes
                .insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

            let evm_env = next_block_evm_env(&evm_config, &last_block_header, &assembled.base)?;
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

/// The EVM environment for the block `base` opens, on `parent`. The basefee is
/// the producer's: it is what the sealed header carries, and deriving it from
/// a parent the overlay assembled would follow any gap in that parent.
fn next_block_evm_env<ChainSpec>(
    evm_config: &OpEvmConfig<ChainSpec>,
    parent: &Header,
    base: &OpFlashblockPayloadBase,
) -> Result<EvmEnv<OpSpecId>>
where
    ChainSpec: EthChainSpec<Header = Header> + OpHardforks + 'static,
{
    let mut evm_env = evm_config
        .next_evm_env(parent, &OpNextBlockEnvAttributes::from(base.clone()))
        .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
    let declared: u64 = base.base_fee_per_gas.saturating_to();
    if evm_env.block_env.basefee != declared {
        Metrics::base_fee_mismatch().increment(1);
        warn!(
            message = "derived basefee differs from the base slice; using the base slice's",
            block = %base.block_number,
            derived = evm_env.block_env.basefee,
            declared,
        );
        evm_env.block_env.basefee = declared;
    }
    Ok(evm_env)
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
