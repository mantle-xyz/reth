//! Subscription types for the `eth_` `PubSub` RPC extension.

use alloy_consensus::Eip658Value;
use alloy_primitives::{Address, Bloom};
use alloy_rpc_types_eth::{Log, pubsub::SubscriptionKind};
use derive_more::From;
use op_alloy_rpc_types::Transaction;
use serde::{Deserialize, Serialize};

/// A full transaction object with its associated logs and receipt-equivalent fields.
///
/// Returned by `newFlashblockTransactions` when `full = true` or a log filter is given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithLogs {
    /// The full transaction object.
    #[serde(flatten)]
    pub transaction: Transaction,
    /// Logs emitted by this transaction.
    pub logs: Vec<Log>,
    /// Gas consumed by this transaction's execution.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Status of the transaction, serialized the same way as `eth_getTransactionReceipt`.
    #[serde(flatten)]
    pub status: Eip658Value,
    /// Cumulative gas used in the block up to and including this transaction.
    #[serde(with = "alloy_serde::quantity")]
    pub cumulative_gas_used: u64,
    /// Contract address created, if this was a contract creation transaction.
    pub contract_address: Option<Address>,
    /// Bloom filter for all logs emitted by this transaction.
    pub logs_bloom: Bloom,
}

/// Subscription kind covering both standard Ethereum types and flashblocks types.
///
/// Encapsulating [`SubscriptionKind`] rather than redefining its variants inherits
/// upstream additions automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, From)]
#[serde(untagged)]
pub enum ExtendedSubscriptionKind {
    /// Standard Ethereum subscription types, proxied to reth's `EthPubSub`.
    #[from]
    Standard(SubscriptionKind),
    /// Flashblocks-specific subscription types.
    #[from]
    Flashblocks(FlashblocksSubscriptionKind),
}

/// Flashblocks-specific subscription types.
///
/// The variant names determine the wire values via `rename_all = "camelCase"` and
/// must not be changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlashblocksSubscriptionKind {
    /// Fires each time a new flashblock is processed, carrying the pending block state.
    NewFlashblocks,
    /// Logs from pending flashblock state matching the filter.
    PendingLogs,
    /// Transactions from flashblocks as they are sequenced.
    ///
    /// Accepts `true` for full objects with logs, `false` (default) for hashes only,
    /// or a log filter object to select transactions with at least one matching log.
    NewFlashblockTransactions,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(*kind),
            Self::Flashblocks(_) => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Flashblocks(_))
    }
}
