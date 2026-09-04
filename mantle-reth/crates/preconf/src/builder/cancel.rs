//! Cancellation signal for the preconf payload job's inner builder loop.
//!
//! The payload service may drop a job mid-build when the CL picks a
//! different fork, or it may keep it alive past `engine_getPayload` while
//! still letting it converge to a better block. In either case the inner
//! builder loop must be able to react cooperatively — abort `apply` calls,
//! release the in-flight EVM state, and stop polling the fifo / sweep
//! sources.
//!
//! Implementation: a `watch::Sender<bool>` shared between the outer job
//! (writer) and the builder loop (reader). `signal()` flips the flag to
//! `true`; `is_cancelled()` is a fast non-async check the builder loop
//! can call between iterations; `wait()` is the async path used inside
//! `tokio::select!` so the loop wakes immediately on cancel instead of
//! waiting for the next sweep tick.
//!
//! The signal is **one-shot** — once flipped, callers expect the loop to
//! exit shortly after. There is no "uncancel".

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::watch;

/// Cancel handle for a single payload job. Constructed once per job by
/// [`PreconfPayloadJobGenerator`]; cloned into the inner builder loop.
///
/// [`PreconfPayloadJobGenerator`]: crate::builder::payload_job_generator::PreconfPayloadJobGenerator
#[derive(Debug, Clone)]
pub struct JobCancel {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
    /// Serializes cancelling against work that must not be cut in half — see
    /// [`Self::unless_cancelled`].
    gate: Arc<Mutex<()>>,
}

impl JobCancel {
    /// Create a new cancel handle in the un-cancelled state.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx: Arc::new(tx), rx, gate: Arc::new(Mutex::new(())) }
    }

    /// Flip the cancel flag. Subsequent `is_cancelled()` calls return
    /// `true`, and any task awaiting `wait()` is woken. Idempotent — a
    /// second call is a no-op.
    ///
    /// Waits for any [`Self::unless_cancelled`] work already in flight, so a
    /// cancel cannot land in the middle of one. Whoever got past the check
    /// finishes first, then this takes effect.
    pub fn signal(&self) {
        let _gate = self.gate.lock();
        // `send` only fails when all receivers have been dropped, in
        // which case the cancel is meaningless anyway.
        let _ = self.tx.send(true);
    }

    /// Run `work` unless the job has already been cancelled, holding off
    /// cancellation until it returns. `None` means it was already cancelled.
    ///
    /// Reading the flag and acting on it are two steps, and a cancel landing
    /// between them is the case the check exists to stop — so both happen
    /// under one gate. Concretely, for slice publishing: a slice either goes
    /// out before the payload is resolved, or not at all. Never after.
    ///
    /// `work` must not block for long and must not `.await`: cancellation is
    /// on hold for its duration.
    ///
    /// Both the check and `work` have to stay inside the gate. Lifting the
    /// check out — reading the flag into a local and acting on it afterwards —
    /// compiles, reads the same, and silently gives up the guarantee.
    pub fn unless_cancelled<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let _gate = self.gate.lock();
        (!*self.rx.borrow()).then(work)
    }

    /// Fast non-async read of the cancel flag.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Async wait until the cancel flag flips to `true`. Returns
    /// immediately if already cancelled.
    ///
    /// Designed to be awaited inside `tokio::select!` alongside the
    /// builder loop's fifo / sweep / resolve branches.
    pub async fn wait(&self) {
        // Clone the receiver locally so we can call `changed()` (which
        // takes `&mut self`) without exposing `&mut self` on `JobCancel`.
        let mut rx = self.rx.clone();
        // Already cancelled? Return immediately.
        if *rx.borrow() {
            return;
        }
        // `changed()` returns Err only when the sender is dropped — by
        // that point the job has been torn down and we should exit
        // anyway, so treat sender-drop as cancel.
        let _ = rx.changed().await;
    }
}

impl Default for JobCancel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn new_is_not_cancelled() {
        let c = JobCancel::new();
        assert!(!c.is_cancelled());
    }

    #[tokio::test]
    async fn signal_flips_flag() {
        let c = JobCancel::new();
        c.signal();
        assert!(c.is_cancelled());
    }

    #[tokio::test]
    async fn signal_is_idempotent() {
        let c = JobCancel::new();
        c.signal();
        c.signal();
        assert!(c.is_cancelled());
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_already_cancelled() {
        let c = JobCancel::new();
        c.signal();
        // Should not block — wrap in a tight timeout to prove that.
        timeout(Duration::from_millis(50), c.wait()).await.expect("wait should be instant");
    }

    #[tokio::test]
    async fn wait_returns_when_signal_fires() {
        let c = JobCancel::new();
        let c2 = c.clone();
        let waiter = tokio::spawn(async move { c2.wait().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        c.signal();
        timeout(Duration::from_millis(100), waiter)
            .await
            .expect("wait should resolve after signal")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn cancel_propagates_across_clones() {
        let a = JobCancel::new();
        let b = a.clone();
        a.signal();
        assert!(b.is_cancelled(), "clone should observe parent's signal");
    }

    #[tokio::test]
    async fn cancel_from_clone_propagates_back() {
        let a = JobCancel::new();
        let b = a.clone();
        b.signal();
        assert!(a.is_cancelled(), "parent should observe clone's signal");
    }

    // ============ unless_cancelled ============
    //
    // Checking the flag and then acting on it are two steps, and a cancel
    // landing between them is exactly the case the check exists to stop.
    // These pin that the pair is indivisible.

    #[tokio::test]
    async fn unless_cancelled_runs_the_work_when_live() {
        let c = JobCancel::new();

        assert_eq!(c.unless_cancelled(|| 7), Some(7));
    }

    #[tokio::test]
    async fn unless_cancelled_skips_the_work_once_cancelled() {
        let c = JobCancel::new();
        c.signal();

        let mut ran = false;
        let outcome = c.unless_cancelled(|| ran = true);

        assert!(outcome.is_none());
        assert!(!ran, "the work must not run after cancellation");
    }

    /// The point of the gate: work that got past the check finishes before a
    /// concurrent cancel takes effect. Without it, `signal` could flip the
    /// flag while the work is half done — which for slice publishing means a
    /// slice going out after the payload was already resolved.
    ///
    /// The observable is what `signal` can see the moment it returns: it
    /// returns only once it holds the gate, so with the gate the work must
    /// already be finished by then. Asserting after joining the worker would
    /// prove nothing — the work is finished by then either way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_waits_for_work_already_past_the_check() {
        use std::sync::{
            Arc as StdArc,
            atomic::{AtomicBool, Ordering},
        };

        let c = JobCancel::new();
        let finished = StdArc::new(AtomicBool::new(false));

        let worker = {
            let c = c.clone();
            let finished = StdArc::clone(&finished);
            tokio::task::spawn_blocking(move || {
                c.unless_cancelled(|| {
                    // Long enough that a concurrent `signal` is certain to
                    // arrive mid-work if the gate does not hold it off.
                    std::thread::sleep(Duration::from_millis(100));
                    finished.store(true, Ordering::SeqCst);
                })
            })
        };

        // Let the worker get past the check, then cancel and read the flag
        // the instant the cancel took effect.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let finished_when_cancelled = {
            let finished = StdArc::clone(&finished);
            tokio::task::spawn_blocking(move || {
                c.signal();
                finished.load(Ordering::SeqCst)
            })
            .await
            .expect("signaller task")
        };

        assert!(worker.await.expect("worker task").is_some(), "the worker was live at the gate");
        assert!(finished_when_cancelled, "the cancel took effect while the work was still running",);
    }

    /// And the other direction: a cancel that gets the gate first shuts the
    /// work out entirely rather than letting it start.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn work_arriving_after_a_cancel_never_starts() {
        let c = JobCancel::new();
        let signaller = {
            let c = c.clone();
            tokio::task::spawn_blocking(move || c.signal())
        };
        signaller.await.expect("signaller task");

        assert!(c.unless_cancelled(|| unreachable!("must not run")).is_none());
    }
}
