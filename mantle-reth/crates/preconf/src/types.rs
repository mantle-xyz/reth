//! Common types shared across preconf modules.

use alloy_primitives::{Bytes, Log, TxHash, U256};
use serde::{Deserialize, Serialize};

/// Preconfirmation status — matches the wire-layer `PreconfStatus` exposed
/// by `mantle-reth-rpc-ext`.
///
/// State machine (transitions via `PreconfTxSet::mark_*` / `recover_*` /
/// `reset_success_to_waiting`):
///
/// ```text
///                     ┌──→ Success   (applied to an in-flight builder;
///                     │              dropped by `forward()` on canon commit;
///                     │              if still present after canon → stale
///                     │              in-flight, reset via
///                     │              `reset_success_to_waiting` → Waiting.
///                     │              Note: EVM revert / halt also reach this
///                     │              status — the receipt carries
///                     │              `status = false`, but the tx does land
///                     │              on chain, matching op-geth semantics.)
///                     ├──→ Failed    (NOT on chain — reth builder rejected
///                     │              the tx pre-execute: nonce-too-low /
///                     │              gas-over-block-limit / other
///                     │              `BlockExecutionError::Validation`.
///                     │              Distinct from EVM revert / halt, which
///                     │              flow through the Success arm above.
///                     │              Reclaimable: same-hash retry revives
///                     │              via `push_if_absent` → Waiting.)
/// [push] → Waiting ──┤
///                     ├──→ Timeout   (NOT on chain — client's deadline hit;
///                     │               same-hash retry revives via
///                     │               `push_if_absent` → Waiting)
///                     └──→ Canceled  (NOT on chain — server pre-apply reject:
///                                      block gas budget, admin kick, etc.;
///                                      same-hash retry revives via
///                                      `push_if_absent` → Waiting)
/// ```
///
/// All three "not on chain" states (`Failed` / `Timeout` / `Canceled`)
/// are **reclaimable** — the pool eviction hook fires from `mark_*`, and
/// a subsequent same-hash resubmit is revived back to `Waiting` by
/// `push_if_absent`. This mirrors the "typically transient" nature of
/// each cause: `Timeout` (client just gave up too early), `Canceled`
/// (block gas budget resets next slot), `Failed` (in-flight state race that
/// the next slot's fresh block state usually resolves).
///
/// **Fifo-layer `Failed` vs wire-layer `PreconfStatus::Failed`** — they
/// mean different things and are NOT connected by a direct mapping:
/// - Fifo `Failed` = builder rejected pre-execute, tx NOT on chain
/// - Wire `Failed` (see `mantle-reth-rpc-ext::PreconfStatus`) = `receipt.status == false` (revert /
///   halt), tx IS on chain
///
/// The wire-layer status is derived by the RPC handler from the
/// returned `PreconfReceipt.status` field, not from this enum.
///
/// All forward transitions are CAS: they require current status == Waiting,
/// otherwise `MarkError::IllegalTransition(current)` is returned.
///
/// **Success is not strictly terminal**: a `Success` entry that still exists
/// in the fifo means "applied to an in-flight builder but that builder's
/// block was never canon'd" — because `canon_handler::forward()` drops the
/// entry entirely on canon commit. On a new payload job start, such stale
/// `Success` entries are reset to `Waiting` and re-applied against the new
/// builder to honor the mantle preconf SLA ("receipt returned → tx must
/// land on chain"). The presence-of-entry acts as the "in-flight, not
/// canon" flag; no separate `InFlight` variant is needed.
///
/// **Timeout vs Canceled vs Failed** — all three are "not on chain,
/// reclaimable" but signal different causes to the client:
/// - `Timeout` — the RPC handler's deadline elapsed. Client's request was accepted; server may or
///   may not have run apply.
/// - `Canceled` — the block-gas-budget pre-apply gate rejected the tx (e.g. block gas budget).
///   Server explicitly declined; no EVM state change.
/// - `Failed` — reth's block builder rejected pre-execute (in-flight nonce / balance race, block
///   gas exhausted at builder level). tx NOT on chain; typically resolves on next slot.
///
/// SDKs retry all three the same way: same-hash resubmit is safe;
/// `push_if_absent` revives the fifo entry back to `Waiting` and the
/// dispatch loop picks it up. Client-visible fast Err (same-slot dedup
/// forwards the stored reason) or Ok(Timeout) (RPC deadline) both
/// signal "try next slot".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PreconfStatus {
    /// Awaiting builder apply.
    #[serde(rename = "waiting")]
    Waiting,
    /// Builder apply succeeded; receipt available.
    #[serde(rename = "success")]
    Success,
    /// Reth builder rejected pre-execute (in-flight nonce/balance race,
    /// block gas exhausted at builder level, or other
    /// `BlockExecutionError::Validation`). tx NOT on chain. Reclaimable
    /// via same-hash resubmit (`push_if_absent` Timeout/Canceled/Failed →
    /// Waiting revive branch). Distinct from wire-layer `Failed`, which
    /// means EVM revert with tx on chain.
    #[serde(rename = "failed")]
    Failed,
    /// Server-side timeout — only Waiting can transition here (CAS).
    #[serde(rename = "timeout")]
    Timeout,
    /// Server pre-apply rejection (block gas budget, admin action, ...) —
    /// only Waiting can transition here (CAS). Recoverable via
    /// `recover_from_canceled`.
    #[serde(rename = "canceled")]
    Canceled,
}

/// Origin of a preconf entry in the fifo. Determines which pre-apply
/// gates apply during dispatch.
///
/// - `Rpc` — pushed by the RPC handler on behalf of an active client session. Subject to the
///   deadline and per-block gas budget gates so the client's SLA and server budget are both
///   honored.
/// - `Replay` — pushed to fulfill a commitment the sequencer has already promised to a client.
///   Covers two triggers:
///     - **Startup journal replay** (`restore_preconf_state`) — commitments persisted before a
///       crash.
///     - **Reorg reinject** — the pool re-admits a previously sealed tx after reorg; the pool
///       listener detects the case via `journal.sealed` membership. In both cases the Mantle
///       preconf SLA (*"once a receipt has been returned to the client, the tx must land on
///       chain"*) requires these entries to **bypass** the deadline and per-block gas budget gates.
///       They remain subject to the status / dedup gates and the underlying block gas limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreconfSource {
    /// Live RPC submission — subject to all pre-apply gates.
    Rpc,
    /// Replay of a previously-promised commitment (startup journal
    /// restore or pool reorg reinject). Bypasses deadline and
    /// gas-budget gates so promised txs are guaranteed to land.
    Replay,
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
    /// Same hash already present and in an active status
    /// (`Waiting` / `Success` / `Failed`) — idempotent no-op.
    AlreadyExists,
    /// Same hash was in a **reclaimable** terminal state
    /// (`Timeout` / `Canceled`) and has been revived back to `Waiting`.
    /// Any fresh responder that the RPC handler attached to
    /// `pending_responders` is now installed on the entry, and the
    /// entry's insertion clock is refreshed to the fresh submission
    /// time — so dispatch's deadline gate measures against the second
    /// submission, not the (already-expired) first.
    ///
    /// This closes the "same-hash resubmit after timeout" loop that
    /// would otherwise wedge under the pool-eviction callback: the
    /// second `pool.add_transaction` returns `Ok(_)` (fresh admission)
    /// rather than `Err(AlreadyImported)`, so any RPC-side revive
    /// logic keyed on `AlreadyImported` never fires — but the pool
    /// listener still ends up calling `push_if_absent`, which now
    /// revives the reclaimable entry here and broadcasts.
    Revived,
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
    /// Cumulative cost across the sender's pending txs exceeds its balance, so
    /// the pool parks this tx (`!ENOUGH_BALANCE`) instead of promoting it —
    /// rejected synchronously to avoid a full-timeout block, same as
    /// [`Self::NonceGap`]. Best-effort: the builder's gates are the final
    /// authority.
    #[error(
        "insufficient funds: sender balance {balance} < required {required} \
         (cumulative cost across the sender's pending txs)"
    )]
    InsufficientFunds {
        /// Sender's on-chain balance.
        balance: U256,
        /// Cumulative cost required to promote this tx: the sum of
        /// `cost + extra_balance_cost` over the sender's gapless pending
        /// chain, including this tx.
        required: U256,
    },
    /// Pool rejected the transaction (validator error / underpriced / etc.).
    #[error("pool rejected: {0}")]
    PoolRejected(String),
    /// Builder apply returned a terminal `Failed` status.
    #[error("builder rejected: {0}")]
    BuilderRejected(String),
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
    /// The preconf tx's estimated data-availability (DA) footprint would
    /// push the in-flight block past a configured DA limit (per-tx,
    /// per-block, or the post-Jovian footprint-gas bound). Rejected
    /// **before** touching the builder — a preconf tx over the DA budget
    /// would make the sealed block DA-invalid and get rejected by op-node,
    /// silently breaking the commitment (a DA consensus constraint).
    ///
    /// Unlike [`Self::BlockGasBudgetExceeded`] (an operator-hardening
    /// budget bypassed by `Replay`-sourced entries), the DA limit is a
    /// consensus constraint enforced for **all** sources. The tx stays
    /// reclaimable — a same-hash resubmit in a later slot (with DA
    /// headroom) is revived and applied.
    #[error("preconf tx exceeds DA limit: tx DA {tx_da} bytes, {used} already used, limit {limit}")]
    DaLimitExceeded {
        /// DA bytes already committed to the in-flight block.
        used: u64,
        /// This tx's estimated DA footprint.
        tx_da: u64,
        /// The DA bound that would be exceeded (per-tx / per-block bytes,
        /// or the post-Jovian footprint-gas bound, whichever fired).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: the serde tag names for `PreconfStatus` are the wire
    /// format seen by RPC clients. Any accidental case / spelling change
    /// silently breaks SDKs — pin the exact strings here.
    #[test]
    fn preconf_status_serde_wire_format() {
        for (status, expected) in [
            (PreconfStatus::Waiting, "\"waiting\""),
            (PreconfStatus::Success, "\"success\""),
            (PreconfStatus::Failed, "\"failed\""),
            (PreconfStatus::Timeout, "\"timeout\""),
            (PreconfStatus::Canceled, "\"canceled\""),
        ] {
            let s = serde_json::to_string(&status).unwrap();
            assert_eq!(s, expected, "wire format for {status:?} changed");
            let round: PreconfStatus = serde_json::from_str(&s).unwrap();
            assert_eq!(round, status);
        }
    }

    /// The Display strings for user-facing error variants are part of the
    /// public wire contract — SDKs may parse them (e.g. the exact
    /// "nonce gap: tx nonce N > pending nonce M" wording). Pin the
    /// substrings to catch accidental rewording.
    #[test]
    fn preconf_error_display_wording_is_stable() {
        assert_eq!(
            PreconfError::NonceGap { tx_nonce: 85, pending_nonce: 75 }.to_string(),
            "nonce gap: tx nonce 85 > pending nonce 75",
        );
        assert_eq!(
            PreconfError::Timeout { timeout_ms: 200 }.to_string(),
            "preconf timeout after 200ms",
        );
        assert_eq!(
            PreconfError::BlockGasBudgetExceeded {
                max: 6_000_000,
                used: 5_500_000,
                limit: 800_000,
            }
            .to_string(),
            "preconf block gas budget exhausted: used 5500000 of preconf_max_gas_per_block \
             6000000; tx gas limit 800000",
        );
        assert_eq!(
            PreconfError::NotPreconfEligible.to_string(),
            "transaction is not preconf eligible (whitelist miss)",
        );
        assert_eq!(
            PreconfError::DaLimitExceeded { used: 1_000, tx_da: 500, limit: 1_200 }.to_string(),
            "preconf tx exceeds DA limit: tx DA 500 bytes, 1000 already used, limit 1200",
        );
        assert_eq!(
            PreconfError::InsufficientFunds {
                balance: U256::from(100),
                required: U256::from(150),
            }
            .to_string(),
            "insufficient funds: sender balance 100 < required 150 \
             (cumulative cost across the sender's pending txs)",
        );
    }

    /// `PreconfReceipt`'s `PartialEq` is byte-equal at the field
    /// level, not derived semantically. This test locks the field set
    /// so a future field addition without updating the wire mapper
    /// surfaces as a compile error (missing field literal below), and
    /// each field participates in equality (a differing value on any
    /// one field must make the two receipts distinct).
    #[test]
    fn preconf_receipt_field_level_diff_participates_in_partialeq() {
        use alloy_primitives::{Address, B256, Bytes, Log, LogData};

        // Reference construction — every field explicitly named so a
        // struct-shape change (new / removed field) forces update.
        let base = PreconfReceipt {
            tx_hash: B256::from([1; 32]),
            block_height: 100,
            status: true,
            logs: vec![Log {
                address: Address::from([2; 20]),
                data: LogData::new_unchecked(vec![B256::from([3; 32])], Bytes::from(vec![4, 5, 6])),
            }],
            gas_used: 21_000,
            reason: String::new(),
            revert_data: Bytes::new(),
        };
        assert_eq!(base, base.clone(), "identical clone equals base");

        // Each field diverging in isolation makes the receipt unequal.
        let mut r = base.clone();
        r.tx_hash = B256::ZERO;
        assert_ne!(r, base);

        let mut r = base.clone();
        r.block_height = 101;
        assert_ne!(r, base);

        let mut r = base.clone();
        r.status = false;
        assert_ne!(r, base);

        let mut r = base.clone();
        r.logs.clear();
        assert_ne!(r, base);

        let mut r = base.clone();
        r.gas_used = 22_000;
        assert_ne!(r, base);

        let mut r = base.clone();
        r.reason = "revert".to_string();
        assert_ne!(r, base);

        let mut r = base.clone();
        r.revert_data = Bytes::from(vec![0xff]);
        assert_ne!(r, base);
    }
}
