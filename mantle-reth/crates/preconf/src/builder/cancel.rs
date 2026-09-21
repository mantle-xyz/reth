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

use tokio::sync::watch;

/// Cancel handle for a single payload job. Constructed once per job by
/// [`PreconfPayloadJobGenerator`]; cloned into the inner builder loop.
///
/// [`PreconfPayloadJobGenerator`]: crate::builder::payload_job_generator::PreconfPayloadJobGenerator
#[derive(Debug, Clone)]
pub struct JobCancel {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl JobCancel {
    /// Create a new cancel handle in the un-cancelled state.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx: Arc::new(tx), rx }
    }

    /// Flip the cancel flag. Subsequent `is_cancelled()` calls return
    /// `true`, and any task awaiting `wait()` is woken. Idempotent — a
    /// second call is a no-op.
    pub fn signal(&self) {
        // `send` only fails when all receivers have been dropped, in
        // which case the cancel is meaningless anyway.
        let _ = self.tx.send(true);
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
}
