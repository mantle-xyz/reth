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

/// Why a build stopped.
///
/// The build loop reacts to both the same way — wrap up — but what becomes of
/// the work differs, and only the loop's tail can act on that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// `engine_getPayload` arrived: the block is on its way to the consensus
    /// layer, and what it executed is expected to land.
    Resolved,
    /// The job was thrown away — superseded by a later one for the same height,
    /// timed out by the stall watchdog, or dropped with its payload never
    /// asked for. Nothing it executed will reach the chain through it.
    Abandoned,
}

/// Cancel handle for a single payload job. Constructed once per job by
/// [`PreconfPayloadJobGenerator`]; cloned into the inner builder loop.
///
/// [`PreconfPayloadJobGenerator`]: crate::builder::payload_job_generator::PreconfPayloadJobGenerator
#[derive(Debug, Clone)]
pub struct JobCancel {
    tx: Arc<watch::Sender<Option<CancelReason>>>,
    rx: watch::Receiver<Option<CancelReason>>,
    /// Serializes cancelling against work that must not be cut in half — see
    /// [`Self::unless_cancelled`].
    gate: Arc<Mutex<()>>,
}

impl JobCancel {
    /// Create a new cancel handle in the un-cancelled state.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { tx: Arc::new(tx), rx, gate: Arc::new(Mutex::new(())) }
    }

    /// Stop the build because the consensus layer asked for the payload.
    ///
    /// The first reason recorded is the one that sticks, and a later call
    /// changes nothing; see [`Self::reason`].
    pub fn resolve(&self) {
        self.signal_with(CancelReason::Resolved);
    }

    /// Stop the build because the job is being thrown away.
    ///
    /// The first reason recorded is the one that sticks, and a later call
    /// changes nothing; see [`Self::reason`].
    pub fn abandon(&self) {
        self.signal_with(CancelReason::Abandoned);
    }

    /// Flip the cancel flag. Subsequent `is_cancelled()` calls return
    /// `true`, and any task awaiting `wait()` is woken.
    ///
    /// **The first reason is the one that sticks**, and a second call changes
    /// nothing. Both can fire for one job — `resolve_kind` asks for a payload
    /// and the job is dropped straight after — and what the build has to know
    /// is what stopped it, not what happened to the handle afterwards.
    ///
    /// Waits for any [`Self::unless_cancelled`] work already in flight, so a
    /// cancel cannot land in the middle of one. Whoever got past the check
    /// finishes first, then this takes effect.
    fn signal_with(&self, reason: CancelReason) {
        let _gate = self.gate.lock();
        if self.rx.borrow().is_some() {
            return;
        }
        // `send` only fails when all receivers have been dropped, in
        // which case the cancel is meaningless anyway.
        let _ = self.tx.send(Some(reason));
    }

    /// Why the build stopped, or `None` while it is still running.
    pub fn reason(&self) -> Option<CancelReason> {
        *self.rx.borrow()
    }

    /// Run `work` unless the job has been abandoned, holding off both endings
    /// until it returns. `None` means it had been abandoned already.
    ///
    /// Abandoned rather than cancelled: only one of the two endings makes what
    /// the build executed untrue. A resolved payload is on its way to the
    /// consensus layer carrying exactly that work.
    ///
    /// Check and `work` both stay inside the gate, because an ending landing
    /// between them is what this exists to stop. Lifting the check out, into a
    /// local acted on afterwards, compiles, reads the same, and gives up the
    /// guarantee.
    ///
    /// `work` must not `.await`: the gate is a blocking mutex, and holding it
    /// across a yield point deadlocks whoever is ending the job.
    pub fn unless_abandoned<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let _gate = self.gate.lock();
        (*self.rx.borrow() != Some(CancelReason::Abandoned)).then(work)
    }

    /// Fast non-async read of the cancel flag.
    pub fn is_cancelled(&self) -> bool {
        self.rx.borrow().is_some()
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
        if rx.borrow().is_some() {
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

    /// A resolved job is being sealed, so what it executed is going on chain
    /// and announcing it is telling the truth. Only an abandoned one has to be
    /// held back.
    #[tokio::test]
    async fn work_runs_for_a_live_job_and_for_a_resolved_one() {
        let live = JobCancel::new();
        assert_eq!(live.unless_abandoned(|| "ran"), Some("ran"));

        let asked_for = JobCancel::new();
        asked_for.resolve();
        assert_eq!(
            asked_for.unless_abandoned(|| "ran"),
            Some("ran"),
            "a resolved payload is on its way to the chain; its slices are not a lie",
        );
    }

    #[tokio::test]
    async fn work_does_not_run_for_an_abandoned_job() {
        let dropped = JobCancel::new();
        dropped.abandon();
        assert_eq!(dropped.unless_abandoned(|| "ran"), None);
    }

    /// The gate, not just the predicate: abandoning waits for work already
    /// inside. Without it a slice could go out after the build was thrown away,
    /// which is the single thing this guard exists to prevent.
    #[tokio::test]
    async fn abandoning_waits_for_work_already_inside_the_gate() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let cancel = Arc::new(JobCancel::new());
        let inside = Arc::new(AtomicBool::new(false));
        let left = Arc::new(AtomicBool::new(false));

        let held = {
            let (cancel, inside, left) = (cancel.clone(), inside.clone(), left.clone());
            std::thread::spawn(move || {
                cancel.unless_abandoned(|| {
                    inside.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(200));
                    left.store(true, Ordering::SeqCst);
                })
            })
        };

        while !inside.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        cancel.abandon();
        assert!(left.load(Ordering::SeqCst), "abandon returned while the work was still running");
        held.join().expect("the work completes");
    }

    /// The two ways a build ends are not the same thing, and only one of them
    /// means the block is on its way to the consensus layer.
    #[tokio::test]
    async fn a_resolved_job_is_distinguishable_from_an_abandoned_one() {
        let asked_for = JobCancel::new();
        asked_for.resolve();
        assert_eq!(asked_for.reason(), Some(CancelReason::Resolved));

        let dropped = JobCancel::new();
        dropped.abandon();
        assert_eq!(dropped.reason(), Some(CancelReason::Abandoned));
    }

    #[tokio::test]
    async fn a_live_job_has_no_reason_yet() {
        assert_eq!(JobCancel::new().reason(), None);
    }

    /// Whichever came first is what the build sees. A job the generator
    /// superseded and the consensus layer then asked for is still a job whose
    /// work was thrown away, and the reverse — asked for, then dropped — is a
    /// block already on its way.
    #[tokio::test]
    async fn the_first_reason_is_the_one_that_sticks() {
        let c = JobCancel::new();
        c.resolve();
        c.abandon();
        assert_eq!(c.reason(), Some(CancelReason::Resolved));

        let c = JobCancel::new();
        c.abandon();
        c.resolve();
        assert_eq!(c.reason(), Some(CancelReason::Abandoned));
    }

    #[tokio::test]
    async fn new_is_not_cancelled() {
        let c = JobCancel::new();
        assert!(!c.is_cancelled());
    }

    #[tokio::test]
    async fn signal_flips_flag() {
        let c = JobCancel::new();
        c.abandon();
        assert!(c.is_cancelled());
    }

    #[tokio::test]
    async fn signal_is_idempotent() {
        let c = JobCancel::new();
        c.abandon();
        c.abandon();
        assert!(c.is_cancelled());
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_already_cancelled() {
        let c = JobCancel::new();
        c.abandon();
        // Should not block — wrap in a tight timeout to prove that.
        timeout(Duration::from_millis(50), c.wait()).await.expect("wait should be instant");
    }

    #[tokio::test]
    async fn wait_returns_when_signal_fires() {
        let c = JobCancel::new();
        let c2 = c.clone();
        let waiter = tokio::spawn(async move { c2.wait().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        c.abandon();
        timeout(Duration::from_millis(100), waiter)
            .await
            .expect("wait should resolve after signal")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn cancel_propagates_across_clones() {
        let a = JobCancel::new();
        let b = a.clone();
        a.abandon();
        assert!(b.is_cancelled(), "clone should observe parent's signal");
    }

    #[tokio::test]
    async fn cancel_from_clone_propagates_back() {
        let a = JobCancel::new();
        let b = a.clone();
        b.abandon();
        assert!(a.is_cancelled(), "parent should observe clone's signal");
    }
}
