//! `PreconfPayloadJobGenerator` — implements reth's [`PayloadJobGenerator`]
//! trait, spawning one [`PreconfPayloadBuilder::build_payload`] async
//! task per payload job on the ambient tokio runtime.
//!
//! ## Lifecycle (per slot)
//!
//! 1. CL hits `engine_forkchoiceUpdatedVx`. The payload service calls
//!    [`Self::new_payload_job`] with the new
//!    [`BuildNewPayload<Attrs>`].
//! 2. The generator looks up the parent header via
//!    [`BlockReaderIdExt::sealed_header_by_hash`] and assembles a
//!    [`BuildArguments`] in the upstream shape (so we can reuse the
//!    fork's `build_payload` signature verbatim).
//! 3. A fresh [`JobCancel`] and a `watch::channel(None)` are created.
//!    A tokio task is spawned that drives
//!    [`PreconfPayloadBuilder::build_payload`] to completion, then
//!    sends the final `Option<OpBuiltPayload<N>>` into the watch
//!    sender. On error the sender is dropped (without sending).
//! 4. The [`PreconfPayloadJob`] returned to the payload service holds
//!    the watch receiver + cancel handle + join handle. Subsequent
//!    `best_payload()` / `resolve_kind()` calls read through those.
//!
//! ## What this generator does NOT do (yet)
//!
//! - **`on_new_state`**: no cached-reads pre-warming. Step 5 leaves
//!   the default no-op trait impl in place; the cached-reads
//!   optimisation lands alongside the cli wiring in Step 7 if needed.
//! - **`ensure_only_one_payload`**: base cancels existing payload
//!   jobs before spawning a new one (see `Base/.../generator.rs`).
//!   We don't — `last_payload` cancel-on-drop semantics aren't needed
//!   until production rollout, and skipping it keeps this step focused
//!   on the trait wiring. The [`PayloadJob`]'s own `Drop` impl handles
//!   spawned-task cleanup.
//! - **Deadlines**: no auto-cancel on slot deadline; the payload
//!   service drives cancel via `resolve_kind` instead.
//!
//! [`PayloadJobGenerator`]: reth_payload_builder::PayloadJobGenerator
//! [`PreconfPayloadBuilder::build_payload`]: crate::builder::payload_builder::PreconfPayloadBuilder::build_payload
//! [`BlockReaderIdExt::sealed_header_by_hash`]: reth_storage_api::BlockReaderIdExt::sealed_header_by_hash

use std::{marker::PhantomData, sync::Arc};

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
use tracing::warn;

use crate::builder::{
    cancel::JobCancel,
    payload_builder::PreconfPayloadBuilder,
    payload_job::PreconfPayloadJob,
};

/// `PayloadJobGenerator` impl that spawns the mantle preconf-aware
/// build loop on each new payload request.
///
/// **OP-stack specific**: hardcodes
/// [`OpPayloadAttrs`] (RPC variant) as the job's exposed payload
/// attributes and [`OpPayloadBuilderAttributes<N::SignedTx>`] (builder
/// variant) for internal block building. This mirrors upstream's
/// `OpPayloadBuilder::try_build` / `convert_build_args` split — see
/// `op-reth/crates/payload/src/builder.rs:344`. The generic `Attrs`
/// parameter was tried in earlier iterations (Step 7c) but couldn't
/// express the wrapper-unwrap step
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
    /// `fn() -> N` marker so the struct is `Send + Sync` without
    /// constraining `N` itself.
    _pd: PhantomData<fn() -> N>,
}

impl<Pool, Client, Evm, N> PreconfPayloadJobGenerator<Pool, Client, Evm, N> {
    /// Wrap a template builder so each `new_payload_job` call clones it.
    pub const fn new(builder: PreconfPayloadBuilder<Pool, Client, Evm>) -> Self {
        Self { builder, _pd: PhantomData }
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

impl<Pool, Client, Evm, N> PayloadJobGenerator
    for PreconfPayloadJobGenerator<Pool, Client, Evm, N>
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
    N::SignedTx: From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>>
        + TryFrom<TxEnvelope>,
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
        let builder_attrs = OpPayloadBuilderAttributes::<TxTy<N>>::from_rpc_attrs(
            parent_hash,
            id,
            rpc_attrs.0,
        )
        .map_err(PayloadBuilderError::other)?;

        let config = PayloadConfig::new(Arc::new(parent_header), builder_attrs, id);
        let args: BuildArguments<OpPayloadBuilderAttributes<TxTy<N>>, OpBuiltPayload<N>> =
            BuildArguments::new(
            // No cached-reads yet — Step 7 may wire this in via the
            // service builder's `on_new_state` cache.
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
        let cancel_for_build = cancel.clone();
        let (payload_tx, payload_rx) = watch::channel::<Option<OpBuiltPayload<N>>>(None);

        let builder_clone = self.builder.clone();

        // Why `spawn_blocking + current_thread runtime + block_on`
        // instead of plain `tokio::spawn`:
        //
        // Upstream's `OpPayloadBuilderCtx::block_builder(&mut state)`
        // returns `impl BlockBuilder + '_`, and the concrete type is
        // not `Send` (its State<DB> path holds `Box<dyn StateProvider
        // + Send>` which is not `Sync`, so `&State<DB>` is not Send).
        // Holding that builder across the select! `.await` makes the
        // whole build future non-`Send`, so `tokio::spawn(future)`
        // rejects it at the trait-bound level.
        //
        // `spawn_blocking` accepts a `FnOnce() -> R + Send + 'static`
        // closure (not a future), runs it on tokio's blocking-thread
        // pool, and returns a `JoinHandle<R>`. Inside that closure we
        // build a single-threaded tokio runtime (`new_current_thread`)
        // and `block_on` the !Send future on it. The future never
        // crosses thread boundaries (it runs to completion on the
        // blocking-pool thread), so Send is not needed.
        //
        // Cost: one OS thread from the blocking pool per active
        // payload job. The pool defaults to 512 threads; the worst
        // case (slot duration × concurrent payload requests) is well
        // under that. Production hardening (e.g. a bounded pool of
        // worker threads we own) is a follow-up if observed needed.
        let handle = tokio::task::spawn_blocking(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
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
            rt.block_on(async move {
                match builder_clone.build_payload(args, cancel_for_build).await {
                    Ok(payload) => {
                        // `send` only fails when the receiver has been
                        // dropped, which means the job was torn down
                        // before we finished. Nothing we can do — log
                        // and exit.
                        if payload_tx.send(Some(payload)).is_err() {
                            tracing::trace!(
                                target: "mantle::preconf::payload_job_generator",
                                "payload receiver dropped before build completed"
                            );
                        }
                    }
                    Err(err) => {
                        warn!(
                            target: "mantle::preconf::payload_job_generator",
                            ?err,
                            "preconf payload build failed"
                        );
                        // Drop `payload_tx` without sending → the watch
                        // receiver's `changed()` returns Err, which
                        // `ResolvePayloadFuture` surfaces as
                        // `MissingPayload`.
                        drop(payload_tx);
                    }
                }
            });
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
    //! coverage lands alongside the e2e tests in Step 9 (where reth's
    //! `MockEthProvider` or similar is plumbed up).
    //!
    //! Compile-time check: the constructor is callable with concrete
    //! types. Just instantiating `PreconfPayloadJobGenerator::new(...)`
    //! would require a real `PreconfPayloadBuilder` which itself needs
    //! Pool/Client/Evm — also heavy. We rely on `cargo check` /
    //! downstream cli build to catch type-plumbing regressions until
    //! Step 9.

    use super::*;
    use std::marker::PhantomData;

    // Compile-time witness that the generator can name its types.
    #[allow(dead_code)]
    fn _witness_name<Pool, Client, Evm, N>(
        _: &PreconfPayloadJobGenerator<Pool, Client, Evm, N>,
    ) {
        let _phantom: PhantomData<N> = PhantomData;
    }
}
