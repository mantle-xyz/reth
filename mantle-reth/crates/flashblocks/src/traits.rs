//! Traits for the flashblocks module.

use std::sync::Arc;

use arc_swap::Guard;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use tokio::sync::broadcast;

use crate::PendingBlocks;

/// Trait for receiving flashblock updates.
pub trait FlashblocksReceiver {
    /// Called when a new flashblock is received.
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload);
}

/// Core API for accessing flashblock state and data.
pub trait FlashblocksAPI {
    /// Retrieves the pending blocks.
    ///
    /// RPC readers must go through `EthApiExt::fresh_pending` or
    /// `PendingStateOverrides::pending_execution_context`, which skip a preview that no
    /// longer leads the canonical tip.
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>>;

    /// Subscribes to flashblock updates.
    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>>;
}
