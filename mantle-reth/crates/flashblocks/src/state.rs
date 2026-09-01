//! Shared flashblock state and update queue.

use std::sync::Arc;

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

    /// Handles a canonical block being received.
    pub fn on_canonical_block_received(&self, block: RecoveredBlock<OpBlock>) {
        let block_number = block.number;
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
