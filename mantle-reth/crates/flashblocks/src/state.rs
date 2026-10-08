//! Shared flashblock state and update queue.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::Header;
use arc_swap::{ArcSwapOption, Guard};
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use tokio::sync::{
    Mutex,
    broadcast::{self, Sender},
    mpsc,
};
use tracing::{debug, error, info};

use crate::{
    FlashblocksAPI, FlashblocksReceiver, PendingBlocks,
    metrics::Metrics,
    processor::{StateProcessor, StateUpdate},
};

/// Roughly four seconds of flashblocks.
const BUFFER_SIZE: usize = 20;

/// Manages the pending flashblock state and processes incoming updates.
#[derive(Debug)]
pub struct FlashblocksState {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<StateUpdate>>>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
    /// Latest canonical height seen by the notification path, read on ingest so superseded
    /// payloads can be rejected before they reach the queue.
    last_canonical_block: AtomicU64,
    max_trailing_depth: u64,
    max_leading_depth: u64,
    max_cache_ahead_blocks: u64,
}

impl FlashblocksState {
    /// Creates a new flashblocks state manager.
    ///
    /// Call [`start`](Self::start) with a client to spawn the state processor once the
    /// node is launched.
    pub fn new(
        max_trailing_depth: u64,
        max_leading_depth: u64,
        max_cache_ahead_blocks: u64,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<StateUpdate>();
        let pending_blocks: Arc<ArcSwapOption<PendingBlocks>> = Arc::new(ArcSwapOption::new(None));
        let (flashblock_sender, _) = broadcast::channel(BUFFER_SIZE);

        Self {
            pending_blocks,
            queue: tx,
            rx: Arc::new(Mutex::new(rx)),
            flashblock_sender,
            last_canonical_block: AtomicU64::new(0),
            max_trailing_depth,
            max_leading_depth,
            max_cache_ahead_blocks,
        }
    }

    /// Starts the flashblocks state processor with the given client.
    pub fn start<Client>(&self, client: Client)
    where
        Client: StateProviderFactory
            + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
            + BlockReaderIdExt<Header = Header>
            + Clone
            + 'static,
    {
        let state_processor = StateProcessor::new(
            client,
            Arc::clone(&self.pending_blocks),
            self.max_trailing_depth,
            self.max_leading_depth,
            self.max_cache_ahead_blocks,
            Arc::clone(&self.rx),
            self.flashblock_sender.clone(),
        );

        tokio::spawn(async move {
            state_processor.start().await;
        });
    }

    /// Records the canonical height and drops a snapshot the chain has already passed. Doing
    /// this on the notification task, rather than waiting for the processor to dequeue, bounds
    /// staleness by chain progress instead of by processor progress.
    fn note_canonical(&self, canonical_block_number: u64) {
        // Deliberately not `fetch_max`: a reorg can move the tip down, and keeping the higher
        // height would suppress every flashblock built on the replacement chain.
        self.last_canonical_block.store(canonical_block_number, Ordering::Relaxed);
        self.drop_pending_behind(canonical_block_number);
    }

    /// Drops the published snapshot once canonical caught up to or passed its tip. Mirrors
    /// `ReconciliationStrategy::CatchUp`, so it only moves that verdict earlier: no new
    /// threshold, and both depth guards stay untouched.
    fn drop_pending_behind(&self, canonical_block_number: u64) {
        let published = self.pending_blocks.load();
        let Some(stale) = published.as_ref() else { return };

        let latest_pending_block = stale.latest_block_number();
        if latest_pending_block > canonical_block_number {
            return;
        }

        // Clear only the snapshot that was judged. The processor publishes concurrently, and
        // anything it published after the load above tracks a later tip than this one. Losing
        // this race costs nothing, because an absent snapshot is always safe to serve.
        let previous = self.pending_blocks.compare_and_swap(&published, None);
        if !previous.as_ref().is_some_and(|previous| Arc::ptr_eq(previous, stale)) {
            return;
        }

        Metrics::pending_drop_stale().increment(1);
        debug!(
            message = "dropped pending snapshot the canonical chain has passed",
            canonical_block = canonical_block_number,
            latest_pending_block,
        );
    }

    /// Handles a canonical block being received.
    pub fn on_canonical_block_received(&self, block: RecoveredBlock<OpBlock>) {
        let block_number = block.number;
        self.note_canonical(block_number);
        match self.queue.send(StateUpdate::Canonical(block, None)) {
            Ok(_) => {
                info!(message = "added canonical block to processing queue", block_number)
            }
            Err(e) => {
                error!(message = "could not add canonical block to processing queue", block_number, error = %e);
            }
        }
    }

    /// Sets the pending blocks directly, bypassing the processing pipeline.
    pub fn set_pending_blocks_for_testing(&self, pending_blocks: Option<PendingBlocks>) {
        self.pending_blocks.store(pending_blocks.map(Arc::new));
    }

    /// Queues a canonical block and resolves once the processor has reconciled it.
    ///
    /// Lets tests sequence delivery without sleeping. Errors only if the
    /// processor task is gone.
    ///
    /// Bypasses [`Self::on_canonical_block_received`]: this drives the processor, it does not
    /// exercise the ingest path, whose filters are unit tested against the real methods.
    pub async fn process_canonical_block_for_testing(
        &self,
        block: RecoveredBlock<OpBlock>,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.queue
            .send(StateUpdate::Canonical(block, Some(ack_tx)))
            .map_err(|_| "flashblocks processing queue closed")?;
        ack_rx.await.map_err(|_| "flashblocks processor dropped the acknowledgement")
    }

    /// Queues a flashblock and resolves once the processor has handled it.
    ///
    /// Lets tests sequence injections without sleeping. Errors only if the
    /// processor task is gone.
    ///
    /// Bypasses [`FlashblocksReceiver::on_flashblock_received`] for the same reason.
    pub async fn process_flashblock_for_testing(
        &self,
        flashblock: MantleFlashblockPayload,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.queue
            .send(StateUpdate::Flashblock(flashblock, Some(ack_tx)))
            .map_err(|_| "flashblocks processing queue closed")?;
        ack_rx.await.map_err(|_| "flashblocks processor dropped the acknowledgement")
    }
}

impl FlashblocksReceiver for FlashblocksState {
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload) {
        let flashblock_index = flashblock.index;
        let block_number = flashblock.metadata.block_number;

        // Rejecting superseded payloads here keeps them out of the queue entirely, so a backlog
        // cannot grow on work that could never produce a publishable snapshot. The processor
        // repeats the check for payloads that were fresh on arrival but went stale while queued.
        if block_number <= self.last_canonical_block.load(Ordering::Relaxed) {
            debug!(
                message = "dropping flashblock for an already canonical block",
                block_number, flashblock_index,
            );
            Metrics::flashblock_superseded().increment(1);
            return;
        }

        match self.queue.send(StateUpdate::Flashblock(flashblock, None)) {
            Ok(_) => {
                debug!(
                    message = "added flashblock to processing queue",
                    block_number, flashblock_index,
                );
            }
            Err(e) => {
                error!(message = "could not add flashblock to processing queue", block_number, flashblock_index, error = %e);
            }
        }
    }
}

impl FlashblocksAPI for FlashblocksState {
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>> {
        self.pending_blocks.load()
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>> {
        self.flashblock_sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Sealed};
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::PayloadId;
    use mantle_reth_flashblocks_types::MantleFlashblockMetadata;
    use op_alloy_rpc_types_engine::{OpFlashblockPayloadBase, OpFlashblockPayloadDelta};

    use super::*;
    use crate::PendingBlocksBuilder;

    const TRAILING: u64 = 3;
    const LEADING: u64 = 3;
    const CACHE_AHEAD: u64 = 5;

    fn state() -> FlashblocksState {
        FlashblocksState::new(TRAILING, LEADING, CACHE_AHEAD)
    }

    fn flashblock_for_block(block_number: u64) -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(OpFlashblockPayloadBase { block_number, ..Default::default() }),
            diff: OpFlashblockPayloadDelta::default(),
            metadata: MantleFlashblockMetadata { block_number, ..Default::default() },
        }
    }

    /// Builds a snapshot whose earliest and latest pending blocks differ, matching the shape
    /// [`PendingBlocksBuilder::from_previous`] produces on a live chain.
    fn pending_spanning(earliest: u64, latest: u64) -> PendingBlocks {
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([flashblock_for_block(earliest)]);
        builder.with_header(Sealed::new_unchecked(
            Header { number: earliest, ..Default::default() },
            B256::ZERO,
        ));
        if latest != earliest {
            builder.with_flashblocks([flashblock_for_block(latest)]);
            builder.with_header(Sealed::new_unchecked(
                Header { number: latest, ..Default::default() },
                B256::ZERO,
            ));
        }
        builder.build().expect("pending fixture builds")
    }

    fn pending_anchored_at(block_number: u64) -> PendingBlocks {
        pending_spanning(block_number, block_number)
    }

    fn canonical_block(block_number: u64) -> RecoveredBlock<OpBlock> {
        RecoveredBlock::new_unhashed(
            Block {
                header: Header { number: block_number, ..Default::default() },
                body: BlockBody::default(),
            },
            vec![],
        )
    }

    /// The processor is never started here, so only the notification path can clear the
    /// snapshot. That is the property that bounds staleness while the processor is busy.
    #[test]
    fn canonical_notification_drops_a_retired_snapshot_without_the_processor() {
        let state = state();
        state.set_pending_blocks_for_testing(Some(pending_anchored_at(1)));

        state.on_canonical_block_received(canonical_block(1));

        assert!(
            state.get_pending_blocks().is_none(),
            "a snapshot the chain has caught up to must not survive the notification"
        );
    }

    /// Mirrors `ReconciliationStrategy::CatchUp`, which fires on `latest <= canonical`: a
    /// snapshot that still leads must be left alone, or every notification would wipe live
    /// state. This is what keeps the eager drop from becoming a second depth guard.
    #[test]
    fn canonical_notification_keeps_a_snapshot_that_still_leads() {
        let state = state();
        state.set_pending_blocks_for_testing(Some(pending_spanning(1, 3)));

        state.on_canonical_block_received(canonical_block(2));

        assert!(
            state.get_pending_blocks().is_some(),
            "normal lag must not clear pending, however wide the snapshot spans"
        );
    }

    /// Not only about queue growth: the processor has no ingest-side filter, so a slice for a
    /// settled block arrives with no previous state, carries index 0, and reopens an overlay
    /// for a block that is already canonical.
    #[tokio::test]
    async fn a_superseded_flashblock_never_enters_the_queue() {
        let state = state();
        state.on_canonical_block_received(canonical_block(7));

        state.on_flashblock_received(flashblock_for_block(3));

        let mut rx = state.rx.lock().await;
        assert!(
            matches!(rx.try_recv(), Ok(StateUpdate::Canonical(..))),
            "the canonical notification itself is still queued for reconciliation"
        );
        assert!(
            rx.try_recv().is_err(),
            "a flashblock for an already canonical block must not consume queue capacity"
        );
    }

    #[tokio::test]
    async fn a_flashblock_ahead_of_canonical_still_enters_the_queue() {
        let state = state();
        state.on_canonical_block_received(canonical_block(7));

        state.on_flashblock_received(flashblock_for_block(8));

        let mut rx = state.rx.lock().await;
        assert!(matches!(rx.try_recv(), Ok(StateUpdate::Canonical(..))));
        assert!(
            matches!(rx.try_recv(), Ok(StateUpdate::Flashblock(..))),
            "a slice ahead of canonical must still be queued"
        );
    }

    /// A reorg can move the tip down. Storing rather than maxing is what keeps slices built on
    /// the replacement chain from being rejected as superseded.
    #[tokio::test]
    async fn a_lower_canonical_tip_reopens_the_heights_above_it() {
        let state = state();
        state.on_canonical_block_received(canonical_block(7));
        state.on_canonical_block_received(canonical_block(5));

        state.on_flashblock_received(flashblock_for_block(6));

        let mut rx = state.rx.lock().await;
        assert!(matches!(rx.try_recv(), Ok(StateUpdate::Canonical(..))));
        assert!(matches!(rx.try_recv(), Ok(StateUpdate::Canonical(..))));
        assert!(
            matches!(rx.try_recv(), Ok(StateUpdate::Flashblock(..))),
            "after the tip moves down, slices above it must be admitted again"
        );
    }
}
