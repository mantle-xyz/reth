//! Common types shared across preconf modules.

use alloy_primitives::{B256, Bytes, Log, TxHash};
use serde::{Deserialize, Serialize};

/// Preconfirmation status — matches the wire-layer `PreconfStatus` exposed
/// by `mantle-reth-rpc-ext`.
///
/// State machine (`mark_succeeded` / `mark_failed` / `mark_timeout`
/// / `recover_from_timeout`):
///
/// ```text
///                  ┌──→ Success    (terminal)
/// Waiting ────────┼──→ Failed     (terminal)
///                  └──→ Timeout    (CAS from Waiting; recoverable to Waiting)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PreconfStatus {
    /// Awaiting builder apply.
    #[serde(rename = "waiting")]
    Waiting,
    /// Builder apply succeeded; receipt available.
    #[serde(rename = "success")]
    Success,
    /// Builder apply failed (revert / halt).
    #[serde(rename = "failed")]
    Failed,
    /// Server-side timeout — only Waiting can transition here (CAS).
    #[serde(rename = "timeout")]
    Timeout,
}

/// Receipt produced by builder apply; mirrored to the wire-layer
/// `PreconfTxReceipt` exposed by `mantle-reth-rpc-ext`.
///
/// Conversion to the public wire format happens at the RPC handler boundary
/// to keep this crate free of jsonrpsee dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreconfReceipt {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Predicted L2 block number — set by builder when applying.
    pub block_height: u64,
    /// Whether execution succeeded (status == 1).
    pub status: bool,
    /// EVM logs emitted by the transaction.
    pub logs: Vec<Log>,
    /// Cumulative gas used by the transaction.
    pub gas_used: u64,
    /// Optional revert / halt reason — empty on success.
    pub reason: String,
    /// Raw revert return data — used by RPC handler to abi-decode the revert
    /// reason matching op-geth's `abi.UnpackRevert` behavior.
    pub revert_data: Bytes,
}

/// Result of [`crate::preconf_tx_set::PreconfTxSet::push_if_absent`].
///
/// `push_if_absent` is the single source of truth for fifo membership
/// invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushResult {
    /// New entry created and broadcast notified.
    Inserted,
    /// Same hash already present — idempotent no-op.
    AlreadyExists,
    /// Different hash but same (sender, nonce) in an active status —
    /// blocks the replacement attempt (carrying the existing hash so callers
    /// can inspect / log it).
    ConflictActive(TxHash),
}

/// Errors returned by [`crate::preconf_tx_set::PreconfTxSet::attach_responder`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    /// Existing entry already holds a responder, OR
    /// `pending_responders` already has this hash.
    /// RPC handler should immediately return `AlreadyInProgress` to the client
    /// instead of waiting on a hung oneshot.
    #[error("responder already attached for this hash")]
    AlreadyAttached,
}

/// Errors returned by `PreconfTxSet::mark_succeeded` / `mark_failed`
/// / `mark_timeout` — all share the same `Waiting → target` CAS body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkError {
    /// Entry no longer present — safe to ignore (terminal signal is best-effort).
    #[error("entry not found")]
    NotFound,
    /// Existing status cannot transition to the requested terminal.
    /// Typical race: RPC handler timeout fires same instant builder commits.
    /// Whichever loses gets this error; caller logs but does not panic.
    #[error("illegal transition from {0:?}")]
    IllegalTransition(PreconfStatus),
}

/// Errors returned by [`crate::preconf_tx_set::PreconfTxSet::recover_from_timeout`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoverError {
    /// Entry no longer present — typically lost a race with `clean_timeout`.
    /// RPC handler then falls back to `cancel_responder` with a cleaner error.
    #[error("entry not found")]
    NotFound,
    /// Entry exists but is not in `Timeout` state — recovery is only valid
    /// from `Timeout`. Caller logs current status.
    #[error("expected Timeout but found {0:?}")]
    NotTimeout(PreconfStatus),
}

/// Top-level preconf error returned to RPC clients.
///
/// Maps to `PreconfTxEvent.status + reason` at the wire layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreconfError {
    /// Whitelist check failed — tx (from, to) does not match preconf eligibility.
    #[error("transaction is not preconf eligible (whitelist miss)")]
    NotPreconfEligible,
    /// Nonce gap — tx nonce > pool's pending nonce for sender.
    ///
    /// **Client-visible behavioral choice**: when a client submits a tx
    /// whose nonce skips ahead of the pool's pending nonce, the RPC
    /// handler rejects synchronously with this error rather than
    /// admitting the tx into the pool's queued sub-pool. Clients are
    /// expected to resend in nonce order after observing this code.
    ///
    /// Rationale: admitting the tx and letting later promotions silently
    /// lift it into a preconf commitment hides the failure from the
    /// client — they would see a generic timeout while the tx still
    /// lands on chain, defeating the preconf contract. Surfacing the
    /// gap immediately keeps the client's view and the chain's view in
    /// sync, at the cost of requiring SDKs to handle this error code
    /// explicitly.
    #[error("nonce gap: tx nonce {tx_nonce} > pending nonce {pending_nonce}")]
    NonceGap {
        /// Sender's tx nonce.
        tx_nonce: u64,
        /// Pool's reported pending nonce for the sender.
        pending_nonce: u64,
    },
    /// Pool rejected the transaction (validator error / underpriced / etc.).
    #[error("pool rejected: {0}")]
    PoolRejected(String),
    /// Builder apply returned a terminal `Failed` status.
    #[error("builder rejected: {0}")]
    BuilderRejected(String),
    /// Per-tx gas limit exceeded — operator hardening against pathological
    /// large-gas spam.
    #[error("tx gas limit {limit} exceeds preconf_max_gas_per_tx {max}")]
    GasLimitExceeded {
        /// Configured `preconf_max_gas_per_tx`.
        max: u64,
        /// Tx's gas limit.
        limit: u64,
    },
    /// Cumulative preconf gas budget for the current block has been
    /// exhausted — caller may retry once the next block opens.
    #[error(
        "preconf block gas budget exhausted: used {used} of preconf_max_gas_per_block {max}; \
         tx gas limit {limit}"
    )]
    BlockGasBudgetExceeded {
        /// Configured `preconf_max_gas_per_block`.
        max: u64,
        /// Gas already committed to the preconf path in the current block.
        used: u64,
        /// Tx's gas limit that would have pushed `used` past `max`.
        limit: u64,
    },
    /// Another in-flight responder already exists for this hash.
    /// Maps to `AttachError::AlreadyAttached` at the fifo layer.
    #[error("a preconf request is already in progress for this hash")]
    AlreadyInProgress,
    /// Server-side timeout — receipt did not arrive within `preconf_timeout`.
    #[error("preconf timeout after {timeout_ms}ms")]
    Timeout {
        /// Configured timeout in milliseconds.
        timeout_ms: u64,
    },
    /// Catch-all for unexpected internal errors.
    #[error("internal: {0}")]
    Internal(String),
}

/// Convenience marker for a recoverable transaction hash — currently aliased
/// to `B256` to match the rest of the codebase. Kept as a type alias so we can
/// strengthen it later if needed.
pub type PreconfTxHash = B256;
