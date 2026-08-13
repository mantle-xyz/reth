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
use alloy_primitives::{Address, Sealed, TxHash, U256};
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
    PreconfClassifier, PreconfConfig, PreconfTxSet,
    apply::{ApplyError, apply_preconf_tx},
    builder::{cancel::JobCancel, dispatch},
    classifier::Verdict,
    types::{PreconfError, PreconfReceipt, PreconfSource},
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
    /// Decides which arm owns a transaction. Read synchronously from the
    /// pool best-tx step, which is why it cannot be the (async) fifo.
    classifier: Arc<PreconfClassifier>,
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
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
    ) -> Self {
        Self { pool, client, evm_config, builder_config, cfg, classifier, fifo }
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

    /// Borrow the shared classifier handle.
    pub const fn classifier(&self) -> &Arc<PreconfClassifier> {
        &self.classifier
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
) -> Result<PreconfReceipt, ApplyError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    // Conversion / ec-recover failures are per-tx faults (a malformed
    // envelope can never land) → `Rejected`, not `Fatal`.
    let envelope = (*tx).clone();
    let signed: N::SignedTx = envelope.try_into().map_err(|_| {
        ApplyError::Rejected(PreconfError::BuilderRejected(
            "TxEnvelope → N::SignedTx conversion failed".into(),
        ))
    })?;
    let recovered: Recovered<N::SignedTx> = signed.try_into_recovered().map_err(|_| {
        ApplyError::Rejected(PreconfError::BuilderRejected(
            "ec-recover failed for preconf tx".into(),
        ))
    })?;
    apply_preconf_tx(builder, recovered, hash, height)
}

/// Immutable per-block gas / DA / fee constraints, snapshotted once at
/// `build_payload` start. Single source of truth shared by both dispatch arms:
/// the preconf admission gate ([`preconf_admission`]) and the pool best-tx path
/// ([`apply_one_best_tx`]).
#[derive(Debug, Clone, Copy)]
struct BuildConstraints {
    /// Block gas hard cap (also the footprint-gas DA bound).
    block_gas_limit: u64,
    /// Max DA bytes for the whole block (`da_config.max_da_block_size`).
    block_da_limit: Option<u64>,
    /// Max DA bytes for a single tx (`da_config.max_da_tx_size`).
    tx_da_limit: Option<u64>,
    /// Post-Jovian footprint-gas scalar; `Some` only when Jovian is active.
    da_footprint_gas_scalar: Option<u16>,
    /// Block base fee.
    base_fee: u64,
    /// Payload attributes timestamp (interop-deadline validation).
    timestamp: u64,
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

/// Outcome of the pre-dispatch block-capacity admission check for a preconf
/// tx. Decided **before** the hash is dispatched to
/// [`dispatch::apply_one_preconf`], which drives the [`PreconfTxSet`] entry
/// status machine (`Waiting → Success`/`Failed`). Keeps admission policy (can
/// this tx enter the current block?) separate from the execution result (did
/// it succeed once admitted?).
#[derive(Debug)]
enum Admission {
    /// The tx fits the current in-flight block's remaining DA + gas → dispatch.
    Admit,
    /// The tx fits an *empty* block but not the current one (transient
    /// capacity). Only returned for [`PreconfSource::Replay`]: the entry is
    /// left `Waiting` and retried next slot. Never marks the fifo terminal.
    Defer,
    /// The tx cannot enter a block: it exceeds a per-tx/per-block limit even
    /// in an empty block (permanent), or it is transient-over but RPC-sourced
    /// (RPC does not defer). A server pre-apply rejection — the tx never
    /// reaches the builder, so dispatch maps this to `mark_canceled` (like the
    /// preconf block-gas-budget gate), not `mark_failed`.
    Reject(PreconfError),
}

/// Block-capacity admission for a preconf tx — the unified DA + real-block-gas
/// gate run **before** dispatch. Pure function over the tx footprint, the
/// in-flight block's cumulative usage, the configured limits, and the source.
///
/// Classification rule = *"does this tx fit an empty block?"*:
/// - **Permanent** (exceeds a per-tx / per-block bound even alone) → `Reject`.
/// - **Fits** the current block's remaining headroom → `Admit`.
/// - **Transient** (fits an empty block, but the current block is too full) → `Defer` for `Replay`,
///   `Reject` for `Rpc`.
fn preconf_admission(
    tx_da: u64,
    tx_gas_limit: u64,
    da_used: u64,
    gas_used: u64,
    limits: BuildConstraints,
    source: PreconfSource,
) -> Admission {
    // ── Permanent: does the tx fit an *empty* block (da_used = gas_used = 0)? ──
    // A tx that alone exceeds a per-tx / per-block bound can never be included
    // in any block → hard reject regardless of source.
    if let Some(limit) = limits.tx_da_limit &&
        tx_da > limit
    {
        return Admission::Reject(PreconfError::DaLimitExceeded { used: da_used, tx_da, limit });
    }
    if let Some(limit) = limits.block_da_limit &&
        tx_da > limit
    {
        return Admission::Reject(PreconfError::DaLimitExceeded { used: da_used, tx_da, limit });
    }
    if let Some(scalar) = limits.da_footprint_gas_scalar &&
        tx_da.saturating_mul(scalar as u64) > limits.block_gas_limit
    {
        return Admission::Reject(PreconfError::DaLimitExceeded {
            used: da_used,
            tx_da,
            limit: limits.block_gas_limit,
        });
    }
    if tx_gas_limit > limits.block_gas_limit {
        return Admission::Reject(PreconfError::BuilderRejected(format!(
            "tx gas limit {tx_gas_limit} exceeds block gas limit {}",
            limits.block_gas_limit
        )));
    }

    // ── tx fits an empty block. Does it fit the *current* block's remainder? ──
    let da_total = da_used.saturating_add(tx_da);
    let over_block_da = limits.block_da_limit.is_some_and(|l| da_total > l);
    let over_footprint = limits
        .da_footprint_gas_scalar
        .is_some_and(|s| da_total.saturating_mul(s as u64) > limits.block_gas_limit);
    let over_gas = gas_used.saturating_add(tx_gas_limit) > limits.block_gas_limit;

    if over_block_da || over_footprint || over_gas {
        // Transient: fits an empty block, but the current block is too full.
        return match source {
            // Replay is a must-land commitment — keep it Waiting and retry
            // next slot (fresh block DA/gas budget). Handled by the caller as
            // "do not dispatch"; the fifo entry is never marked terminal.
            PreconfSource::Replay => Admission::Defer,
            // RPC does not defer (client is waiting); reject so it can resubmit.
            PreconfSource::Rpc => {
                let reason = if over_gas && !over_block_da && !over_footprint {
                    PreconfError::BuilderRejected(format!(
                        "block gas headroom exhausted: used {gas_used}, need {tx_gas_limit}, \
                         block limit {}",
                        limits.block_gas_limit
                    ))
                } else {
                    PreconfError::DaLimitExceeded {
                        used: da_used,
                        tx_da,
                        limit: limits.block_da_limit.unwrap_or(limits.block_gas_limit),
                    }
                };
                Admission::Reject(reason)
            }
        };
    }

    Admission::Admit
}

/// Apply a preconf tx and fold its gas, DA footprint, **and priority fee** into
/// `info` so the pool best-tx arm (which reads `info.cumulative_gas_used` /
/// `info.cumulative_da_bytes_used` via [`ExecutionInfo::is_tx_over_limits`]) sees
/// the true running block totals — preconf and pool share one block DA + gas
/// budget — and the sealed payload's block value (`total_fees`) includes preconf
/// revenue, same as pool txs.
///
/// No DA gate here: [`preconf_admission`] already enforces the per-tx / per-block
/// DA + footprint bounds against the same `info.cumulative_da_bytes_used`
/// (unchanged in the single-task window between admission and apply), and only
/// dispatches on `Admit`. A gate here would be dead code.
fn apply_preconf_with_da<N, B>(
    builder: &mut B,
    info: &mut ExecutionInfo,
    limits: BuildConstraints,
    tx: Arc<TxEnvelope>,
    hash: TxHash,
    height: u64,
) -> Result<PreconfReceipt, ApplyError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    let tx_da = estimated_tx_da_size(&tx);
    // Miner tip is independent of gas used — capture it before `tx` is consumed
    // by apply, then fold `tip × gas_used` into the block value below.
    let miner_tip = tx.effective_tip_per_gas(limits.base_fee).unwrap_or_default();
    let receipt = convert_and_apply_preconf::<N, _>(builder, tx, hash, height)?;
    info.cumulative_da_bytes_used = info.cumulative_da_bytes_used.saturating_add(tx_da);
    info.cumulative_gas_used += receipt.gas_used;
    // Count the preconf tx's priority fee toward `total_fees` (the payload block
    // value), mirroring the pool best-tx path. Without this, `engine_getPayload`'s
    // `blockValue` and `is_better_payload` ignore preconf-sourced revenue.
    info.total_fees += U256::from(miner_tip) * U256::from(receipt.gas_used);
    Ok(receipt)
}

/// Block-capacity admission + same-sender cascade for a single preconf hash,
/// run **before** dispatching to [`dispatch::apply_one_preconf`] (which drives
/// the [`PreconfTxSet`] entry status machine). This is the one funnel all
/// dispatch paths (carryover, `fifo_rx`, lagged reconcile) go through, so
/// admission policy is applied uniformly and `apply_one_preconf` stays purely
/// "execute an admitted tx + record result".
///
/// Decision:
/// - **same-sender cascade** (Replay only): if a lower-nonce entry from this sender was already
///   deferred / rejected this slot, inherit that outcome (a successor cannot land before its
///   predecessor). Prevents a deferred tx1's successor tx2 from being admitted and then
///   nonce-too-high failing.
/// - **[`preconf_admission`]**: `Admit` → dispatch; `Defer` (Replay, transient capacity) → keep
///   `Waiting`, record the sender block, retry next slot; `Reject` (permanent, or RPC transient) →
///   `mark_canceled` (server pre-apply rejection, not `mark_failed`) + responder error.
///
/// The `info` cumulative reads happen *before* the `&mut info` apply closure
/// is constructed (the values are `u64` copies), so there is no borrow clash.
#[allow(clippy::too_many_arguments)]
async fn admit_and_dispatch<N, B>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    hash: TxHash,
    loop_state: &mut dispatch::LoopState,
    builder: &mut B,
    info: &mut ExecutionInfo,
    limits: BuildConstraints,
) -> Result<(), PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    let Some(entry) = fifo.find_by_hash(&hash).await else { return Ok(()) };
    let (source, sender, nonce) = (entry.source, entry.from, entry.nonce);
    let tx_da = estimated_tx_da_size(&entry.tx);
    let tx_gas = entry.tx.gas_limit();
    drop(entry);

    // (1) Same-sender cascade — Replay entries only. A successor inherits the
    // predecessor's non-admission outcome; it cannot execute before the
    // predecessor lands.
    if source == PreconfSource::Replay &&
        let Some(kind) = loop_state.sender_blocked_at(&sender, nonce)
    {
        match kind {
            dispatch::BlockKind::Defer => {
                metrics::counter!("preconf.fifo.replay_deferred_total").increment(1);
                debug!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?sender, nonce,
                    "replay tx cascade-deferred (predecessor deferred)"
                );
                return Ok(());
            }
            dispatch::BlockKind::Reject => {
                // Server pre-apply rejection (predecessor can't land → nonce
                // gap) — never handed to the builder, so `Canceled`, not
                // `Failed`.
                let _ = fifo.mark_canceled(&hash).await;
                loop_state.record_excluded(
                    hash,
                    PreconfError::BuilderRejected(
                        "preconf predecessor from same sender rejected (nonce gap)".into(),
                    ),
                );
                return Ok(());
            }
        }
    }

    // (2) Block-capacity admission. Reads are `u64` copies → the immutable
    // borrow of `*info` ends before the `&mut info` closure below.
    let da_used = info.cumulative_da_bytes_used;
    let gas_used = info.cumulative_gas_used;
    match preconf_admission(tx_da, tx_gas, da_used, gas_used, limits, source) {
        Admission::Admit => {
            let mut apply_fn =
                |tx, h, height| apply_preconf_with_da::<N, _>(builder, info, limits, tx, h, height);
            // Propagate a fatal apply error to abort the whole build; a
            // per-tx rejection resolves inside `apply_one_preconf` and
            // returns `Ok(())`.
            dispatch::apply_one_preconf(fifo, cfg, hash, loop_state, &mut apply_fn).await?;
        }
        Admission::Defer => {
            loop_state.block_sender(sender, nonce, dispatch::BlockKind::Defer);
            metrics::counter!("preconf.fifo.replay_deferred_total").increment(1);
            debug!(
                target: "mantle::preconf::dispatch",
                ?hash, ?sender, nonce,
                "replay tx deferred (transient block capacity); keeping Waiting for next slot"
            );
        }
        Admission::Reject(e) => {
            loop_state.block_sender(sender, nonce, dispatch::BlockKind::Reject);
            metrics::counter!("preconf.fifo.da_rejected_total").increment(1);
            // Server pre-apply capacity rejection (DA / real block gas) — the
            // tx never reaches the builder, so `Canceled` (like the preconf
            // block-gas-budget gate), not `Failed` (which means the builder ran
            // and rejected it).
            let _ = fifo.mark_canceled(&hash).await;
            if let Some(resp) = fifo.take_responder(&hash).await {
                let _ = resp.send(Err(e.clone()));
            }
            loop_state.record_excluded(hash, e);
        }
    }
    Ok(())
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

/// Preamble that walks the fifo snapshot in insertion order and returns the
/// carryover hashes to (re)dispatch for this build, in order:
///
/// - **`Waiting`** — journal-restored or dead-window RPC pushes whose broadcast never reached this
///   job's subscriber. Returned with the original `source` intact so genuinely stale `Rpc` entries
///   get timed out by the deadline gate.
/// - **`Success`** — stale in-flight from a discarded prior job. A canon'd entry would have been
///   removed by the immediately-preceding [`sync_fifo_forward_to_head`], so any Success reaching
///   here is an un-canon'd in-flight (client already got a receipt; must land).
///   `reset_success_to_waiting` promotes the source to `Replay` so gates bypass and the
///   previously-returned receipt is honored; then the hash is returned for dispatch.
/// - **`Failed` / `Timeout` / `Canceled`** — skipped (terminal).
///
/// The caller dispatches each returned hash through [`admit_and_dispatch`]
/// **before** draining the broadcast / pool arms, so carryover lands ahead of
/// any concurrently-queued fresh RPC pushes. `apply_one_preconf`'s dedup gate
/// prevents double-apply if a carryover hash is also observed via broadcast
/// later. Returning the hash list (rather than applying inline) keeps this
/// helper free of EVM/builder types and unit-testable.
async fn replay_fifo_carryover(fifo: &PreconfTxSet) -> Vec<TxHash> {
    use crate::types::PreconfStatus;
    let mut carryover_hashes = Vec::new();
    for view in fifo.entries().await {
        match view.status {
            PreconfStatus::Waiting => carryover_hashes.push(view.hash),
            PreconfStatus::Success => {
                if fifo.reset_success_to_waiting(&view.hash).await.is_ok() {
                    carryover_hashes.push(view.hash);
                }
            }
            // Terminal for carryover purposes. `Broken` is here because dispatch
            // has already exhausted `preconf_max_apply_attempts` on it —
            // retrying it every subsequent job would spin forever. Only a
            // same-hash resubmit revives it (`push_if_absent`).
            PreconfStatus::Failed |
            PreconfStatus::Timeout |
            PreconfStatus::Canceled |
            PreconfStatus::Broken => {}
        }
    }
    carryover_hashes
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

/// Adaptive-N pool-admission pacer — owns all pool-arm gas-pacing state for
/// one build. Groups the running consumption (`used`), the time-proportional
/// ceiling (`quota`, bumped one `per_batch` per sweep tick, capped at the
/// block gas limit) and the increment, so the select! loop's pool arm reads a
/// single object instead of scattered locals + a `LoopState` counter.
///
/// Distinct from `ExecutionInfo::cumulative_gas_used` (the all-source block
/// total): `used` tracks **only** the pool best-tx arm, so pacing is not
/// perturbed by preconf-tx or deposit gas.
#[derive(Debug)]
struct PoolPacer {
    /// Gas admitted by the pool best-tx arm so far this build.
    used: u64,
    /// Current admission ceiling — pool txs admit while `used < quota`.
    quota: u64,
    /// Per-sweep-tick quota increment (`PoolQuotaSchedule::gas_per_batch`).
    per_batch: u64,
    /// Hard cap the quota is clamped to (the block gas limit).
    block_gas_limit: u64,
}

impl PoolPacer {
    /// Start with a drained quota (`0`) — the pool arm cannot admit until the
    /// first sweep tick raises the ceiling by `per_batch`.
    fn new(per_batch: u64, block_gas_limit: u64) -> Self {
        Self { used: 0, quota: 0, per_batch, block_gas_limit }
    }

    /// Whether the pool arm may admit another tx under the current ceiling.
    fn can_admit(&self) -> bool {
        self.used < self.quota
    }

    /// Record `delta` gas consumed by a just-admitted pool best-tx.
    fn record(&mut self, delta: u64) {
        self.used = self.used.saturating_add(delta);
    }

    /// Raise the admission ceiling by one batch on a sweep tick, clamped to
    /// the block gas limit.
    fn tick(&mut self) {
        self.quota = self.quota.saturating_add(self.per_batch).min(self.block_gas_limit);
    }
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
    classifier: &PreconfClassifier,
    best_txs: &mut impl PayloadTransactions<
        Transaction: PoolTransaction<Consensus = N::SignedTx> + OpPooledTx,
    >,
    builder: &mut Builder,
    info: &mut ExecutionInfo,
    constraints: &BuildConstraints,
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
    // response (responder never called).
    //
    // Skipping here does not drop the tx — it merely constrains it to the
    // preconf ordering. That rests on "the listener creates a fifo entry for
    // every preconf-eligible tx entering the pool", which is **not**
    // unconditional: `push_if_absent` answers `ConflictActive` when a different
    // hash already holds the tx's `(sender, nonce)`, and such an entry gets no
    // fifo record at all. What rules that out is a precondition enforced
    // elsewhere — `PreconfAwareValidator`'s replacement guard refuses a second
    // preconf tx for a `(sender, nonce)` that one already occupies, so a tx the
    // pool accepted cannot collide. Before that guard covered the pool→fifo
    // window the claim was simply false, and the colliding tx was skipped by
    // *both* arms: silently never applied, with no error to its client.
    //
    // One deliberate exception survives: a `Verdict::Promised` tx is exempt from
    // that guard (journal restore must re-admit an acknowledged commitment
    // unconditionally), so it can reach the pool on an occupied nonce and be
    // refused a fifo entry. Losing is the intended outcome there — the fifo's
    // documented policy is to keep the fresher entry — and the restored tx is
    // dropped by the pool itself once the winner's nonce lands.
    //
    // The predicate is the **frozen verdict**, never a live allowlist read: the
    // fifo entry the preconf arm will apply was created by the listener from
    // that same record, so re-deriving eligibility here would let an allowlist
    // update between the two decisions strand the tx with neither arm applying
    // it (Case A / Case B of the classifier design).
    if classifier.verdict(tx.hash()).is_some_and(Verdict::is_preconf) {
        best_txs.mark_invalid(tx.sender(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }
    let interop = tx.interop_deadline();
    let tx_da_size = tx.estimated_da_size();
    let tx = tx.into_consensus();

    if info.is_tx_over_limits(
        tx_da_size,
        constraints.block_gas_limit,
        constraints.tx_da_limit,
        constraints.block_da_limit,
        tx.gas_limit(),
        constraints.da_footprint_gas_scalar,
    ) {
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }

    if tx.is_eip4844() || tx.is_deposit() {
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    }

    if let Some(interop) = interop &&
        !is_valid_interop(interop, constraints.timestamp)
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
    let miner_fee = tx
        .effective_tip_per_gas(constraints.base_fee)
        .expect("fee is always valid; execution succeeded");
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
    ///    - `fifo_rx.recv()` — preconf-tx dispatch (`admit_and_dispatch` per hash on `Ok`; on
    ///      `Lagged` re-scan the fifo snapshot through the same gate; break on `Closed`).
    ///    - **Level-triggered pool arm** (`ready(()) if PoolPacer::can_admit()`) — each fire admits
    ///      exactly one pool best-tx, then returns to `select!`. Cancel and preconf get preempt
    ///      chances between every pool tx via biased priority.
    ///    - `sweep_ticker.tick()` — edge-triggered ticker. Raises the `PoolPacer` ceiling by
    ///      `PoolQuotaSchedule::gas_per_batch` on each tick (adaptive-N derivation adapts `N` to
    ///      remaining slot time so pool aims to fill the block regardless of build delay —
    ///      op-rbuilder flashblocks pattern). Doesn't apply directly; the level-triggered pool arm
    ///      consumes the new headroom.
    ///
    ///    Before the loop, a **carryover replay preamble**
    ///    (`replay_fifo_carryover`) applies any stale in-flight or
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

        // Immutable per-block constraints — snapshotted once, constant across
        // the build. Shared by the preconf admission gate and the pool best-tx
        // arm so both paths enforce one block gas + DA budget.
        let constraints = BuildConstraints {
            block_gas_limit,
            block_da_limit,
            tx_da_limit,
            da_footprint_gas_scalar,
            base_fee,
            timestamp: attrs_timestamp,
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
        if schedule.build_delay_ms > 100 {
            debug!(
                target: "mantle::preconf::payload_builder",
                build_delay_ms = schedule.build_delay_ms,
                time_drift_ms = schedule.time_drift.as_millis() as u64,
                ticks_remaining = schedule.ticks_remaining,
                gas_per_batch = schedule.gas_per_batch,
                "delayed build start; adapting pool quota to remaining slot"
            );
        }

        let mut sweep_ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + schedule.first_offset,
            self.cfg.sweep_interval,
        );
        // Adaptive-N pool admission pacer. Starts with a drained quota (0) —
        // the pool arm cannot admit until the first sweep tick raises the
        // ceiling by `gas_per_batch`; its `can_admit()` guard is a level
        // trigger that self-disables once the current allocation is drained.
        let mut pool_pacer = PoolPacer::new(schedule.gas_per_batch, block_gas_limit);

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

        // Sample the pending-backlog gauge once per build job (~per slot).
        self.fifo.publish_pending_gauge().await;

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
            // Dispatch carryover entries through the admission gate before the
            // select! loop's arms, so they land ahead of any concurrently
            // queued fresh RPC pushes. `admit_and_dispatch` builds the apply
            // closure (which folds gas/DA into `info`) internally per hash.
            for hash in replay_fifo_carryover(&self.fifo).await {
                admit_and_dispatch::<N, _>(
                    &self.fifo,
                    &self.cfg,
                    hash,
                    &mut loop_state,
                    &mut builder,
                    &mut info,
                    constraints,
                )
                .await?;
            }
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
                    match recv {
                        Ok(hash) => {
                            admit_and_dispatch::<N, _>(
                                &self.fifo, &self.cfg, hash, &mut loop_state,
                                &mut builder, &mut info, constraints,
                            )
                            .await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Broadcast overflow — re-scan the fifo snapshot and
                            // run every hash through the admission gate. Dedup
                            // (loop_state) inside `apply_one_preconf` skips any
                            // already committed/excluded this build.
                            warn!(
                                target: "mantle::preconf::dispatch",
                                skipped = n,
                                "fifo broadcast lagged; reconciling via snapshot"
                            );
                            for hash in self.fifo.snapshot().await {
                                admit_and_dispatch::<N, _>(
                                    &self.fifo, &self.cfg, hash, &mut loop_state,
                                    &mut builder, &mut info, constraints,
                                )
                                .await?;
                            }
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
                    && pool_pacer.can_admit() =>
                {
                    let iter = best_txs_iter.as_mut().expect("guard verified Some");
                    let before = info.cumulative_gas_used;
                    match apply_one_best_tx::<N, _>(
                        &self.classifier,
                        iter,
                        &mut builder,
                        &mut info,
                        &constraints,
                    )? {
                        BestTxStep::Continue => {
                            // delta == 0 → tx was filtered (mark_invalid /
                            // nonce-too-low); iterator has advanced. Next
                            // select! iteration re-fires this arm and
                            // pulls the next tx.
                            let delta = info.cumulative_gas_used - before;
                            if delta > 0 {
                                pool_pacer.record(delta);
                            }
                        }
                        BestTxStep::Done => best_txs_iter = None,
                    }
                }
                // Only raises the pacer's ceiling; the pool arm above drains
                // the new headroom on subsequent iterations.
                _ = sweep_ticker.tick() => {
                    pool_pacer.tick();
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
        let classifier = Arc::new(PreconfClassifier::from_config(&cfg));
        let fifo = Arc::new(PreconfTxSet::new(8));
        let builder_config = OpBuilderConfig::default();
        let builder = PreconfPayloadBuilder::new(
            DummyPool,
            DummyClient,
            DummyEvm,
            builder_config,
            cfg.clone(),
            classifier.clone(),
            fifo.clone(),
        );
        assert!(Arc::ptr_eq(builder.cfg(), &cfg));
        assert!(Arc::ptr_eq(builder.classifier(), &classifier));
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

    /// `replay_fifo_carryover` returns `Waiting` + `Success` hashes (each a
    /// carryover source) in insertion order and skips terminal-non-success
    /// statuses (`Failed` / `Timeout` / `Canceled`). `Success` entries are
    /// promoted back to `Waiting` with source `Replay`; `Waiting` entries are
    /// left untouched. Dispatch (admission + apply) is done by the caller.
    #[tokio::test]
    async fn replay_fifo_carryover_plans_waiting_and_success_only() {
        let fifo = PreconfTxSet::new(16);
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

        let carryover_hashes = replay_fifo_carryover(&fifo).await;

        // Only Waiting + Success, in insertion order.
        assert_eq!(carryover_hashes, vec![*t_wait.tx_hash(), *t_succ.tx_hash()]);
        // Waiting entry untouched.
        assert_eq!(
            fifo.find_by_hash(t_wait.tx_hash()).await.unwrap().status,
            PreconfStatus::Waiting,
        );
        // Success entry promoted to Waiting + Replay.
        let succ = fifo.find_by_hash(t_succ.tx_hash()).await.unwrap();
        assert_eq!(succ.status, PreconfStatus::Waiting);
        assert_eq!(succ.source, PreconfSource::Replay);
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

    /// Waiting entries keep their original `source` — the helper only
    /// upgrades source on the `Success → Waiting` reset path, not on entries
    /// that were already `Waiting`. Intentional: Rpc-sourced Waiting entries
    /// must still respect the deadline gate downstream.
    #[tokio::test]
    async fn replay_fifo_carryover_preserves_waiting_source() {
        let fifo = PreconfTxSet::new(16);
        let t_rpc = tx(0xb0, 0);
        let t_journal = tx(0xb1, 0);
        fifo.push_if_absent(t_rpc.clone(), Address::from([1; 20]), PreconfSource::Rpc).await;
        fifo.push_if_absent(t_journal.clone(), Address::from([2; 20]), PreconfSource::Replay).await;

        let carryover_hashes = replay_fifo_carryover(&fifo).await;

        assert_eq!(carryover_hashes, vec![*t_rpc.tx_hash(), *t_journal.tx_hash()]);
        assert_eq!(fifo.find_by_hash(t_rpc.tx_hash()).await.unwrap().source, PreconfSource::Rpc,);
        assert_eq!(
            fifo.find_by_hash(t_journal.tx_hash()).await.unwrap().source,
            PreconfSource::Replay,
        );
    }

    /// Carryover returns entries in fifo insertion order — critical for SLA
    /// determinism vs concurrent RPC pushes that might race the preamble.
    #[tokio::test]
    async fn replay_fifo_carryover_preserves_fifo_order() {
        let fifo = PreconfTxSet::new(16);
        let mut expected = Vec::new();
        for i in 0..3u8 {
            let t = tx(0xc0 + i, 0);
            expected.push(*t.tx_hash());
            fifo.push_if_absent(t, Address::from([i + 1; 20]), PreconfSource::Rpc).await;
            fifo.mark_succeeded(&expected[i as usize]).await.unwrap();
        }

        let carryover_hashes = replay_fifo_carryover(&fifo).await;
        assert_eq!(carryover_hashes, expected, "carryover must respect FIFO insertion order");
    }

    /// Stale `Success` entries are promoted to `Waiting` + `Replay` so the
    /// downstream dispatch bypasses the RPC-only deadline / gas gates and the
    /// previously-returned receipt is honored (SLA: must land).
    #[tokio::test]
    async fn replay_fifo_carryover_promotes_success_to_replay() {
        let fifo = PreconfTxSet::new(16);
        let t = tx(0xd0, 0);
        let hash = *t.tx_hash();
        fifo.push_if_absent(t, Address::from([1; 20]), PreconfSource::Rpc).await;
        fifo.mark_succeeded(&hash).await.unwrap();

        let carryover_hashes = replay_fifo_carryover(&fifo).await;

        assert_eq!(carryover_hashes, vec![hash]);
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Waiting);
        assert_eq!(entry.source, PreconfSource::Replay);
    }

    /// Empty fifo — helper returns an empty list, no error.
    #[tokio::test]
    async fn replay_fifo_carryover_on_empty_fifo_is_noop() {
        let fifo = PreconfTxSet::new(16);
        assert!(replay_fifo_carryover(&fifo).await.is_empty());
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

    // ============ preconf_admission (block-capacity gate) ============
    //
    // Pure-function tests for the pre-dispatch admission gate. Classifies a tx as
    // Admit / Defer / Reject via the "fits an empty block?" rule. This is the
    // single DA + block-gas gate — the former `preconf_da_check` was a dead
    // subset of it (see `apply_preconf_with_da`), so its DA bounds are exercised
    // here too.

    const BLOCK_GAS: u64 = 30_000_000;

    fn da_limits(block: Option<u64>, per_tx: Option<u64>, scalar: Option<u16>) -> BuildConstraints {
        BuildConstraints {
            block_gas_limit: BLOCK_GAS,
            block_da_limit: block,
            tx_da_limit: per_tx,
            da_footprint_gas_scalar: scalar,
            base_fee: 0,
            timestamp: 0,
        }
    }

    /// A tx well within all DA + gas headroom → `Admit`, both sources.
    #[test]
    fn preconf_admission_within_headroom_admits() {
        let limits = da_limits(Some(10_000), Some(1_000), None);
        for src in [PreconfSource::Replay, PreconfSource::Rpc] {
            assert!(
                matches!(preconf_admission(100, 21_000, 0, 0, limits, src), Admission::Admit),
                "src={src:?}"
            );
        }
    }

    /// Per-tx DA over-limit is permanent (empty block can't hold it) →
    /// `Reject` for both sources, never `Defer`.
    #[test]
    fn preconf_admission_per_tx_da_over_is_permanent_reject() {
        let limits = da_limits(Some(10_000), Some(1_000), None);
        for src in [PreconfSource::Replay, PreconfSource::Rpc] {
            match preconf_admission(1_001, 21_000, 0, 0, limits, src) {
                Admission::Reject(PreconfError::DaLimitExceeded { limit, .. }) => {
                    assert_eq!(limit, 1_000);
                }
                other => panic!("src={src:?}: expected Reject(DaLimitExceeded), got {other:?}"),
            }
        }
    }

    /// tx gas limit above the real block gas limit is permanent → `Reject`
    /// for both sources.
    #[test]
    fn preconf_admission_gas_over_block_limit_is_permanent_reject() {
        let limits = da_limits(None, None, None); // BLOCK_GAS = 30_000_000
        for src in [PreconfSource::Replay, PreconfSource::Rpc] {
            match preconf_admission(100, BLOCK_GAS + 1, 0, 0, limits, src) {
                Admission::Reject(PreconfError::BuilderRejected(_)) => {}
                other => panic!("src={src:?}: expected Reject(BuilderRejected), got {other:?}"),
            }
        }
    }

    /// Transient DA (tx fits an empty block, but cumulative overflows the
    /// per-block limit): `Replay` → `Defer`, `Rpc` → `Reject`.
    #[test]
    fn preconf_admission_transient_da_defers_replay_rejects_rpc() {
        let limits = da_limits(Some(1_000), Some(1_000), None);
        // tx_da 200 ≤ per-tx & block limits (fits empty block), but
        // da_used 900 + 200 = 1100 > 1000 block limit (current block full).
        assert!(matches!(
            preconf_admission(200, 21_000, 900, 0, limits, PreconfSource::Replay),
            Admission::Defer
        ));
        assert!(matches!(
            preconf_admission(200, 21_000, 900, 0, limits, PreconfSource::Rpc),
            Admission::Reject(_)
        ));
    }

    /// Transient gas (tx fits an empty block, but cumulative overflows the
    /// real block gas limit): `Replay` → `Defer`, `Rpc` → `Reject`.
    #[test]
    fn preconf_admission_transient_gas_defers_replay_rejects_rpc() {
        let limits = da_limits(None, None, None); // BLOCK_GAS = 30_000_000
        let tx_gas = 2_000_000; // ≤ block gas (fits empty block)
        let gas_used = BLOCK_GAS - 1_000_000; // remaining 1M < 2M needed
        assert!(matches!(
            preconf_admission(100, tx_gas, 0, gas_used, limits, PreconfSource::Replay),
            Admission::Defer
        ));
        assert!(matches!(
            preconf_admission(100, tx_gas, 0, gas_used, limits, PreconfSource::Rpc),
            Admission::Reject(_)
        ));
    }

    /// Boundary: cumulative exactly at the block limit still `Admit`s (gate
    /// uses strict `>` for over-limit).
    #[test]
    fn preconf_admission_cumulative_boundary_admits() {
        let limits = da_limits(Some(1_000), Some(1_000), None);
        // 900 + 100 == 1000 block limit → fits.
        assert!(matches!(
            preconf_admission(100, 21_000, 900, 0, limits, PreconfSource::Replay),
            Admission::Admit
        ));
    }

    // ============ PoolPacer (Adaptive-N pool admission pacing) ============

    /// Quota starts drained (0): no admission until the first sweep tick
    /// raises the ceiling by `per_batch`.
    #[test]
    fn pool_pacer_starts_drained_then_first_tick_opens_admission() {
        let mut p = PoolPacer::new(1_000, 5_000);
        assert!(!p.can_admit(), "quota starts at 0 → cannot admit before first tick");
        p.tick();
        assert!(p.can_admit(), "after one tick quota=1000 > used=0 → can admit");
    }

    /// `record` accumulates consumption; admission gates on `used < quota`
    /// (strict — boundary `used == quota` cannot admit, matching the old
    /// `pool_gas_used < pool_quota` guard).
    #[test]
    fn pool_pacer_record_accumulates_and_gates_admission() {
        let mut p = PoolPacer::new(1_000, 5_000);
        p.tick(); // quota = 1000
        p.record(600);
        assert!(p.can_admit(), "used 600 < quota 1000");
        p.record(400);
        assert!(!p.can_admit(), "used 1000 == quota 1000 → cannot admit (strict <)");
    }

    /// Ticks raise the ceiling by `per_batch` but never past the block gas
    /// limit.
    #[test]
    fn pool_pacer_tick_clamps_to_block_gas_limit() {
        let mut p = PoolPacer::new(4_000, 5_000);
        p.tick();
        assert_eq!(p.quota, 4_000);
        p.tick(); // 8000 → clamp to 5000
        assert_eq!(p.quota, 5_000);
        p.tick(); // stays clamped
        assert_eq!(p.quota, 5_000);
    }

    /// `record` saturates rather than overflowing.
    #[test]
    fn pool_pacer_record_saturates() {
        let mut p = PoolPacer::new(1, 1);
        p.record(u64::MAX);
        p.record(u64::MAX);
        assert_eq!(p.used, u64::MAX);
    }
}
