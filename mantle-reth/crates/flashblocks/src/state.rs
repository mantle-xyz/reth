//! Shared flashblock state and update queue.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
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
use tracing::{debug, error, info, warn};

use crate::{
    FlashblocksAPI, FlashblocksReceiver, Metrics, PendingBlocks,
    processor::{PROCESSOR_RESTART_DELAY, QueueConsumer, StateProcessor, StateUpdate, supervise},
};

/// Roughly four seconds of flashblocks.
const BUFFER_SIZE: usize = 20;

/// Most slices that may wait in the update queue; further slices are dropped.
pub const MAX_QUEUED_SLICES: usize = 256;

/// Holds the pending flashblock state and queues incoming updates for the
/// [`StateProcessor`] that [`FlashblocksState::start`] spawns; the processing
/// itself happens there, not here.
#[derive(Debug)]
pub struct FlashblocksState {
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    queue: mpsc::UnboundedSender<StateUpdate>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<StateUpdate>>>,
    queue_depth: Arc<AtomicUsize>,
    slice_depth: Arc<AtomicUsize>,
    flashblock_sender: Sender<Arc<PendingBlocks>>,
    max_trailing_depth: u64,
    max_leading_depth: u64,
    max_cache_ahead_blocks: u64,
    /// Runs the started processor's `reset_after_panic`; set by `start`.
    reset_processor: StdMutex<Option<ResetProcessor>>,
}

type ResetFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
struct ResetProcessor(Arc<dyn Fn(String) -> ResetFuture + Send + Sync>);

impl std::fmt::Debug for ResetProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResetProcessor")
    }
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
            slice_depth: Arc::new(AtomicUsize::new(0)),
            flashblock_sender,
            max_trailing_depth,
            max_leading_depth,
            max_cache_ahead_blocks,
            reset_processor: StdMutex::new(None),
        }
    }

    /// Starts the flashblocks state processor with the given client.
    ///
    /// After a panic the processor clears the preview and its slice cache, waits
    /// [`PROCESSOR_RESTART_DELAY`] and takes updates again, with no cap on restarts.
    pub fn start<Client>(&self, client: Client)
    where
        Client: StateProviderFactory
            + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
            + BlockReaderIdExt<Header = Header>
            + Clone
            + 'static,
    {
        let state_processor = Arc::new(StateProcessor::new(
            client,
            Arc::clone(&self.pending_blocks),
            self.max_trailing_depth,
            self.max_leading_depth,
            self.max_cache_ahead_blocks,
            QueueConsumer::new(
                Arc::clone(&self.rx),
                Arc::clone(&self.queue_depth),
                Arc::clone(&self.slice_depth),
            ),
            self.flashblock_sender.clone(),
        ));

        let resetting = Arc::clone(&state_processor);
        *self.reset_processor.lock().unwrap() = Some(ResetProcessor(Arc::new(move |panic| {
            let processor = Arc::clone(&resetting);
            Box::pin(async move { processor.reset_after_panic(&panic).await })
        })));
        let restarting = Arc::clone(&state_processor);
        tokio::spawn(supervise(
            move || {
                let processor = Arc::clone(&state_processor);
                async move { processor.start().await }
            },
            move |panic: String| {
                let processor = Arc::clone(&restarting);
                async move { processor.reset_after_panic(&panic).await }
            },
            PROCESSOR_RESTART_DELAY,
        ));
    }

    /// Updates queued but not yet taken by the processor.
    ///
    /// The queue carries both canonical notifications and slices, so a processor
    /// falling behind shows up here before it shows up anywhere else. Only slices are
    /// capped, at [`MAX_QUEUED_SLICES`]; canonical notifications are always queued.
    pub fn queued_updates(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Queues an update, keeping both depth counters in step.
    ///
    /// Counted before the send so neither reads low while a value is in flight,
    /// and rolled back if the send fails. Does not check [`MAX_QUEUED_SLICES`].
    fn enqueue(&self, update: StateUpdate) -> Result<(), mpsc::error::SendError<StateUpdate>> {
        let is_slice = matches!(update, StateUpdate::Flashblock(..));
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
        if is_slice {
            self.slice_depth.fetch_add(1, Ordering::Relaxed);
        }
        self.queue.send(update).inspect_err(|_| {
            self.queue_depth.fetch_sub(1, Ordering::Relaxed);
            if is_slice {
                self.slice_depth.fetch_sub(1, Ordering::Relaxed);
            }
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

    /// Runs the started processor's post-panic reset, as the supervisor does.
    ///
    /// Panics if [`start`](Self::start) has not been called.
    pub async fn reset_processor_after_panic_for_testing(&self, panic: &str) {
        let reset = self
            .reset_processor
            .lock()
            .unwrap()
            .clone()
            .expect("the processor must be started before it can be reset");
        (reset.0)(panic.to_owned()).await;
    }

    /// Queues a canonical block and resolves once the processor has reconciled it.
    ///
    /// Lets tests sequence delivery without sleeping. Errors if the processor
    /// panics while handling it; never resolves if [`start`](Self::start) was not called.
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
    /// Lets tests sequence injections without sleeping. Errors if the processor
    /// panics while handling it; never resolves if [`start`](Self::start) was not called.
    /// Not subject to [`MAX_QUEUED_SLICES`].
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
        if self.slice_depth.load(Ordering::Relaxed) >= MAX_QUEUED_SLICES {
            Metrics::queue_dropped_slices().increment(1);
            warn!(
                message = "dropped flashblock: processing queue is full",
                block_number,
                flashblock_index,
                limit = MAX_QUEUED_SLICES,
            );
            return;
        }
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
