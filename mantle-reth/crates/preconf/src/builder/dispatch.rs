//! Preconf dispatch helpers for
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload).
//!
//! The select! main loop inside `build_payload` calls these helpers
//! one hash at a time. State-machine and responder-ownership invariants
//! are preserved from the retired `BuilderLoop` (still in
//! `builder/builder.rs` until Step 7 deletes it):
//!
//! - **Dedup**: a hash already in `committed` or `excluded` is
//!   short-circuited before any fifo / EVM work.
//! - **Status gate**: only `Waiting` entries proceed; terminal entries
//!   are recorded as excluded and skipped.
//! - **Pre-apply deadline**: when `entry.inserted_at.elapsed() +
//!   safety_margin >= preconf_timeout`, the tx is *not* applied; the
//!   fifo entry is flipped to `Timeout` and the responder is cancelled
//!   directly here (matching the carry-over from P5 dev-plan §"Pre-apply
//!   deadline check").
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
//! the state-machine invariants are exercised in isolation. Real
//! end-to-end behaviour is covered by integration tests once
//! `build_payload` is fully wired (Step 5+).

use std::{collections::HashSet, sync::Arc};

use alloy_consensus::TxEnvelope;
use alloy_primitives::TxHash;
use tracing::{debug, trace, warn};

use crate::{
    PreconfConfig, PreconfTxSet,
    types::{PreconfError, PreconfReceipt, PreconfStatus},
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
}

impl LoopState {
    /// Construct a fresh local state for a payload job targeting
    /// `predicted_height` (the parent's block number + 1).
    pub(super) fn new(predicted_height: u64) -> Self {
        Self { committed: HashSet::new(), excluded: HashSet::new(), predicted_height }
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
/// `apply_fn` is `FnOnce` — invoked at most once on success-path
/// reach. If a dedup / status / deadline guard fires earlier,
/// `apply_fn` is never called.
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

    // Pre-apply deadline check — see crate-level docs.
    let margin = cfg.preconf_timeout / 5;
    if entry.inserted_at.elapsed() + margin >= cfg.preconf_timeout {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            elapsed_ms = entry.inserted_at.elapsed().as_millis() as u64,
            "pre-apply deadline passed; aborting"
        );
        let _ = fifo.mark_timeout(&hash).await;
        fifo.cancel_responder(
            &hash,
            PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 },
        )
        .await;
        loop_state.record_excluded(hash);
        return;
    }

    // ── Apply via caller-supplied closure (real EVM in production,
    //    synthetic receipt in tests). ────────────────────────────────
    match apply_fn(entry.tx.clone(), hash, loop_state.predicted_height) {
        Ok(receipt) => {
            loop_state.record_committed(hash);
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
/// not yet seen by this loop instance. Idempotent — `contains()`
/// guards against re-processing.
///
/// Takes the same apply closure shape as
/// [`apply_one_preconf`]; the closure must be `FnMut` here because the
/// loop may invoke it multiple times (once per pending hash). Note: the
/// closure captures `&mut builder` in production, so it can apply each
/// reconciled tx to the same in-flight block.
pub(super) async fn reconcile_lagged<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    loop_state: &mut LoopState,
    mut apply_fn: F,
) where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, PreconfError>,
{
    warn!(
        target: "mantle::preconf::dispatch",
        "broadcast lagged; reconciling via fifo snapshot"
    );
    for hash in fifo.snapshot().await {
        if loop_state.contains(&hash) {
            continue;
        }
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
        fifo.attach_responder(hash, resp_tx).await.unwrap();
        assert!(matches!(
            fifo.push_if_absent(tx.clone(), Address::ZERO).await,
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
        fifo.push_if_absent(tx, Address::ZERO).await;

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
        fifo.attach_responder(hash, resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO).await;

        // Sleep past the deadline (incl. safety_margin = timeout/5 = 10ms).
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
        fifo.attach_responder(hash, resp_tx).await.unwrap();
        fifo.push_if_absent(tx, Address::ZERO).await;

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
}
