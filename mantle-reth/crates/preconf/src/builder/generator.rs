//! [`PreconfPayloadJobGenerator`] — a delegating
//! [`PayloadJobGenerator`] that wraps each produced job with
//! [`PreconfPayloadJob`].
//!
//! The generator owns the cross-job shared state — the commitment fifo
//! and the runtime config — and threads a clone of each into every job it
//! creates. The actual block-building remains the inner generator's
//! responsibility; the preconf wrapper exists to give the upcoming
//! builder loop a place to hook in without forking the OP payload-builder.
//!
//! For the skeleton, [`PayloadJobGenerator::new_payload_job`] just calls
//! through and wraps the result. [`PayloadJobGenerator::on_new_state`] is
//! forwarded as-is — the canonical-state listener is handled separately
//! in `canon_handler.rs` and does not need to also fire through here.

use std::sync::Arc;

use reth_chain_state::CanonStateNotification;
use reth_payload_builder::{BuildNewPayload, PayloadId, PayloadJob, PayloadJobGenerator};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_primitives_traits::NodePrimitives;

use crate::{
    PreconfConfig, PreconfTxSet,
    builder::{
        builder::{PreconfApplierFactory, default_applier_factory},
        job::PreconfPayloadJob,
    },
};

/// Wraps an inner [`PayloadJobGenerator`] so every produced job carries
/// the shared preconf fifo + config handles.
///
/// `Inner` is typically `BasicPayloadJobGenerator<OpPayloadBuilder<...>>`
/// — the standard OP block-builder generator. Any other generator that
/// satisfies the trait will work too, which keeps this layer free of
/// OP-specific type bounds.
#[derive(Clone)]
pub struct PreconfPayloadJobGenerator<Inner> {
    inner: Inner,
    fifo: Arc<PreconfTxSet>,
    cfg: Arc<PreconfConfig>,
    /// Applier factory consulted on every [`PayloadJobGenerator::new_payload_job`]
    /// call. Defaults to [`default_applier_factory`] which produces a
    /// [`PromiseApplier`](crate::builder::PromiseApplier) per slot;
    /// override via [`Self::with_applier_factory`].
    applier_factory: PreconfApplierFactory,
}

// Manual Debug — `PreconfApplierFactory` is `dyn Fn`, which has no
// Debug impl, but the rest of the struct is informative.
impl<Inner: std::fmt::Debug> std::fmt::Debug for PreconfPayloadJobGenerator<Inner> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfPayloadJobGenerator")
            .field("inner", &self.inner)
            .field("fifo", &self.fifo)
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl<Inner> PreconfPayloadJobGenerator<Inner> {
    /// Construct a generator bound to a shared `fifo` and `cfg` with
    /// the default applier factory ([`default_applier_factory`]). The
    /// service builder is the typical caller — it constructs the fifo
    /// once and hands the same `Arc` clones to the validator, the pool
    /// listener, the RPC handler, the canonical-state handler, and this
    /// generator.
    pub fn new(inner: Inner, fifo: Arc<PreconfTxSet>, cfg: Arc<PreconfConfig>) -> Self {
        Self { inner, fifo, cfg, applier_factory: default_applier_factory() }
    }

    /// Replace the applier factory. Future jobs produced by this
    /// generator will call `factory()` once each to obtain their loop's
    /// applier. Returns `self` for builder-style chaining.
    pub fn with_applier_factory(mut self, factory: PreconfApplierFactory) -> Self {
        self.applier_factory = factory;
        self
    }

    /// Borrow the inner generator. Useful for tests that need to assert
    /// against the wrapped impl.
    pub const fn inner(&self) -> &Inner {
        &self.inner
    }
}

impl<Inner> PayloadJobGenerator for PreconfPayloadJobGenerator<Inner>
where
    Inner: PayloadJobGenerator,
    // The job that comes out of the inner generator must be `Unpin` so
    // the wrapper can polymorphically poll it without a pin-projection
    // macro. Both the OP basic payload job and the Eth one satisfy this
    // because their state lives behind spawned tasks.
    Inner::Job: PayloadJob + Unpin,
{
    type Job = PreconfPayloadJob<Inner::Job>;

    fn new_payload_job(
        &self,
        input: BuildNewPayload<<Self::Job as PayloadJob>::PayloadAttributes>,
        id: PayloadId,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let inner_job = self.inner.new_payload_job(input, id)?;
        Ok(PreconfPayloadJob::with_applier_factory(
            inner_job,
            self.fifo.clone(),
            self.cfg.clone(),
            self.applier_factory.clone(),
        ))
    }

    fn on_new_state<N: NodePrimitives>(&mut self, new_state: CanonStateNotification<N>) {
        // Forward unchanged. The preconf canonical-state cleanup lives
        // in `canon_handler.rs` as a free-standing subscriber, so it
        // does not need to be re-driven here.
        self.inner.on_new_state(new_state);
    }
}

#[cfg(test)]
mod tests {
    //! The generator is pure delegation; the only non-trivial bits are
    //! the type plumbing (which is verified by the compiler) and the
    //! `Arc::clone` thread-through of `fifo` / `cfg` into each new job
    //! (verified by reading the implementation — there is no observable
    //! state to assert against without a full inner-generator stub,
    //! which would re-introduce the heavyweight scaffolding we
    //! deliberately removed from `job.rs::tests`).
    //!
    //! Construction smoke-test below confirms the workspace deps line up
    //! and that the inner accessor compiles end-to-end.

    use super::*;

    // Sentinel type stands in for `Inner`. Not a real `PayloadJobGenerator`
    // — that would need a full `Job: PayloadJob` impl. The struct's
    // generic-parameter bound `PayloadJobGenerator` is only required by
    // the trait impl, not by `new`/`inner` themselves, so this smoke
    // test still exercises the constructor.
    #[derive(Debug)]
    struct InnerSentinel;

    #[test]
    fn new_holds_inner_and_shared_handles() {
        let fifo = Arc::new(PreconfTxSet::new(64));
        let cfg = Arc::new(PreconfConfig::default());
        let generator = PreconfPayloadJobGenerator::new(InnerSentinel, fifo.clone(), cfg.clone());
        // `inner()` accessor compiles + returns the inner reference.
        let _ = generator.inner();
        // Arc counts: outer + inside generator = 2 each.
        assert_eq!(Arc::strong_count(&fifo), 2);
        assert_eq!(Arc::strong_count(&cfg), 2);
    }
}
