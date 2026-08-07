//! `PreconfPayloadJobGenerator` — implements reth's [`PayloadJobGenerator`]
//! trait, spawning one [`PreconfPayloadBuilder::build_payload`] async
//! task per payload job on the ambient tokio runtime.
//!
//! ## Lifecycle (per slot)
//!
//! 1. CL hits `engine_forkchoiceUpdatedVx`. The payload service calls [`Self::new_payload_job`]
//!    with the new [`BuildNewPayload<Attrs>`].
//! 2. The generator looks up the parent header via [`BlockReaderIdExt::sealed_header_by_hash`] and
//!    assembles a [`BuildArguments`] in the upstream shape (so we can reuse the fork's
//!    `build_payload` signature verbatim).
//! 3. A fresh [`JobCancel`] and a `watch::channel(None)` are created. A tokio task is spawned that
//!    drives [`PreconfPayloadBuilder::build_payload`] to completion, then sends the final
//!    `Option<OpBuiltPayload<N>>` into the watch sender. On error the sender is dropped (without
//!    sending). A panic inside the build is caught (`catch_unwind`), logged at `error!`, counted
//!    via `preconf.build.panic_total`, and then degrades the same way (sender dropped →
//!    `MissingPayload`) rather than being silently swallowed by the never-awaited join handle.
//! 4. The [`PreconfPayloadJob`] returned to the payload service holds the watch receiver + cancel
//!    handle + join handle. Subsequent `best_payload()` / `resolve_kind()` calls read through
//!    those.
//!
//! ## `ensure_only_one_payload`
//!
//! [`Self::new_payload_job`] signal-cancels the previously spawned build, keeping **at most one
//! live**. Otherwise a build that is never resolved (`getPayload`) nor evicted (reorg `Drop`) would
//! linger forever — still subscribed to the shared [`PreconfTxSet`] broadcast — and could apply
//! preconf txs into a block that never commits, stealing them from the job that will. It is a no-op
//! in steady state (the previous job was already cancelled by its own `resolve_kind`); it only
//! matters for abandoned / superseded jobs. Upstream [`BasicPayloadJobGenerator`] instead bounds
//! job lifetime with a per-job deadline, which we avoid — it would cut a slow build short and break
//! the preconf must-land SLA.
//!
//! ## What this generator does NOT do (yet)
//!
//! - **`on_new_state`**: no cached-reads pre-warming. Default no-op trait impl in place; the
//!   cached-reads optimisation is a follow-up if pool-side cache misses dominate slot latency.
//!
//! [`PreconfTxSet`]: crate::preconf_tx_set::PreconfTxSet
//! [`BasicPayloadJobGenerator`]: reth_basic_payload_builder::BasicPayloadJobGenerator
//!
//! [`PayloadJobGenerator`]: reth_payload_builder::PayloadJobGenerator
//! [`PreconfPayloadBuilder::build_payload`]: crate::builder::payload_builder::PreconfPayloadBuilder::build_payload
//! [`BlockReaderIdExt::sealed_header_by_hash`]: reth_storage_api::BlockReaderIdExt::sealed_header_by_hash

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use alloy_consensus::TxEnvelope;
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth_optimism_evm::ConfigurePostExecEvm;
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_payload_builder::{
    OpPayloadAttrs, OpPayloadBuilderAttributes, OpPayloadPrimitives,
};
use reth_payload_builder::{BuildNewPayload, PayloadId, PayloadJobGenerator};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::BuildNextEnv;
use reth_primitives_traits::{HeaderTy, TxTy};
use reth_revm::cancelled::CancelOnDrop;
use reth_storage_api::BlockReaderIdExt;
use tokio::sync::watch;
use tracing::{error, warn};

use crate::builder::{
    cancel::JobCancel, payload_builder::PreconfPayloadBuilder, payload_job::PreconfPayloadJob,
};

/// `PayloadJobGenerator` impl that spawns the mantle preconf-aware
/// build loop on each new payload request.
///
/// **OP-stack specific**: hardcodes
/// [`OpPayloadAttrs`] (RPC variant) as the job's exposed payload
/// attributes and [`OpPayloadBuilderAttributes<N::SignedTx>`] (builder
/// variant) for internal block building. This mirrors upstream's
/// `OpPayloadBuilder::try_build` / `convert_build_args` split — see
/// `op-reth/crates/payload/src/builder.rs:344`. A generic `Attrs`
/// parameter cannot express the wrapper-unwrap step
/// (`OpPayloadAttrs.0 → OpPayloadAttributes → from_rpc_attrs`)
/// through the trait system cleanly; going OP-specific is the path
/// upstream itself takes.
///
/// `N` is bound on the type so the associated `Job` type can name
/// `OpBuiltPayload<N>` concretely. For mantle's OP-stack target, pick
/// `N = OpPrimitives`.
pub struct PreconfPayloadJobGenerator<Pool, Client, Evm, N> {
    /// Template builder — cloned per job. Carries the shared
    /// `Arc<PreconfConfig>` / `Arc<PreconfTxSet>` and the OP builder
    /// config (DA / gas / SDM-enable).
    builder: PreconfPayloadBuilder<Pool, Client, Evm>,
    /// `ensure_only_one_payload`: cancel handle of the most-recently
    /// spawned build. Signalled when the next job is created so at most
    /// one build task is ever live — see [`Self::new_payload_job`].
    last_cancel: Arc<Mutex<Option<JobCancel>>>,
    /// `fn() -> N` marker so the struct is `Send + Sync` without
    /// constraining `N` itself.
    _pd: PhantomData<fn() -> N>,
}

impl<Pool, Client, Evm, N> PreconfPayloadJobGenerator<Pool, Client, Evm, N> {
    /// Wrap a template builder so each `new_payload_job` call clones it.
    // Not `const` — `Arc::new`/`Mutex::new` are not const-constructible.
    pub fn new(builder: PreconfPayloadBuilder<Pool, Client, Evm>) -> Self {
        Self { builder, last_cancel: Arc::new(Mutex::new(None)), _pd: PhantomData }
    }

    /// Borrow the inner template builder. Useful in tests / assertions.
    pub const fn builder(&self) -> &PreconfPayloadBuilder<Pool, Client, Evm> {
        &self.builder
    }
}

impl<Pool, Client, Evm, N> std::fmt::Debug for PreconfPayloadJobGenerator<Pool, Client, Evm, N>
where
    PreconfPayloadBuilder<Pool, Client, Evm>: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfPayloadJobGenerator")
            .field("builder", &self.builder)
            .finish_non_exhaustive()
    }
}

impl<Pool, Client, Evm, N> PayloadJobGenerator for PreconfPayloadJobGenerator<Pool, Client, Evm, N>
where
    // Builder must be Clone + Send + 'static so we can move a clone
    // into the spawned tokio task.
    PreconfPayloadBuilder<Pool, Client, Evm>: Clone + Send + 'static,
    // Pool / Client / Evm constraints needed by build_payload's where
    // clause. Repeated here because the trait impl is a fresh
    // generics scope.
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_optimism_txpool::OpPooledTx<Consensus = N::SignedTx>,
        > + Clone
        + Send
        + 'static,
    Client: reth_storage_api::StateProviderFactory
        + reth_chainspec::ChainSpecProvider<ChainSpec: reth_optimism_forks::OpHardforks>
        + BlockReaderIdExt<Header = HeaderTy<N>>
        + Clone
        + Send
        + 'static,
    <Client as reth_chainspec::ChainSpecProvider>::ChainSpec:
        reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
    Evm: ConfigurePostExecEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<
                OpPayloadBuilderAttributes<TxTy<N>>,
                HeaderTy<N>,
                <Client as reth_chainspec::ChainSpecProvider>::ChainSpec,
            >,
        > + Clone
        + Send
        + 'static,
    N: OpPayloadPrimitives,
    N::SignedTx:
        From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>> + TryFrom<TxEnvelope>,
{
    type Job = PreconfPayloadJob<OpPayloadAttrs, OpBuiltPayload<N>>;

    fn new_payload_job(
        &self,
        input: BuildNewPayload<<Self::Job as reth_payload_builder::PayloadJob>::PayloadAttributes>,
        id: PayloadId,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let BuildNewPayload { attributes: rpc_attrs, parent_hash, cache, trie_handle } = input;

        // Look up parent header — mirrors Base's pattern. Genesis edge
        // case (parent_hash zero) intentionally surfaces as
        // MissingParentBlock; the payload service handles that via the
        // empty-payload fallback path.
        let parent_header = self
            .builder
            .client()
            .sealed_header_by_hash(parent_hash)
            .map_err(PayloadBuilderError::from)?
            .ok_or(PayloadBuilderError::MissingParentBlock(parent_hash))?;

        // The job exposes the RPC-variant attributes (`OpPayloadAttrs`,
        // the newtype wrapper the engine sends). Internally
        // `build_payload` consumes the builder-variant
        // (`OpPayloadBuilderAttributes<N::SignedTx>`), so we unwrap
        // `rpc_attrs.0 → OpPayloadAttributes` and feed it through
        // `from_rpc_attrs` — same shape as upstream's private
        // `convert_build_args` (op-reth payload/src/builder.rs:344).
        let attributes_for_job = rpc_attrs.clone();
        let builder_attrs =
            OpPayloadBuilderAttributes::<TxTy<N>>::from_rpc_attrs(parent_hash, id, rpc_attrs.0)
                .map_err(PayloadBuilderError::other)?;

        let config = PayloadConfig::new(Arc::new(parent_header), builder_attrs, id);
        let args: BuildArguments<OpPayloadBuilderAttributes<TxTy<N>>, OpBuiltPayload<N>> =
            BuildArguments::new(
                // No cached-reads yet — a follow-up may wire this in via
                // `on_new_state` if pool-side cache misses become
                // significant.
                Default::default(),
                cache,
                trie_handle,
                config,
                // The CancelOnDrop in BuildArguments is consumed by the
                // upstream OpPayloadBuilderCtx. Our async cancel signal is
                // separate (see build_payload's body) — this fresh handle
                // is never flipped from the outside.
                CancelOnDrop::default(),
                None,
            );

        // Wire up the (cancel, payload-result) channels shared with
        // the spawned build task.
        let cancel = JobCancel::new();

        // `ensure_only_one_payload`: cancel the previously spawned build so at
        // most one is ever live (rationale in the module docs). Idempotent in
        // steady state — the previous job was already cancelled by `resolve_kind`.
        if let Some(prev) = self
            .last_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(cancel.clone())
        {
            prev.signal();
        }

        let cancel_for_build = cancel.clone();
        let (payload_tx, payload_rx) = watch::channel::<Option<OpBuiltPayload<N>>>(None);

        let builder_clone = self.builder.clone();

        // The build future is `!Send` (upstream's block builder holds a
        // non-`Sync` `State<DB>` across the select! `.await`), so
        // `tokio::spawn` rejects it. Instead run it to completion on a local
        // `current_thread` runtime inside `spawn_blocking`, where it never
        // crosses threads and `Send` is not required.
        //
        // Cost: one blocking-pool thread per active job (pool default 512, far
        // above our concurrent-jobs worst case); a bounded owned pool is a
        // follow-up if ever needed.
        let handle = tokio::task::spawn_blocking(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(err) => {
                    warn!(
                        target: "mantle::preconf::payload_job_generator",
                        ?err,
                        "failed to construct current_thread runtime for payload build"
                    );
                    // Drop `payload_tx` without sending → resolve future
                    // surfaces `MissingPayload`.
                    drop(payload_tx);
                    return;
                }
            };
            // The join handle is never awaited (`PreconfPayloadJob::_join_handle`
            // is RAII-only), so a `build_payload` panic would be swallowed
            // silently; catch it here to log + count + degrade to `MissingPayload`.
            let build_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                rt.block_on(builder_clone.build_payload(args, cancel_for_build))
            }));

            match build_result {
                Ok(Ok(payload)) => {
                    // `send` only fails when the receiver has been dropped,
                    // which means the job was torn down before we finished.
                    // Nothing we can do — log and exit.
                    if payload_tx.send(Some(payload)).is_err() {
                        tracing::trace!(
                            target: "mantle::preconf::payload_job_generator",
                            "payload receiver dropped before build completed"
                        );
                    }
                }
                Ok(Err(err)) => {
                    warn!(
                        target: "mantle::preconf::payload_job_generator",
                        ?err,
                        %id,
                        "preconf payload build failed"
                    );
                    // Drop `payload_tx` without sending → the watch
                    // receiver's `changed()` returns Err, which
                    // `ResolvePayloadFuture` surfaces as `MissingPayload`.
                    drop(payload_tx);
                }
                Err(panic) => {
                    // Panic payloads are `&str` (from `expect`) or `String`.
                    let panic_msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    metrics::counter!("preconf.build.panic_total").increment(1);
                    error!(
                        target: "mantle::preconf::payload_job_generator",
                        %id,
                        panic = %panic_msg,
                        "preconf payload build panicked; degrading to MissingPayload"
                    );
                    // Drop the sender → `MissingPayload`, same as the error arm.
                    drop(payload_tx);
                }
            }
        });

        Ok(PreconfPayloadJob::new(attributes_for_job, payload_rx, cancel, handle))
    }

    // `on_new_state`: keep the default no-op impl. Cached-reads
    // pre-warming via canonical-state notifications is deferred to a
    // follow-up step (Base does this; mantle can copy if/when we
    // observe pool-side cache misses dominating slot latency).
}

#[cfg(test)]
mod tests {
    //! Generator-level tests require a full reth provider stack
    //! (`StateProviderFactory` + `ChainSpecProvider` + `BlockReaderIdExt`
    //! impls), which is too heavy for a unit-test mod. Integration
    //! coverage is deferred to the e2e test suite (where reth's
    //! `MockEthProvider` or similar is plumbed up).
    //!
    //! Compile-time check: the constructor is callable with concrete
    //! types. Just instantiating `PreconfPayloadJobGenerator::new(...)`
    //! would require a real `PreconfPayloadBuilder` which itself needs
    //! Pool/Client/Evm — also heavy. We rely on `cargo check` /
    //! downstream cli build to catch type-plumbing regressions until
    //! the e2e suite lands.

    use super::*;
    use std::marker::PhantomData;

    // Compile-time witness that the generator can name its types.
    #[allow(dead_code)]
    fn _witness_name<Pool, Client, Evm, N>(_: &PreconfPayloadJobGenerator<Pool, Client, Evm, N>) {
        let _phantom: PhantomData<N> = PhantomData;
    }
}
