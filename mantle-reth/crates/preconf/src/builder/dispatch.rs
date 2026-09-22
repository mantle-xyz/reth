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
use tracing::{debug, trace, warn};

use reth_payload_builder_primitives::PayloadBuilderError;

use crate::{
    ApplyHold, PreconfConfig, PreconfTxSet,
    apply::ApplyError,
    types::{PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
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
    /// This build's registered holds, keyed by hash. Consumed by a verdict, or
    /// released for whatever is left at exit. Ownership rules out a double
    /// release, and this map being the *only* source of releasable hashes rules
    /// out decrementing an entry this build never registered — such as one
    /// pushed mid-build that never reached this build's loop arm.
    holds: HashMap<TxHash, ApplyHold>,
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
            holds: HashMap::new(),
        }
    }

    /// Record a freshly registered hold. Replaces any existing hold for the
    /// same hash (the displaced one decrements on drop).
    pub(super) fn insert_hold(&mut self, hold: ApplyHold) {
        self.holds.insert(hold.hash(), hold);
    }

    /// `true` when this build has registered `hash`.
    pub(super) fn holds_hash(&self, hash: &TxHash) -> bool {
        self.holds.contains_key(hash)
    }

    /// Take the hold for `hash` so it can be consumed by a verdict.
    pub(super) fn take_hold(&mut self, hash: &TxHash) -> Option<ApplyHold> {
        self.holds.remove(hash)
    }

    /// Drain every hold this build still owns — the build is exiting without
    /// a verdict for these entries.
    pub(super) fn drain_holds(&mut self) -> Vec<ApplyHold> {
        self.holds.drain().map(|(_, h)| h).collect()
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

    // The deadline and gas-budget gates below apply only to entries nobody has
    // been promised yet. Once a receipt has gone out — whether this round
    // (`Success`) or in an earlier one (`Replay`) — refusing the tx here would
    // silently break that promise, so both bypass them.
    let is_promised =
        entry.status == PreconfStatus::Success || entry.source == PreconfSource::Replay;

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
    if !is_promised {
        metrics::histogram!("preconf.dispatch.elapsed_at_gate_ms")
            .record(elapsed_at_gate.as_millis() as f64);
    }

    if !is_promised && elapsed_at_gate + margin >= cfg.preconf_timeout {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            elapsed_ms = elapsed_at_gate.as_millis() as u64,
            "pre-apply deadline passed; aborting"
        );
        metrics::counter!("preconf.dispatch.deadline_skipped_total").increment(1);
        // The one place that already walks every entry — carryover, the
        // broadcast arm and a `Lagged` re-scan all funnel through here — so a
        // separate sweeper would just repeat the traversal. `finalize_timeout`
        // answers the responder and removes the entry; the hold is therefore
        // dropped rather than consumed, so nothing finalises twice.
        fifo.finalize_timeout(&hash, cfg.preconf_timeout).await;
        drop(loop_state.take_hold(&hash));
        let reason = PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 };
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
    if !is_promised &&
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
        let reason = PreconfError::BlockGasBudgetExceeded {
            max: cfg.preconf_max_gas_per_block,
            used: loop_state.preconf_gas_used,
            limit: tx_gas_limit,
        };
        // A local verdict: this build's preconf budget is spent, which says
        // nothing about any other in-flight build. Only terminal once this was
        // the last holder.
        if let Some(hold) = loop_state.take_hold(&hash) {
            fifo.finish_failure(hold, reason.clone()).await;
        }
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
            // No quorum: a success is a promise carryover keeps even if this
            // block is discarded. `finish_success` refuses only when the
            // deadline already finalised the entry — the sole way to preempt an
            // in-flight success — and the receipt is then discarded.
            if let Some(hold) = loop_state.take_hold(&hash) &&
                !fifo.finish_success(hold, receipt).await
            {
                trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash,
                    "entry already finalised; success not recorded"
                );
            }
        }
        // Per-tx rejection: the tx itself is invalid. Mark the entry
        // `Failed` (revivable via same-hash resubmit), evict it from the
        // pool, hand the client the concrete error, and keep building.
        Err(ApplyError::Rejected(err)) => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?err,
                "preconf apply rejected tx; marking entry as Failed"
            );
            metrics::counter!("preconf.tx.failure_total").increment(1);
            loop_state.record_excluded(hash, err.clone());
            // `InvalidTx` is judged against *this* build's in-flight state —
            // nonce and balance both depend on what preceded it in this block.
            // Another build may apply the same tx cleanly, so the verdict is
            // terminal only once this was the last holder.
            if let Some(hold) = loop_state.take_hold(&hash) {
                fifo.finish_failure(hold, err).await;
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
    /// Test entry point mirroring production: `admit_and_dispatch` registers
    /// this build's hold before calling `apply_one_preconf`, so a test that
    /// skipped it would silently exercise a state the builder never produces
    /// (no hold ⇒ no verdict ⇒ the responder is never answered).
    async fn dispatch_one<F>(
        fifo: &PreconfTxSet,
        cfg: &PreconfConfig,
        hash: TxHash,
        loop_state: &mut LoopState,
        apply_fn: F,
    ) -> Result<(), PayloadBuilderError>
    where
        F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, ApplyError>,
    {
        if !loop_state.holds_hash(&hash) &&
            let Some(hold) = fifo.register(&hash).await
        {
            loop_state.insert_hold(hold);
        }
        apply_one_preconf(fifo, cfg, hash, loop_state, apply_fn).await
    }

    fn synthetic_receipt(hash: TxHash) -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: hash,
            block_height: 0,
            status: true,
            logs: Vec::new(),
            gas_used: 0,
            reason: String::new(),
            revert_data: alloy_primitives::Bytes::new(),
        }
    }

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
        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

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
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x22);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(1);
        // `Cell` so the assert_eq below can read while the FnMut
        // closure still mutably borrows it (Cell uses interior
        // mutability with `&self`).
        let call_count = std::cell::Cell::new(0u32);
        let mut counting_apply = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        dispatch_one(&fifo, &cfg, hash, &mut state, &mut counting_apply).await.unwrap();
        assert_eq!(call_count.get(), 1);
        assert_eq!(state.committed_len(), 1);

        // Second call: dedup guard fires before apply_fn is invoked.
        dispatch_one(&fifo, &cfg, hash, &mut state, &mut counting_apply).await.unwrap();
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

        let mut state = LoopState::new(7);
        let apply_called = std::cell::Cell::new(false);
        let mut tracking_apply = |tx, h, height| {
            apply_called.set(true);
            synthetic_ok(tx, h, height)
        };
        dispatch_one(&fifo, &cfg, hash, &mut state, &mut tracking_apply).await.unwrap();

        // apply closure must NOT have been invoked — deadline gate fires
        // earlier so the in-flight builder is untouched.
        assert!(!apply_called.get(), "apply closure must skip when deadline exceeded");

        // Responder must observe Timeout error.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));

        // Loop state recorded exclusion (not commit).
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);

        // Finalised: the entry is removed, freeing the slot at once.
        assert!(!fifo.contains(&hash).await);
    }

    /// A resubmit after a timeout is a brand-new entry, not a revived one:
    /// the timeout removed the old entry outright. So the second attempt is
    /// judged entirely on its own `inserted_at` and applies normally.
    #[tokio::test]
    async fn resubmit_after_timeout_is_a_fresh_entry() {
        use std::time::Instant;

        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0x55);
        let hash = *tx.tx_hash();

        let (resp_tx1, resp_rx1) = oneshot::channel();
        fifo.attach_responder(hash, Instant::now(), resp_tx1).await.unwrap();
        fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        let mut state = LoopState::new(1);
        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();
        let err = resp_rx1.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));
        assert!(!fifo.contains(&hash).await, "the timeout removed the entry");

        // The resubmit inserts afresh — no revive branch is involved.
        let (resp_tx2, resp_rx2) = oneshot::channel();
        fifo.attach_responder(hash, Instant::now(), resp_tx2).await.unwrap();
        assert_eq!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted,
        );

        let mut state2 = LoopState::new(1);
        dispatch_one(&fifo, &cfg, hash, &mut state2, synthetic_ok).await.unwrap();
        assert_eq!(state2.committed_len(), 1, "the fresh entry applies normally");
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
        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_err).await.unwrap();

        // Responder got the apply error verbatim.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        assert!(matches!(err, PreconfError::BuilderRejected(_)));

        // Loop state recorded exclusion, NOT commit.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);

        // Sole holder, so the rejection is terminal: the entry is removed
        // outright rather than parked in a terminal state, freeing the
        // `(sender, nonce)` slot for an immediate resubmit.
        assert!(!fifo.contains(&hash).await, "a finalised rejection removes the entry");
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
        let out = dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_fatal).await;

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
        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

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

        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

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

        // Server pre-apply rejection by the sole holder — no EVM state
        // change, and the entry is gone so nothing will retry it.
        assert!(!fifo.contains(&hash).await);
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
            dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();
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
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let mut state = LoopState::new(1);
        let call_count = std::cell::Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };

        // hash never seen — no push_if_absent, no attach_responder.
        dispatch_one(&fifo, &cfg, TxHash::from([0xdd; 32]), &mut state, &mut apply_fn)
            .await
            .unwrap();

        assert_eq!(call_count.get(), 0, "apply_fn must not be invoked when entry missing");
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 0, "must NOT record excluded — future re-push allowed");
        assert_eq!(state.preconf_gas_used(), 0);
    }
    /// A `Success` entry is applied like any other: every build puts it into
    /// its own block, because any of those blocks may be the adopted one. Its
    /// receipt is redundant, so `finish_success` refuses and drops it — the
    /// client already holds one.
    #[tokio::test]
    async fn a_promised_entry_is_applied_again_by_this_build() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0xee);
        let hash = *tx.tx_hash();

        let (resp_tx, mut resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, std::time::Instant::now(), resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let hold = fifo.register(&hash).await.unwrap();
        assert!(fifo.finish_success(hold, synthetic_receipt(hash)).await);
        assert!(resp_rx.try_recv().is_ok(), "the first success delivered the receipt");

        let mut state = LoopState::new(1);
        let call_count = std::cell::Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        dispatch_one(&fifo, &cfg, hash, &mut state, &mut apply_fn).await.unwrap();

        assert_eq!(call_count.get(), 1, "a promised entry is still applied here");
        assert_eq!(state.committed_len(), 1);
        assert_eq!(
            fifo.find_by_hash(&hash).await.unwrap().status,
            PreconfStatus::Success,
            "status unchanged",
        );
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
        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

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

        dispatch_one(&fifo, &cfg, hash, &mut state, synthetic_ok).await.unwrap();

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
        dispatch_one(&fifo, &cfg, j_hash, &mut state, synthetic_ok).await.unwrap();
        assert_eq!(state.preconf_gas_used(), 30_000);

        // RPC tx: 21_000 gas. 30_000 + 21_000 = 51_000 > 40_000 → rejected.
        let r_tx = make_tx_with_gas(0xe3, 0, 21_000);
        let r_hash = *r_tx.tx_hash();
        fifo.push_if_absent(r_tx, Address::from([2; 20]), PreconfSource::Rpc).await;
        dispatch_one(&fifo, &cfg, r_hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "only the journal tx committed");
        assert_eq!(state.excluded_len(), 1, "RPC tx was gated out");
        let j_entry = fifo.find_by_hash(&j_hash).await.unwrap();
        assert_eq!(j_entry.status, PreconfStatus::Success);
        assert!(!fifo.contains(&r_hash).await, "the gated-out RPC tx was finalised away");
    }

    // `apply_lock` is gone: the deadline no longer waits for an in-flight
    // apply, so a tx may land under a `Timeout` answer — which is why `Timeout`
    // means "unknown, check the chain". The narrower replacement lives in the
    // fifo: `finish_success` refuses once the entry is finalised, covered by
    // `preconf_tx_set::tests::the_deadline_sweep_preempts_a_late_success`.
}
