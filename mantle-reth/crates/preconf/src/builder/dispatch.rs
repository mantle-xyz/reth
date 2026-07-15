//! Preconf dispatch helpers for
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload).
//!
//! The select! main loop inside `build_payload` calls these helpers
//! one hash at a time. Four invariants are enforced for every hash:
//!
//! - **Dedup**: a hash already in `committed` or `excluded` is
//!   short-circuited before any fifo / EVM work.
//! - **Status gate**: only `Waiting` entries proceed; terminal entries
//!   are recorded as excluded and skipped.
//! - **Pre-apply deadline**: when `entry.inserted_at.elapsed() +
//!   safety_margin >= preconf_timeout`, the tx is *not* applied; the
//!   fifo entry is flipped to `Timeout` and the responder is cancelled
//!   directly here. This closes the race where the RPC client has
//!   already given up but the builder is about to commit a receipt.
//! - **Responder ownership**: every terminal path (success, deadline
//!   skip, status-already-terminal) calls exactly one of
//!   `take_responder` / `cancel_responder`, never both.
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

use std::{collections::{HashMap, HashSet}, sync::Arc};

use alloy_consensus::TxEnvelope;
use alloy_primitives::TxHash;
use tracing::{debug, trace, warn};

use crate::{
    PreconfConfig, PreconfTxSet,
    types::{PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
};

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
    /// Cumulative pool best-tx gas committed in this block. Compared
    /// against the time-proportional pool quota
    /// (`payload_builder::pool_cumulative_quota`) at each sweep tick;
    /// pool txs only admit while `pool_gas_used < quota`. Independent
    /// of `preconf_gas_used` so quota accounting reflects only the
    /// pool arm, though both contribute to `ExecutionInfo::cumulative_gas_used`.
    pool_gas_used: u64,
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
            pool_gas_used: 0,
        }
    }

    /// Cumulative preconf gas committed in this block so far. Used by
    /// the payload builder to keep `ExecutionInfo::cumulative_gas_used`
    /// in sync (so pool best-tx `is_tx_over_limits` sees the true block
    /// gas usage), and by tests to assert budget tracking.
    pub(super) fn preconf_gas_used(&self) -> u64 {
        self.preconf_gas_used
    }

    /// Cumulative pool best-tx gas committed in this block so far.
    /// Independent counter used by the sweep-ticker branch to compare
    /// against the time-proportional pool quota.
    pub(super) fn pool_gas_used(&self) -> u64 {
        self.pool_gas_used
    }

    /// Record a pool best-tx apply that consumed `gas_used` gas.
    /// Called by the payload builder's sweep-tick arm after each
    /// successful `apply_one_best_tx`.
    pub(super) fn record_pool_gas(&mut self, gas_used: u64) {
        self.pool_gas_used = self.pool_gas_used.saturating_add(gas_used);
    }

    /// `true` iff the hash has already been committed or excluded by
    /// this loop instance. Retained as a bool wrapper for tests; the
    /// dispatch loop uses [`Self::is_committed`] / [`Self::excluded_reason`]
    /// to distinguish the two branches.
    #[cfg(test)]
    pub(super) fn contains(&self, hash: &TxHash) -> bool {
        self.committed.contains(hash) || self.excluded.contains_key(hash)
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
pub(super) async fn apply_one_preconf<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    hash: TxHash,
    loop_state: &mut LoopState,
    mut apply_fn: F,
) where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, PreconfError>,
{
    // Dedup — short-circuit if we've already made a decision on this
    // hash in this build. Committed hashes just return silently (the
    // apply's responder was consumed by `take_responder` earlier);
    // excluded hashes forward the stored rejection reason to any
    // newly-attached responder so a same-slot resubmit sees the same
    // error the first attempt fired, rather than waiting the full
    // `preconf_timeout` for the RPC-layer deadline to elapse.
    if loop_state.is_committed(&hash) {
        trace!(target: "mantle::preconf::dispatch", ?hash, "dedup hit; already committed");
        return;
    }
    if let Some(reason) = loop_state.excluded_reason(&hash).cloned() {
        trace!(
            target: "mantle::preconf::dispatch",
            ?hash, ?reason,
            "dedup hit; forwarding prior rejection to any pending responder"
        );
        fifo.cancel_responder(&hash, reason).await;
        return;
    }

    let Some(entry) = fifo.find_by_hash(&hash).await else {
        trace!(target: "mantle::preconf::dispatch", ?hash, "no fifo entry; skipping");
        return;
    };

    if entry.status != PreconfStatus::Waiting {
        // Already terminal — either a prior iteration finished it or
        // the RPC timeout flipped it. Record so the next broadcast
        // event short-circuits at the dedup gate above; the reason is
        // derived from the terminal status since we did not run the
        // gate ourselves.
        let reason = match entry.status {
            PreconfStatus::Timeout => PreconfError::Timeout {
                timeout_ms: cfg.preconf_timeout.as_millis() as u64,
            },
            other => PreconfError::Internal(format!(
                "preconf entry already terminal ({other:?}) at dispatch entry"
            )),
        };
        loop_state.record_excluded(hash, reason);
        return;
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
        let reason =
            PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 };
        fifo.cancel_responder(&hash, reason.clone()).await;
        loop_state.record_excluded(hash, reason);
        return;
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
    if is_rpc
        && loop_state.preconf_gas_used.saturating_add(tx_gas_limit)
            > cfg.preconf_max_gas_per_block
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
        return;
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
        return;
    };

    // Re-check status under lock. The RPC deadline branch may have
    // already transitioned the entry to `Timeout` in the window between
    // our earlier gate reads and this acquisition; running `apply_fn`
    // now would violate the invariant "committed to builder state ⇒
    // wire not Timeout".
    if let Some(re_entry) = fifo.find_by_hash(&hash).await {
        if re_entry.status != PreconfStatus::Waiting {
            trace!(
                target: "mantle::preconf::dispatch",
                ?hash, status = ?re_entry.status,
                "status flipped before we acquired apply_lock; skipping apply"
            );
            let reason = match re_entry.status {
                PreconfStatus::Timeout => PreconfError::Timeout {
                    timeout_ms: cfg.preconf_timeout.as_millis() as u64,
                },
                other => PreconfError::Internal(format!(
                    "preconf entry flipped to {other:?} before apply_lock"
                )),
            };
            loop_state.record_excluded(hash, reason);
            return;
        }
    }

    // ── Apply via caller-supplied closure (real EVM in production,
    //    synthetic receipt in tests). ────────────────────────────────
    let apply_started = std::time::Instant::now();
    let apply_result = apply_fn(entry.tx.clone(), hash, loop_state.predicted_height);
    let apply_duration = apply_started.elapsed();
    // Distribution of EVM apply latency — feeds SAFETY_MARGIN tuning.
    // Recorded once per call regardless of outcome; success / failure
    // counters (below) provide the breakdown.
    metrics::histogram!("preconf.execute.duration_ms")
        .record(apply_duration.as_millis() as f64);

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
        Err(err) => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?err,
                "preconf apply failed; marking entry as Failed"
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
    }
}

/// On broadcast `Lagged`, walk the fifo snapshot and apply any entry
/// not yet seen by this loop instance. Dedup is delegated to
/// [`apply_one_preconf`]'s gate ①. `apply_fn` must be `FnMut` since
/// it is invoked once per pending hash.
///
/// `skipped` is the count reported by `RecvError::Lagged(n)` — surfaced
/// to the warn log for devnet observability of lag severity.
pub(super) async fn reconcile_lagged<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    loop_state: &mut LoopState,
    skipped: u64,
    mut apply_fn: F,
) where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, PreconfError>,
{
    warn!(
        target: "mantle::preconf::dispatch",
        skipped,
        "broadcast lagged; reconciling via fifo snapshot"
    );
    for hash in fifo.snapshot().await {
        apply_one_preconf(fifo, cfg, hash, loop_state, &mut apply_fn).await;
    }
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

    /// Test apply closure that fabricates an always-success receipt
    /// using `tx.gas_limit()` as the reported `gas_used`. Mirrors the
    /// semantics of the retired `PromiseApplier`, kept here to exercise
    /// the dispatch state machine without standing up a real EVM.
    fn synthetic_ok(
        tx: Arc<TxEnvelope>,
        hash: TxHash,
        height: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
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

    /// Test apply closure that always errors — exercises the
    /// `mark_failed` + `take_responder(Err)` branch.
    fn synthetic_err(
        _: Arc<TxEnvelope>,
        _: TxHash,
        _: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
        Err(PreconfError::BuilderRejected("synthetic error for test".into()))
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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;

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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut counting_apply).await;
        assert_eq!(call_count.get(), 1);
        assert_eq!(state.committed_len(), 1);

        // Second call: dedup guard fires before apply_fn is invoked.
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut counting_apply).await;
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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut tracking_apply).await;

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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_err).await;

        // Responder got the apply error verbatim.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        assert!(matches!(err, PreconfError::BuilderRejected(_)));

        // Loop state recorded exclusion, NOT commit.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.excluded_len(), 1);

        // Fifo entry transitioned to Failed (not Success).
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Failed);
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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;

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

        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;

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
    /// LoopState `pool_gas_used` tracks the pool best-tx sweep independently
    /// of `preconf_gas_used` — `record_pool_gas` must accumulate and stay
    /// separate from `preconf_gas_used`.
    #[test]
    fn loop_state_pool_gas_used_accumulates_independently_of_preconf() {
        let mut state = LoopState::new(1);
        assert_eq!(state.pool_gas_used(), 0);
        assert_eq!(state.preconf_gas_used(), 0);

        state.record_pool_gas(21_000);
        state.record_pool_gas(50_000);
        assert_eq!(state.pool_gas_used(), 71_000);
        // preconf counter untouched.
        assert_eq!(state.preconf_gas_used(), 0);

        // Saturating add: extreme delta must not panic.
        state.record_pool_gas(u64::MAX);
        assert_eq!(state.pool_gas_used(), u64::MAX);
    }

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
            apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;
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
    /// - NOT record the hash in loop_state (allowing a future re-push
    ///   of the same hash to proceed normally)
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
        apply_one_preconf(&fifo, &cfg, TxHash::from([0xdd; 32]), &mut state, &mut apply_fn).await;

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

            apply_one_preconf(&fifo, &cfg, hash, &mut state, &mut apply_fn).await;

            assert_eq!(
                call_count.get(),
                0,
                "apply_fn must not run when status={pre_status:?}",
            );
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

    /// reconcile_lagged applies pending fifo entries in FIFO (insertion)
    /// order. Locks the `snapshot()` → VecDeque `order` guarantee so a
    /// future refactor to HashMap iteration would fail this test.
    #[tokio::test]
    async fn reconcile_lagged_applies_all_pending_in_fifo_order() {
        use std::cell::RefCell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();

        let mut hashes = Vec::new();
        for i in 0..3u8 {
            let tx = make_tx_with_gas(0xa0 + i, i as u64, 21_000);
            let h = *tx.tx_hash();
            fifo.push_if_absent(tx, Address::from([i + 1; 20]), PreconfSource::Rpc).await;
            hashes.push(h);
        }

        let mut state = LoopState::new(1);
        let seen: RefCell<Vec<TxHash>> = RefCell::new(Vec::new());
        let mut apply_fn = |tx, h, height| {
            seen.borrow_mut().push(h);
            synthetic_ok(tx, h, height)
        };
        reconcile_lagged(&fifo, &cfg, &mut state, 0, &mut apply_fn).await;

        assert_eq!(*seen.borrow(), hashes, "reconcile must apply in FIFO order");
        assert_eq!(state.committed_len(), 3);
        assert_eq!(state.excluded_len(), 0);
    }

    /// reconcile_lagged relies on `apply_one_preconf`'s gate ① for
    /// dedup. Hashes already in `loop_state.committed` must be skipped
    /// (apply_fn not invoked).
    #[tokio::test]
    async fn reconcile_lagged_dedups_against_prior_committed() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();

        let mut hashes = Vec::new();
        for i in 0..3u8 {
            let tx = make_tx_with_gas(0xb0 + i, i as u64, 21_000);
            hashes.push(*tx.tx_hash());
            fifo.push_if_absent(tx, Address::from([i + 1; 20]), PreconfSource::Rpc).await;
        }

        let mut state = LoopState::new(1);
        // Pretend two of them were already handled via broadcast path.
        state.record_committed(hashes[0]);
        state.record_committed(hashes[1]);

        let call_count = Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        reconcile_lagged(&fifo, &cfg, &mut state, 0, &mut apply_fn).await;

        assert_eq!(call_count.get(), 1, "only the one un-seen hash must trigger apply");
        assert_eq!(state.committed_len(), 3);
    }

    /// reconcile_lagged shares `LoopState.preconf_gas_used` with the
    /// broadcast path. Pre-loading `preconf_gas_used` so the second tx
    /// would exceed `preconf_max_gas_per_block` must cause it to be
    /// Canceled via the same gate `apply_one_preconf` uses.
    #[tokio::test]
    async fn reconcile_lagged_shares_gas_budget_with_apply_one_preconf() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 42_000,
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);

        // Two pending waiting txs of 21_000 gas each.
        let tx1 = make_tx_with_gas(0xc0, 0, 21_000);
        let h1 = *tx1.tx_hash();
        fifo.push_if_absent(tx1, Address::from([1; 20]), PreconfSource::Rpc).await;

        let tx2 = make_tx_with_gas(0xc1, 0, 21_000);
        let h2 = *tx2.tx_hash();
        fifo.push_if_absent(tx2, Address::from([2; 20]), PreconfSource::Rpc).await;

        // Pre-load 21_000: 21_000 + 21_000 = 42_000 = max → tx1 accepted
        // at boundary; 42_000 + 21_000 = 63_000 > 42_000 → tx2 rejected.
        let mut state = LoopState::new(1);
        state.preconf_gas_used = 21_000;

        reconcile_lagged(&fifo, &cfg, &mut state, 0, synthetic_ok).await;

        assert_eq!(state.preconf_gas_used(), 42_000, "only tx1 counted");
        assert_eq!(state.committed_len(), 1);
        assert_eq!(state.excluded_len(), 1);

        let e1 = fifo.find_by_hash(&h1).await.unwrap();
        assert_eq!(e1.status, PreconfStatus::Success);
        let e2 = fifo.find_by_hash(&h2).await.unwrap();
        assert_eq!(e2.status, PreconfStatus::Canceled);
    }

    /// reconcile_lagged iterates the whole `order` VecDeque including
    /// entries in terminal states (mark_* keeps entries until forward /
    /// clean_reclaimable removes them). Those must be filtered by
    /// `apply_one_preconf`'s status gate — apply_fn only fires for the
    /// Waiting entry.
    #[tokio::test]
    async fn reconcile_lagged_skips_non_waiting_entries() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();

        let waiting = make_tx_with_gas(0xd0, 0, 21_000);
        let h_waiting = *waiting.tx_hash();
        fifo.push_if_absent(waiting, Address::from([1; 20]), PreconfSource::Rpc).await;

        let timed = make_tx_with_gas(0xd1, 0, 21_000);
        let h_timed = *timed.tx_hash();
        fifo.push_if_absent(timed, Address::from([2; 20]), PreconfSource::Rpc).await;
        fifo.mark_timeout(&h_timed).await.unwrap();

        let succ = make_tx_with_gas(0xd2, 0, 21_000);
        let h_succ = *succ.tx_hash();
        fifo.push_if_absent(succ, Address::from([3; 20]), PreconfSource::Rpc).await;
        fifo.mark_succeeded(&h_succ).await.unwrap();

        let mut state = LoopState::new(1);
        let call_count = Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        reconcile_lagged(&fifo, &cfg, &mut state, 0, &mut apply_fn).await;

        assert_eq!(call_count.get(), 1, "apply_fn only for the Waiting entry");
        assert_eq!(state.committed_len(), 1);
        assert_eq!(state.excluded_len(), 2, "terminal entries recorded as excluded");
        assert!(state.contains(&h_waiting));
        assert!(state.contains(&h_timed));
        assert!(state.contains(&h_succ));
    }

    // ── Source-differentiated gate tests (mantle preconf SLA: journal
    //    replay must never silently drop a promised tx). ─────────────

    /// Journal-replayed entries bypass the pre-apply deadline gate.
    /// Even after the timeout has elapsed since insertion, apply_fn
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
        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;

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

        apply_one_preconf(&fifo, &cfg, hash, &mut state, synthetic_ok).await;

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

    /// Mixed sources share the LoopState `preconf_gas_used` accounting:
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
        apply_one_preconf(&fifo, &cfg, j_hash, &mut state, synthetic_ok).await;
        assert_eq!(state.preconf_gas_used(), 30_000);

        // RPC tx: 21_000 gas. 30_000 + 21_000 = 51_000 > 40_000 → rejected.
        let r_tx = make_tx_with_gas(0xe3, 0, 21_000);
        let r_hash = *r_tx.tx_hash();
        fifo.push_if_absent(r_tx, Address::from([2; 20]), PreconfSource::Rpc).await;
        apply_one_preconf(&fifo, &cfg, r_hash, &mut state, synthetic_ok).await;

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
            apply_one_preconf(&fifo_clone, &cfg_clone, hash, &mut state, slow_apply).await;
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
