//! Traits for the flashblocks module.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256};
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
/// RPC-shaped values cross this boundary as [`serde_json::Value`] for the reason
/// `EthApiOverride` documents.
pub trait PendingBlocksAPI {
    /// Get the canonical block number on top of which all pending state is built.
    ///
    /// Returns `pending`, not a block number, when there is no overlay: the tag is
    /// left for the standard implementation to resolve.
    fn get_canonical_block_number(&self) -> BlockNumberOrTag;

    /// Get how many transactions the address sent in pending state.
    ///
    /// A delta, not a nonce: the caller adds it to the address's canonical
    /// transaction count to answer `eth_getTransactionCount` on `pending`.
    fn get_transaction_count(&self, address: Address) -> U256;

    /// Retrieves the current block. If `full` is true, includes full transaction details.
    fn get_block(&self, full: bool) -> Option<serde_json::Value>;

    /// Gets transaction receipt by hash.
    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<serde_json::Value>;

    /// Gets transaction details by hash.
    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<serde_json::Value>;

    /// Gets balance for an address. Returns `None` if the address was not touched.
    fn get_balance(&self, address: Address) -> Option<U256>;

    /// Gets code for an address. `None` means fall back to canonical state;
    /// empty bytes are a positive answer, not a miss. [`PendingBlocks::get_code`]
    /// explains why.
    fn get_code(&self, address: Address) -> Option<Bytes>;

    /// Gets a storage slot's value. `None` means fall back to canonical state;
    /// `Some(ZERO)` is a positive answer. [`PendingBlocks::get_storage_at`]
    /// explains why.
    fn get_storage_at(&self, address: Address, slot: B256) -> Option<B256>;

    /// Gets the state overrides for the pending blocks.
    fn get_state_overrides(&self) -> Option<StateOverride>;

    /// Gets logs from pending state matching the provided filter.
    fn get_pending_logs(&self, filter: &Filter) -> Vec<Log>;
}
