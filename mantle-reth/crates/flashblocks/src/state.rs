//! Shared flashblock state and update queue.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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
    processor::{QueueConsumer, StateProcessor, StateUpdate},
};

/// Roughly four seconds of flashblocks.
const BUFFER_SIZE: usize = 20;

/// Holds the pending flashblock state and queues incoming updates for the
/// [`StateProcessor`] that [`FlashblocksState::start`] spawns; the processing
/// itself happens there, not here.
#[derive(Debug)]
pub struct FlashblocksState {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<StateUpdate>>>,
    queue_depth: Arc<AtomicUsize>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
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
            queue_depth: Arc::new(AtomicUsize::new(0)),
            flashblock_sender,
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
            QueueConsumer::new(Arc::clone(&self.rx), Arc::clone(&self.queue_depth)),
            self.flashblock_sender.clone(),
        );

        tokio::spawn(async move {
            state_processor.start().await;
        });
    }

    /// Updates queued but not yet taken by the processor.
    ///
    /// The queue is unbounded and carries both canonical notifications and slices, so
    /// a processor falling behind shows up here before it shows up anywhere else.
    pub fn queued_updates(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Queues an update, keeping [`queued_updates`](Self::queued_updates) in step.
    ///
    /// Counted before the send so the depth never reads low while a value is in
    /// flight, and rolled back if the processor is already gone.
    fn enqueue(&self, update: StateUpdate) -> Result<(), mpsc::error::SendError<StateUpdate>> {
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
        self.queue.send(update).inspect_err(|_| {
            self.queue_depth.fetch_sub(1, Ordering::Relaxed);
        })
    }

    /// Handles a canonical block being received.
    pub fn on_canonical_block_received(&self, block: RecoveredBlock<OpBlock>) {
        let block_number = block.number;
        match self.enqueue(StateUpdate::Canonical(block, None)) {
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
    pub async fn process_canonical_block_for_testing(
        &self,
        block: RecoveredBlock<OpBlock>,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue(StateUpdate::Canonical(block, Some(ack_tx)))
            .map_err(|_| "flashblocks processing queue closed")?;
        ack_rx.await.map_err(|_| "flashblocks processor dropped the acknowledgement")
    }

    /// Queues a flashblock and resolves once the processor has handled it.
    ///
    /// Lets tests sequence injections without sleeping. Errors only if the
    /// processor task is gone.
    pub async fn process_flashblock_for_testing(
        &self,
        flashblock: MantleFlashblockPayload,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue(StateUpdate::Flashblock(flashblock, Some(ack_tx)))
            .map_err(|_| "flashblocks processing queue closed")?;
        ack_rx.await.map_err(|_| "flashblocks processor dropped the acknowledgement")
    }
}

impl FlashblocksReceiver for FlashblocksState {
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload) {
        let flashblock_index = flashblock.index;
        let block_number = flashblock.metadata.block_number;
        match self.enqueue(StateUpdate::Flashblock(flashblock, None)) {
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
