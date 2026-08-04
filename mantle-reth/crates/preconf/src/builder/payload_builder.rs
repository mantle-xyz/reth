//! `PreconfPayloadBuilder` — mantle's preconf-aware OP payload builder.
//!
//! Forked from `reth_optimism_payload_builder::OpPayloadBuilder`. A
//! fork (rather than a wrapper) is required so preconf txs and
//! sequencer txs execute against the same in-flight `State<DB>`; that
//! is what makes the RPC-returned receipt byte-equal to the sealed
//! block's receipt.
//!
//! Reuses the upstream [`OpPayloadBuilderCtx`] verbatim — mantle does
//! **not** add fields to ctx. The preconf-specific state
//! (`PreconfConfig`, `PreconfTxSet`) lives on this struct and is
//! threaded into the build loop body.
//!
//! [`OpPayloadBuilderCtx`]: reth_optimism_payload_builder::builder::OpPayloadBuilderCtx

use std::sync::Arc;

use alloy_consensus::{
    BlockHeader, Sealable, Transaction, TxEnvelope, Typed2718, transaction::Recovered,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::Evm;
use alloy_primitives::{Address, Sealed, TxHash, TxKind, U256};
use op_alloy_consensus::{SDMGasEntry, TxPostExec, build_post_exec_tx};
use op_revm::{L1BlockInfo, constants::L1_BLOCK_CONTRACT};
use reth_basic_payload_builder::BuildArguments;
use reth_evm::execute::{
    BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockValidationError,
};
use reth_execution_types::BlockExecutionOutput;
use reth_optimism_evm::{ConfigurePostExecEvm, PostExecExecutorExt};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_payload_builder::{
    OpAttributes, OpPayloadPrimitives,
    builder::{ExecutionInfo, OpPayloadBuilderCtx},
    config::OpBuilderConfig,
};
use reth_optimism_primitives::OpTransaction;
use reth_optimism_txpool::{
    OpPooledTx,
    estimated_da_size::DataAvailabilitySized,
    interop::{MaybeInteropTransaction, is_valid_interop},
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::{BuildNextEnv, BuiltPayloadExecutedBlock};
use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
use reth_primitives_traits::{HeaderTy, SignedTransaction, TxTy};
use reth_revm::{
    State, cancelled::CancelOnDrop, context::Block as RevmBlockTrait,
    database::StateProviderDatabase,
};
use reth_transaction_pool::PoolTransaction;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    PreconfConfig, PreconfTxSet,
    apply::apply_preconf_tx,
    builder::{cancel::JobCancel, dispatch},
    types::{PreconfError, PreconfReceipt},
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
/// loop lives in [`Self::build_payload`], invoked once per payload
/// job by the matching `PreconfPayloadJobGenerator`.
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

/// Convert an `Arc<TxEnvelope>` to the pipeline's `N::SignedTx` and
/// recover its signer, then apply against the in-flight builder.
///
/// Shared helper used both by the carryover preamble and the `fifo_rx`
/// arm of the select! loop; the callers wrap this in a thin closure
/// that captures `&mut builder`, which is required so
/// `dispatch::apply_one_preconf`'s `apply_fn` callback signature
/// (`FnMut(Arc<TxEnvelope>, TxHash, u64) -> ...`) stays free of the
/// builder generic.
fn convert_and_apply_preconf<N, B>(
    builder: &mut B,
    tx: Arc<TxEnvelope>,
    hash: TxHash,
    height: u64,
) -> Result<PreconfReceipt, PreconfError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    let envelope = (*tx).clone();
    let signed: N::SignedTx = envelope.try_into().map_err(|_| {
        PreconfError::BuilderRejected("TxEnvelope → N::SignedTx conversion failed".into())
    })?;
    let recovered: Recovered<N::SignedTx> = signed
        .try_into_recovered()
        .map_err(|_| PreconfError::BuilderRejected("ec-recover failed for preconf tx".into()))?;
    apply_preconf_tx(builder, recovered, hash, height)
}

/// Per-block DA limits threaded into the preconf apply path. Snapshotted
/// once at `build_payload` start (constant across the build). Mirrors the
/// inputs the pool best-tx path feeds into
/// [`ExecutionInfo::is_tx_over_limits`], minus the gas-limit term (preconf
/// gas is gated separately in [`dispatch::apply_one_preconf`]).
#[derive(Debug, Clone, Copy)]
struct PreconfDaLimits {
    /// Max DA bytes for the whole block (`da_config.max_da_block_size`).
    block_da_limit: Option<u64>,
    /// Max DA bytes for a single tx (`da_config.max_da_tx_size`).
    tx_da_limit: Option<u64>,
    /// Post-Jovian footprint-gas scalar; `Some` only when Jovian is active.
    da_footprint_gas_scalar: Option<u16>,
    /// Block gas limit — the bound the footprint-gas variant compares against.
    block_gas_limit: u64,
}

/// Estimate a preconf tx's data-availability footprint in bytes.
///
/// Uses the same fastlz-based estimator (`op_alloy_flz::tx_estimated_size_fjord_bytes`
/// over the EIP-2718 encoding) that `OpPooledTransaction::estimated_da_size`
/// uses, so the preconf and pool paths produce byte-identical estimates and
/// share one consistent block DA budget.
fn estimated_tx_da_size(tx: &TxEnvelope) -> u64 {
    op_alloy_flz::tx_estimated_size_fjord_bytes(&tx.encoded_2718())
}

/// DA-footprint (H3) pre-check for a preconf tx. Replicates the DA portion
/// of [`ExecutionInfo::is_tx_over_limits`] (per-tx bytes, per-block bytes,
/// and the post-Jovian footprint-gas bound) but **omits** the block-gas
/// term — preconf gas is enforced by [`dispatch::apply_one_preconf`]'s own
/// budget gate + the block builder itself.
///
/// Applies to **all** sources (RPC and Replay): unlike the operator gas
/// budget, exceeding the DA limit would make the sealed block DA-invalid
/// (op-node would reject it), so the constraint is a consensus invariant,
/// not a soft budget. On over-limit the tx is left reclaimable (dispatch
/// maps the `Err` to fifo `Failed`; a later-slot resubmit with DA headroom
/// revives it).
fn preconf_da_check(tx_da: u64, da_used: u64, limits: PreconfDaLimits) -> Result<(), PreconfError> {
    if limits.tx_da_limit.is_some_and(|l| tx_da > l) {
        return Err(PreconfError::DaLimitExceeded {
            used: da_used,
            tx_da,
            limit: limits.tx_da_limit.expect("is_some_and matched"),
        });
    }
    let total = da_used.saturating_add(tx_da);
    if limits.block_da_limit.is_some_and(|l| total > l) {
        return Err(PreconfError::DaLimitExceeded {
            used: da_used,
            tx_da,
            limit: limits.block_da_limit.expect("is_some_and matched"),
        });
    }
    if let Some(scalar) = limits.da_footprint_gas_scalar {
        let footprint = total.saturating_mul(scalar as u64);
        if footprint > limits.block_gas_limit {
            return Err(PreconfError::DaLimitExceeded {
                used: da_used,
                tx_da,
                limit: limits.block_gas_limit,
            });
        }
    }
    Ok(())
}

/// Apply a preconf tx with the DA-footprint (H3) gate in front. On success
/// folds this tx's gas and DA footprint into `info` so the pool best-tx arm
/// (which reads `info.cumulative_gas_used` / `info.cumulative_da_bytes_used`
/// via [`ExecutionInfo::is_tx_over_limits`]) sees the true running block
/// totals — preconf and pool share one block DA + gas budget.
///
/// The DA gate runs **before** [`convert_and_apply_preconf`], so an
/// over-DA tx never touches the in-flight `State<DB>` (no cache pollution).
fn apply_preconf_with_da<N, B>(
    builder: &mut B,
    info: &mut ExecutionInfo,
    limits: PreconfDaLimits,
    tx: Arc<TxEnvelope>,
    hash: TxHash,
    height: u64,
) -> Result<PreconfReceipt, PreconfError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    let tx_da = estimated_tx_da_size(&tx);
    if let Err(e) = preconf_da_check(tx_da, info.cumulative_da_bytes_used, limits) {
        metrics::counter!("preconf.fifo.da_rejected_total").increment(1);
        return Err(e);
    }
    let receipt = convert_and_apply_preconf::<N, _>(builder, tx, hash, height)?;
    info.cumulative_da_bytes_used = info.cumulative_da_bytes_used.saturating_add(tx_da);
    info.cumulative_gas_used += receipt.gas_used;
    Ok(receipt)
}

/// Synchronous canon-forward — drops fifo entries whose nonce has
/// already been sealed as of the parent block. Called at
/// `build_payload` start, **before** [`replay_fifo_carryover`]; the
/// pair together replace the async `canon_handler::forward()` sweep
/// that used to race with new payload jobs (FCU for slot N+1 fires
/// before / during / after `canon_handler`'s notification handler for
/// slot N, so a new build could observe stale `Success` entries and
/// incorrectly replay them via `reset_success_to_waiting`).
///
/// Iterates the fifo once to collect the set of unique senders, queries
/// each sender's on-chain nonce from the parent-block state provider,
/// and calls [`PreconfTxSet::forward`] per sender. Idempotent — a
/// sender with no fifo entries or all entries at `nonce ≥ on_chain_nonce`
/// results in a no-op forward.
async fn sync_fifo_forward_to_head<S>(fifo: &PreconfTxSet, state_provider: &S)
where
    S: reth_storage_api::StateProvider + ?Sized,
{
    use std::collections::HashSet;
    let entries = fifo.entries().await;
    let senders: HashSet<Address> = entries.iter().map(|e| e.from).collect();
    drop(entries);
    for sender in senders {
        let on_chain_nonce = state_provider.account_nonce(&sender).ok().flatten().unwrap_or(0);
        fifo.forward(&sender, on_chain_nonce).await;
    }
}

/// Preamble that walks the fifo snapshot in insertion order and
/// applies every carryover entry to the new build:
///
/// - **`Waiting`** — journal-restored or dead-window RPC pushes whose broadcast never reached this
///   job's subscriber. Applied with the original `source` intact so genuinely stale `Rpc` entries
///   get timed out by the deadline gate.
/// - **`Success`** — stale in-flight from a discarded prior job. A canon'd entry would have been
///   removed by the immediately-preceding [`sync_fifo_forward_to_head`], so any Success reaching
///   this arm is an un-canon'd in-flight (client already got a receipt; must land).
///   `reset_success_to_waiting` promotes the source to `Replay` so gates bypass and the
///   previously-returned receipt is honored.
/// - **`Failed` / `Timeout` / `Canceled`** — skipped (terminal).
///
/// Applying directly here (rather than via the broadcast queue)
/// guarantees carryover lands ahead of any concurrently-queued fresh
/// RPC pushes. `apply_one_preconf`'s gate ① dedup prevents double-apply
/// if a carryover entry is also observed via broadcast later.
async fn replay_fifo_carryover<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    loop_state: &mut dispatch::LoopState,
    mut apply_fn: F,
) where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, PreconfError>,
{
    use crate::types::PreconfStatus;
    for view in fifo.entries().await {
        match view.status {
            PreconfStatus::Waiting => {
                dispatch::apply_one_preconf(fifo, cfg, view.hash, loop_state, &mut apply_fn).await;
            }
            PreconfStatus::Success => {
                if fifo.reset_success_to_waiting(&view.hash).await.is_ok() {
                    dispatch::apply_one_preconf(fifo, cfg, view.hash, loop_state, &mut apply_fn)
                        .await;
                }
            }
            PreconfStatus::Failed | PreconfStatus::Timeout | PreconfStatus::Canceled => {}
        }
    }
}

/// Derived schedule for the adaptive-N pool quota — see
/// `build_payload` Stage 3 setup. Extracted as a pure function so the
/// (`time_drift`, `sweep_interval`, `slot_duration`, `block_gas_limit`)
/// → `(ticks_remaining, gas_per_batch, first_offset, build_delay_ms)`
/// mapping is unit-testable without a real payload build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolQuotaSchedule {
    /// Time-until-slot-deadline used for the derivation, clamped to
    /// `[sweep_interval, slot_duration]`.
    time_drift: std::time::Duration,
    /// Number of quota ticks fitting in `time_drift` (rounded up so a
    /// residual remainder gets its own tick). Always ≥ 1.
    ticks_remaining: u64,
    /// Per-tick pool gas share — `block_gas_limit / ticks_remaining`.
    gas_per_batch: u64,
    /// First tick offset — aligns subsequent ticks to `sweep_interval`
    /// boundaries within the slot. Equal to `sweep_interval` when the
    /// slot already sits on a boundary.
    first_offset: std::time::Duration,
    /// Delay from the target slot start to now, in milliseconds.
    /// Useful for observability / alerting.
    build_delay_ms: u64,
}

/// Compute the adaptive-N pool gas schedule from wall-clock inputs.
///
/// `time_drift_or_fallback` should be the caller's already-saturated
/// remaining-time-to-slot-deadline (falling back to `sweep_interval`
/// for late-FCU / clock-skew cases). This helper is deterministic
/// modulo integer arithmetic — no wall-clock reads inside.
fn derive_pool_quota_schedule(
    time_drift_or_fallback: std::time::Duration,
    sweep_interval: std::time::Duration,
    slot_duration: std::time::Duration,
    block_gas_limit: u64,
) -> PoolQuotaSchedule {
    let time_drift = time_drift_or_fallback.min(slot_duration);
    let interval_ms = sweep_interval.as_millis().max(1) as u64;
    let drift_ms = time_drift.as_millis() as u64;
    let ticks_remaining = drift_ms.div_ceil(interval_ms).max(1);
    let gas_per_batch = block_gas_limit / ticks_remaining;
    let first_offset_ms = drift_ms.checked_rem(interval_ms).unwrap_or(0);
    let first_offset = if first_offset_ms == 0 {
        sweep_interval
    } else {
        std::time::Duration::from_millis(first_offset_ms)
    };
    let build_delay_ms = slot_duration.saturating_sub(time_drift).as_millis() as u64;
    PoolQuotaSchedule { time_drift, ticks_remaining, gas_per_batch, first_offset, build_delay_ms }
}

/// Outcome of one iteration of the pool best-tx step inside the
/// select! loop.
enum BestTxStep {
    /// Iterator still has candidates; the caller should keep polling.
    Continue,
    /// Iterator exhausted (or the current tx would over-fill the block
    /// and marking-invalid drained descendants). Caller should disable
    /// the best-tx branch.
    Done,
}

/// One iteration of the pool best-tx loop: pulls the next candidate,
/// applies limits / filtering, executes against the in-flight builder,
/// updates `info`. Ported from `OpPayloadBuilderCtx::execute_best_transactions`
/// but factored out so each call handles exactly one tx — lets the
/// unified select! loop interleave best-tx application with preconf
/// commitment application.
#[allow(clippy::too_many_arguments)]
fn apply_one_best_tx<N, Builder>(
    cfg: &PreconfConfig,
    best_txs: &mut impl PayloadTransactions<
        Transaction: PoolTransaction<Consensus = N::SignedTx> + OpPooledTx,
    >,
    builder: &mut Builder,
    info: &mut ExecutionInfo,
    block_gas_limit: u64,
    block_da_limit: Option<u64>,
    tx_da_limit: Option<u64>,
    base_fee: u64,
    attrs_timestamp: u64,
    da_footprint_gas_scalar: Option<u16>,
) -> Result<BestTxStep, PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    Builder: BlockBuilder<Primitives = N>,
{
    let Some(tx) = best_txs.next(()) else {
        return Ok(BestTxStep::Done);
    };
    // Preconf-eligible txs are applied EXCLUSIVELY via the preconf arm.
    // Without this filter, the pool arm could grab a preconf-eligible tx
    // that was just admitted to the pool but whose fifo entry hasn't
    // been pushed yet by the async pool listener — the tx would land on
    // chain via the pool path while the client sees a Timeout/Failed
    // response (responder never called). The preconf listener creates a
    // fifo entry for every preconf-eligible tx entering the pool, so
    // skipping here does not drop the tx — it merely constrains it to
    // the preconf ordering.
    let sender = tx.sender();
    let to = match tx.kind() {
        TxKind::Call(addr) => Some(addr),
        TxKind::Create => None,
    };
    if cfg.is_preconf_tx(&sender, to.as_ref()) {
        best_txs.mark_invalid(sender, tx.nonce());
        return Ok(BestTxStep::Continue);
    }
    let interop = tx.interop_deadline();
    let tx_da_size = tx.estimated_da_size();
    let tx = tx.into_consensus();

    if info.is_tx_over_limits(
        tx_da_size,
        block_gas_limit,
        tx_da_limit,
        block_da_limit,
        tx.gas_limit(),
        da_footprint_gas_scalar,
    ) {
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }

    if tx.is_eip4844() || tx.is_deposit() {
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }

    if let Some(interop) = interop &&
        !is_valid_interop(interop, attrs_timestamp)
    {
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }

    let gas_used = match builder.execute_transaction(tx.clone()) {
        Ok(g) => g,
        Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. })) => {
            if !error.is_nonce_too_low() {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
            }
            return Ok(BestTxStep::Continue);
        }
        Err(err) => {
            return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
        }
    };
    let tx_gas_used = gas_used.tx_gas_used();
    info.cumulative_gas_used += tx_gas_used;
    info.cumulative_da_bytes_used += tx_da_size;
    let miner_fee =
        tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
    info.total_fees += U256::from(miner_fee) * U256::from(tx_gas_used);
    Ok(BestTxStep::Continue)
}

// ─── build_payload (async) ──────────────────────────────────────────────────

// Forked from `reth_optimism_payload_builder::builder::OpBuilder::build`.
// The sync upstream is converted into `async fn` so the preconf-tx
// select! arm can be interleaved without restructuring the signature.
// Generic bounds are copied verbatim from upstream.
impl<Pool, Client, Evm> PreconfPayloadBuilder<Pool, Client, Evm> {
    /// Drive a single payload job to completion. Returns the final
    /// [`OpBuiltPayload<N>`] on success; `cancel` cuts the build short
    /// and seals whatever has been applied so far.
    ///
    /// ## Execution stages
    ///
    /// 1. **Prelude** — construct upstream [`OpPayloadBuilderCtx`], fetch the parent-block state
    ///    provider twice (owned form; needed to keep the async future `Send`), preload the L1 block
    ///    contract into the DB cache.
    /// 2. **Stage 1** — `apply_pre_execution_changes` (EIP-2935 / 4788
    ///    + OP-stack predeploys).
    /// 3. **Stage 2** — `execute_sequencer_transactions` (deposits + L1 info + system txs).
    /// 4. **Stage 3** — unified `select!` loop with four `biased` branches:
    ///    - `cancel.wait()` — exits the loop.
    ///    - `fifo_rx.recv()` — preconf-tx apply (`apply_one_preconf` on `Ok`, `reconcile_lagged` on
    ///      `Lagged`, break on `Closed`).
    ///    - **Level-triggered pool arm** (`ready(()) if pool_gas_used < pool_quota`) — each fire
    ///      admits exactly one pool best-tx, then returns to `select!`. Cancel and preconf get
    ///      preempt chances between every pool tx via biased priority.
    ///    - `sweep_ticker.tick()` — edge-triggered ticker. Bumps `pool_quota` by
    ///      [`PoolQuotaSchedule::gas_per_batch`] on each tick (adaptive-N derivation adapts `N` to
    ///      remaining slot time so pool aims to fill the block regardless of build delay —
    ///      op-rbuilder flashblocks pattern). Doesn't apply directly; the level-triggered pool arm
    ///      consumes the new headroom.
    ///
    ///    Before the loop, a **carryover replay preamble**
    ///    ([`replay_fifo_carryover`]) applies any stale in-flight or
    ///    journal-restored entries directly (bypassing the broadcast
    ///    queue) so they land ahead of concurrently-queued RPC pushes.
    /// 5. **Stage 4** — SDM post-exec refund tx (only when `ctx.sdm_production_enabled()`).
    /// 6. **Stage 5** — `builder.finish` → seal + wrap into `OpBuiltPayload`.
    ///
    /// Both preconf-tx and best-tx apply into the same in-flight
    /// `State<DB>`, which is what makes the RPC-returned receipt
    /// byte-equal to the sealed block's receipt.
    ///
    /// ## Generic parameter placement
    ///
    /// `N` and `Attrs` are bound on the method (not the `impl`) because
    /// this is an inherent async method rather than a
    /// [`reth_basic_payload_builder::PayloadBuilder`] impl (whose sync
    /// `try_build` is incompatible with our async select! loop). No
    /// struct field depends on `N` / `Attrs`, so method-level binding
    /// keeps the struct free of `PhantomData<(N, Attrs)>` and lets a
    /// single builder serve multiple primitive sets.
    ///
    /// [`OpPayloadBuilderCtx`]: reth_optimism_payload_builder::builder::OpPayloadBuilderCtx
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
        N::SignedTx:
            From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>> + TryFrom<TxEnvelope>,
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
        //
        // The fork rebuilds `State<DB>` fresh on every call, so
        // upstream's `CachedReads` reuse optimization is not wired here
        // — the field is accepted for signature compatibility with
        // `BuildArguments` and deliberately ignored.
        let BuildArguments { cached_reads: _cached_reads, config, best_payload, .. } = args;

        // ── Construct upstream OpPayloadBuilderCtx ─────────────────────
        let chain_spec = self.client.chain_spec();
        let parent_hash = config.parent_header.hash();
        // `cancel: CancelOnDrop::default()` — a fresh sync flag that is
        // never flipped. Our job-level async cancel (`JobCancel`) drives
        // teardown via the select! loop instead. Consequence: upstream's
        // best-tx scan does not observe job cancellation and runs to
        // completion (bounded by block gas limit) before the loop starts.
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
        // Double fetch is deliberate: one goes into `StateProviderDatabase`
        // (owned by the in-flight `State<DB>`), the other passes to
        // `builder.finish(...)` for state-root computation. Owned rather
        // than borrowed because `&Box<dyn StateProvider + Send>` held
        // across async `.await` points would break `Send`.
        let state_provider_for_finish = self.client.state_by_block_hash(parent_hash)?;
        let state_provider_for_db = self.client.state_by_block_hash(parent_hash)?;
        let state_db = StateProviderDatabase::new(state_provider_for_db);
        let mut db = State::builder().with_database(state_db).with_bundle_update().build();

        // Preload L1 block contract into the DB cache; otherwise the DA
        // footprint gas scalar fetch panics on first tx. (Forked from
        // upstream `OpBuilder::build`.)
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

        // ── Stage 3: unified select! loop (see method rustdoc) ────────

        // Pool iterator — one-shot snapshot at build start.
        let best_txs_iter_opt = (!ctx.attributes().no_tx_pool()).then(|| {
            let attrs = ctx.best_transaction_attributes(builder.evm_mut().block());
            BestPayloadTransactions::new(self.pool.best_transactions_with_attributes(attrs))
        });

        // Snapshot per-block limits (constant across the build).
        let mut block_gas_limit = builder.evm_mut().block().gas_limit();
        if let Some(cfg_limit) = self.builder_config.gas_limit_config.gas_limit() {
            block_gas_limit = cfg_limit.min(block_gas_limit);
        }
        let block_da_limit = self.builder_config.da_config.max_da_block_size();
        let tx_da_limit = self.builder_config.da_config.max_da_tx_size();
        let base_fee = builder.evm_mut().block().basefee();
        let attrs_timestamp = ctx.attributes().timestamp();
        // Post-Jovian DA footprint scalar is a per-block constant set by
        // the Stage 2 L1 info tx — read once, reuse across all admissions.
        let da_footprint_gas_scalar =
            self.client.chain_spec().is_jovian_active_at_timestamp(attrs_timestamp).then(|| {
                L1BlockInfo::fetch_da_footprint_gas_scalar(builder.evm_mut().db_mut())
                    .expect("DA footprint should always be available from the database post jovian")
            });

        // DA-footprint (H3) limits for the preconf apply path — snapshotted
        // once, constant across the build. Same inputs the pool best-tx arm
        // feeds `ExecutionInfo::is_tx_over_limits`, so both paths enforce one
        // shared block DA budget.
        let preconf_da_limits = PreconfDaLimits {
            block_da_limit,
            tx_da_limit,
            da_footprint_gas_scalar,
            block_gas_limit,
        };

        let mut best_txs_iter = best_txs_iter_opt;
        let mut fifo_rx = self.fifo.subscribe();
        let predicted_height = ctx.parent().number() + 1;
        let mut loop_state = dispatch::LoopState::new(predicted_height);

        // Adaptive-N pool quota schedule — see `derive_pool_quota_schedule`.
        // SystemTime is read only here for the initial offset; the tokio
        // ticker itself is monotonic.
        let slot_deadline =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(attrs_timestamp);
        let time_drift_input = slot_deadline
            .duration_since(std::time::SystemTime::now())
            .unwrap_or(self.cfg.sweep_interval);
        let schedule = derive_pool_quota_schedule(
            time_drift_input,
            self.cfg.sweep_interval,
            self.cfg.slot_duration,
            block_gas_limit,
        );
        let gas_per_batch = schedule.gas_per_batch;

        if schedule.build_delay_ms > 100 {
            warn!(
                target: "mantle::preconf::payload_builder",
                build_delay_ms = schedule.build_delay_ms,
                time_drift_ms = schedule.time_drift.as_millis() as u64,
                ticks_remaining = schedule.ticks_remaining,
                gas_per_batch,
                "delayed build start; adapting pool quota to remaining slot"
            );
        }

        let mut sweep_ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + schedule.first_offset,
            self.cfg.sweep_interval,
        );
        // Cumulative pool gas budget. Starts at 0 — pool cannot admit
        // until the first sweep tick bumps it by `gas_per_batch`. From
        // then on, each tick bumps by another `gas_per_batch`; the pool
        // arm's guard `pool_gas_used < pool_quota` is a level trigger
        // that self-disables once the current allocation is drained.
        let mut pool_quota: u64 = 0;

        // `no_tx_pool=true` on the payload attrs signals a
        // **deterministic derivation build**: the block must exactly
        // reproduce what other nodes derive from L1 data (deposits +
        // sequencer-batched txs only). Injecting any preconf tx here —
        // whether a fresh RPC push, a Waiting carryover, or a
        // Replay-sourced journal entry — would diverge the block hash
        // from the network consensus and cause a safe-head fork. Gate
        // the entire preconf pipeline on this flag; fifo entries stay
        // put and get dispatched on the next `no_tx_pool=false` build
        // (their SLA is upheld by delayed landing, NOT by forcing them
        // into the derivation block).
        let allow_preconf = !ctx.attributes().no_tx_pool();

        // Synchronous canon-forward — drop fifo entries whose nonce is
        // already sealed as of parent block. Replaces the async
        // `canon_handler::forward()` which raced with new PayloadJob
        // start (see `sync_fifo_forward_to_head` docs for details).
        // Reads via `state_provider_for_finish` (owned, not-yet-moved
        // into `builder.finish`); `.account_nonce(...)` takes `&self`
        // so the later move at Stage 5 is unaffected.
        //
        // Runs regardless of `allow_preconf`: `forward` only prunes
        // canon-stale entries, it does not apply any tx into the
        // in-flight block, so it is safe (and desirable — keeps fifo
        // aligned with chain state) during derivation builds too.
        sync_fifo_forward_to_head(&self.fifo, state_provider_for_finish.as_ref()).await;

        // Carryover replay preamble — apply stale in-flight / journal-
        // restored entries directly (see `replay_fifo_carryover`). The
        // block scope drops `apply_fn` so its `&mut builder` borrow is
        // released before the select! loop's arms.
        //
        // Skipped entirely when `!allow_preconf` — the fifo entries
        // (including Replay-sourced ones with `must-land` SLA) remain
        // in the fifo and get dispatched on the next normal-slot build.
        if allow_preconf {
            // `apply_preconf_with_da` folds the DA gate + gas/DA accounting
            // into `info` on success, so no manual `cumulative_gas_used`
            // sync is needed after the call (unlike the pre-H3 code).
            let mut apply_fn = |tx, hash, height| {
                apply_preconf_with_da::<N, _>(
                    &mut builder,
                    &mut info,
                    preconf_da_limits,
                    tx,
                    hash,
                    height,
                )
            };
            replay_fifo_carryover(&self.fifo, &self.cfg, &mut loop_state, &mut apply_fn).await;
        }

        // no_tx_pool builds have no dispatch work: both arms are gated.
        // Skip straight to seal — mirrors upstream `OpBuilder::build`
        // returning right after Stage 2 in this case.
        loop {
            if !allow_preconf {
                break;
            }
            tokio::select! {
                biased;
                () = cancel.wait() => break,
                recv = fifo_rx.recv() => {
                    // Closure re-created per arm-entry so its `&mut builder` /
                    // `&mut info` borrows do not clash with the pool arm.
                    // `apply_preconf_with_da` runs the DA gate then folds this
                    // tx's gas + DA footprint into `info` on success, keeping
                    // the pool arm's `is_tx_over_limits` view accurate.
                    let mut apply_fn = |tx, hash, height| {
                        apply_preconf_with_da::<N, _>(
                            &mut builder,
                            &mut info,
                            preconf_da_limits,
                            tx,
                            hash,
                            height,
                        )
                    };
                    match recv {
                        Ok(hash) => {
                            dispatch::apply_one_preconf(
                                &self.fifo, &self.cfg, hash, &mut loop_state,
                                &mut apply_fn,
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            dispatch::reconcile_lagged(
                                &self.fifo, &self.cfg, &mut loop_state, n,
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
                // Admits ONE pool tx per fire, then returns to select! so
                // biased cancel / preconf can preempt between every tx.
                _ = std::future::ready(()), if best_txs_iter.is_some()
                    && loop_state.pool_gas_used() < pool_quota =>
                {
                    let iter = best_txs_iter.as_mut().expect("guard verified Some");
                    let before = info.cumulative_gas_used;
                    match apply_one_best_tx::<N, _>(
                        &self.cfg,
                        iter,
                        &mut builder,
                        &mut info,
                        block_gas_limit,
                        block_da_limit,
                        tx_da_limit,
                        base_fee,
                        attrs_timestamp,
                        da_footprint_gas_scalar,
                    )? {
                        BestTxStep::Continue => {
                            // delta == 0 → tx was filtered (mark_invalid /
                            // nonce-too-low); iterator has advanced. Next
                            // select! iteration re-fires this arm and
                            // pulls the next tx.
                            let delta = info.cumulative_gas_used - before;
                            if delta > 0 {
                                loop_state.record_pool_gas(delta);
                            }
                        }
                        BestTxStep::Done => best_txs_iter = None,
                    }
                }
                // Only bumps `pool_quota`; the pool arm above drains
                // the new headroom on subsequent iterations.
                _ = sweep_ticker.tick() => {
                    pool_quota = pool_quota
                        .saturating_add(gas_per_batch)
                        .min(block_gas_limit);
                }
            }
        }

        // ── Stage 4: SDM post-exec refund tx ───────────────────────────
        // `take_post_exec_entries` collects entries from ALL prior applies
        // uniformly (sequencer / pool / preconf) — preconf-tx contributions
        // are automatically included, no special handling needed.
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

        Ok(OpBuiltPayload::new(ctx.payload_id(), sealed_block, info.total_fees, Some(executed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PreconfSource, PreconfStatus};
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{B256, Signature};

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

    fn tx(byte: u8, nonce: u64) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce, gas_limit: 21_000, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    fn synthetic_ok(
        tx: Arc<TxEnvelope>,
        hash: TxHash,
        height: u64,
    ) -> Result<PreconfReceipt, PreconfError> {
        use alloy_primitives::Bytes;
        Ok(PreconfReceipt {
            tx_hash: hash,
            block_height: height,
            status: true,
            logs: Vec::new(),
            gas_used: tx.gas_limit(),
            reason: String::new(),
            revert_data: Bytes::new(),
        })
    }

    /// `replay_fifo_carryover` applies both `Waiting` and `Success`
    /// entries (each is a carryover source) and skips terminal-non-
    /// success statuses (`Failed` / `Timeout` / `Canceled`).
    #[tokio::test]
    async fn replay_fifo_carryover_applies_waiting_and_success_only() {
        use std::cell::RefCell;
        let fifo = PreconfTxSet::new(16);
        let cfg = PreconfConfig::default();
        // Five entries covering every non-transient status.
        let t_wait = tx(0xa1, 0);
        let t_succ = tx(0xa2, 0);
        let t_fail = tx(0xa3, 0);
        let t_to = tx(0xa4, 0);
        let t_cancel = tx(0xa5, 0);
        fifo.push_if_absent(t_wait.clone(), Address::from([1; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_succ.clone(), Address::from([2; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_fail.clone(), Address::from([3; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_to.clone(), Address::from([4; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_cancel.clone(), Address::from([5; 20]), PreconfSource::Rpc).await;
        fifo.mark_succeeded(t_succ.tx_hash()).await.unwrap();
        fifo.mark_failed(t_fail.tx_hash()).await.unwrap();
        fifo.mark_timeout(t_to.tx_hash()).await.unwrap();
        fifo.mark_canceled(t_cancel.tx_hash()).await.unwrap();

        let mut loop_state = dispatch::LoopState::new(1);
        let seen: RefCell<Vec<TxHash>> = RefCell::new(Vec::new());
        let apply_fn = |tx, hash, height| {
            seen.borrow_mut().push(hash);
            synthetic_ok(tx, hash, height)
        };
        replay_fifo_carryover(&fifo, &cfg, &mut loop_state, apply_fn).await;

        // Waiting + Success applied (in insertion order); Failed / Timeout /
        // Canceled skipped.
        assert_eq!(*seen.borrow(), vec![*t_wait.tx_hash(), *t_succ.tx_hash()]);
        // Both applied entries now Success (Waiting → Success direct; Success
        // → Waiting → Success round trip).
        assert_eq!(
            fifo.find_by_hash(t_wait.tx_hash()).await.unwrap().status,
            PreconfStatus::Success,
        );
        assert_eq!(
            fifo.find_by_hash(t_succ.tx_hash()).await.unwrap().status,
            PreconfStatus::Success,
        );
        // Terminal-non-success entries untouched.
        assert_eq!(
            fifo.find_by_hash(t_fail.tx_hash()).await.unwrap().status,
            PreconfStatus::Failed,
        );
        assert_eq!(fifo.find_by_hash(t_to.tx_hash()).await.unwrap().status, PreconfStatus::Timeout,);
        assert_eq!(
            fifo.find_by_hash(t_cancel.tx_hash()).await.unwrap().status,
            PreconfStatus::Canceled,
        );
    }

    /// Waiting entries keep their original `source` after replay — the
    /// helper only upgrades source on the `Success → Waiting` reset
    /// path, not on entries that were already `Waiting`. This is
    /// intentional: Rpc-sourced Waiting entries must still respect the
    /// deadline gate so genuinely stale RPC pushes get timed out.
    #[tokio::test]
    async fn replay_fifo_carryover_preserves_waiting_source() {
        let fifo = PreconfTxSet::new(16);
        let cfg = PreconfConfig::default();
        let t_rpc = tx(0xb0, 0);
        let t_journal = tx(0xb1, 0);
        fifo.push_if_absent(t_rpc.clone(), Address::from([1; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_journal.clone(), Address::from([2; 20]), PreconfSource::Replay).await;

        let mut loop_state = dispatch::LoopState::new(1);
        replay_fifo_carryover(&fifo, &cfg, &mut loop_state, synthetic_ok).await;

        // Sources preserved for entries that were already Waiting.
        assert_eq!(fifo.find_by_hash(t_rpc.tx_hash()).await.unwrap().source, PreconfSource::Rpc,);
        assert_eq!(
            fifo.find_by_hash(t_journal.tx_hash()).await.unwrap().source,
            PreconfSource::Replay,
        );
    }

    /// Carryover replay applies entries in fifo insertion order —
    /// critical for SLA determinism vs concurrent RPC pushes that
    /// might race the preamble.
    #[tokio::test]
    async fn replay_fifo_carryover_preserves_fifo_order() {
        use std::cell::RefCell;
        let fifo = PreconfTxSet::new(16);
        let cfg = PreconfConfig::default();
        let mut expected = Vec::new();
        for i in 0..3u8 {
            let t = tx(0xc0 + i, 0);
            expected.push(*t.tx_hash());
            fifo.push_if_absent(t, Address::from([i + 1; 20]), PreconfSource::Rpc).await;
            fifo.mark_succeeded(&expected[i as usize]).await.unwrap();
        }

        let mut loop_state = dispatch::LoopState::new(1);
        let seen: RefCell<Vec<TxHash>> = RefCell::new(Vec::new());
        let apply_fn = |tx, hash, height| {
            seen.borrow_mut().push(hash);
            synthetic_ok(tx, hash, height)
        };
        replay_fifo_carryover(&fifo, &cfg, &mut loop_state, apply_fn).await;

        assert_eq!(*seen.borrow(), expected, "carryover replay must respect FIFO insertion order");
    }

    /// Stale Success entries always bypass the RPC-only deadline and
    /// gas-budget gates — even under a tight budget the replay must
    /// apply them (SLA: receipt already returned → tx must land).
    /// Confirms the source-promotion side effect of
    /// `reset_success_to_waiting` reaches `apply_one_preconf`'s gate.
    #[tokio::test]
    async fn replay_fifo_carryover_bypasses_gas_budget_gate_for_stale_success() {
        let fifo = PreconfTxSet::new(16);
        // Tight budget that would reject an Rpc-sourced entry.
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 10_000, // < tx.gas_limit (21_000)
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let t = tx(0xd0, 0);
        let hash = *t.tx_hash();
        fifo.push_if_absent(t, Address::from([1; 20]), PreconfSource::Rpc).await;
        fifo.mark_succeeded(&hash).await.unwrap();

        let mut loop_state = dispatch::LoopState::new(1);
        replay_fifo_carryover(&fifo, &cfg, &mut loop_state, synthetic_ok).await;

        // Applied despite over-budget — replay promoted source to Replay
        // which bypasses the gas gate.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
        assert_eq!(entry.source, PreconfSource::Replay);
    }

    /// Empty fifo — helper is a no-op, does not error.
    #[tokio::test]
    async fn replay_fifo_carryover_on_empty_fifo_is_noop() {
        let fifo = PreconfTxSet::new(16);
        let cfg = PreconfConfig::default();
        let mut loop_state = dispatch::LoopState::new(1);
        replay_fifo_carryover(&fifo, &cfg, &mut loop_state, synthetic_ok).await;
        assert!(fifo.entries().await.is_empty());
    }

    // ============ try_include_post_exec_tx ============
    //
    // Tests exercise the helper's three branches via a fake `Tx` /
    // `execute` closure, without spinning up a real BlockBuilder — the
    // helper is upstream-shaped and doesn't depend on EVM state.

    /// A minimal `Tx` stand-in that satisfies `From<Sealed<TxPostExec>>`
    /// so `try_include_post_exec_tx` can construct it. The body is
    /// irrelevant — the `execute` closure treats it as an opaque token.
    struct FakePostExecTx;
    impl From<alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>> for FakePostExecTx {
        fn from(_: alloy_primitives::Sealed<op_alloy_consensus::TxPostExec>) -> Self {
            Self
        }
    }

    /// Empty `entries` short-circuits to `Ok(false)` and the `execute`
    /// closure is never invoked. Locks the "no-op signal" contract.
    #[test]
    fn try_include_post_exec_tx_empty_entries_returns_ok_false_without_invoking_execute() {
        use std::cell::Cell;
        let invoked = Cell::new(false);
        let result =
            try_include_post_exec_tx::<FakePostExecTx, std::io::Error>(42, Vec::new(), |_| {
                invoked.set(true);
                Ok(0)
            });
        assert!(matches!(result, Ok(false)));
        assert!(!invoked.get(), "execute closure must not be called on empty entries");
    }

    /// Non-empty entries + `execute` returns Ok: helper returns
    /// `Ok(true)` and invokes `execute` exactly once with a
    /// `Recovered<Tx>` synthesised from the built post-exec tx.
    #[test]
    fn try_include_post_exec_tx_non_empty_ok_path_invokes_execute_once() {
        use std::cell::Cell;
        let call_count = Cell::new(0u32);
        let entries = vec![SDMGasEntry::default()];
        let result =
            try_include_post_exec_tx::<FakePostExecTx, std::io::Error>(42, entries, |_recovered| {
                call_count.set(call_count.get() + 1);
                Ok(21_000)
            });
        assert!(matches!(result, Ok(true)));
        assert_eq!(call_count.get(), 1, "execute must be invoked exactly once");
    }

    /// `execute` Err path: helper wraps the closure error in
    /// `PayloadBuilderError::evm(..)` (fatal for the payload build).
    #[test]
    fn try_include_post_exec_tx_execute_err_wraps_into_payload_builder_error() {
        let entries = vec![SDMGasEntry::default()];
        let err = try_include_post_exec_tx::<FakePostExecTx, std::io::Error>(42, entries, |_| {
            Err(std::io::Error::other("synthetic execute failure"))
        })
        .expect_err("execute Err must surface as PayloadBuilderError");
        // Only sanity-check that the error chain reaches down to our
        // synthetic message — the wrapping variant `evm(..)` is an
        // internal detail of `PayloadBuilderError`.
        let chain = format!("{err:#}");
        assert!(chain.contains("synthetic execute failure"), "unexpected error chain: {chain}");
    }

    // ============ derive_pool_quota_schedule ============
    //
    // Pure-function math tests for the adaptive-N pool quota schedule.
    // These are wall-clock free — the caller pre-computes `time_drift`,
    // so the helper is deterministic.

    const TEST_SLOT: std::time::Duration = std::time::Duration::from_millis(2000);
    const TEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
    const TEST_BLOCK_GAS: u64 = 30_000_000;

    /// No delay: full slot remaining. Schedule matches the "no-delay"
    /// case documented in the state-machine comment — 10 ticks × 3M
    /// each, first tick aligned to `sweep_interval`.
    #[test]
    fn quota_schedule_full_slot_produces_ten_ticks_of_three_million_each() {
        let s = derive_pool_quota_schedule(TEST_SLOT, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
        // Aligned drift → first tick equals full interval.
        assert_eq!(s.first_offset, TEST_INTERVAL);
        assert_eq!(s.build_delay_ms, 0);
        assert_eq!(s.time_drift, TEST_SLOT);
    }

    /// Delay 1s (1s remaining). Adaptive-N shrinks to 5 ticks × 6M each
    /// — pool still fills the whole block over the remaining window.
    #[test]
    fn quota_schedule_one_second_delay_produces_five_ticks_of_six_million_each() {
        let drift = std::time::Duration::from_millis(1000);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 5);
        assert_eq!(s.gas_per_batch, 6_000_000);
        assert_eq!(s.first_offset, TEST_INTERVAL); // 1000 % 200 == 0 → align to interval
        assert_eq!(s.build_delay_ms, 1000);
    }

    /// Non-aligned drift: `first_offset` shrinks to the remainder so
    /// every subsequent tick lands on an interval boundary within the
    /// slot. 900ms remaining → first tick after 100ms, then every
    /// 200ms → 5 ticks total: [100, 300, 500, 700, 900].
    #[test]
    fn quota_schedule_non_aligned_drift_uses_remainder_as_first_offset() {
        let drift = std::time::Duration::from_millis(900);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.first_offset, std::time::Duration::from_millis(100));
        // ceil(900/200) = 5 ticks; each tick admits 6M.
        assert_eq!(s.ticks_remaining, 5);
        assert_eq!(s.gas_per_batch, 6_000_000);
        assert_eq!(s.build_delay_ms, 1100);
    }

    /// Extreme delay: less than one interval remaining. Schedule still
    /// admits one tick with the full block budget — a "single-shot"
    /// pool sweep at the end of the slot rather than degenerating to
    /// zero pool admission.
    #[test]
    fn quota_schedule_sub_interval_drift_yields_one_tick_full_budget() {
        let drift = std::time::Duration::from_millis(120);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 1);
        assert_eq!(s.gas_per_batch, TEST_BLOCK_GAS);
        // 120 % 200 = 120 → first tick after 120ms.
        assert_eq!(s.first_offset, std::time::Duration::from_millis(120));
        assert_eq!(s.build_delay_ms, 1880);
    }

    /// Late FCU / clock skew: caller has already fallen back to
    /// `sweep_interval` (its typical fallback). Schedule handles it
    /// gracefully — one tick, full budget, offset = interval.
    #[test]
    fn quota_schedule_fallback_to_sweep_interval_is_valid() {
        let s = derive_pool_quota_schedule(TEST_INTERVAL, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.ticks_remaining, 1);
        assert_eq!(s.gas_per_batch, TEST_BLOCK_GAS);
        assert_eq!(s.first_offset, TEST_INTERVAL);
    }

    /// Drift exceeds `slot_duration` (misconfigured attrs.timestamp far
    /// in the future). Clamped to `slot_duration` — no unbounded quota.
    #[test]
    fn quota_schedule_over_long_drift_clamps_to_slot_duration() {
        let drift = std::time::Duration::from_secs(60);
        let s = derive_pool_quota_schedule(drift, TEST_INTERVAL, TEST_SLOT, TEST_BLOCK_GAS);
        assert_eq!(s.time_drift, TEST_SLOT);
        // Same as full-slot case.
        assert_eq!(s.ticks_remaining, 10);
        assert_eq!(s.gas_per_batch, 3_000_000);
    }

    /// Sum-invariant: `ticks_remaining × gas_per_batch` ≤
    /// `block_gas_limit` (integer division floor). Pool never
    /// over-admits by design; slight under-admission (up to
    /// `ticks_remaining - 1` gas due to floor) is acceptable and
    /// bounded.
    #[test]
    fn quota_schedule_total_admission_never_exceeds_block_gas() {
        for drift_ms in [100u64, 200, 500, 900, 1000, 1500, 1900, 2000] {
            let s = derive_pool_quota_schedule(
                std::time::Duration::from_millis(drift_ms),
                TEST_INTERVAL,
                TEST_SLOT,
                TEST_BLOCK_GAS,
            );
            let total_admitted = s.ticks_remaining.saturating_mul(s.gas_per_batch);
            assert!(
                total_admitted <= TEST_BLOCK_GAS,
                "drift_ms={drift_ms}: total {total_admitted} exceeds block gas {TEST_BLOCK_GAS}",
            );
            // Under-admission bound: at most (ticks_remaining - 1) gas
            // lost to floor rounding.
            let under = TEST_BLOCK_GAS - total_admitted;
            assert!(
                under < s.ticks_remaining,
                "drift_ms={drift_ms}: under-admission {under} exceeds ticks {ticks}",
                ticks = s.ticks_remaining,
            );
        }
    }

    // ============ preconf_da_check (H3 DA footprint gate) ============
    //
    // Pure-function tests for the DA pre-check. No EVM / builder needed —
    // the gate is byte-arithmetic over (tx_da, cumulative_da, limits).

    const BLOCK_GAS: u64 = 30_000_000;

    fn da_limits(block: Option<u64>, per_tx: Option<u64>, scalar: Option<u16>) -> PreconfDaLimits {
        PreconfDaLimits {
            block_da_limit: block,
            tx_da_limit: per_tx,
            da_footprint_gas_scalar: scalar,
            block_gas_limit: BLOCK_GAS,
        }
    }

    /// No DA limits configured (pre-Jovian, no `da_config`) → every tx passes.
    /// This is the default integration-harness state, so preconf behaviour
    /// is unchanged unless an operator sets DA limits.
    #[test]
    fn preconf_da_check_no_limits_always_passes() {
        let limits = da_limits(None, None, None);
        assert!(preconf_da_check(1_000_000, 5_000_000, limits).is_ok());
        assert!(preconf_da_check(u64::MAX, u64::MAX, limits).is_ok());
    }

    /// Per-tx DA limit: `tx_da > limit` rejects; boundary (`==`) passes
    /// (gate uses `>`).
    #[test]
    fn preconf_da_check_per_tx_limit_boundary_and_reject() {
        let limits = da_limits(None, Some(100), None);
        assert!(preconf_da_check(100, 0, limits).is_ok(), "boundary equal must pass");
        match preconf_da_check(101, 0, limits) {
            Err(PreconfError::DaLimitExceeded { used, tx_da, limit }) => {
                assert_eq!((used, tx_da, limit), (0, 101, 100));
            }
            other => panic!("expected DaLimitExceeded, got {other:?}"),
        }
    }

    /// Per-block DA limit is checked against the *cumulative* DA
    /// (`used + tx_da`), not the single tx. Boundary passes, over rejects.
    #[test]
    fn preconf_da_check_block_limit_uses_cumulative() {
        let limits = da_limits(Some(1_000), None, None);
        // 900 already used + 100 = 1000 == limit → ok.
        assert!(preconf_da_check(100, 900, limits).is_ok());
        // 900 + 101 = 1001 > 1000 → reject; error reports the block limit.
        match preconf_da_check(101, 900, limits) {
            Err(PreconfError::DaLimitExceeded { used, tx_da, limit }) => {
                assert_eq!((used, tx_da, limit), (900, 101, 1_000));
            }
            other => panic!("expected DaLimitExceeded, got {other:?}"),
        }
    }

    /// Post-Jovian footprint-gas bound: `(used + tx_da) * scalar` must not
    /// exceed `block_gas_limit`. Boundary passes, over rejects (error
    /// reports the gas bound).
    #[test]
    fn preconf_da_check_footprint_gas_scalar_bound() {
        // scalar 1000, block gas 1_000_000 → total DA must stay ≤ 1000 bytes.
        let limits = PreconfDaLimits {
            block_da_limit: None,
            tx_da_limit: None,
            da_footprint_gas_scalar: Some(1_000),
            block_gas_limit: 1_000_000,
        };
        // 1000 * 1000 = 1_000_000 == limit → ok.
        assert!(preconf_da_check(1_000, 0, limits).is_ok());
        // 1001 * 1000 = 1_001_000 > 1_000_000 → reject.
        match preconf_da_check(1_001, 0, limits) {
            Err(PreconfError::DaLimitExceeded { limit, .. }) => assert_eq!(limit, 1_000_000),
            other => panic!("expected DaLimitExceeded, got {other:?}"),
        }
    }

    /// Per-tx limit fires before the block limit when both would reject —
    /// the earliest gate wins so the error names the tightest bound hit
    /// first (per-tx). Guards evaluation order.
    #[test]
    fn preconf_da_check_per_tx_limit_takes_precedence() {
        let limits = da_limits(Some(50), Some(100), None);
        // tx_da 200 exceeds both; per-tx (100) is checked first.
        match preconf_da_check(200, 0, limits) {
            Err(PreconfError::DaLimitExceeded { limit, .. }) => assert_eq!(limit, 100),
            other => panic!("expected DaLimitExceeded, got {other:?}"),
        }
    }
}
