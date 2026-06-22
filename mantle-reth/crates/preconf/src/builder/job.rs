//! [`PreconfPayloadJob`] — a [`PayloadJob`] wrapper that delegates block
//! construction to an inner job (typically the OP payload-builder's)
//! while running the preconf event/responder pipeline on a side task.
//!
//! Lifecycle, per slot:
//!
//! 1. [`PreconfPayloadJobGenerator::new_payload_job`] constructs a new
//!    [`PreconfPayloadJob`] wrapping a fresh inner job.
//! 2. The constructor spawns a [`BuilderLoop`] on the ambient tokio
//!    runtime. The loop owns its own fifo subscription, applier, and
//!    tx tracker for the slot's duration.
//! 3. All [`PayloadJob`] trait methods forward to `inner`. The preconf
//!    loop surfaces its results through the fifo's responder channels
//!    (already plumbed to the RPC handler in [`crate::rpc`]), not
//!    through this job's interface.
//! 4. [`PreconfPayloadJob::resolve_kind`] flips the loop's cancel
//!    signal when the CL signals "give me the earliest payload" or
//!    the inner decides not to keep alive.
//! 5. On [`Drop`], the cancel signal is flipped unconditionally and
//!    the join handle is dropped; the loop exits on its next poll.
//!
//! The applier today is [`PromiseApplier`] — it returns always-success
//! synthetic receipts without running the EVM. Replacing it with an
//! EVM-backed applier is a follow-up integration step.
//!
//! [`PreconfPayloadJobGenerator::new_payload_job`]: crate::builder::generator::PreconfPayloadJobGenerator::new_payload_job
//! [`PreconfPayloadJobGenerator`]: crate::builder::generator::PreconfPayloadJobGenerator

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use reth_payload_builder::{KeepPayloadJobAlive, PayloadJob};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadKind;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::{
    PreconfConfig, PreconfTxSet,
    builder::{builder::BuilderLoop, builder::PromiseApplier, cancel::JobCancel},
};

/// A [`PayloadJob`] that delegates block construction to `Inner` while
/// running a side [`BuilderLoop`] for the preconf event/responder
/// pipeline.
///
/// `Inner` is the underlying job (e.g. the OP basic payload job) which
/// continues to drive the actual block construction. The preconf
/// extensions run **alongside** the inner job's polling, not replace
/// it: each [`PreconfPayloadJob`] spawns one builder-loop task that
/// owns the per-slot fifo subscription, applier, and tx tracker.
#[derive(Debug)]
pub struct PreconfPayloadJob<Inner> {
    /// Wrapped underlying payload job. All [`PayloadJob`] and
    /// [`Future`] calls forward straight to this field; the preconf
    /// loop runs in parallel and surfaces results through the fifo's
    /// responder channels, not through this job's interface.
    inner: Inner,

    /// Cancel signal shared with the spawned builder loop. Flipped on
    /// drop (graceful shutdown) and when [`PreconfPayloadJob::resolve_kind`]
    /// observes a `PayloadKind::Earliest` request or a
    /// `KeepPayloadJobAlive::No` decision.
    cancel: JobCancel,

    /// Join handle for the builder loop. Wrapped in `Option` so [`Drop`]
    /// can take ownership without leaking the handle. Always `Some`
    /// between construction and drop.
    loop_handle: Option<JoinHandle<()>>,
}

impl<Inner> PreconfPayloadJob<Inner> {
    /// Wrap an existing inner [`PayloadJob`] with the preconf-specific
    /// surface, spawning the builder loop on the ambient tokio runtime.
    ///
    /// The applier today is [`PromiseApplier`] — it synthesises always-
    /// success receipts without running the EVM. Replacing it with an
    /// EVM-backed applier is the next integration step (deferred so
    /// the rest of the structural wiring can land first).
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime context (the
    /// [`tokio::spawn`] call requires one). reth's payload service
    /// invokes [`PayloadJobGenerator::new_payload_job`] from inside its
    /// runtime, so this is always satisfied in production.
    ///
    /// [`PayloadJobGenerator::new_payload_job`]: reth_payload_builder::PayloadJobGenerator::new_payload_job
    pub fn new(inner: Inner, fifo: Arc<PreconfTxSet>, cfg: Arc<PreconfConfig>) -> Self {
        let cancel = JobCancel::new();
        // `block_height = 0` is a placeholder: the receipt's block_height
        // is a "predicted L2 block number" promised to the client. We
        // don't yet plumb the parent block number through to the job;
        // until that lands, clients should treat `block_height` as
        // informational only and use `eth_getTransactionReceipt` for
        // the authoritative slot.
        let loop_state =
            BuilderLoop::new(PromiseApplier, fifo.clone(), cfg.clone(), cancel.clone(), 0);
        let loop_handle = tokio::spawn(loop_state.run());
        Self { inner, cancel, loop_handle: Some(loop_handle) }
    }

    /// Read-only access to the cancel handle. Tests use this to verify
    /// `resolve_kind` flips the signal; the spawned builder loop keeps
    /// its own clone for `select!`.
    pub fn cancel_handle(&self) -> JobCancel {
        self.cancel.clone()
    }

    /// Whether [`PayloadKind`] / [`KeepPayloadJobAlive`] combination
    /// implies the inner builder loop should stop. Extracted so the
    /// decision is testable without spinning up a full [`PayloadJob`]
    /// stub.
    pub fn should_cancel(kind: PayloadKind, keep: KeepPayloadJobAlive) -> bool {
        matches!(kind, PayloadKind::Earliest) || matches!(keep, KeepPayloadJobAlive::No)
    }
}

// ─── Drop ──────────────────────────────────────────────────────────────────

impl<Inner> Drop for PreconfPayloadJob<Inner> {
    fn drop(&mut self) {
        // Signal the loop to exit. The loop's `select!` has a `biased`
        // cancel branch as its first option, so the wake propagates on
        // the next poll regardless of which arm it was waiting on.
        self.cancel.signal();
        // Drop the join handle without awaiting — the loop will exit
        // on its own thanks to the cancel signal. If for some reason
        // it hangs, the fifo's broadcast sender drop (when the last
        // `Arc<PreconfTxSet>` clone goes away) closes its `recv()`,
        // which is the loop's hard backstop.
        if let Some(h) = self.loop_handle.take() {
            debug!(target: "mantle::preconf::job", "PreconfPayloadJob dropped — signalled builder loop cancel");
            // Leak the JoinHandle deliberately: tokio's default behaviour
            // is to let the task continue running after the handle is
            // dropped, which is exactly what we want here (graceful
            // shutdown driven by the cancel signal, not abort).
            drop(h);
        }
    }
}

// ─── Future ────────────────────────────────────────────────────────────────

impl<Inner> Future for PreconfPayloadJob<Inner>
where
    // `Unpin` lets us project to `&mut Inner` without a `pin-project`
    // macro. OP / Eth payload jobs are Unpin because their state lives
    // behind spawned tasks, so this bound holds in practice.
    Inner: PayloadJob + Unpin,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The preconf builder loop runs on its own spawned task and
        // surfaces its results through the fifo's responder channels —
        // we don't poll its join handle here. The inner job's terminal
        // status (timeout, error, success) is the one the payload
        // service observes.
        Pin::new(&mut self.get_mut().inner).poll(cx)
    }
}

// ─── PayloadJob ────────────────────────────────────────────────────────────

impl<Inner> PayloadJob for PreconfPayloadJob<Inner>
where
    Inner: PayloadJob + Unpin,
{
    type PayloadAttributes = Inner::PayloadAttributes;
    type ResolvePayloadFuture = Inner::ResolvePayloadFuture;
    type BuiltPayload = Inner::BuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        // The inner job is authoritative for block construction; the
        // preconf loop's role is per-tx responder delivery, not block
        // assembly. (EVM-backed applier integration is a follow-up.)
        self.inner.best_payload()
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        self.inner.payload_attributes()
    }

    fn payload_timestamp(&self) -> Result<u64, PayloadBuilderError> {
        // Forward to inner rather than letting the trait's default impl
        // re-allocate full attributes just to read a timestamp — keeps
        // the perf footprint identical to the wrapped job.
        self.inner.payload_timestamp()
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        let (fut, keep) = self.inner.resolve_kind(kind);

        // When the CL signals "give me the earliest payload, don't bother
        // building further" or the inner job decides it won't be kept
        // alive, flip the cancel so the upcoming preconf builder loop
        // can stop spending budget on speculation. The returned `keep`
        // decision still belongs to the inner job.
        if Self::should_cancel(kind, keep) {
            self.cancel.signal();
        }

        (fut, keep)
    }
}

#[cfg(test)]
mod tests {
    //! Tests cover:
    //!
    //! - The static `should_cancel` decision matrix — exercised without
    //!   any [`PayloadJob`] scaffolding.
    //! - The spawned [`BuilderLoop`] lifecycle: construction spawns it,
    //!   it processes a real fifo event via [`PromiseApplier`], and
    //!   drop signals cancel.
    //!
    //! Inner `()` stands in for a [`PayloadJob`] in the lifecycle tests
    //! — `PreconfPayloadJob::new` does not require the trait bound, so
    //! the unit (`()`) sentinel suffices for testing the spawn side.

    use super::*;
    use std::time::Duration;

    use alloy_consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy_primitives::{Address, B256, Signature};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use crate::types::PushResult;

    #[test]
    fn should_cancel_on_earliest_regardless_of_keep() {
        assert!(PreconfPayloadJob::<()>::should_cancel(
            PayloadKind::Earliest,
            KeepPayloadJobAlive::Yes,
        ));
        assert!(PreconfPayloadJob::<()>::should_cancel(
            PayloadKind::Earliest,
            KeepPayloadJobAlive::No,
        ));
    }

    #[test]
    fn should_cancel_on_keep_no_regardless_of_kind() {
        assert!(PreconfPayloadJob::<()>::should_cancel(
            PayloadKind::WaitForPending,
            KeepPayloadJobAlive::No,
        ));
    }

    #[test]
    fn should_not_cancel_on_wait_keep_yes() {
        assert!(!PreconfPayloadJob::<()>::should_cancel(
            PayloadKind::WaitForPending,
            KeepPayloadJobAlive::Yes,
        ));
    }

    fn make_tx(hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce: 0, gas_limit: 21_000, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    #[tokio::test]
    async fn spawned_loop_delivers_promise_receipt_for_pushed_tx() {
        // Verifies the full structural wiring: constructing a job
        // spawns a loop, the loop subscribes to the fifo, a subsequent
        // push reaches it (via the has_pending safety net even if the
        // broadcast races subscription), and the responder receives a
        // synthetic success receipt.
        let fifo = Arc::new(PreconfTxSet::new(16));
        let cfg = Arc::new(PreconfConfig {
            sweep_interval: Duration::from_millis(5),
            ..PreconfConfig::default()
        });

        let tx = make_tx(0xab);
        let hash = *tx.tx_hash();
        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.attach_responder(hash, resp_tx).await.unwrap();

        // Job construction spawns the loop. Inner = () because we only
        // exercise the lifecycle here, not the delegating PayloadJob
        // surface.
        let job = PreconfPayloadJob::new((), fifo.clone(), cfg);

        let push = fifo.push_if_absent(tx, Address::ZERO).await;
        assert!(matches!(push, PushResult::Inserted));

        let receipt = timeout(Duration::from_millis(500), resp_rx)
            .await
            .expect("responder timed out — loop did not fire")
            .expect("oneshot closed")
            .expect("PromiseApplier never errs");
        assert_eq!(receipt.tx_hash, hash);
        assert!(receipt.status, "PromiseApplier returns success");

        // Drop the job — signals cancel, loop exits.
        drop(job);
    }

    #[tokio::test]
    async fn drop_signals_cancel_to_spawned_loop() {
        let fifo = Arc::new(PreconfTxSet::new(16));
        let cfg = Arc::new(PreconfConfig::default());

        let cancel = {
            let job = PreconfPayloadJob::new((), fifo, cfg);
            let c = job.cancel_handle();
            assert!(!c.is_cancelled(), "fresh job must not be cancelled");
            c
            // `job` dropped at end of scope.
        };
        assert!(cancel.is_cancelled(), "Drop impl must flip the cancel signal");
    }
}
