//! Preconf dispatch helpers for
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload).
//!
//! The select! main loop inside `build_payload` calls these helpers
//! one hash at a time. Four invariants are enforced for every hash:
//!
//! - **Dedup**: a hash already in `committed` or `excluded` is short-circuited before any fifo /
//!   EVM work.
//! - **Status gate**: only `Waiting` entries proceed; terminal entries are recorded as excluded and
//!   skipped.
//! - **Pre-apply deadline**: when `entry.inserted_at.elapsed() + safety_margin >= preconf_timeout`,
//!   the tx is *not* applied; the fifo entry is flipped to `Timeout` and the responder is cancelled
//!   directly here. This closes the race where the RPC client has already given up but the builder
//!   is about to commit a receipt.
//! - **Responder ownership**: every terminal path (success, deadline skip, status-already-terminal)
//!   calls exactly one of `take_responder` / `cancel_responder`, never both.
//!
//! ## Apply-fn injection
//!
//! The actual EVM apply is **injected as a closure** by the caller
//! (typically
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload)).
//! This keeps `dispatch.rs` free of EVM types and trait gymnastics
//! around the `BlockBuilder` generic — `apply_one_preconf` just
//! orchestrates the fifo state machine and responder ownership, while
//! the closure captures `&mut builder` and runs
//! [`apply_preconf_tx`](crate::apply::apply_preconf_tx) against the
//! in-flight state.
//!
//! Tests in this module pass a synthetic-receipt closure (no EVM) so
//! the state-machine invariants are exercised in isolation. End-to-end
//! EVM behaviour is covered by devnet integration tests.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alloy_consensus::TxEnvelope;
use alloy_primitives::{Address, TxHash};
use tracing::{debug, error, trace, warn};

use reth_payload_builder_primitives::PayloadBuilderError;

use crate::{
    PreconfConfig, PreconfTxSet,
    apply::ApplyError,
    types::{ApplyFailure, PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
};

/// How a sender's preconf chain is blocked for the rest of the current slot
/// once one of its txs cannot enter the in-flight block. Same-sender entries
/// at a nonce ≥ the blocked head inherit this outcome — they depend on the
/// blocked predecessor landing first, so admitting them independently would
/// only produce a spurious nonce-too-high failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    /// Predecessor was deferred (transient capacity) → successor is also kept
    /// `Waiting` and retried next slot.
    Defer,
    /// Predecessor was permanently rejected → successor can never land either
    /// (permanent nonce gap) → `mark_canceled` (server pre-apply rejection).
    Reject,
}

/// Per-job local state for the preconf dispatch loop.
///
/// Owned by [`build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload)
/// — one per payload job. Dropped when the build completes / cancels.
#[derive(Debug)]
pub(super) struct LoopState {
    /// Hashes already committed to the in-flight block.
    committed: HashSet<TxHash>,
    /// Hashes excluded — terminal-non-success, deadline-skip, etc. The
    /// stored [`PreconfError`] is the rejection reason from the first
    /// time this hash was excluded; a subsequent same-slot resubmit
    /// dedups against this map and forwards the same reason to any
    /// newly-attached responder, so the client observes a consistent
    /// error rather than a slow-Timeout on retry.
    excluded: HashMap<TxHash, PreconfError>,
    /// Predicted L2 block height for this slot. Stamped onto every
    /// receipt as `PreconfReceipt::block_height`.
    predicted_height: u64,
    /// Cumulative preconf-path gas committed in this block. Compared
    /// against `cfg.preconf_max_gas_per_block` before each apply; when
    /// adding the next tx's `gas_limit` would exceed the budget, the
    /// apply is aborted with `PreconfError::BlockGasBudgetExceeded`.
    /// Incremented by the actual `receipt.gas_used` after a successful
    /// apply — reserving `gas_limit` would over-count against later
    /// txs that could still fit.
    preconf_gas_used: u64,
    /// Senders whose preconf chain is blocked this slot: `sender → (lowest
    /// blocked nonce, kind)`. Populated when a preconf tx is deferred or
    /// permanently rejected by the block-capacity admission gate; consulted
    /// before admitting any same-sender entry so nonce successors inherit the
    /// predecessor's outcome instead of being applied out of order (which
    /// would nonce-too-high fail). Slot-local — reset each build.
    blocked_senders: HashMap<Address, (u64, BlockKind)>,
}

impl LoopState {
    /// Construct a fresh local state for a payload job targeting
    /// `predicted_height` (the parent's block number + 1).
    pub(super) fn new(predicted_height: u64) -> Self {
        Self {
            committed: HashSet::new(),
            excluded: HashMap::new(),
            predicted_height,
            preconf_gas_used: 0,
            blocked_senders: HashMap::new(),
        }
    }

    /// Record that `sender`'s preconf chain is blocked from `nonce` onward
    /// this slot with `kind`. Keeps the **lowest** blocked nonce (and that
    /// head's kind), so the earliest non-admitted tx governs the chain even
    /// if entries are seen slightly out of order.
    pub(super) fn block_sender(&mut self, sender: Address, nonce: u64, kind: BlockKind) {
        self.blocked_senders
            .entry(sender)
            .and_modify(|head| {
                if nonce < head.0 {
                    *head = (nonce, kind);
                }
            })
            .or_insert((nonce, kind));
    }

    /// If `sender` is blocked this slot at a head nonce ≤ `nonce`, return the
    /// [`BlockKind`] the entry should inherit. `None` when the sender is
    /// unblocked or `nonce` is below the blocked head (a predecessor that
    /// should still be attempted).
    pub(super) fn sender_blocked_at(&self, sender: &Address, nonce: u64) -> Option<BlockKind> {
        self.blocked_senders
            .get(sender)
            .and_then(|(head_nonce, kind)| (nonce >= *head_nonce).then_some(*kind))
    }

    /// Cumulative preconf gas committed in this block so far. Test-only
    /// accessor for budget-tracking assertions — production accounting
    /// reads the `preconf_gas_used` field directly in the budget gate, and
    /// the payload builder now folds gas into `ExecutionInfo` inside
    /// `apply_preconf_with_da` rather than syncing via this getter.
    #[cfg(test)]
    pub(super) fn preconf_gas_used(&self) -> u64 {
        self.preconf_gas_used
    }

    /// `true` iff the hash was recorded as committed (apply succeeded).
    /// Callers use this to distinguish "already applied, silently skip"
    /// from "already excluded, forward the recorded rejection reason".
    pub(super) fn is_committed(&self, hash: &TxHash) -> bool {
        self.committed.contains(hash)
    }

    /// If `hash` was previously excluded in this loop instance, return
    /// the stored rejection reason. `None` when the hash is either
    /// unseen or was committed. Callers forward the returned error to
    /// any late-arriving responder so a same-slot resubmit sees the
    /// same wire error as the first submission, rather than waiting the
    /// full `preconf_timeout` and getting a generic `Ok(Timeout)`.
    pub(super) fn excluded_reason(&self, hash: &TxHash) -> Option<&PreconfError> {
        self.excluded.get(hash)
    }

    /// Mark hash as committed. Idempotent.
    pub(super) fn record_committed(&mut self, hash: TxHash) {
        self.committed.insert(hash);
    }

    /// Mark hash as excluded with the rejection reason. The first
    /// exclusion wins — subsequent calls with the same hash keep the
    /// original reason so re-submissions in the same slot observe the
    /// wire error that fired on the initial gate.
    pub(super) fn record_excluded(&mut self, hash: TxHash, reason: PreconfError) {
        self.excluded.entry(hash).or_insert(reason);
    }

    /// Drop a hash from the excluded map. Called by `apply_one_preconf`
    /// when a prior `Timeout` exclusion needs to be re-evaluated
    /// against a fresh `entry.inserted_at` (refreshed by
    /// `attach_responder` on same-hash resubmit), so a stale exclusion
    /// does not shadow a legitimately re-eligible tx.
    pub(super) fn clear_excluded(&mut self, hash: &TxHash) {
        self.excluded.remove(hash);
    }

    /// Number of committed hashes — used by tests/metrics.
    #[cfg(test)]
    pub(super) fn committed_len(&self) -> usize {
        self.committed.len()
    }

    /// Number of excluded hashes — used by tests/metrics.
    #[cfg(test)]
    pub(super) fn excluded_len(&self) -> usize {
        self.excluded.len()
    }
}

/// Handle one preconf hash end-to-end: dedup → fetch → status gate →
/// pre-apply deadline → caller-supplied apply → fifo mark + responder
/// send.
///
/// `apply_fn` receives `(tx, hash, predicted_height)` and is responsible
/// for executing the transaction against the in-flight `BlockBuilder` /
/// `State<DB>` and producing the receipt. The caller injects the
/// closure so this module stays free of EVM-builder generics.
/// Per call, `apply_fn` is invoked at most once — on success-path
/// reach. If a dedup / status / deadline / gas-budget guard fires
/// earlier, `apply_fn` is not called. (The type stays `FnMut` because
/// [`reconcile_lagged`] reuses the same closure across many hashes.)
///
/// All terminal paths invoke `take_responder` or `cancel_responder`
/// exactly once.
///
/// Returns `Err(PayloadBuilderError)` **only** when `apply_fn` reports an
/// [`ApplyError::Fatal`] — a non-tx-specific execution error (DB / header /
/// fatal precompile). In that case the caller must abort the whole build
/// (mirroring the pool arm); the fifo entry is left `Waiting` and its
/// responder untouched so the commitment is retried on the next build cycle
/// rather than reneged on. Every other path — success, per-tx
/// [`ApplyError::Rejected`], and all pre-apply gates — returns `Ok(())`.
pub(super) async fn apply_one_preconf<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    hash: TxHash,
    loop_state: &mut LoopState,
    mut apply_fn: F,
) -> Result<(), PayloadBuilderError>
where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, ApplyError>,
{
    // Dedup — short-circuit if we've already made a decision on this
    // hash in this build. Committed hashes just return silently (the
    // apply's responder was consumed by `take_responder` earlier);
    // excluded hashes forward the stored rejection reason to any
    // newly-attached responder so a same-slot resubmit sees the same
    // error the first attempt fired, rather than waiting the full
    // `preconf_timeout` for the RPC-layer deadline to elapse.
    //
    // Exception: a prior `Timeout` exclusion is **re-evaluated** rather
    // than forwarded. The deadline gate below checks
    // `entry.inserted_at.elapsed()` against `cfg.preconf_timeout`, and
    // `attach_responder`'s reclaimable-state branch refreshes
    // `inserted_at` when the client resubmits after a Timeout. So the
    // deadline that fired for the first submission does NOT apply to
    // the fresh submission — forwarding the stale Timeout would deny
    // service to a legitimately re-eligible tx. We drop the stale
    // exclusion here and let the gate below fire against the refreshed
    // clock; if the deadline is still exceeded, the gate will re-record
    // exclusion with the fresh timeout.
    if loop_state.is_committed(&hash) {
        trace!(target: "mantle::preconf::dispatch", ?hash, "dedup hit; already committed");
        return Ok(());
    }
    if let Some(reason) = loop_state.excluded_reason(&hash).cloned() {
        if matches!(reason, PreconfError::Timeout { .. }) {
            trace!(
                target: "mantle::preconf::dispatch",
                ?hash,
                "prior exclusion was Timeout; clearing to re-evaluate against refreshed inserted_at"
            );
            loop_state.clear_excluded(&hash);
            // Fall through to gate evaluation.
        } else {
            trace!(
                target: "mantle::preconf::dispatch",
                ?hash, ?reason,
                "dedup hit; forwarding prior rejection to any pending responder"
            );
            fifo.cancel_responder(&hash, reason).await;
            return Ok(());
        }
    }

    let Some(entry) = fifo.find_by_hash(&hash).await else {
        trace!(target: "mantle::preconf::dispatch", ?hash, "no fifo entry; skipping");
        return Ok(());
    };

    if entry.status != PreconfStatus::Waiting {
        // Already terminal — either a prior iteration finished it or
        // the RPC timeout flipped it. Record so the next broadcast
        // event short-circuits at the dedup gate above; the reason is
        // derived from the terminal status since we did not run the
        // gate ourselves.
        let reason = match entry.status {
            PreconfStatus::Timeout => {
                PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 }
            }
            PreconfStatus::Broken => {
                PreconfError::CommitmentBroken { attempts: entry.apply_failures }
            }
            other => PreconfError::Internal(format!(
                "preconf entry already terminal ({other:?}) at dispatch entry"
            )),
        };
        loop_state.record_excluded(hash, reason);
        return Ok(());
    }

    // The deadline and per-block gas budget gates below only apply to
    // RPC-sourced entries. Journal-replayed entries bypass both to
    // honor the mantle preconf SLA: "once a receipt has been returned
    // to the client, the tx must land on chain". Rejecting them here
    // would silently break that commitment. They remain subject to the
    // status / dedup gates above and to the underlying block gas limit
    // enforced by the block builder.
    let is_rpc = entry.source == PreconfSource::Rpc;

    // Pre-apply deadline check — see crate-level docs. `cfg.safety_margin`
    // (default 40ms, see `DEFAULT_SAFETY_MARGIN`) is sized to slightly
    // exceed measured p99 apply latency on the target hardware so the
    // skip only fires on genuine races rather than merely slow-but-in-
    // budget applies. Kept separate from `preconf_timeout` (the client-
    // facing SLA) so hardware tuning does not implicitly widen the client
    // contract. Setting `cfg.safety_margin = Duration::ZERO` opens the
    // full race window for tests that need to exercise `rpc.rs`'s
    // race-resolution branch.
    let margin = cfg.safety_margin;

    // Sample the elapsed-since-insertion for every RPC-sourced dispatch
    // decision (skipped and applied alike). Downstream analysis reads
    // the distribution to see how close the pipeline runs to the client
    // deadline, informing tuning of `SAFETY_MARGIN` and
    // `preconf_timeout`. Replay-sourced entries are excluded because
    // their `inserted_at` reflects a journal restore, not the client's
    // clock.
    let elapsed_at_gate = entry.inserted_at.elapsed();
    if is_rpc {
        metrics::histogram!("preconf.dispatch.elapsed_at_gate_ms")
            .record(elapsed_at_gate.as_millis() as f64);
    }

    if is_rpc && elapsed_at_gate + margin >= cfg.preconf_timeout {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            elapsed_ms = elapsed_at_gate.as_millis() as u64,
            "pre-apply deadline passed; aborting"
        );
        metrics::counter!("preconf.dispatch.deadline_skipped_total").increment(1);
        let _ = fifo.mark_timeout(&hash).await;
        let reason = PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 };
        fifo.cancel_responder(&hash, reason.clone()).await;
        loop_state.record_excluded(hash, reason);
        return Ok(());
    }

    // Block-level preconf gas budget gate. Pessimistic check: if adding
    // this tx's `gas_limit` would push cumulative preconf gas past
    // `cfg.preconf_max_gas_per_block`, abort now. Sizing off `gas_limit`
    // (worst case) ensures the reservation stays sound even if the
    // closure ends up spending less than the tx claimed. Uses `>` so
    // exact-boundary hits (`used + limit == max`) are accepted.
    //
    // fifo state is `Canceled` (server pre-apply rejection, tx not on
    // chain) — semantically distinct from `Failed` (EVM apply ran and
    // reverted, tx on chain).
    let tx_gas_limit = alloy_consensus::Transaction::gas_limit(entry.tx.as_ref());
    if is_rpc &&
        loop_state.preconf_gas_used.saturating_add(tx_gas_limit) > cfg.preconf_max_gas_per_block
    {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            used = loop_state.preconf_gas_used,
            limit = tx_gas_limit,
            max = cfg.preconf_max_gas_per_block,
            "block gas budget exhausted; aborting apply"
        );
        metrics::counter!("preconf.dispatch.gas_budget_skipped_total").increment(1);
        let _ = fifo.mark_canceled(&hash).await;
        let reason = PreconfError::BlockGasBudgetExceeded {
            max: cfg.preconf_max_gas_per_block,
            used: loop_state.preconf_gas_used,
            limit: tx_gas_limit,
        };
        fifo.cancel_responder(&hash, reason.clone()).await;
        loop_state.record_excluded(hash, reason);
        return Ok(());
    }

    // ── Point of no return begins here ────────────────────────────────
    //
    // Acquire the per-entry `apply_lock` before running `apply_fn`.
    // Held through the entire commit path (`apply_fn` + mark_* + send)
    // so the RPC deadline branch in `rpc::handle_inner` cannot mark
    // this entry `Timeout` while its receipt is on its way to the
    // client. Guarantees "wire Timeout ⇒ tx not committed to builder
    // state".
    let Some(_apply_guard) = fifo.lock_for_apply(&hash).await else {
        // Entry vanished between the gate reads above and this
        // acquisition — very rare (raced with `drop_hash`). Skip.
        loop_state.record_excluded(
            hash,
            PreconfError::Internal("preconf entry vanished before apply_lock".into()),
        );
        return Ok(());
    };

    // Re-check status under lock. The RPC deadline branch may have
    // already transitioned the entry to `Timeout` in the window between
    // our earlier gate reads and this acquisition; running `apply_fn`
    // now would violate the invariant "committed to builder state ⇒
    // wire not Timeout".
    if let Some(re_entry) = fifo.find_by_hash(&hash).await &&
        re_entry.status != PreconfStatus::Waiting
    {
        trace!(
            target: "mantle::preconf::dispatch",
            ?hash, status = ?re_entry.status,
            "status flipped before we acquired apply_lock; skipping apply"
        );
        let reason = match re_entry.status {
            PreconfStatus::Timeout => {
                PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 }
            }
            PreconfStatus::Broken => {
                PreconfError::CommitmentBroken { attempts: re_entry.apply_failures }
            }
            other => PreconfError::Internal(format!(
                "preconf entry flipped to {other:?} before apply_lock"
            )),
        };
        loop_state.record_excluded(hash, reason);
        return Ok(());
    }

    // ── Apply via caller-supplied closure (real EVM in production,
    //    synthetic receipt in tests). ────────────────────────────────
    let apply_started = std::time::Instant::now();
    let apply_result = apply_fn(entry.tx.clone(), hash, loop_state.predicted_height);
    let apply_duration = apply_started.elapsed();
    // Distribution of EVM apply latency — feeds SAFETY_MARGIN tuning.
    // Recorded once per call regardless of outcome; success / failure
    // counters (below) provide the breakdown.
    metrics::histogram!("preconf.execute.duration_ms").record(apply_duration.as_millis() as f64);

    match apply_result {
        Ok(receipt) => {
            metrics::counter!("preconf.tx.success_total").increment(1);
            loop_state.record_committed(hash);
            loop_state.preconf_gas_used =
                loop_state.preconf_gas_used.saturating_add(receipt.gas_used);
            if let Err(e) = fifo.mark_succeeded(&hash).await {
                // Lost a race with clean_timeout / cancel — entry already
                // gone or in a non-Waiting state. Log and continue; the
                // responder still gets the receipt if it exists.
                trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?e,
                    "mark_succeeded lost race"
                );
            }
            if let Some(resp) = fifo.take_responder(&hash).await {
                let _ = resp.send(Ok(receipt));
            }
        }
        // Per-tx rejection of an entry the client is still waiting on.
        // Mark it `Failed` (revivable via same-hash resubmit), evict it from
        // the pool, hand the client the concrete error, and keep building.
        //
        // Guarded on `is_rpc`: an already-acknowledged commitment must not take
        // this path — see the `Rejected` arm below.
        Err(ApplyError::Rejected(err)) if is_rpc => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?err,
                "preconf apply rejected tx; marking entry as Failed"
            );
            metrics::counter!("preconf.tx.failure_total").increment(1);
            loop_state.record_excluded(hash, err.clone());
            if let Err(e) = fifo.mark_failed(&hash).await {
                trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?e,
                    "mark_failed lost race"
                );
            }
            if let Some(resp) = fifo.take_responder(&hash).await {
                let _ = resp.send(Err(err));
            }
        }
        // Fatal, non-tx-specific execution error (DB / header / fatal
        // precompile). The execution environment is untrustworthy, so we
        // abort the whole build — same policy as the pool arm
        // (`payload_builder::apply_one_best_tx`). Crucially we do NOT
        // `mark_failed` / evict / respond: the entry stays `Waiting` and
        // its responder stays attached so the still-valid commitment is
        // retried on the next build cycle instead of being silently
        // dropped while a possibly-corrupt block gets sealed.
        Err(ApplyError::Fatal(e)) => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?e,
                "preconf apply hit fatal execution error; aborting build \
                 (commitment left Waiting for retry)"
            );
            metrics::counter!("preconf.tx.fatal_total").increment(1);
            return Err(e);
        }
        // This entry's receipt has already gone out (`Replay` covers journal
        // restore, reorg reinject, and stale-in-flight replay; all three imply a
        // Success receipt was returned).
        //
        // `mark_failed` would make it replaceable by another hash, let
        // `clean_reclaimable` sweep it, AND evict it from the pool — which
        // destroys the very thing a retry needs. So instead: keep it `Waiting`
        // and in the pool, and let the next payload job's carryover preamble
        // apply it against fresh block state. Give up only after
        // `preconf_max_apply_attempts`, and give up into `Broken`, which is not
        // replaceable.
        //
        // Only `Rejected` reaches here: `Fatal` returned above, because an
        // untrustworthy execution environment says nothing about this
        // commitment and must not burn one of its retry attempts.
        Err(ApplyError::Rejected(err)) => {
            metrics::counter!("preconf.tx.failure_total").increment(1);
            // Recording the exclusion is load-bearing, not cosmetic: `loop_state`
            // is per-build, and without it a later broadcast event (or
            // `reconcile_lagged`) in *this same block* would apply the entry
            // again and burn the whole retry budget inside one slot. The budget
            // is meant to span slots.
            match fifo.record_apply_failure(&hash, cfg.preconf_max_apply_attempts).await {
                Ok(ApplyFailure::Retrying { attempts }) => {
                    warn!(
                        target: "mantle::preconf::dispatch",
                        ?hash, ?err, attempts,
                        max = cfg.preconf_max_apply_attempts,
                        "apply failed for an already-acknowledged commitment; keeping it Waiting to retry next slot"
                    );
                    metrics::counter!("preconf.tx.replay_retry_total").increment(1);
                    loop_state.record_excluded(hash, err);
                }
                Ok(ApplyFailure::Broken { attempts }) => {
                    // The only moment a broken commitment becomes observable.
                    error!(
                        target: "mantle::preconf::dispatch",
                        ?hash, ?err, attempts,
                        "COMMITMENT BROKEN: receipt was returned to the client but the tx could not be applied; giving up and pinning its nonce"
                    );
                    metrics::counter!("preconf.tx.commitment_broken_total").increment(1);
                    loop_state.record_excluded(hash, PreconfError::CommitmentBroken { attempts });
                }
                Err(e) => trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?e,
                    "record_apply_failure lost race"
                ),
            }
            // Deliberately no `take_responder` / `send(Err)`: a `Replay` entry
            // normally has no responder, and if a client did resubmit the same
            // hash it should keep waiting — the tx it is waiting for is still
            // being retried.
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_consensus::{Signed, Transaction, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, Signature};
    use tokio::sync::oneshot;

    use crate::types::PushResult;

    use super::*;

    fn make_tx(hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce: 0, gas_limit: 21_000, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    // ============ LoopState::blocked_senders (same-sender cascade) ============

    /// Blocking a sender at nonce `n0` makes every same-sender entry at
    /// `nonce ≥ n0` inherit the kind; lower nonces (predecessors) and other
    /// senders stay unblocked.
    #[test]
    fn blocked_senders_cascade_query() {
        let s = Address::from([9u8; 20]);
        let other = Address::from([8u8; 20]);
        let mut st = LoopState::new(1);

        assert_eq!(st.sender_blocked_at(&s, 5), None, "unblocked sender");

        st.block_sender(s, 5, BlockKind::Defer);
        assert_eq!(st.sender_blocked_at(&s, 5), Some(BlockKind::Defer), "at head nonce");
        assert_eq!(st.sender_blocked_at(&s, 6), Some(BlockKind::Defer), "above head nonce");
        assert_eq!(st.sender_blocked_at(&s, 4), None, "predecessor below head");
        assert_eq!(st.sender_blocked_at(&other, 6), None, "other sender");
    }

    /// `block_sender` keeps the lowest nonce as the chain head (and that
    /// head's kind); a later higher-nonce block must not raise the head, a
    /// lower-nonce block lowers it and its kind governs.
    #[test]
    fn block_sender_keeps_lowest_nonce_head() {
        let s = Address::from([9u8; 20]);
        let mut st = LoopState::new(1);

        st.block_sender(s, 5, BlockKind::Defer);
        st.block_sender(s, 8, BlockKind::Reject); // higher — must not move head
        assert_eq!(st.sender_blocked_at(&s, 8), Some(BlockKind::Defer), "head stays at 5/Defer");

        st.block_sender(s, 3, BlockKind::Reject); // lower — lowers head, its kind wins
        assert_eq!(st.sender_blocked_at(&s, 3), Some(BlockKind::Reject));
        assert_eq!(st.sender_blocked_at(&s, 4), Some(BlockKind::Reject));
    }

    /// Test apply closure that fabricates an always-success receipt
    /// using `tx.gas_limit()` as the reported `gas_used`. Mirrors the
    /// semantics of the retired `PromiseApplier`, kept here to exercise
    /// the dispatch state machine without standing up a real EVM.
    fn synthetic_ok(
        tx: Arc<TxEnvelope>,
        hash: TxHash,
        height: u64,
    ) -> Result<PreconfReceipt, ApplyError> {
        Ok(PreconfReceipt {
            tx_hash: hash,
            block_height: height,
            status: true,
            logs: Vec::new(),
            gas_used: tx.gas_limit(),
            reason: String::new(),
            revert_data: Bytes::new(),
        })
    }

    /// Test apply closure that reports a per-tx REJECTION — exercises the
    /// `ApplyError::Rejected` → `mark_failed` + `take_responder(Err)` branch.
    fn synthetic_err(_: Arc<TxEnvelope>, _: TxHash, _: u64) -> Result<PreconfReceipt, ApplyError> {
        Err(ApplyError::Rejected(PreconfError::BuilderRejected("synthetic error for test".into())))
    }

    /// Test apply closure that reports a FATAL execution error — exercises
    /// the `ApplyError::Fatal` → build-abort branch. `std::fmt::Error` is a
    /// convenient zero-field `Error + Send + Sync` payload.
    fn synthetic_fatal(
        _: Arc<TxEnvelope>,
        _: TxHash,
        _: u64,
    ) -> Result<PreconfReceipt, ApplyError> {
        Err(ApplyError::Fatal(PayloadBuilderError::other(std::fmt::Error)))
    }

    #[tokio::test]
    async fn apply_one_preconf_calls_closure_and_marks_succeeded() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x11);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        assert!(matches!(
            fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        let mut state = LoopState::new(42);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        // Responder got the synthetic receipt.
        let receipt = resp_rx.await.expect("responder closed").expect("synthetic ok");
        assert_eq!(receipt.tx_hash, hash);
        assert_eq!(receipt.block_height, 42);
        assert!(receipt.status);
        assert_eq!(receipt.gas_used, 21_000);

        // Loop state recorded.
        assert_eq!(state.committed_len(), 1);
        assert_eq!(state.excluded_len(), 0);

        // Fifo entry transitioned to Success.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn dedup_hit_skips_second_apply() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x22);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(1);
        // `Cell` so the assert_eq below can read while the FnMut
        // closure still mutably borrows it (Cell uses interior
        // mutability with `&self`).
        let call_count = Cell::new(0u32);
        let mut counting_apply = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut counting_apply).await.unwrap();
        assert_eq!(call_count.get(), 1);
        assert_eq!(state.committed_len(), 1);

        // Second call: dedup guard fires before apply_fn is invoked.
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut counting_apply).await.unwrap();
        assert_eq!(call_count.get(), 1, "apply closure must not be called twice");
        assert_eq!(state.committed_len(), 1);
    }

    #[tokio::test]
    async fn deadline_skip_marks_timeout_and_cancels_responder() {
        // Configure a 50ms preconf_timeout so the deadline check fires
        // deterministically after a short sleep.
        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0x33);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        // Sleep past the deadline. `SAFETY_MARGIN` is a hard 40ms but the
        // sleep of 60ms also exceeds `preconf_timeout` (50ms) on its own.
        tokio::time::sleep(Duration::from_millis(60)).await;

        use std::cell::Cell;
        let mut state = LoopState::new(7);
        let apply_called = Cell::new(false);
        let mut tracking_apply = |tx, h, height| {
            apply_called.set(true);
            synthetic_ok(tx, h, height)
        };
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut tracking_apply).await.unwrap();

        // apply closure must NOT have been invoked — deadline gate fires
        // earlier so the in-flight builder is untouched.
        assert!(!apply_called.get(), "apply closure must skip when deadline exceeded");

        // Responder must observe Timeout error.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));

        // Loop state recorded exclusion (not commit).
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);

        // Fifo entry is now Timeout.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Timeout);
    }

    /// Simulates a same-slot client resubmit after a Timeout:
    /// 1. First dispatch: deadline gate fires, records `Timeout` excluded.
    /// 2. Client resubmits: `attach_responder` refreshes `inserted_at`, `push_if_absent` revives
    ///    fifo entry back to `Waiting`.
    /// 3. Second dispatch: dedup finds the prior `Timeout` reason but **clears** it (since the
    ///    deadline gate is tied to the entry's `inserted_at`, which is now fresh) and falls through
    ///    to re-evaluate the gates. With fresh insertion time well under `preconf_timeout`, the
    ///    gate does not fire and apply proceeds. The fresh responder observes the receipt.
    ///
    /// Locks the "Timeout is not a stable exclusion" invariant: a
    /// regression that forwards stored Timeout via `cancel_responder`
    /// would deny service to a legitimately re-eligible tx.
    #[tokio::test]
    async fn dedup_timeout_re_evaluates_gate_on_fresh_inserted_at() {
        use std::time::Instant;

        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0x55);
        let hash = *tx.tx_hash();

        // Step 1: initial insert; sleep past deadline so the gate fires.
        let (resp_tx1, resp_rx1) = oneshot::channel();
        fifo.attach_responder(hash, Instant::now(), resp_tx1).await.unwrap();
        fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();
        // First dispatch: Timeout via deadline gate.
        let err = resp_rx1.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));
        assert_eq!(state.excluded_len(), 1);
        assert_eq!(fifo.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Timeout,);

        // Step 2: client resubmit — refresh `inserted_at`, revive to Waiting.
        let (resp_tx2, resp_rx2) = oneshot::channel();
        fifo.attach_responder(hash, Instant::now(), resp_tx2).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert_eq!(
            fifo.find_by_hash(&hash).await.unwrap().status,
            PreconfStatus::Waiting,
            "revive must flip status back to Waiting",
        );

        // Step 3: second dispatch. Dedup CLEARS the stale Timeout and
        // falls through; deadline gate reads fresh inserted_at (< 50ms)
        // and passes; apply succeeds.
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.excluded_len(), 0, "stale Timeout exclusion must be cleared");
        assert_eq!(state.committed_len(), 1, "second dispatch must apply successfully");

        // Fresh responder observes the receipt from the successful apply.
        let receipt = resp_rx2.await.expect("responder closed").expect("must be Ok");
        assert!(receipt.status);
    }

    #[tokio::test]
    async fn apply_failure_marks_failed_and_sends_err_to_responder() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x44);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(99);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err).await.unwrap();

        // Responder got the apply error verbatim.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        assert!(matches!(err, PreconfError::BuilderRejected(_)));

        // Loop state recorded exclusion, NOT commit.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);

        // Fifo entry transitioned to Failed (not Success).
        //
        // This is also the `Rpc` half of the D4 split: a *live* submission must
        // keep failing fast, so the entry ends up in the replaceable `Failed`
        // and its client learns immediately. Only `Replay` entries get the
        // retry treatment — see `a_replayed_commitment_is_not_marked_failed_*`.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Failed);
        assert_eq!(entry.source, PreconfSource::Rpc, "the fail-fast half is Rpc-only");
        assert_eq!(entry.apply_failures, 0, "Rpc path must not touch the replay budget");
    }

    // ===== D4: an already-acknowledged commitment must not become replaceable
    // ===== (docs/preconf-commitment-retention-until-irrevocable.md §4.10)

    /// Push an entry that is already in the "receipt has gone out" shape: a
    /// `Replay`-sourced `Waiting` entry with no responder, which is what
    /// `reset_success_to_waiting`, a reorg reinject, and a journal restore all
    /// produce.
    async fn push_replayed(fifo: &PreconfTxSet, tx: Arc<TxEnvelope>) -> TxHash {
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Replay).await,
            PushResult::Inserted
        ));
        hash
    }

    /// The core of D4. Before the fix this landed in `Failed`, which is
    /// replaceable by any same-nonce tx, swept by `clean_reclaimable`, and
    /// evicted from the pool — silently breaking a commitment the client already
    /// holds a receipt for.
    #[tokio::test]
    async fn a_replayed_commitment_is_not_marked_failed_on_apply_error() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let hash = push_replayed(&fifo, make_tx(0x91)).await;

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");

        let entry = fifo.find_by_hash(&hash).await.expect("entry must survive the failure");
        assert_eq!(
            entry.status,
            PreconfStatus::Waiting,
            "an acknowledged commitment stays Waiting so the next job retries it",
        );
        assert_eq!(entry.apply_failures, 1);
        // Recorded as excluded so a second broadcast in *this* block cannot burn
        // another attempt — the budget is meant to span slots, not events.
        assert_eq!(state.excluded_len(), 1);
        assert_eq!(state.committed_len(), 0);
    }

    /// A `Fatal` apply error must **not** spend one of the retry attempts.
    ///
    /// This pins the arm ordering rather than any one arm. The two error axes
    /// are independent: the error's *kind* (`Rejected` — this transaction is
    /// invalid — versus `Fatal` — the execution environment is untrustworthy)
    /// and the entry's *source* (`Rpc` versus an already-acknowledged replay).
    /// `Fatal` returns before the D4 budget is touched, because a DB / header /
    /// fatal-precompile error says nothing whatsoever about this transaction;
    /// charging it an attempt would march an otherwise-applicable commitment
    /// toward `Broken` for reasons entirely outside it, and `Broken` is the one
    /// state there is no way back from.
    ///
    /// Move the catch-all above the `Fatal` arm and this goes red.
    #[tokio::test]
    async fn a_fatal_apply_error_does_not_charge_the_replay_budget() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let hash = push_replayed(&fifo, make_tx(0x92)).await;

        let mut state = LoopState::new(7);
        let outcome = apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_fatal).await;

        assert!(outcome.is_err(), "a fatal execution error aborts the whole build");

        let entry = fifo.find_by_hash(&hash).await.expect("the commitment must survive");
        assert_eq!(
            entry.status,
            PreconfStatus::Waiting,
            "left Waiting, responder still attached, for the next build cycle",
        );
        assert_eq!(
            entry.apply_failures, 0,
            "a fatal environment error must not count against the commitment",
        );
        assert_eq!(
            state.excluded_len(),
            0,
            "nor exclude it from this slot — the build is being abandoned, not the tx",
        );
    }

    /// `mark_failed` fires the pool-eviction hook, which would remove the very
    /// tx the retry needs. The retry path must not.
    #[tokio::test]
    async fn a_replayed_commitment_stays_in_the_pool_after_an_apply_error() {
        use std::sync::Mutex as StdMutex;

        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let evicted: Arc<StdMutex<Vec<TxHash>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = evicted.clone();
        fifo.set_pool_eviction_callback(Arc::new(move |h| sink.lock().unwrap().push(h)));

        let hash = push_replayed(&fifo, make_tx(0x92)).await;
        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");

        assert!(
            evicted.lock().unwrap().is_empty(),
            "retrying a commitment must not evict it from the pool",
        );
    }

    /// The `max_attempts`-th failure gives up — but into `Broken`, which is not
    /// replaceable, rather than into `Failed`, which is.
    #[tokio::test]
    async fn the_third_apply_failure_moves_the_commitment_to_broken() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig { preconf_max_apply_attempts: 3, ..PreconfConfig::default() };
        let hash = push_replayed(&fifo, make_tx(0x93)).await;

        for attempt in 1..=2u8 {
            // Fresh LoopState each round: one attempt per payload job.
            let mut state = LoopState::new(7);
            apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
                .await
                .expect("a per-tx rejection keeps the build going; no Fatal here");
            let entry = fifo.find_by_hash(&hash).await.unwrap();
            assert_eq!(
                entry.status,
                PreconfStatus::Waiting,
                "attempt {attempt} of 3 must still be retrying",
            );
            assert_eq!(entry.apply_failures, attempt);
        }

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Broken, "the 3rd failure gives up");
        assert_eq!(entry.apply_failures, 3);
        // The give-up reason must be the dedicated variant, not a generic
        // builder error — it is what the RPC layer surfaces to a resubmitting
        // client.
        assert!(matches!(
            state.excluded_reason(&hash),
            Some(PreconfError::CommitmentBroken { attempts: 3 })
        ));
    }

    /// A `Broken` entry must not be retried by later jobs — dispatch's status
    /// gate treats it as terminal and reports the give-up reason.
    #[tokio::test]
    async fn a_broken_commitment_is_not_applied_again() {
        use std::cell::Cell;

        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig { preconf_max_apply_attempts: 1, ..PreconfConfig::default() };
        let hash = push_replayed(&fifo, make_tx(0x94)).await;

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        assert_eq!(fifo.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Broken);

        // A later job: the apply closure must not run at all.
        let calls = Cell::new(0u32);
        let mut counting = |tx, h, height| {
            calls.set(calls.get() + 1);
            synthetic_ok(tx, h, height)
        };
        let mut next_state = LoopState::new(8);
        apply_one_preconf(&fifo, &cfg, hash, &mut next_state, &mut counting)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");

        assert_eq!(calls.get(), 0, "a broken commitment must not be re-applied");
        assert!(matches!(
            next_state.excluded_reason(&hash),
            Some(PreconfError::CommitmentBroken { attempts: 1 }),
        ));
    }

    /// A success in between clears the budget: a commitment that bounces across
    /// several discarded in-flight blocks must not accumulate failures from
    /// rounds it actually survived.
    #[tokio::test]
    async fn a_successful_apply_resets_the_replay_failure_budget() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig { preconf_max_apply_attempts: 2, ..PreconfConfig::default() };
        let hash = push_replayed(&fifo, make_tx(0x95)).await;

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        assert_eq!(fifo.find_by_hash(&hash).await.unwrap().apply_failures, 1);

        // Next job succeeds, then that block is discarded too.
        let mut state = LoopState::new(8);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        assert_eq!(fifo.find_by_hash(&hash).await.unwrap().apply_failures, 0);
        fifo.reset_success_to_waiting(&hash).await.unwrap();

        // With the budget reset, one more failure is a retry — not a give-up.
        let mut state = LoopState::new(9);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Waiting);
        assert_eq!(entry.apply_failures, 1);
    }

    /// A FATAL apply error (DB / header / fatal precompile) must abort the
    /// whole build — `apply_one_preconf` returns `Err` — and, unlike the
    /// per-tx `Rejected` path, must leave the commitment intact for retry:
    /// the fifo entry stays `Waiting` (NOT `Failed`, so it is never evicted
    /// from the pool), nothing is recorded in loop state, and the responder
    /// stays attached so the client keeps waiting for the next build cycle.
    /// This is the asymmetry the fix removes: the pool arm already aborts
    /// on this class; the preconf arm used to silently drop the commitment.
    #[tokio::test]
    async fn apply_fatal_aborts_build_and_leaves_entry_waiting() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x4f);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(7);
        let out = apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_fatal).await;

        // 1. Propagates as a build-aborting error.
        assert!(out.is_err(), "fatal apply must abort the build (return Err)");

        // 2. Entry left Waiting — NOT terminal, so never evicted from pool.
        let entry = fifo.find_by_hash(&hash).await.expect("entry must survive a fatal abort");
        assert_eq!(
            entry.status,
            PreconfStatus::Waiting,
            "fatal must not mark the entry terminal — it stays revivable for retry"
        );

        // 3. Neither committed nor excluded — the commitment is untouched.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 0, "fatal must not record the tx as excluded");

        // 4. Responder still attached (not consumed) so the client keeps waiting for the retry
        //    rather than receiving a spurious error.
        assert!(
            fifo.take_responder(&hash).await.is_some(),
            "fatal must leave the responder attached for the retry"
        );
        // The oneshot sender is still alive until we drop it here.
        drop(resp_rx);
    }

    /// Build a synthetic tx with a caller-chosen `gas_limit` and hash
    /// byte. Used by the block-gas-budget tests below.
    fn make_tx_with_gas(hash_byte: u8, nonce: u64, gas_limit: u64) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce, gas_limit, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    /// Boundary: `used + tx.gas_limit == cfg.preconf_max_gas_per_block`
    /// must be accepted (gate uses `>`, not `>=`). Locks the corner
    /// against off-by-one drift in the future.
    #[tokio::test]
    async fn apply_one_preconf_at_exact_budget_boundary_accepts() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 21_000,
            preconf_max_gas_per_tx: 21_000, // per-tx cap must not shadow the test
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx_with_gas(0x55, 0, 21_000);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        let receipt = resp_rx.await.expect("responder closed").expect("must succeed at boundary");
        assert_eq!(receipt.gas_used, 21_000);
        assert_eq!(state.preconf_gas_used(), 21_000);
        assert_eq!(state.committed_len(), 1);
        assert_eq!(state.excluded_len(), 0);
    }

    /// Over budget: `used + tx.gas_limit > preconf_max_gas_per_block`
    /// rejects with `BlockGasBudgetExceeded { max, used, limit }`,
    /// flips fifo to `Failed`, records exclusion.
    #[tokio::test]
    async fn apply_one_preconf_over_budget_rejects_with_typed_error() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000, // per-tx cap must not shadow the test
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);

        // Pre-load loop_state as if a prior tx of 21_000 gas had already
        // committed — 21_000 + next tx 21_000 = 42_000 > 40_000.
        let mut state = LoopState::new(1);
        state.preconf_gas_used = 21_000;

        let tx = make_tx_with_gas(0x66, 0, 21_000);
        let hash = *tx.tx_hash();
        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        // Responder got the typed error with all three fields.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        match err {
            PreconfError::BlockGasBudgetExceeded { max, used, limit } => {
                assert_eq!(max, 40_000);
                assert_eq!(used, 21_000);
                assert_eq!(limit, 21_000);
            }
            other => panic!("expected BlockGasBudgetExceeded, got {other:?}"),
        }

        // Loop state: excluded, NOT committed. preconf_gas_used unchanged.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);
        assert_eq!(state.preconf_gas_used(), 21_000);

        // Fifo entry flipped to Canceled — server pre-apply rejected,
        // no EVM state change, tx will NOT land on chain.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Canceled);
    }

    /// Cumulative tracking: successful applies increment
    /// `preconf_gas_used` by the receipt's `gas_used`. Three sequential
    /// applies must sum correctly.
    #[tokio::test]
    async fn apply_one_preconf_success_increments_preconf_gas_used() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 10_000_000,
            preconf_max_gas_per_tx: 5_000_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);

        let mut expected_total: u64 = 0;
        for (i, gas) in [21_000u64, 50_000, 30_000].into_iter().enumerate() {
            let tx = make_tx_with_gas(0x77 + i as u8, i as u64, gas);
            let hash = *tx.tx_hash();
            fifo.push_if_absent(tx, Address::from([i as u8 + 1; 20]), PreconfSource::Rpc).await;
            apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();
            expected_total += gas;
            assert_eq!(
                state.preconf_gas_used(),
                expected_total,
                "after apply {} of {}, expected preconf_gas_used {}",
                i + 1,
                3,
                expected_total,
            );
        }
        assert_eq!(state.committed_len(), 3);
    }

    /// Gate ② — `fifo.find_by_hash` returns None (entry evicted between
    /// broadcast and pickup, or a stale broadcast event surviving a
    /// `clean_reclaimable` race). `apply_one_preconf` must:
    ///
    /// - NOT invoke `apply_fn`
    /// - NOT touch responders
    /// - NOT record the hash in `loop_state` (allowing a future re-push of the same hash to proceed
    ///   normally)
    #[tokio::test]
    async fn missing_fifo_entry_is_silent_noop() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let mut state = LoopState::new(1);
        let call_count = Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };

        // hash never seen — no push_if_absent, no attach_responder.
        apply_one_preconf(&fifo, &cfg, TxHash::from([0xdd; 32]), &mut state, &mut apply_fn)
            .await
            .unwrap();

        assert_eq!(call_count.get(), 0, "apply_fn must not be invoked when entry missing");
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 0, "must NOT record excluded — future re-push allowed");
        assert_eq!(state.preconf_gas_used(), 0);
    }

    /// Gate ③ — `entry.status != Waiting`. Terminal / reclaimable
    /// entries reach `apply_one_preconf` when a stale broadcast event
    /// fires post-transition. They must be recorded as excluded (so the
    /// next broadcast for the same hash short-circuits at gate ①) and
    /// must NOT invoke `apply_fn` or touch responders.
    ///
    /// Run once per non-Waiting variant so any future state added to
    /// `PreconfStatus` gets caught if it's not handled by the gate.
    #[tokio::test]
    async fn non_waiting_status_records_excluded_and_skips_apply() {
        use std::cell::Cell;
        for pre_status in [
            PreconfStatus::Success,
            PreconfStatus::Failed,
            PreconfStatus::Timeout,
            PreconfStatus::Canceled,
        ] {
            let fifo = PreconfTxSet::new(8);
            let cfg = PreconfConfig::default();
            let tx = make_tx(0xee);
            let hash = *tx.tx_hash();

            let (resp_tx, mut resp_rx) = oneshot::channel();
            fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

            // Drive the entry into the target non-Waiting state.
            match pre_status {
                PreconfStatus::Success => fifo.mark_succeeded(&hash).await.unwrap(),
                PreconfStatus::Failed => fifo.mark_failed(&hash).await.unwrap(),
                PreconfStatus::Timeout => fifo.mark_timeout(&hash).await.unwrap(),
                PreconfStatus::Canceled => fifo.mark_canceled(&hash).await.unwrap(),
                _ => unreachable!(),
            }

            let mut state = LoopState::new(1);
            let call_count = Cell::new(0u32);
            let mut apply_fn = |tx, h, height| {
                call_count.set(call_count.get() + 1);
                synthetic_ok(tx, h, height)
            };

            apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut apply_fn).await.unwrap();

            assert_eq!(call_count.get(), 0, "apply_fn must not run when status={pre_status:?}",);
            assert_eq!(state.committed_len(), 0);
            assert_eq!(state.excluded_len(), 1, "must record_excluded for status={pre_status:?}");
            // Responder untouched — the entry's terminal transition path
            // (mark_succeeded/failed/timeout/canceled) is responsible for
            // resolving the responder; the dispatch loop must NOT double-send.
            assert!(
                resp_rx.try_recv().is_err(),
                "responder must NOT be touched by dispatch for status={pre_status:?}"
            );

            // Fifo entry still there, still in the pre-transitioned status
            // (except Failed/Success which the test doesn't remove; forward
            // cleanup handles that later).
            let entry = fifo.find_by_hash(&hash).await.unwrap();
            assert_eq!(entry.status, pre_status);
        }
    }

    // ── Source-differentiated gate tests (mantle preconf SLA: journal
    //    replay must never silently drop a promised tx). ─────────────

    /// Journal-replayed entries bypass the pre-apply deadline gate.
    /// Even after the timeout has elapsed since insertion, `apply_fn`
    /// still fires and the tx transitions to Success — the RPC-source
    /// counterpart under the same conditions goes to Timeout (see
    /// `deadline_skip_marks_timeout_and_cancels_responder`).
    #[tokio::test]
    async fn replay_source_bypasses_deadline_gate() {
        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0xe0);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Replay).await;

        // Sleep well past the deadline (50ms) + safety margin (40ms).
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "journal-replayed tx must apply despite deadline");
        assert_eq!(state.excluded_len(), 0);
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    /// Journal-replayed entries bypass the per-block gas budget gate.
    /// Pre-loading `preconf_gas_used` so an RPC entry would be rejected
    /// (see `apply_one_preconf_over_budget_rejects_with_typed_error`)
    /// still admits a Replay-sourced tx.
    #[tokio::test]
    async fn replay_source_bypasses_gas_budget_gate() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);
        state.preconf_gas_used = 21_000; // 21_000 + 21_000 = 42_000 > 40_000

        let tx = make_tx_with_gas(0xe1, 0, 21_000);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::from([1; 20]), PreconfSource::Replay).await;

        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "journal tx must apply despite over-budget");
        assert_eq!(state.excluded_len(), 0);
        assert_eq!(
            state.preconf_gas_used(),
            42_000,
            "gas_used still accumulates so subsequent RPC entries see the true cost",
        );
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    /// Mixed sources share the `LoopState` `preconf_gas_used` accounting:
    /// a Journal tx that bypasses the gate still contributes to the
    /// running total, so a subsequent RPC tx sees the true cost and
    /// can be gated properly.
    #[tokio::test]
    async fn mixed_sources_share_gas_accounting() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);

        // Journal tx: 30_000 gas, bypasses budget gate.
        let j_tx = make_tx_with_gas(0xe2, 0, 30_000);
        let j_hash = *j_tx.tx_hash();
        fifo.push_if_absent(j_tx, Address::from([1; 20]), PreconfSource::Replay).await;
        apply_one_preconf(&fifo, &cfg, j_hash, &mut state, synthetic_ok).await.unwrap();
        assert_eq!(state.preconf_gas_used(), 30_000);

        // RPC tx: 21_000 gas. 30_000 + 21_000 = 51_000 > 40_000 → rejected.
        let r_tx = make_tx_with_gas(0xe3, 0, 21_000);
        let r_hash = *r_tx.tx_hash();
        fifo.push_if_absent(r_tx, Address::from([2; 20]), PreconfSource::Rpc).await;
        apply_one_preconf(&fifo, &cfg, r_hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "only the journal tx committed");
        assert_eq!(state.excluded_len(), 1, "RPC tx was gated out");
        let j_entry = fifo.find_by_hash(&j_hash).await.unwrap();
        assert_eq!(j_entry.status, PreconfStatus::Success);
        let r_entry = fifo.find_by_hash(&r_hash).await.unwrap();
        assert_eq!(r_entry.status, PreconfStatus::Canceled);
    }

    /// Race regression: an RPC-side timeout deadline fires **while**
    /// `apply_one_preconf` is inside `apply_fn`. The per-entry
    /// `apply_lock` acquired by dispatch before `apply_fn` must block
    /// the RPC's `lock_for_apply` acquisition until dispatch finishes
    /// `mark_succeeded` + `resp.send(...)`. When the RPC finally
    /// acquires the lock, fifo status is `Success` and `resp_rx` has
    /// the receipt queued — so `try_recv` returns it, and the client
    /// sees `Success`, not `Timeout`.
    ///
    /// Regression guard for the SLA invariant "wire `Timeout` ⇒ tx not
    /// committed to builder state". Without the `apply_lock` scheme,
    /// the RPC deadline branch would previously flip the entry to
    /// `Timeout` while dispatch had already committed the tx to the
    /// in-flight builder, producing an on-chain landing under a
    /// Timeout wire response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn apply_lock_blocks_rpc_timeout_race_and_yields_success() {
        let fifo = Arc::new(PreconfTxSet::new(8));
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x99);
        let hash = *tx.tx_hash();

        let (resp_tx, mut resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        assert!(matches!(
            fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        // Slow apply closure — `std::thread::sleep` blocks the worker
        // that dispatch runs on but leaves other workers free (test
        // uses multi_thread). Simulates a ~200ms EVM apply.
        const APPLY_DURATION: Duration = Duration::from_millis(200);
        let slow_apply = |tx: Arc<TxEnvelope>, h: TxHash, height: u64| {
            std::thread::sleep(APPLY_DURATION);
            Ok(PreconfReceipt {
                tx_hash: h,
                block_height: height,
                status: true,
                logs: Vec::new(),
                gas_used: alloy_consensus::Transaction::gas_limit(tx.as_ref()),
                reason: String::new(),
                revert_data: Bytes::new(),
            })
        };

        // Spawn dispatch — will grab apply_lock and run slow_apply for
        // ~200ms before releasing.
        let fifo_clone = fifo.clone();
        let cfg_clone = cfg.clone();
        let dispatch_task = tokio::spawn(async move {
            let mut state = LoopState::new(7);
            apply_one_preconf(&fifo_clone, &cfg_clone, hash, &mut state, slow_apply).await.unwrap();
        });

        // Give dispatch time to enter `apply_fn` (holding apply_lock).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate the RPC-side deadline branch: acquire apply_lock.
        // Dispatch is inside apply_fn → this must block until dispatch
        // finishes mark_succeeded + send + drop guard.
        let acquire_start = std::time::Instant::now();
        let guard = fifo.lock_for_apply(&hash).await;
        let acquire_duration = acquire_start.elapsed();
        assert!(guard.is_some(), "lock_for_apply must return Some for the pushed entry");

        // Must have waited for dispatch to finish (~150ms remaining
        // after our 50ms head start).
        assert!(
            acquire_duration >= Duration::from_millis(100),
            "RPC lock acquisition should have blocked on dispatch's apply_lock; \
             waited {acquire_duration:?} but expected ≥ 100ms",
        );

        // Under the lock, fifo status must be final and the receipt
        // must be queued in resp_rx (dispatch's `resp.send(...)`
        // completed inside the critical section).
        let final_status = fifo.find_by_hash(&hash).await.map(|e| e.status);
        assert_eq!(
            final_status,
            Some(PreconfStatus::Success),
            "dispatch must have finished mark_succeeded before releasing apply_lock",
        );

        match resp_rx.try_recv() {
            Ok(Ok(receipt)) => {
                assert_eq!(receipt.tx_hash, hash);
                assert!(receipt.status);
            }
            other => panic!("resp_rx must have queued receipt; got {other:?}"),
        }

        drop(guard);
        dispatch_task.await.expect("dispatch task join");
    }
}
