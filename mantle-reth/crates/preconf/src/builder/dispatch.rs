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

use std::{collections::HashSet, sync::Arc, time::Duration};

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
    /// Hashes excluded — terminal-non-success, deadline-skip, etc.
    excluded: HashSet<TxHash>,
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
            excluded: HashSet::new(),
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
    /// this loop instance.
    pub(super) fn contains(&self, hash: &TxHash) -> bool {
        self.committed.contains(hash) || self.excluded.contains(hash)
    }

    /// Mark hash as committed. Idempotent.
    pub(super) fn record_committed(&mut self, hash: TxHash) {
        self.committed.insert(hash);
    }

    /// Mark hash as excluded. Idempotent.
    pub(super) fn record_excluded(&mut self, hash: TxHash) {
        self.excluded.insert(hash);
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
    if loop_state.contains(&hash) {
        trace!(target: "mantle::preconf::dispatch", ?hash, "dedup hit; skipping");
        return;
    }

    let Some(entry) = fifo.find_by_hash(&hash).await else {
        trace!(target: "mantle::preconf::dispatch", ?hash, "no fifo entry; skipping");
        return;
    };

    if entry.status != PreconfStatus::Waiting {
        // Already terminal — either a prior iteration finished it or
        // the RPC timeout flipped it. Record so the next broadcast
        // event short-circuits at the dedup gate above.
        loop_state.record_excluded(hash);
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

    // Pre-apply deadline check — see crate-level docs.
    //
    // Safety margin is a fixed 40ms — sized to slightly exceed measured
    // p99 apply latency on the target hardware, so deadline-skip only
    // fires on genuine race conditions rather than merely slow but
    // in-budget applies. Hardcoded (rather than scaled off
    // `preconf_timeout`) because the two knobs serve different purposes:
    // `preconf_timeout` is the client-facing SLA, whereas the safety
    // margin tracks builder execution jitter and should stay bounded
    // even if the SLA widens.
    const SAFETY_MARGIN: Duration = Duration::from_millis(40);
    let margin = SAFETY_MARGIN;

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
        fifo.cancel_responder(
            &hash,
            PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 },
        )
        .await;
        loop_state.record_excluded(hash);
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
        fifo.cancel_responder(
            &hash,
            PreconfError::BlockGasBudgetExceeded {
                max: cfg.preconf_max_gas_per_block,
                used: loop_state.preconf_gas_used,
                limit: tx_gas_limit,
            },
        )
        .await;
        loop_state.record_excluded(hash);
        return;
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
            loop_state.record_excluded(hash);
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

}
