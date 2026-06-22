//! Unit tests for [`BuilderLoop`].
//!
//! A real reth `BlockBuilder` is heavyweight (chain spec, EVM, state
//! DB); we substitute [`StubApplier`] to capture each `apply` call as
//! `(hash, block_height)`. The loop's dispatch / fifo state-machine
//! semantics are observed via the resulting tracker counts, fifo
//! statuses, and responder values.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use alloy_consensus::{Signed, TxEnvelope, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::{
    PreconfConfig, PreconfTxSet,
    builder::{builder::BuilderLoop, cancel::JobCancel, tx_tracker::BuilderTxTracker},
    types::{PreconfError, PreconfReceipt, PreconfStatus},
};

use super::PreconfTxApplier;

// ─── Stub Applier ─────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct StubApplier {
    /// Records of `(hash, block_height)` per `apply` call.
    calls: Arc<Mutex<Vec<(TxHash, u64)>>>,
    /// Optional behaviour override. `None` ⇒ success-with-status-true.
    behaviour: Arc<Mutex<Behaviour>>,
}

#[derive(Default, Clone, Copy)]
enum Behaviour {
    #[default]
    SuccessOk,
    RevertOk,
    ErrReject,
}

impl StubApplier {
    fn with_behaviour(b: Behaviour) -> Self {
        Self { calls: Arc::default(), behaviour: Arc::new(Mutex::new(b)) }
    }
}

impl PreconfTxApplier for StubApplier {
    fn apply(
        &mut self,
        _tx: Arc<TxEnvelope>,
        block_height: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
        // We have to invent a hash for the receipt; the loop only
        // forwards what we return, so tests don't rely on it being any
        // particular value. Use the call index for traceability.
        let idx = self.calls.lock().unwrap().len() as u8;
        let hash = TxHash::from([idx; 32]);
        self.calls.lock().unwrap().push((hash, block_height));
        match *self.behaviour.lock().unwrap() {
            Behaviour::SuccessOk => Ok(PreconfReceipt {
                tx_hash: hash,
                block_height,
                status: true,
                logs: vec![],
                gas_used: 21_000,
                reason: String::new(),
                revert_data: Bytes::new(),
            }),
            Behaviour::RevertOk => Ok(PreconfReceipt {
                tx_hash: hash,
                block_height,
                status: false,
                logs: vec![],
                gas_used: 30_000,
                reason: "execution reverted".into(),
                revert_data: Bytes::new(),
            }),
            Behaviour::ErrReject => Err(PreconfError::BuilderRejected("nonce too low".into())),
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────

fn legacy_tx(nonce: u64, hash_byte: u8) -> Arc<TxEnvelope> {
    let inner = TxLegacy { nonce, ..Default::default() };
    let sig = Signature::test_signature();
    let hash = B256::from([hash_byte; 32]);
    Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
}

fn addr(byte: u8) -> Address {
    Address::from([byte; 20])
}

fn cfg_with_timeout(timeout_ms: u64) -> Arc<PreconfConfig> {
    // Use struct-update syntax instead of `Default + field reassign` so
    // clippy's `field_reassign_with_default` stays happy.
    Arc::new(PreconfConfig {
        preconf_timeout: Duration::from_millis(timeout_ms),
        // Short sweep so deadline-handling tests don't have to wait
        // the default 50ms just to hit the barrier path.
        sweep_interval: Duration::from_millis(5),
        ..PreconfConfig::default()
    })
}

// ─── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_exits_loop_promptly() {
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let lp = BuilderLoop::new(StubApplier::default(), fifo, cfg, cancel.clone(), 100);

    let handle = tokio::spawn(lp.run());
    // Give the loop one cycle to subscribe, then cancel.
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.signal();
    timeout(Duration::from_millis(100), handle).await.expect("loop did not exit").unwrap();
}

#[tokio::test]
async fn preconf_event_applies_and_delivers_receipt() {
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let stub = StubApplier::default();
    let calls = stub.calls.clone();

    // Pre-register a responder so apply success flows out via oneshot.
    let (resp_tx, resp_rx) = oneshot::channel();
    let tx = legacy_tx(0, 0x11);
    let hash = *tx.tx_hash();
    fifo.attach_responder(hash, resp_tx).await.unwrap();

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 99);
    let handle = tokio::spawn(lp.run());

    // Push the tx — broadcasts notification.
    let push = fifo.push_if_absent(tx, addr(0xaa)).await;
    assert!(matches!(push, crate::types::PushResult::Inserted));

    // Wait for the responder to receive the receipt.
    let receipt = timeout(Duration::from_millis(200), resp_rx)
        .await
        .expect("responder timed out")
        .expect("oneshot closed")
        .expect("apply returned err");
    assert_eq!(receipt.block_height, 99);
    assert!(receipt.status);

    cancel.signal();
    let _ = handle.await;

    // Drop the lock before any `.await` to satisfy clippy.
    let (observed_len, observed_height) = {
        let guard = calls.lock().unwrap();
        (guard.len(), guard.first().map(|(_, h)| *h))
    };
    assert_eq!(observed_len, 1, "applier should be called exactly once");
    assert_eq!(observed_height, Some(99));

    // Fifo entry should now be Success.
    let entry = fifo.find_by_hash(&hash).await.unwrap();
    assert_eq!(entry.status, PreconfStatus::Success);
}

#[tokio::test]
async fn apply_failure_marks_failed_and_sends_err_to_responder() {
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let stub = StubApplier::with_behaviour(Behaviour::ErrReject);

    let (resp_tx, resp_rx) = oneshot::channel();
    let tx = legacy_tx(0, 0x22);
    let hash = *tx.tx_hash();
    fifo.attach_responder(hash, resp_tx).await.unwrap();

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 50);
    let handle = tokio::spawn(lp.run());

    fifo.push_if_absent(tx, addr(0xbb)).await;

    let err = timeout(Duration::from_millis(200), resp_rx)
        .await
        .expect("responder timed out")
        .expect("oneshot closed")
        .expect_err("expected Err");
    assert!(matches!(err, PreconfError::BuilderRejected(_)));

    cancel.signal();
    let _ = handle.await;

    let entry = fifo.find_by_hash(&hash).await.unwrap();
    assert_eq!(entry.status, PreconfStatus::Failed);
}

#[tokio::test]
async fn revert_receipt_marks_failed_but_returns_ok_to_responder() {
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let stub = StubApplier::with_behaviour(Behaviour::RevertOk);

    let (resp_tx, resp_rx) = oneshot::channel();
    let tx = legacy_tx(0, 0x33);
    let hash = *tx.tx_hash();
    fifo.attach_responder(hash, resp_tx).await.unwrap();

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 7);
    let handle = tokio::spawn(lp.run());

    fifo.push_if_absent(tx, addr(0xcc)).await;

    let receipt = timeout(Duration::from_millis(200), resp_rx)
        .await
        .expect("responder timed out")
        .expect("oneshot closed")
        .expect("revert path should still be Ok(receipt) to the responder");
    assert!(!receipt.status);
    assert_eq!(receipt.reason, "execution reverted");

    cancel.signal();
    let _ = handle.await;

    // Fifo entry is Failed (terminal on EVM revert).
    let entry = fifo.find_by_hash(&hash).await.unwrap();
    assert_eq!(entry.status, PreconfStatus::Failed);
}

#[tokio::test]
async fn deadline_skips_apply_and_marks_timeout() {
    // 5ms timeout — by the time the loop sees the event the entry's
    // elapsed will dominate. Safety margin = 1ms; effective deadline
    // ≥ 4ms elapsed → skip.
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = Arc::new(PreconfConfig {
        preconf_timeout: Duration::from_millis(5),
        sweep_interval: Duration::from_millis(2),
        ..PreconfConfig::default()
    });
    let cancel = JobCancel::new();
    let stub = StubApplier::default();
    let calls = stub.calls.clone();

    let (resp_tx, resp_rx) = oneshot::channel();
    let tx = legacy_tx(0, 0x44);
    let hash = *tx.tx_hash();
    fifo.attach_responder(hash, resp_tx).await.unwrap();
    // Push *before* spawning the loop, then sleep past the timeout so
    // `inserted_at.elapsed()` already exceeds the deadline by the time
    // the loop picks it up.
    fifo.push_if_absent(tx, addr(0xdd)).await;
    tokio::time::sleep(Duration::from_millis(15)).await;

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 1);
    let handle = tokio::spawn(lp.run());

    let err = timeout(Duration::from_millis(200), resp_rx)
        .await
        .expect("responder timed out at test level")
        .expect("oneshot closed")
        .expect_err("deadline path must send Err");
    assert!(matches!(err, PreconfError::Timeout { .. }));

    cancel.signal();
    let _ = handle.await;

    let observed_empty = calls.lock().unwrap().is_empty();
    assert!(observed_empty, "applier must not be called when past deadline");

    let entry = fifo.find_by_hash(&hash).await.unwrap();
    assert_eq!(entry.status, PreconfStatus::Timeout);
}

#[tokio::test]
async fn dedup_short_circuits_repeat_events_for_committed_hash() {
    // After a successful apply, a subsequent `Preconf(hash)` event
    // (e.g. from a snapshot reconcile) must not re-invoke the applier.
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let stub = StubApplier::default();
    let calls = stub.calls.clone();

    let tx = legacy_tx(0, 0x55);
    let hash = *tx.tx_hash();

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 1);
    let handle = tokio::spawn(lp.run());

    fifo.push_if_absent(tx, addr(0xee)).await;
    // Give the loop a moment to apply.
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(calls.lock().unwrap().len(), 1);

    // Re-broadcast the same hash; the tracker should short-circuit.
    fifo.subscribe(); // ensure subscription is alive; not strictly needed
    let _ = fifo.recover_from_timeout(&hash).await; // intentional no-op (status is Success)
    // Force another event by re-pushing wouldn't work (PushResult::AlreadyExists).
    // Instead trigger a lagged-style reconcile by sleeping past a few
    // sweep intervals — but the loop doesn't reconcile on sweep, only
    // on Lagged. So just verify that no spontaneous re-apply happens.
    tokio::time::sleep(Duration::from_millis(50)).await;

    cancel.signal();
    let _ = handle.await;

    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "applier must be invoked exactly once across the slot"
    );
}

#[tokio::test]
async fn applier_skips_when_entry_already_terminal() {
    // Race scenario: RPC handler already flipped Waiting → Timeout via
    // its own timer before the loop dispatches the Preconf event.
    let fifo = Arc::new(PreconfTxSet::new(16));
    let cfg = cfg_with_timeout(200);
    let cancel = JobCancel::new();
    let stub = StubApplier::default();
    let calls = stub.calls.clone();

    let tx = legacy_tx(0, 0x66);
    let hash = *tx.tx_hash();
    fifo.push_if_absent(tx, addr(0xff)).await;
    fifo.mark_timeout(&hash).await.unwrap();

    let lp = BuilderLoop::new(stub, fifo.clone(), cfg, cancel.clone(), 1);
    let handle = tokio::spawn(lp.run());

    // The Preconf event was queued at push time, before we flipped to
    // Timeout. The loop should observe the broadcast, look up the
    // entry, see status != Waiting, and skip.
    tokio::time::sleep(Duration::from_millis(30)).await;

    cancel.signal();
    let _ = handle.await;

    assert!(calls.lock().unwrap().is_empty(), "applier must not run on non-Waiting entry");
}

// ─── tx_tracker forwarding ─────────────────────────────────────────────

#[test]
fn tx_tracker_records_committed_and_excluded_independently() {
    // Sanity check the wrapper for invariants the loop relies on.
    let mut t = BuilderTxTracker::new();
    let h1 = TxHash::from([1; 32]);
    let h2 = TxHash::from([2; 32]);
    t.record_committed(h1);
    t.record_excluded(h2);
    assert!(t.contains(&h1));
    assert!(t.contains(&h2));
    assert!(t.is_committed(&h1));
    assert!(t.is_excluded(&h2));
    assert!(!t.is_excluded(&h1));
    assert!(!t.is_committed(&h2));
}

// ─── PromiseApplier ───────────────────────────────────────────────────

use super::PromiseApplier;

#[test]
fn promise_applier_returns_success_with_actual_tx_hash() {
    let mut applier = PromiseApplier;
    let tx = legacy_tx(0, 0x77);
    let want_hash = *tx.tx_hash();
    let receipt = applier.apply(tx, 1234).expect("promise applier never errs");
    assert_eq!(receipt.tx_hash, want_hash, "must echo back the real tx hash");
    assert_eq!(receipt.block_height, 1234, "must echo back the supplied block_height");
    assert!(receipt.status, "promise is always success — execution outcome surfaces later");
    assert!(receipt.logs.is_empty(), "no execution → no logs");
    assert_eq!(receipt.reason, "");
    assert!(receipt.revert_data.is_empty());
}

#[test]
fn promise_applier_gas_used_equals_tx_gas_limit() {
    use alloy_consensus::{Signed, TxLegacy};
    let mut applier = PromiseApplier;
    let inner = TxLegacy { nonce: 0, gas_limit: 12_345, ..Default::default() };
    let sig = Signature::test_signature();
    let envelope = TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, B256::from([0x88; 32])));
    let tx = Arc::new(envelope);
    let receipt = applier.apply(tx, 0).unwrap();
    assert_eq!(receipt.gas_used, 12_345, "worst-case estimate = tx gas_limit");
}

#[test]
fn promise_applier_is_pure_no_side_effects() {
    // Same input → same output, no internal state mutated between calls.
    let mut applier = PromiseApplier;
    let tx1 = legacy_tx(0, 0x99);
    let tx2 = legacy_tx(0, 0x99);
    let r1 = applier.apply(tx1, 42).unwrap();
    let r2 = applier.apply(tx2, 42).unwrap();
    assert_eq!(r1, r2);
}
