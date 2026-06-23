//! `PreconfPayloadBuilder` — mantle's preconf-aware OP payload builder.
//!
//! Forked from `reth_optimism_payload_builder::OpPayloadBuilder` and
//! `op-rbuilder`'s `StandardOpPayloadBuilder` (pinned via workspace
//! Cargo.toml `reth-optimism-payload-builder`). See
//! `docs/design/preconf-dev-plan.md` §P5f for the rationale (wrapper
//! mode cannot satisfy "RPC receipt == sealed receipt byte-for-byte"
//! because the applier has no channel into the inner OP builder's
//! `State<DB>`; only the fork shares state).
//!
//! Reuses the upstream [`OpPayloadBuilderCtx`] verbatim — mantle does
//! **not** add fields to ctx. The preconf-specific state
//! (`PreconfConfig`, `PreconfTxSet`, the preconf-tx mpsc receiver)
//! lives on this struct and is threaded into the build loop body.
//!
//! Stage A.1 lands across multiple commits:
//!
//! - **Step 2 (commit `08a4722c0`)**: struct skeleton + accessors
//! - **Step 3a (this commit)**: `build_payload` async signature with the
//!   upstream generic bounds; body is `unimplemented!()`
//! - **Step 3b**: fork `OpBuilder::build` body (deposits + sequencer txs
//!   + best txs + finalize) without preconf
//! - **Step 4**: `dispatch.rs` adds the preconf `select!` arm
//! - **Steps 5-9**: job / generator / cleanup / cli wiring / tests
//!
//! [`OpPayloadBuilderCtx`]: reth_optimism_payload_builder::builder::OpPayloadBuilderCtx

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Sealable, TxEnvelope, transaction::Recovered};
use alloy_evm::Evm;
use alloy_primitives::{Address, Sealed};
use op_alloy_consensus::{SDMGasEntry, TxPostExec, build_post_exec_tx};
use op_revm::constants::L1_BLOCK_CONTRACT;
use reth_basic_payload_builder::BuildArguments;
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError};
use reth_execution_types::BlockExecutionOutput;
use reth_optimism_evm::{ConfigurePostExecEvm, PostExecExecutorExt};
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_payload_builder::{
    OpAttributes, OpPayloadPrimitives,
    builder::OpPayloadBuilderCtx,
    config::OpBuilderConfig,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuildNextEnv, BuiltPayloadExecutedBlock};
use reth_payload_util::BestPayloadTransactions;
use reth_primitives_traits::{HeaderTy, SignedTransaction, TxTy};
use reth_revm::{
    State, cancelled::CancelOnDrop, context::Block as RevmBlockTrait,
    database::StateProviderDatabase,
};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    PreconfConfig, PreconfTxSet,
    apply::apply_preconf_tx,
    builder::{cancel::JobCancel, dispatch},
    types::PreconfError,
};

// Replicated from upstream private helper
// `reth_optimism_payload_builder::builder::try_include_post_exec_tx`.
// Replays the SDM refund entries as a synthetic post-exec transaction
// inside the in-flight block. Returns `Ok(true)` if a tx was included,
// `Ok(false)` if `entries` was empty. Failure is fatal for the payload
// build because a replaying verifier would expect to see this tx.
fn try_include_post_exec_tx<Tx, Err>(
    block_number: u64,
    entries: Vec<SDMGasEntry>,
    execute: impl FnOnce(Recovered<Tx>) -> Result<u64, Err>,
) -> Result<bool, PayloadBuilderError>
where
    Tx: From<Sealed<TxPostExec>>,
    Err: core::error::Error + Send + Sync + 'static,
{
    if entries.is_empty() {
        return Ok(false);
    }
    let sealed = build_post_exec_tx(block_number, entries).seal_slow();
    let recovered = Recovered::new_unchecked(Tx::from(sealed), Address::ZERO);
    execute(recovered).map_err(|err| {
        warn!(
            target: "mantle::preconf::payload_builder",
            %err,
            "post-exec tx execution failed, aborting payload"
        );
        PayloadBuilderError::evm(err)
    })?;
    debug!(
        target: "mantle::preconf::payload_builder",
        "post-exec tx included in block"
    );
    Ok(true)
}

/// Mantle's preconf-aware OP payload builder.
///
/// Construction is via [`PreconfPayloadBuilder::new`]. The driving
/// loop lives in `build_payload` (lands in subsequent steps), invoked
/// once per payload job by the matching
/// `PreconfPayloadJobGenerator`.
///
/// Type parameters:
/// - `Pool` — reth transaction pool (yields the best-txs iterator)
/// - `Client` — state provider factory + chain-spec provider
/// - `Evm` — [`reth_evm::ConfigureEvm`] impl (production: `OpEvmConfig`)
#[derive(Debug, Clone)]
pub struct PreconfPayloadBuilder<Pool, Client, Evm> {
    pool: Pool,
    client: Client,
    evm_config: Evm,
    /// Forwarded to [`OpPayloadBuilderCtx::builder_config`] on every
    /// [`Self::build_payload`] call. Carries OP-specific DA / gas-limit
    /// / SDM-enable settings.
    builder_config: OpBuilderConfig,
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
}

impl<Pool, Client, Evm> PreconfPayloadBuilder<Pool, Client, Evm> {
    /// Wrap a pool / client / EVM config with shared preconf handles
    /// and OP builder config.
    ///
    /// Cloning the resulting builder is cheap — `cfg` / `fifo` are
    /// `Arc`s, pool / client / `evm_config` are typically `Arc`-backed
    /// too, and [`OpBuilderConfig`] is a small `Clone` struct.
    pub const fn new(
        pool: Pool,
        client: Client,
        evm_config: Evm,
        builder_config: OpBuilderConfig,
        cfg: Arc<PreconfConfig>,
        fifo: Arc<PreconfTxSet>,
    ) -> Self {
        Self { pool, client, evm_config, builder_config, cfg, fifo }
    }

    /// Borrow the underlying transaction pool.
    pub const fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Borrow the underlying state-provider client.
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Borrow the EVM configuration.
    pub const fn evm_config(&self) -> &Evm {
        &self.evm_config
    }

    /// Borrow the OP builder config.
    pub const fn builder_config(&self) -> &OpBuilderConfig {
        &self.builder_config
    }

    /// Borrow the shared preconf config handle.
    pub const fn cfg(&self) -> &Arc<PreconfConfig> {
        &self.cfg
    }

    /// Borrow the shared preconf fifo handle.
    pub const fn fifo(&self) -> &Arc<PreconfTxSet> {
        &self.fifo
    }
}

// ─── build_payload (async) ──────────────────────────────────────────────────

// Forked from `reth_optimism_payload_builder::builder::OpBuilder::build`
// (path workspace dep, see Cargo.toml). The sync upstream is converted
// into `async fn` so future steps can interleave a preconf-tx select!
// arm without restructuring the signature. Generic bounds copied
// verbatim from upstream.
impl<Pool, Client, Evm> PreconfPayloadBuilder<Pool, Client, Evm> {
    /// Drive a single payload job to completion against the in-flight
    /// state, applying preconf-tx commitments mid-block via the inner
    /// `select!` loop (added in Step 4).
    ///
    /// Returns the final [`OpBuiltPayload<N>`] on success. Cancellation
    /// (via `cancel`) cuts the build short and seals whatever has been
    /// applied so far (Step 4 will wire the cancel-aware finalize).
    ///
    /// Generic parameters `N` (payload primitives) and `Attrs` (payload
    /// attributes) are bound on the method rather than the `impl`
    /// because each call may target different primitives (e.g.
    /// `OpPrimitives` vs. `MantlePrimitives`) without instantiating a
    /// new builder.
    ///
    /// Step 3b: body forked from upstream `OpBuilder::build` with the
    /// preconf select! loop omitted. The cached-reads optimization
    /// (sequencer mode) is also omitted in this step — see TODO below;
    /// it will land alongside the Step-5 [`PayloadJob`] integration that
    /// owns [`CachedReads`].
    ///
    /// [`PayloadJob`]: reth_payload_builder::PayloadJob
    /// [`CachedReads`]: reth_basic_payload_builder::CachedReads
    #[allow(clippy::unused_async)]
    pub async fn build_payload<N, Attrs>(
        self,
        args: BuildArguments<Attrs, OpBuiltPayload<N>>,
        cancel: JobCancel,
    ) -> Result<OpBuiltPayload<N>, PayloadBuilderError>
    where
        Pool: reth_transaction_pool::TransactionPool<
                Transaction: reth_optimism_txpool::OpPooledTx<Consensus = N::SignedTx>,
            >,
        Client: reth_storage_api::StateProviderFactory
            + reth_chainspec::ChainSpecProvider<ChainSpec: reth_optimism_forks::OpHardforks>,
        <Client as reth_chainspec::ChainSpecProvider>::ChainSpec:
            reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
        N: OpPayloadPrimitives,
        N::SignedTx: From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>>
            + TryFrom<TxEnvelope>,
        Evm: ConfigurePostExecEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<
                    Attrs,
                    HeaderTy<N>,
                    <Client as reth_chainspec::ChainSpecProvider>::ChainSpec,
                >,
            >,
        Attrs: OpAttributes<Transaction = TxTy<N>>,
    {
        // ── Destructure upstream BuildArguments ────────────────────────
        let BuildArguments {
            cached_reads: _cached_reads, // TODO(step 5): cached-reads sequencer path
            config,
            best_payload,
            ..
        } = args;

        // ── Construct upstream OpPayloadBuilderCtx ─────────────────────
        // All fields are `pub` upstream, so direct struct construction is the
        // public API. Mantle adds NO fields to ctx (see crate docs).
        let chain_spec = self.client.chain_spec();
        let parent_hash = config.parent_header.hash();
        // OpPayloadBuilderCtx wants a `CancelOnDrop` (reth_revm's sync
        // flag, polled by upstream's `execute_best_transactions` mid-loop).
        // Our job-level cancel is async (`JobCancel`, polled by the
        // select! arm below). We deliberately decouple them: a fresh
        // CancelOnDrop is given to ctx (never flipped by external
        // signals), and the JobCancel handles end-of-job cleanup via
        // the select! loop. Trade-off: upstream's best-tx scan won't
        // observe job cancellation, but Step 4 runs it to completion
        // before entering the loop, so the only effect is a slightly
        // longer worst-case shutdown.
        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            builder_config: self.builder_config.clone(),
            chain_spec,
            config,
            cancel: CancelOnDrop::default(),
            best_payload,
        };

        debug!(
            target: "mantle::preconf::payload_builder",
            id = %ctx.payload_id(),
            parent_header = ?parent_hash,
            parent_number = ctx.parent().number(),
            "building new preconf-aware payload"
        );

        // ── Fetch latest state ─────────────────────────────────────────
        //
        // We fetch the state provider TWICE: one ownership goes into
        // `StateProviderDatabase` (consumed → owned by the in-flight
        // `State<DB>` for the build loop), the other is held aside to
        // pass to `builder.finish(...)` for state-root computation at
        // seal time.
        //
        // Why owned instead of `&state_provider` like upstream's sync
        // path: holding `&Box<dyn StateProvider + Send>` across the
        // async select! `.await` points would make the build future
        // non-`Send` (because `&Box<T>: Send` requires `T: Sync`, and
        // `dyn StateProvider + Send` is not `Sync`). Owned form
        // sidesteps the borrow entirely. The double-fetch cost is one
        // extra `Arc::clone` worth of work — the underlying database
        // handle is `Arc`-backed in production.
        let state_provider_for_finish = self.client.state_by_block_hash(parent_hash)?;
        let state_provider_for_db = self.client.state_by_block_hash(parent_hash)?;
        let state_db = StateProviderDatabase::new(state_provider_for_db);
        let mut db = State::builder().with_database(state_db).with_bundle_update().build();

        // Load the L1 block contract into the database cache. If the L1
        // block contract is not pre-loaded the database will panic when
        // trying to fetch the DA footprint gas scalar. (Forked from
        // upstream `OpBuilder::build` line 430.)
        db.load_cache_account(L1_BLOCK_CONTRACT).map_err(BlockExecutionError::other)?;

        // ── Stage 1: pre-execution changes ─────────────────────────────
        let mut builder = ctx.block_builder(&mut db)?;
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(
                target: "mantle::preconf::payload_builder",
                %err,
                "failed to apply pre-execution changes"
            );
            PayloadBuilderError::Internal(err.into())
        })?;

        // ── Stage 2: sequencer transactions (deposits + system txs) ────
        let mut info = ctx.execute_sequencer_transactions(&mut builder)?;

        // ── Stage 3a: pool best-txs (synchronous, like upstream) ───────
        //
        // Step 4 keeps the upstream synchronous drain here (one pass
        // through `best_transactions` before entering the async loop).
        // Interleaving best-txs with preconf via per-tx async stepping
        // is deferred to a follow-up step — see dev-plan §P5g.
        //
        // `ctx.cancel` is the decoupled CancelOnDrop, so the `Some(())`
        // return is unreachable in practice; we ignore it for safety.
        if !ctx.attributes().no_tx_pool() {
            let best_txs_attrs = ctx.best_transaction_attributes(builder.evm_mut().block());
            let best_txs = BestPayloadTransactions::new(
                self.pool.best_transactions_with_attributes(best_txs_attrs),
            );
            let _ = ctx.execute_best_transactions(&mut info, &mut builder, best_txs)?;
        }

        // ── Stage 3b: preconf select! main loop ────────────────────────
        //
        // Drains preconf-tx commitments from the fifo's broadcast
        // channel until the job is cancelled. The apply closure
        // captures `&mut builder`, so each preconf-tx is executed
        // against the same in-flight `State<DB>` that produced the
        // sequencer-tx receipts above — which is exactly what makes
        // "RPC receipt == sealed receipt byte-for-byte" hold.
        //
        // Conversion path: `Arc<TxEnvelope> → TxEnvelope → N::SignedTx
        // (via TryFrom<TxEnvelope>) → Recovered<N::SignedTx> (via
        // SignedTransaction::try_into_recovered) → apply_preconf_tx`.
        // For OP-stack primitives `N::SignedTx == OpTxEnvelope`, so
        // the TryFrom impl from op-alloy-consensus' EthereumTxEnvelope
        // bridge satisfies the bound.
        //
        // `biased` so a torn-down job doesn't perform one more apply
        // between cancel and the next yield point.
        {
            let mut fifo_rx = self.fifo.subscribe();
            let predicted_height = ctx.parent().number() + 1;
            let mut loop_state = dispatch::LoopState::new(predicted_height);

            // Apply closure: real EVM execution against the in-flight
            // builder. `&mut builder` is borrowed exclusively here for
            // the duration of one preconf apply; the closure is FnMut
            // so the select! loop can invoke it on every iteration.
            let mut apply_fn =
                |tx: Arc<TxEnvelope>, hash, height| -> Result<_, PreconfError> {
                    let env = (*tx).clone();
                    let signed: N::SignedTx = env
                        .try_into()
                        .map_err(|_| {
                            PreconfError::BuilderRejected(
                                "TxEnvelope → N::SignedTx conversion failed".into(),
                            )
                        })?;
                    let recovered: Recovered<N::SignedTx> = signed
                        .try_into_recovered()
                        .map_err(|_| {
                            PreconfError::BuilderRejected(
                                "ec-recover failed for preconf tx".into(),
                            )
                        })?;
                    apply_preconf_tx(&mut builder, recovered, hash, height)
                };

            loop {
                tokio::select! {
                    biased;
                    () = cancel.wait() => break,
                    recv = fifo_rx.recv() => match recv {
                        Ok(hash) => {
                            dispatch::apply_one_preconf(
                                &self.fifo, &self.cfg, hash, &mut loop_state,
                                &mut apply_fn,
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            dispatch::reconcile_lagged(
                                &self.fifo, &self.cfg, &mut loop_state,
                                &mut apply_fn,
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            debug!(
                                target: "mantle::preconf::payload_builder",
                                "fifo broadcast closed; exiting preconf select!"
                            );
                            break;
                        }
                    }
                }
            }
        }

        // ── Stage 4: SDM post-exec refund tx ───────────────────────────
        if ctx.sdm_production_enabled() {
            let block_number = builder.evm_mut().block().number().saturating_to();
            let entries = builder.executor_mut().take_post_exec_entries();
            try_include_post_exec_tx::<N::SignedTx, _>(block_number, entries, |tx| {
                builder.execute_transaction(tx).map(|g| g.tx_gas_used())
            })?;
        }

        // ── Stage 5: finalize ─────────────────────────────────────────
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } =
            builder.finish(state_provider_for_finish, None)?;

        let sealed_block = Arc::new(block.sealed_block().clone());
        debug!(
            target: "mantle::preconf::payload_builder",
            id = %ctx.attributes().payload_id(),
            sealed_block_header = ?sealed_block.header(),
            "sealed preconf-aware built block"
        );

        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };

        let executed: BuiltPayloadExecutedBlock<N> = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            // Match upstream: keep unsorted; conversion to sorted happens
            // when needed downstream.
            hashed_state: either::Either::Left(Arc::new(hashed_state)),
            trie_updates: either::Either::Left(Arc::new(trie_updates)),
        };

        Ok(OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            Some(executed),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PreconfStatus;

    #[derive(Clone, Debug)]
    struct DummyPool;
    #[derive(Clone, Debug)]
    struct DummyClient;
    #[derive(Clone, Debug)]
    struct DummyEvm;

    #[test]
    fn constructor_threads_shared_handles() {
        let cfg = Arc::new(PreconfConfig::default());
        let fifo = Arc::new(PreconfTxSet::new(8));
        let builder_config = OpBuilderConfig::default();
        let builder = PreconfPayloadBuilder::new(
            DummyPool,
            DummyClient,
            DummyEvm,
            builder_config,
            cfg.clone(),
            fifo.clone(),
        );
        assert!(Arc::ptr_eq(builder.cfg(), &cfg));
        assert!(Arc::ptr_eq(builder.fifo(), &fifo));
        // Arc counts: outer + inside builder = 2 each.
        assert_eq!(Arc::strong_count(&cfg), 2);
        assert_eq!(Arc::strong_count(&fifo), 2);
        // Smoke: PreconfStatus is reachable from this module via crate root
        // re-exports, no need to also test accessor traversals here.
        let _ = PreconfStatus::Waiting;
    }
}
