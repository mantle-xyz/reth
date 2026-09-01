//! Traits for the flashblocks module.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, TxHash, U256};
use alloy_rpc_types_eth::{Filter, Log, state::StateOverride};
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
    fn get_pending_blocks(&self) -> Guard<Option<Arc<PendingBlocks>>>;

    /// Subscribes to flashblock updates.
    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Arc<PendingBlocks>>;
}

/// API for accessing pending blocks data.
///
/// RPC-shaped values cross this boundary as [`serde_json::Value`] so the trait
/// carries no network-specific generics, matching `MantleEthApiExt`.
pub trait PendingBlocksAPI {
    /// Get the canonical block number on top of which all pending state is built.
    fn get_canonical_block_number(&self) -> BlockNumberOrTag;

    /// Get the pending transaction count for an address.
    fn get_transaction_count(&self, address: Address) -> U256;

    /// Retrieves the current block. If `full` is true, includes full transaction details.
    fn get_block(&self, full: bool) -> Option<serde_json::Value>;

    /// Gets transaction receipt by hash.
    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<serde_json::Value>;

    /// Gets transaction details by hash.
    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<serde_json::Value>;

    /// Gets balance for an address. Returns `None` if the address was not touched.
    fn get_balance(&self, address: Address) -> Option<U256>;

    /// Gets the state overrides for the pending blocks.
    fn get_state_overrides(&self) -> Option<StateOverride>;

    /// Gets logs from pending state matching the provided filter.
    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log>;
}
