//! `PreconfPayloadJob` — implements reth's [`PayloadJob`] trait, wrapping
//! the spawned [`PreconfPayloadBuilder::build_payload`] future.
//!
//! The build runs on a separately-spawned tokio task; this job is just
//! the read-side handle the payload service polls:
//!
//! - **`best_payload()`**: reads the latest payload from a `watch`
//!   receiver (set once by the build task when it finishes). Returns
//!   `MissingPayload` until the build completes.
//! - **`payload_attributes()`**: clones the cached attributes — no I/O.
//! - **`resolve_kind()`**: signals the build task's cancel + returns a
//!   future that resolves to the final payload (or `MissingPayload`
//!   if the build errored out and the sender dropped).
//! - **`Future` impl**: returns `Pending` until cancel fires, then
//!   `Ready(Ok(()))` — matches reth's contract that the job future
//!   resolves on completion, not on payload availability.
//!
//! Concretely the read side is one [`tokio::sync::watch::Receiver`]
//! initialised with `None`. The build task sends `Some(payload)` on
//! success; on error it just drops the sender, which makes
//! [`watch::Receiver::changed`] return `Err`.
//!
//! [`PayloadJob`]: reth_payload_builder::PayloadJob
//! [`PreconfPayloadBuilder::build_payload`]: crate::builder::payload_builder::PreconfPayloadBuilder::build_payload

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::FutureExt;
use reth_payload_builder::{KeepPayloadJobAlive, PayloadJob};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuiltPayload, PayloadAttributes, PayloadKind};
use tokio::{sync::watch, task::JoinHandle};

use crate::builder::cancel::JobCancel;

/// A `PayloadJob` driven by a spawned [`PreconfPayloadBuilder::build_payload`]
/// async task. See module docs for the lifecycle contract.
///
/// [`PreconfPayloadBuilder::build_payload`]: crate::builder::payload_builder::PreconfPayloadBuilder::build_payload
pub struct PreconfPayloadJob<Attrs, Payload> {
    /// Cached attributes — re-cloned on every `payload_attributes()` call.
    attributes: Attrs,
    /// Read-side of the watch channel the build task writes the final
    /// payload into. `None` until the build finishes successfully.
    payload_rx: watch::Receiver<Option<Payload>>,
    /// Cancel handle shared with the spawned build task. Flipped by
    /// [`Self::resolve_kind`] when the CL asks for the payload, or by
    /// the [`Drop`] impl when the payload service prunes this job
    /// (e.g. after a reorg switches to a different fork's attributes,
    /// or when job-cache capacity forces eviction). Idempotent — both
    /// paths may fire in sequence without ill effect.
    cancel: JobCancel,
    /// [`JoinHandle`] for the spawned build task. Held solely to keep
    /// the join channel alive for the job's lifetime — no `.await` on
    /// this handle happens. On drop the handle detaches (the closure
    /// keeps running on the blocking-pool thread until it observes
    /// the cancel signal and exits naturally). Renaming to
    /// `_build_task_handle` was considered but the `_` prefix already
    /// signals "field held for RAII, not read".
    _join_handle: JoinHandle<()>,
}

impl<Attrs, Payload> PreconfPayloadJob<Attrs, Payload> {
    /// Construct a new job bound to a spawned build task.
    ///
    /// Typically called from
    /// [`PreconfPayloadJobGenerator::new_payload_job`](crate::builder::payload_job_generator::PreconfPayloadJobGenerator::new_payload_job)
    /// after spawning the build future. Tests can call this directly
    /// to inject a pre-filled `watch::Receiver`.
    pub const fn new(
        attributes: Attrs,
        payload_rx: watch::Receiver<Option<Payload>>,
        cancel: JobCancel,
        join_handle: JoinHandle<()>,
    ) -> Self {
        Self { attributes, payload_rx, cancel, _join_handle: join_handle }
    }

    /// Read-only access to the cancel handle. Tests use this to flip
    /// cancel and observe job teardown; the spawned task holds its own
    /// clone via the build future.
    pub fn cancel_handle(&self) -> JobCancel {
        self.cancel.clone()
    }
}

impl<Attrs, Payload> std::fmt::Debug for PreconfPayloadJob<Attrs, Payload>
where
    Attrs: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfPayloadJob")
            .field("attributes", &self.attributes)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

// ─── Drop impl — pruned-job cancellation ────────────────────────────────────
//
// The payload service holds a bounded LRU-ish cache of active jobs.
// When a new job for a different `PayloadId` is created (typical case:
// reorg — engine sends an FCU pointing at a fresh head/attrs, and the
// old job's PayloadId no longer matches), the service may evict the
// stale job by dropping it. Without an explicit cancel signal the
// spawned build task would keep running against the now-obsolete parent
// state until its own natural exit (Stage 5 finalize), wasting compute
// and holding the shared `PreconfTxSet` in a state visible to the newly-
// started job.
//
// Signaling cancel from Drop guarantees the build task observes the
// cancel arm of its `select!` and unwinds through Stage 4/5 promptly.
// The final `payload_tx.send(...)` will silently fail because the
// receiver has already been dropped alongside the job (see
// `payload_job_generator.rs::new_payload_job` for the send-fail log).
//
// Idempotent with respect to `resolve_kind`'s own `cancel.signal()` —
// `JobCancel::signal` is a single `watch::Sender::send(true)` and a
// second call is a no-op.
impl<Attrs, Payload> Drop for PreconfPayloadJob<Attrs, Payload> {
    fn drop(&mut self) {
        self.cancel.signal();
    }
}

// ─── Future impl ────────────────────────────────────────────────────────────

impl<Attrs, Payload> Future for PreconfPayloadJob<Attrs, Payload>
where
    Attrs: Unpin,
    Payload: Unpin,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The job future resolves on cancel — the payload itself is
        // delivered through `best_payload` / `resolve_kind`. Spinning
        // here is fine because the payload service only polls us when
        // its own deadline timer fires; in practice we'll see cancel
        // before re-poll arrives.
        if self.cancel.is_cancelled() {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
    }
}

// ─── PayloadJob impl ────────────────────────────────────────────────────────

impl<Attrs, Payload> PayloadJob for PreconfPayloadJob<Attrs, Payload>
where
    Attrs: PayloadAttributes + Clone + std::fmt::Debug + Unpin + 'static,
    Payload: BuiltPayload + Clone + std::fmt::Debug + Send + Sync + Unpin + 'static,
{
    type PayloadAttributes = Attrs;
    type ResolvePayloadFuture = ResolvePayloadFuture<Payload>;
    type BuiltPayload = Payload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        self.payload_rx
            .borrow()
            .clone()
            .ok_or(PayloadBuilderError::MissingPayload)
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        Ok(self.attributes.clone())
    }

    fn resolve_kind(
        &mut self,
        _kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        // Signal the spawned build task to wrap up. The select! cancel
        // arm inside `build_payload` will break out of its loop, run
        // SDM post-exec + finalize, and then `payload_rx` will receive
        // the final payload.
        self.cancel.signal();
        let fut = ResolvePayloadFuture::new(self.payload_rx.clone());
        (fut, KeepPayloadJobAlive::No)
    }
}

// ─── ResolvePayloadFuture ───────────────────────────────────────────────────

/// Future returned by [`PreconfPayloadJob::resolve_kind`].
///
/// Waits for the spawned build task to **finish** (sender drops on
/// task exit), then returns whatever the watch channel last held
/// — `Some(payload)` → `Ok`, `None` → `MissingPayload`.
///
/// ## Why "wait for sender drop" rather than "return first `Some`"
///
/// The pre-flashblock model only sent **once** (final payload), so
/// "return first `Some`" was almost-always equivalent. But it has
/// two latent issues:
///
/// 1. **Race window** in the single-set case: if the receiver polls
///    before the spawned task sends, then sees `None`, then polls
///    again right after the send but before the task exits, the
///    "first `Some`" returns immediately. That's fine for single-set
///    but **wrong for flashblocks**, where the sender will set
///    intermediate flashblock payloads — "first `Some`" would return
///    a mid-build snapshot instead of the final block.
/// 2. **Semantic mismatch**: `resolve_kind` is called when the CL asks
///    for the *finished* payload. We should wait for the build to
///    actually complete (sender drop), not opportunistically grab
///    whatever's in the cell.
///
/// New semantic (forward-compatible with flashblocks): loop on
/// `changed().await` until it returns `Err` (sender dropped → build
/// task exited), then read the final value from `borrow()`.
///
/// `resolve_kind` calls `self.cancel.signal()` before constructing
/// this future, so the build task is already wrapping up — the
/// `changed()` loop typically resolves within a few hundred μs.
///
/// Boxed `dyn Future` because `watch::Receiver::changed` is async and
/// the simplest portable expression is an `async move` block.
pub struct ResolvePayloadFuture<Payload> {
    inner: Pin<Box<dyn Future<Output = Result<Payload, PayloadBuilderError>> + Send>>,
}

impl<Payload> ResolvePayloadFuture<Payload>
where
    // `Sync` is needed because the async block holds a
    // `watch::Receiver` whose `borrow()` returns a `Ref<'_, T>` —
    // crossing `.await` requires `T: Sync`. The future itself must
    // also be `Send`, which transitively requires
    // `Receiver<Option<Payload>>: Send`.
    Payload: Clone + Send + Sync + 'static,
{
    /// Build a future that resolves when the build task drops its
    /// `watch::Sender` (task exit, success or failure). Returns the
    /// last value written to the channel, or `MissingPayload` if
    /// nothing was ever written.
    pub fn new(mut payload_rx: watch::Receiver<Option<Payload>>) -> Self {
        let fut = async move {
            // Drain `changed()` notifications until the sender drops.
            // Intermediate `Ok(())` results signal that the watch
            // value was updated — for the single-set model that's the
            // final payload; for future flashblocks each `Ok(())`
            // signals an intermediate flashblock. Either way we keep
            // waiting for the sender to actually drop (i.e. the build
            // task to exit) before reading the latest value.
            while payload_rx.changed().await.is_ok() {
                // No-op: value was updated, but build task may still
                // emit more (flashblocks) or be finishing finalize.
            }
            // Sender dropped — read whatever's in the cell.
            payload_rx
                .borrow()
                .clone()
                .ok_or(PayloadBuilderError::MissingPayload)
        };
        Self { inner: Box::pin(fut) }
    }
}

impl<Payload> std::fmt::Debug for ResolvePayloadFuture<Payload> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvePayloadFuture").finish_non_exhaustive()
    }
}

impl<Payload> Future for ResolvePayloadFuture<Payload>
where
    Payload: Unpin,
{
    type Output = Result<Payload, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().inner.poll_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    //! Tests construct a `PreconfPayloadJob` directly (skipping the
    //! generator) by spawning a trivial task that writes into the
    //! watch channel. This isolates the trait-impl behaviour from the
    //! generator's parent-header lookup.

    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn cancel_resolves_the_job_future() {
        let (_tx, rx) = watch::channel::<Option<()>>(None);
        let cancel = JobCancel::new();
        let handle = tokio::spawn(async {});
        let job = PreconfPayloadJob::new((), rx, cancel.clone(), handle);

        cancel.signal();
        // Should resolve immediately
        timeout(Duration::from_millis(50), job)
            .await
            .expect("job did not resolve after cancel")
            .expect("job returned error");
    }

    /// Dropping a job (payload-service prunes it, e.g. after a reorg
    /// evicts the old-parent job in favor of the new-fork one) must
    /// signal cancel so the spawned build task can unwind promptly
    /// instead of running to natural completion against an obsolete
    /// parent state.
    #[tokio::test]
    async fn dropping_job_signals_cancel() {
        let (_tx, rx) = watch::channel::<Option<()>>(None);
        let cancel = JobCancel::new();
        let cancel_observer = cancel.clone();
        let handle = tokio::spawn(async {});
        let job = PreconfPayloadJob::new((), rx, cancel, handle);

        assert!(!cancel_observer.is_cancelled(), "cancel starts clear");
        drop(job);
        assert!(
            cancel_observer.is_cancelled(),
            "drop must signal cancel so the build task can exit its select! loop"
        );
    }

    /// Drop after cancel has already been signalled (either by
    /// `resolve_kind` or an external caller) is a no-op —
    /// `JobCancel::signal` is idempotent. Locks the invariant so the
    /// Drop-triggered cancel can't accidentally regress state when it
    /// races with a resolve_kind-triggered cancel.
    #[tokio::test]
    async fn drop_after_prior_cancel_is_idempotent() {
        let (_tx, rx) = watch::channel::<Option<()>>(None);
        let cancel = JobCancel::new();
        let cancel_observer = cancel.clone();
        let handle = tokio::spawn(async {});
        let job = PreconfPayloadJob::new((), rx, cancel.clone(), handle);

        // Simulate `resolve_kind`'s cancel firing before drop.
        cancel.signal();
        assert!(cancel_observer.is_cancelled(), "explicit signal marks cancel");

        // Dropping the (already-cancelled) job must not panic or
        // otherwise regress state — cancel stays observably cancelled.
        drop(job);
        assert!(
            cancel_observer.is_cancelled(),
            "drop is idempotent — cancel remains set after redundant signal"
        );
    }

    #[tokio::test]
    async fn resolve_future_returns_value_after_watch_send() {
        let (tx, rx) = watch::channel::<Option<u64>>(None);

        let fut = ResolvePayloadFuture::new(rx);
        // Spawn a writer task to drop value into the channel.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx.send(Some(42));
        });

        let value = timeout(Duration::from_millis(100), fut)
            .await
            .expect("resolve future timed out")
            .expect("value was None");
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn resolve_future_returns_missing_when_sender_drops() {
        let (tx, rx) = watch::channel::<Option<u64>>(None);
        let fut = ResolvePayloadFuture::new(rx);

        // Drop sender without sending — simulates build task erroring out.
        drop(tx);

        let err = timeout(Duration::from_millis(100), fut)
            .await
            .expect("resolve future hung")
            .expect_err("should be MissingPayload");
        assert!(matches!(err, PayloadBuilderError::MissingPayload));
    }

    #[tokio::test]
    async fn resolve_future_returns_latest_after_multiple_sends() {
        // Forward-compat for flashblocks: when the sender writes
        // multiple intermediate values before exiting, the resolve
        // future must return the LAST one (not the first observed).
        let (tx, rx) = watch::channel::<Option<u64>>(None);
        let fut = ResolvePayloadFuture::new(rx);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx.send(Some(1));
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx.send(Some(2));
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx.send(Some(99));
            // task exits → tx dropped → resolve future wakes
        });

        let value = timeout(Duration::from_millis(200), fut)
            .await
            .expect("resolve future timed out")
            .expect("value was None");
        assert_eq!(
            value, 99,
            "resolve_kind must return the LAST sent value (flashblock-prep semantic), \
             not the first observed"
        );
    }

    #[tokio::test]
    async fn resolve_future_does_not_return_early_on_single_send() {
        // Sanity check on the new "wait for sender drop" semantic:
        // even a single `send(Some(...))` should not cause the future
        // to resolve before the sender actually drops. (Old "return
        // first Some" semantic would resolve immediately on send.)
        let (tx, rx) = watch::channel::<Option<u64>>(None);
        let fut = ResolvePayloadFuture::new(rx);

        let send_then_hold = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(Some(7));
            // Hold tx alive for 50ms to observe that the future has
            // NOT resolved yet just because of the send.
            tokio::time::sleep(Duration::from_millis(50)).await;
            // tx drops here
        });

        // Should NOT resolve in 30ms (well before send_then_hold drops tx
        // at ~60ms). If it does, that means the future returned early
        // on first Some — the old buggy semantic.
        let early = tokio::time::timeout(Duration::from_millis(30), fut).await;
        assert!(
            early.is_err(),
            "future resolved before sender dropped — wrong semantic"
        );
        let _ = send_then_hold.await;
    }
}
