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

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::{
    BlockHeader, Sealable, Transaction, TxEnvelope, TxReceipt, Typed2718, transaction::Recovered,
};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_evm::{
    Evm,
    block::{BlockExecutor as _, TxResult},
};
use alloy_primitives::{Address, B256, Sealed, TxHash, U256};
use mantle_reth_flashblocks_types::FlashblockId;
use op_alloy_consensus::{SDMGasEntry, TxPostExec, build_post_exec_tx};
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use op_revm::{L1BlockInfo, constants::L1_BLOCK_CONTRACT};
use reth_basic_payload_builder::BuildArguments;
use reth_evm::execute::{
    BlockAssembler as _, BlockBuilder, BlockBuilderOutcome, BlockExecutionError,
    BlockValidationError,
};
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult, ChangedAccount};
use reth_optimism_evm::{ConfigurePostExecEvm, PostExecExecutorExt};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_payload_builder::{
    OpAttributes, OpPayloadPrimitives, builder::OpPayloadBuilderCtx, config::OpBuilderConfig,
    error::OpPayloadBuilderError,
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
use reth_primitives_traits::{
    Block as _, BlockBody as _, BlockTy, HeaderTy, SealedBlock, SignedTransaction, TxTy,
    WithEncoded,
};
use reth_revm::{
    State, cancelled::CancelOnDrop, context::Block as RevmBlockTrait,
    database::StateProviderDatabase, db::BundleState,
};
use reth_transaction_pool::{BestTransactions, PoolTransaction};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    PreconfClassifier, PreconfConfig, PreconfTxSet,
    apply::{ApplyError, apply_preconf_tx},
    builder::{
        ExecutionInfo,
        cancel::{CancelReason, JobCancel},
        dispatch,
        pacing::{AdmissionPacer, allowances_due, derive_pool_quota_schedule},
    },
    classifier::{Verdict, Whitelist},
    flashblocks::{
        BlockInvariants, FlashblocksProducer, SenderBalances, SliceHeader, SliceLimits,
        build_flashblock, derive_slice_schedule, maintain_pool_at_slice_boundary,
    },
    journal::PreconfJournal,
    preconf_tx_set::TxEntryView,
    types::{PreconfError, PreconfReceipt, PreconfSource},
    whitelist::{WHITELIST_UPDATED_TOPIC0, WhitelistDelta, decode_whitelist_update},
};

/// Stage 2 — the sequencer transactions (deposits, L1 info, system txs), plus
/// the whitelist updates any of them carried.
///
/// **Replicated from `OpPayloadBuilderCtx::execute_sequencer_transactions`**
/// (`op-reth/crates/payload/src/builder.rs:724-765` as of this fork), with
/// `execute_transaction` → `execute_transaction_with_result_closure` so the
/// per-transaction `ExecutionResult` becomes visible. Upstream changes to that
/// function will **not** reach this copy.
///
/// The result is needed because a governance whitelist update arrives as a
/// deposit addressed straight at the whitelist contract (see
/// [`crate::whitelist`]), and it has to be recognised here to bind *this*
/// block's preconf transactions. Calldata alone would be wrong: it says nothing
/// about whether the contract's `onlyL1Gov` gate passed, and a deposit whose L2
/// execution reverted still sits in the block looking exactly like one that
/// succeeded. The emitted [`WHITELIST_UPDATED_TOPIC0`] answers that, since the
/// contract only reaches the `emit` after authorisation passed and every rule
/// applied — so the log decides *whether*, and the calldata is decoded
/// afterwards to learn *what*.
///
/// Returns the deltas in block order; applying them out of order would let an
/// add-then-remove of the same rule resolve backwards.
fn execute_sequencer_transactions_watching_whitelist<N, B>(
    sequencer_txs: &[WithEncoded<TxTy<N>>],
    builder: &mut B,
    whitelist_contract: Option<Address>,
) -> Result<(ExecutionInfo<N::SignedTx>, Vec<WhitelistDelta>), PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    B: BlockBuilder<Primitives = N>,
{
    let mut info = ExecutionInfo::default();
    let mut deltas = Vec::new();

    for sequencer_tx in sequencer_txs {
        // A sequencer's block should never contain blob transactions.
        if sequencer_tx.value().is_eip4844() {
            return Err(PayloadBuilderError::other(OpPayloadBuilderError::BlobTransactionRejected));
        }

        // Deposit transactions have no signature, so this pulls in `from`
        // rather than recovering it.
        let recovered = sequencer_tx.value().try_clone_into_recovered().map_err(|_| {
            PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
        })?;

        let mut emitted_whitelist_update = false;
        let gas_used =
            match builder.execute_transaction_with_result_closure(recovered.clone(), |res| {
                let Some(contract) = whitelist_contract else { return };
                let result = &res.result().result;
                // A reverted or halted transaction wrote nothing, and its logs
                // are discarded on chain — so neither is trusted here.
                if !result.is_success() {
                    return;
                }
                emitted_whitelist_update = result.logs().iter().any(|log| {
                    log.address == contract &&
                        log.topics().first() == Some(&WHITELIST_UPDATED_TOPIC0)
                });
            }) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    debug!(
                        target: "mantle::preconf::payload_builder",
                        %error, ?recovered,
                        "error in sequencer transaction, skipping"
                    );
                    continue;
                }
                Err(err) => return Err(PayloadBuilderError::EvmExecutionError(Box::new(err))),
            };

        if emitted_whitelist_update && let Some(contract) = whitelist_contract {
            let tx = sequencer_tx.value();
            match decode_whitelist_update(tx.to(), tx.input(), contract) {
                Some(delta) if !delta.is_empty() => {
                    debug!(
                        target: "mantle::preconf::payload_builder",
                        add = delta.add.len(), remove = delta.remove.len(),
                        "whitelist update in this block; applying to the build-scoped allowlist"
                    );
                    deltas.push(delta);
                }
                // The contract emitted, so an update certainly happened — we
                // just could not read it. Loud, because the effect is that this
                // block judges its preconf transactions against the pre-update
                // policy while the chain has already moved on. Not fatal: the
                // canonical watcher re-reads the real lists once the block
                // lands, so the divergence lasts one block.
                _ => warn!(
                    target: "mantle::preconf::payload_builder",
                    %contract,
                    "whitelist update emitted but its calldata did not decode; \
                     this block keeps the previous allowlist"
                ),
            }
        }

        info.cumulative_gas_used += gas_used.tx_gas_used();
        info.record(recovered);
    }

    Ok((info, deltas))
}

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
    /// Commitment journal. `None` only on the disabled path, which builds no
    /// preconf transactions and so has nothing to persist — `node.rs` skips
    /// opening a file there because the default config leaves its path unset.
    journal: Option<Arc<PreconfJournal>>,
    /// Slice publishing, when it is switched on. `None` leaves the build loop
    /// behaving exactly as it did before slicing existed, which is what makes
    /// turning flashblocks off a real rollback rather than a second code path.
    flashblocks: Option<FlashblocksProducer>,
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
        Self {
            pool,
            client,
            evm_config,
            builder_config,
            cfg,
            classifier,
            fifo,
            journal: None,
            flashblocks: None,
        }
    }

    /// Persist preconf commitments and, with slicing on, the pool transactions
    /// a slice carries.
    ///
    /// Absent only on the disabled path, which builds no preconf transactions
    /// and so has nothing to persist — `node.rs` skips opening a file there
    /// because the default config leaves its path unset.
    pub fn with_journal(mut self, journal: Arc<PreconfJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Publish a slice of the block being built on every tick.
    pub fn with_flashblocks(mut self, producer: FlashblocksProducer) -> Self {
        self.flashblocks = Some(producer);
        self
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
) -> Result<(PreconfReceipt, Recovered<N::SignedTx>), ApplyError>
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
    let receipt = apply_preconf_tx(builder, recovered.clone(), hash, height)?;

    Ok((receipt, recovered))
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
    info: &mut ExecutionInfo<N::SignedTx>,
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
    let (receipt, recovered) = convert_and_apply_preconf::<N, _>(builder, tx, hash, height)?;
    info.cumulative_da_bytes_used = info.cumulative_da_bytes_used.saturating_add(tx_da);
    info.cumulative_gas_used += receipt.gas_used;
    info.record(recovered);
    // Count the preconf tx's priority fee toward `total_fees` (the payload block
    // value), mirroring the pool best-tx path. Without this, `engine_getPayload`'s
    // `blockValue` and `is_better_payload` ignore preconf-sourced revenue.
    info.total_fees += U256::from(miner_tip) * U256::from(receipt.gas_used);
    Ok(receipt)
}

/// Whether the allowlist in force for this block bars `from -> to` from the
/// preconf path.
///
/// Asked of **every** entry, against the allowlist pinned when this block's
/// build began plus whatever governance update the block itself carried —
/// policy may have moved at any point between admission and build, not only
/// inside this block. The frozen verdict cannot answer it: that records
/// eligibility as of admission, not whether policy still authorizes the tx.
///
/// `all_preconfs` must be checked **ahead of** the lists: that mode never reads
/// the contract (`whitelist::wants_whitelist` returns `None`), so all three sets
/// stay empty and consulting them would bar every transaction on the node.
///
/// Only revocations can bind a block, structurally rather than by omission: a
/// newly-authorized sender's transaction was refused at the RPC door and never
/// entered the fifo, so there is nothing here for the new policy to admit.
///
/// [`PreconfSource::Replay`] is exempt — its receipt has already gone out, and a
/// commitment already acknowledged to a client is owed whatever governance later
/// decides. Same predicate `rpc.rs`'s deadline branch uses to refuse to time
/// such an entry out.
fn barred_by_allowlist(
    cfg: &PreconfConfig,
    whitelist: &Whitelist,
    source: PreconfSource,
    from: &Address,
    to: Option<&Address>,
) -> bool {
    !cfg.all_preconfs && source != PreconfSource::Replay && !whitelist.is_eligible(from, to)
}

/// Block-capacity admission + same-sender cascade for a single preconf hash,
/// run **before** dispatching to [`dispatch::apply_one_preconf`] (which drives
/// the [`PreconfTxSet`] entry status machine). This is the one funnel all
/// dispatch paths (carryover, `fifo_rx`, lagged reconcile) go through, so
/// admission policy is applied uniformly and `apply_one_preconf` stays purely
/// "execute an admitted tx + record result".
///
/// Three gates in order — [`barred_by_allowlist`], the same-sender cascade
/// (Replay only), then [`preconf_admission`]. Each is documented on its own
/// branch in the body.
///
/// The `info` cumulative reads happen *before* the `&mut info` apply closure
/// is constructed (the values are `u64` copies), so there is no borrow clash.
#[allow(clippy::too_many_arguments)]
async fn admit_and_dispatch<N, B>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    journal: Option<&PreconfJournal>,
    hash: TxHash,
    loop_state: &mut dispatch::LoopState,
    builder: &mut B,
    info: &mut ExecutionInfo<N::SignedTx>,
    limits: BuildConstraints,
    whitelist: &Whitelist,
) -> Result<(), PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    N::SignedTx: TryFrom<TxEnvelope>,
    B: BlockBuilder<Primitives = N>,
{
    let Some(entry) = fifo.find_by_hash(&hash).await else { return Ok(()) };
    let (source, sender, nonce) = (entry.source, entry.from, entry.nonce);
    let to = crate::rpc::tx_kind_to_address(entry.tx.kind());
    let tx_da = estimated_tx_da_size(&entry.tx);
    let tx_gas = entry.tx.gas_limit();
    drop(entry);

    // (1) Policy no longer authorizes this sender.
    if barred_by_allowlist(cfg, whitelist, source, &sender, to.as_ref()) {
        metrics::counter!("preconf.whitelist.revoked_total").increment(1);
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash, ?sender, ?to,
            "allowlist no longer authorizes this sender; rejecting before the commitment is applied"
        );
        // `Canceled`, not `Failed`: the builder never saw it, which is the same
        // shape as the capacity rejection below. It also stays revivable by a
        // same-hash resubmit, so a client whose authorization is restored can
        // retry without a new transaction.
        let _ = fifo.mark_canceled(&hash).await;
        if let Some(resp) = fifo.take_responder(&hash).await {
            let _ = resp.send(Err(PreconfError::NotPreconfEligible));
        }
        loop_state.record_excluded(hash, PreconfError::NotPreconfEligible);
        return Ok(());
    }

    // (2) Same-sender cascade — Replay entries only. A successor inherits the
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

    // (3) Block-capacity admission. Reads are `u64` copies → the immutable
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
            dispatch::apply_one_preconf(fifo, cfg, journal, hash, loop_state, &mut apply_fn)
                .await?;
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
/// Reads the senders holding entries, asks the parent-block state for each of
/// their nonces, and drops everything those nonces have passed in one sweep.
/// Idempotent — a sender with no entries, or none below its on-chain nonce,
/// costs nothing.
///
/// Both halves are deliberately whole-fifo rather than per-sender. Asking for
/// the senders alone avoids cloning a view of every entry, and one sweep avoids
/// re-reading every entry once per sender — which is nothing at the handful of
/// commitments preconf alone holds, and quadratic at the tens of thousands a
/// journal replay can restore.
async fn sync_fifo_forward_to_head<S>(fifo: &PreconfTxSet, state_provider: &S)
where
    S: reth_storage_api::StateProvider + ?Sized,
{
    let heads: HashMap<Address, u64> = fifo
        .senders()
        .await
        .into_iter()
        .map(|sender| {
            let nonce = state_provider.account_nonce(&sender).ok().flatten().unwrap_or(0);
            (sender, nonce)
        })
        .collect();
    fifo.forward_all(&heads).await;
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
/// The caller dispatches each hash through [`admit_and_dispatch`] **before**
/// draining the broadcast / pool arms, so carryover lands ahead of any
/// concurrently-queued fresh RPC pushes.
/// `apply_one_preconf`'s dedup gate prevents double-apply if a carryover hash is
/// also observed via broadcast later. Returning a plan (rather than applying
/// inline) keeps this helper free of EVM/builder types and unit-testable.
///
async fn replay_fifo_carryover(fifo: &PreconfTxSet) -> Vec<TxHash> {
    use crate::types::PreconfStatus;
    let mut carryover: Vec<TxEntryView> = Vec::new();
    for view in fifo.entries().await {
        match view.status {
            PreconfStatus::Waiting => carryover.push(view),
            PreconfStatus::Success => {
                if fifo.reset_success_to_waiting(&view.hash).await.is_ok() {
                    carryover.push(view);
                }
            }
            // Terminal for carryover purposes: dispatch gave up on these after
            // the apply was rejected, so retrying them every subsequent job
            // would spin forever. Only a same-hash resubmit revives any of them
            // (`push_if_absent`).
            PreconfStatus::Failed | PreconfStatus::Timeout | PreconfStatus::Canceled => {}
        }
    }
    ordered_for_dispatch(&carryover)
}

/// Order carried-over commitments: each sender's by nonce, senders in the order
/// they first appear.
///
/// The fifo records the order commitments were made, and that order decides who
/// lands when a block fills up — so senders keep it. Sorting by address instead
/// would hand a standing advantage to whoever holds the lower one.
///
/// What the fifo cannot record is that one sender's transactions execute only
/// in nonce order. Its own order is arrival order, and for entries restored from
/// the journal that is the order two writers with different latencies happened
/// to leave them in: a commitment is recorded from its apply, a pool transaction
/// from the slice that carries it, so one sender's consecutive nonces can land
/// in the file the wrong way round. Dispatched that way the higher nonce is
/// rejected as invalid, and for a carried-over entry that rejection is terminal
/// — a commitment broken with no second attempt.
///
/// This is also what orders a recovered pool transaction against a commitment
/// from the same sender: the unlanded index hands its survivors to the fifo
/// before the preamble reads it, so by here there is one source, not two.
fn ordered_for_dispatch(carryover: &[TxEntryView]) -> Vec<TxHash> {
    let mut first_seen: HashMap<Address, usize> = HashMap::new();
    let mut ordered: Vec<(usize, u64, TxHash)> = Vec::with_capacity(carryover.len());
    for (position, view) in carryover.iter().enumerate() {
        let sender_rank = *first_seen.entry(view.from).or_insert(position);
        ordered.push((sender_rank, view.nonce, view.hash));
    }
    // Stable, so two entries claiming one (sender, nonce) keep the order the
    // fifo had them in — which of them wins is not this function's to decide.
    ordered.sort_by_key(|&(sender_rank, nonce, _)| (sender_rank, nonce));
    ordered.into_iter().map(|(_, _, hash)| hash).collect()
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
    info: &mut ExecutionInfo<N::SignedTx>,
    constraints: &BuildConstraints,
    pacer: &mut AdmissionPacer,
) -> Result<BestTxStep, PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    Builder: BlockBuilder<Primitives = N>,
{
    let Some(tx) = best_txs.next(()) else {
        return Ok(BestTxStep::Done);
    };
    // Preconf-eligible txs are applied EXCLUSIVELY via the preconf arm. Without
    // this filter the pool arm could grab one whose fifo entry the async
    // listener has not pushed yet: the tx lands via the pool path while its
    // client sees Timeout/Failed, responder never called.
    //
    // Skipping does not drop the tx, it only constrains it to the preconf
    // ordering — which holds because `PreconfAwareValidator`'s replacement guard
    // refuses a second preconf tx on a `(sender, nonce)` one already occupies,
    // so a tx the pool accepted cannot collide and be refused a fifo entry
    // (`push_if_absent` → `ConflictActive`).
    //
    // One exception: a `Verdict::Promised` tx is exempt from that guard (journal
    // restore re-admits an acknowledged commitment unconditionally), so it can
    // lose the fifo slot. Intended — the fifo keeps the fresher entry, and the
    // pool drops the restored tx once the winner's nonce lands.
    //
    // The predicate is the **frozen verdict**, never a live allowlist read:
    // re-deriving eligibility here would let an allowlist update between the two
    // decisions strand the tx with neither arm applying it (Case A / Case B of
    // the classifier design).
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
        // The block's own ceilings, not a slice's — this one says the block is
        // full, where the allowance rejection below says only that this slice is.
        metrics::counter!("preconf.build.tx_over_block_limits_total").increment(1);
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

    // Charged before running, at the gas the transaction declares, because
    // what it actually burns is not known until it has. Whatever it does not
    // use comes back on settle.
    let Some(ticket) = pacer.reserve(tx.gas_limit(), tx_da_size) else {
        // Too big for what is left of this slice's allowance. Skipped rather
        // than treated as the end of the arm: stopping here would hold back
        // every smaller transaction behind it for the rest of the tick, and a
        // transaction declaring more gas than one slice's whole allowance would
        // hold them back for the rest of the block. The iterator is rebuilt at
        // the next boundary, so the skip lasts one slice and this transaction
        // gets another chance against a larger allowance.
        //
        // The one rejection slicing introduces, and the one to watch: a rate
        // that climbs means the per-slice allowance is holding transactions
        // back that the block itself had room for.
        metrics::counter!("flashblock.slice_allowance_exhausted_total").increment(1);
        best_txs.mark_invalid(tx.signer(), tx.nonce());
        return Ok(BestTxStep::Continue);
    };

    let recovered = tx.clone();
    let gas_used = match builder.execute_transaction(tx.clone()) {
        Ok(g) => g,
        Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. })) => {
            pacer.cancel(ticket);
            // A nonce already used is the pool being behind the block, which
            // the next boundary corrects; anything else is the transaction.
            if error.is_nonce_too_low() {
                metrics::counter!("preconf.build.tx_nonce_already_used_total").increment(1);
            } else {
                metrics::counter!("preconf.build.tx_rejected_by_evm_total").increment(1);
                best_txs.mark_invalid(tx.signer(), tx.nonce());
            }
            return Ok(BestTxStep::Continue);
        }
        Err(err) => {
            pacer.cancel(ticket);
            return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
        }
    };
    let tx_gas_used = gas_used.tx_gas_used();
    pacer.settle(ticket, tx_gas_used);
    info.cumulative_gas_used += tx_gas_used;
    info.cumulative_da_bytes_used += tx_da_size;
    let miner_fee = tx
        .effective_tip_per_gas(constraints.base_fee)
        .expect("fee is always valid; execution succeeded");
    info.total_fees += U256::from(miner_fee) * U256::from(tx_gas_used);
    // The one class of transaction nothing else brings back after a restart:
    // deposits arrive with the attributes, a preconf commitment is journaled
    // when its receipt goes out, and the post-execution transaction is made by
    // the executor rather than sent by anyone.
    info.record_journalable(recovered);
    Ok(BestTxStep::Continue)
}

/// Assemble and seal the block as it stands, so a slice's header fields come
/// from the same code that seals the final one.
///
/// Reuses the node's own block assembler rather than reassembling a header by
/// hand: every field then comes from the code that seals blocks, so a slice
/// cannot drift from the block it is a prefix of. The state root is left at
/// zero — computing it per slice is the one cost slicing deliberately does not
/// pay — which means the hash identifies the slice's contents rather than the
/// block that will eventually be sealed.
///
/// The block is a throwaway: nothing here touches the executor, so it can be
/// called as often as a slice is published.
fn seal_block_so_far<N, Evm, ChainSpec, Attrs, B>(
    ctx: &OpPayloadBuilderCtx<Evm, ChainSpec, Attrs>,
    builder: &mut B,
    info: &ExecutionInfo<N::SignedTx>,
    state_provider: &dyn reth_storage_api::StateProvider,
    da_footprint_gas_scalar: Option<u16>,
) -> Result<SealedBlock<BlockTy<N>>, PayloadBuilderError>
where
    N: OpPayloadPrimitives,
    Evm: ConfigurePostExecEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<N>, ChainSpec>,
        >,
    ChainSpec: reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
    Attrs: OpAttributes<Transaction = TxTy<N>>,
    B: BlockBuilder<Primitives = N, Executor: alloy_evm::block::BlockExecutor>,
{
    let next_env_ctx = Evm::NextBlockEnvCtx::build_next_env(
        ctx.attributes(),
        ctx.parent(),
        ctx.chain_spec.as_ref(),
    )
    .map_err(PayloadBuilderError::other)?;
    let evm_env = ctx
        .evm_config
        .next_evm_env(ctx.parent(), &next_env_ctx)
        .map_err(PayloadBuilderError::other)?;
    let execution_ctx = ctx
        .evm_config
        .context_for_next_block(ctx.parent(), next_env_ctx)
        .map_err(PayloadBuilderError::other)?;

    let receipts = builder.executor().receipts().to_vec();
    // The executor stamps its running gas into every receipt, so the last one
    // holds its current total — the same number the sealed header will carry.
    // Read from there rather than from `info`, whose tally depends on every
    // execution site remembering to fold its gas in.
    let gas_used = receipts.last().map_or(0, |receipt| receipt.cumulative_gas_used());

    let output = BlockExecutionResult {
        receipts,
        requests: Default::default(),
        gas_used,
        // Post-Jovian the assembler puts this in the header as the block's DA
        // footprint. The executor accumulates the same figure and hands it to
        // no one, so it is recomputed here from what the build already tracks
        // for its own limit checks — the same two numbers, multiplied the same
        // way, as the executor's per-transaction estimates summed: the scalar
        // is a per-block constant, and multiplication distributes over the sum.
        // Deposits are excluded on both sides, which is what keeps the two
        // totals the same rather than merely close.
        //
        // Before Jovian the assembler ignores whatever is passed here.
        blob_gas_used: da_footprint_gas_scalar
            .map_or(0, |scalar| info.cumulative_da_bytes_used.saturating_mul(u64::from(scalar))),
    };
    // One clone of the block's transactions per slice: the assembler takes them
    // owned. Bounded by the assembler's signature rather than by choice, and
    // the reason `flashblock.tick_duration` is worth watching on full blocks.
    let transactions = info.executed().iter().map(|tx| tx.inner().clone()).collect::<Vec<_>>();

    let block = ctx
        .evm_config
        .block_assembler()
        .assemble_block(reth_evm::execute::BlockAssemblerInput::new(
            evm_env,
            execution_ctx,
            ctx.parent(),
            transactions,
            &output,
            // Empty, and post-Isthmus that is wrong rather than merely
            // approximate: the assembler reads a bundle only to derive the
            // `L2ToL1MessagePasser` storage root, which `storage_root` computes
            // as the current state plus whatever updates it is handed — so an
            // empty bundle reports the parent's root, while every withdrawal in
            // the block moves the real one. Slices of a block containing
            // withdrawals therefore carry a root the sealed block will not have.
            //
            // The reference implementation passes its live bundle here, which
            // reads as the line to copy and is not: a bundle only holds what
            // `merge_transitions` has folded into it, and slicing never merges
            // — doing so a second time appends an empty revert block and
            // truncates the real one, which is what `slice_state_invariants`
            // pins. Ours would be as empty as this one. Getting the figure
            // means assembling it from the pending transitions instead, for
            // this one predeploy, without touching the bundle.
            //
            // Left until a consumer is known to read the field. Until then it
            // buys one header field on blocks that contain withdrawals, at the
            // price of code that walks state revm expects to own; the rest of
            // the slice header — transactions, receipts, gas — is exact
            // regardless.
            &BundleState::default(),
            state_provider,
            B256::ZERO,
        ))
        .map_err(PayloadBuilderError::other)?;

    Ok(block.seal_slow())
}

/// The header fields that change from one slice to the next.
fn slice_header<B: reth_primitives_traits::Block>(sealed: &SealedBlock<B>) -> SliceHeader {
    let header = sealed.header();
    SliceHeader {
        gas_used: header.gas_used(),
        receipts_root: header.receipts_root(),
        logs_bloom: header.logs_bloom(),
        block_hash: sealed.hash(),
        blob_gas_used: header.blob_gas_used(),
    }
}

/// The header fields that hold for the whole block.
///
/// Taken from the assembled header rather than gathered from the build context:
/// every field then means exactly what it will mean in the sealed block, and
/// there is no second place to keep in step. Read once per block and reused —
/// re-deriving them per slice would make "these do not change" a coincidence
/// rather than a guarantee, and pays for a predeploy storage read every tick.
fn block_invariants<B: reth_primitives_traits::Block>(sealed: &SealedBlock<B>) -> BlockInvariants {
    let header = sealed.header();
    BlockInvariants {
        base: OpFlashblockPayloadBase {
            parent_beacon_block_root: header.parent_beacon_block_root().unwrap_or_default(),
            parent_hash: header.parent_hash(),
            fee_recipient: header.beneficiary(),
            prev_randao: header.mix_hash().unwrap_or_default(),
            block_number: header.number(),
            gas_limit: header.gas_limit(),
            timestamp: header.timestamp(),
            extra_data: header.extra_data().clone(),
            base_fee_per_gas: U256::from(header.base_fee_per_gas().unwrap_or_default()),
        },
        withdrawals: sealed.body().withdrawals().cloned().unwrap_or_default().into_inner(),
        withdrawals_root: header.withdrawals_root().unwrap_or_default(),
    }
}

/// Sender balances for pool maintenance, read from the parent state.
///
/// The live mid-block balances are not reachable from here — the block builder
/// holds the state for the whole build — so a sender who has already spent in
/// this block reads higher than they now are. Only ever higher, which makes it
/// a cost rather than a hazard: too much balance admits a transaction that
/// execution then refuses, too little would withhold a good one.
///
/// Measured: of three transfers spending a third of a balance each, the last
/// falls short by the gas the first two burned, and the pool — told the parent
/// balance at every boundary — keeps it pending. The arm executes it, is
/// refused, returns the allowance it reserved, counts
/// `preconf.build.tx_rejected_by_evm_total`, and carries on. It takes a slice
/// boundary between them to happen at all; within one slice the pool iterator
/// tracks the spending itself.
///
/// Both ways to close it cost more than that one wasted execution: reaching
/// the executor's mid-block state means restructuring who holds state for the
/// build, and tallying spending here means a second copy of the EVM's fee
/// rules, which would drift.
struct ProviderBalances<'a, P: ?Sized>(&'a P);

impl<P: reth_storage_api::AccountReader + ?Sized> SenderBalances for ProviderBalances<'_, P> {
    fn balance_of(&self, address: Address) -> Option<U256> {
        self.0.basic_account(&address).ok().flatten().map(|account| account.balance)
    }
}

/// Everything slicing carries from one tick to the next.
///
/// Held for the life of one block: the block-level fields a subscriber needs
/// are established once, and the index and predecessor pointer are what let a
/// subscriber tell a gap from a fresh start.
struct SliceState {
    producer: FlashblocksProducer,
    /// Post-Jovian DA footprint scalar, or `None` before Jovian.
    ///
    /// A per-block constant read once by the build loop, carried here rather
    /// than re-read per slice: it turns the DA bytes the build has accumulated
    /// into the footprint a Jovian header reports.
    da_footprint_gas_scalar: Option<u16>,
    /// When this block's build began, for the first slice's offset.
    opened_at: std::time::Instant,
    /// When the previous slice went out, for the gap to the next.
    last_published: Option<std::time::Instant>,
    /// Established by the first slice of this block, then reused.
    invariants: Option<BlockInvariants>,
    next_index: u64,
    previous: FlashblockId,
}

impl SliceState {
    fn new(producer: &FlashblocksProducer, da_footprint_gas_scalar: Option<u16>) -> Self {
        // The predecessor of this block's first slice is the last slice of the
        // previous block, which a block-scoped builder does not know — so it
        // comes from what the publisher has actually sent. Only a producer that
        // has published nothing has no predecessor, and the sentinel is how a
        // subscriber tells that fresh start from a gap it should go and fill.
        //
        // Rebuilding a block points the new first slice at the last slice of
        // the attempt being replaced. Deliberate: the index restarting at zero
        // with `base` present is what says a build began again, and claiming no
        // predecessor at all would say something less true.
        let previous =
            producer.publisher.latest_position().map_or(FlashblockId::NO_PREV, |position| {
                FlashblockId {
                    block_number: position.block_number,
                    index: position.flashblock_index,
                }
            });

        Self {
            producer: producer.clone(),
            da_footprint_gas_scalar,
            opened_at: std::time::Instant::now(),
            last_published: None,
            invariants: None,
            next_index: 0,
            previous,
        }
    }

    /// Write this slice's transactions to the journal, then publish it.
    ///
    /// The order is the whole point. A transaction a subscriber has been shown
    /// has to land, so it must be on disk before anyone hears about it —
    /// reversing this leaves a crash window in which a transaction was
    /// announced but not persisted, which is the case the journal exists for.
    ///
    /// The write stays outside the cancel gate because it cannot go inside:
    /// [`JobCancel::unless_cancelled`] takes a synchronous closure. The cost is
    /// that a slice dropped by that guard is already journaled — harmless,
    /// since its transactions belong in this block either way, and a restart
    /// finds them already on chain once it seals.
    ///
    /// A failed write does not stop the slice going out. The journal is a
    /// best-effort recovery substrate, and the alternative — withholding a
    /// slice because a disk write failed — trades a subscriber's whole view of
    /// the block for a durability guarantee it never had (this is `flush`, not
    /// `sync_all`).
    #[allow(clippy::too_many_arguments)]
    async fn journal_and_publish<N, Evm, ChainSpec, Attrs, B, P>(
        &mut self,
        ctx: &OpPayloadBuilderCtx<Evm, ChainSpec, Attrs>,
        builder: &mut B,
        info: &mut ExecutionInfo<N::SignedTx>,
        state_provider: &P,
        cancel: &JobCancel,
        journal: Option<&PreconfJournal>,
    ) where
        N: OpPayloadPrimitives,
        Evm: ConfigurePostExecEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<N>, ChainSpec>,
            >,
        ChainSpec: reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
        Attrs: OpAttributes<Transaction = TxTy<N>>,
        B: BlockBuilder<Primitives = N, Executor: alloy_evm::block::BlockExecutor>,
        P: reth_storage_api::StateProvider,
        N::SignedTx: SignedTransaction,
    {
        // Before the publish below, always. See this function's docs.
        if let Some(journal) = journal {
            let height = ctx.parent().number() + 1;
            let (entries, announced) = info.take_journal_records(height);
            // The index first: whether a transaction was announced does not
            // depend on whether its record reached the disk, and the disk write
            // is allowed to fail and retry.
            journal.note_announced(height, &announced);
            // The one part of a slice that touches the disk, and the one that
            // could put a tick over its interval.
            let started = std::time::Instant::now();
            let write = journal.append_batch(&entries).await;
            metrics::histogram!("flashblock.journal_write_duration_ms")
                .record(started.elapsed().as_secs_f64() * 1000.0);
            if let Err(err) = write {
                warn!(
                    target: "mantle::preconf::flashblocks",
                    %err,
                    index = self.next_index,
                    "slice journal write failed; the records are held for the next write",
                );
            }
        }

        self.publish(ctx, builder, info, state_provider, cancel);
    }

    /// Assemble and publish everything executed since the last slice.
    ///
    /// **Reached through [`Self::journal_and_publish`], and not otherwise.** A
    /// slice's records go to the journal first so that nothing is announced
    /// that has not already been written down; announcing first leaves a window
    /// in which a crash costs subscribers transactions they were shown. Calling
    /// this directly skips that, and nothing in the type system says so — the
    /// order is two statements in one function, and this note is what keeps
    /// them in it.
    ///
    /// Never fails the build. Publishing a slice is a side channel: a state read
    /// that fell over, or an encoding that did not, costs a slice and leaves its
    /// transactions pending for the next one — whereas failing here would cost
    /// the slot its block. A subscriber that ends up short is told by
    /// `prev_flashblock_id` and fills the gap from the canonical chain.
    fn publish<N, Evm, ChainSpec, Attrs, B, P>(
        &mut self,
        ctx: &OpPayloadBuilderCtx<Evm, ChainSpec, Attrs>,
        builder: &mut B,
        info: &mut ExecutionInfo<N::SignedTx>,
        state_provider: &P,
        cancel: &JobCancel,
    ) where
        N: OpPayloadPrimitives,
        Evm: ConfigurePostExecEvm<
                Primitives = N,
                NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<N>, ChainSpec>,
            >,
        ChainSpec: reth_chainspec::EthChainSpec + reth_optimism_forks::OpHardforks,
        Attrs: OpAttributes<Transaction = TxTy<N>>,
        B: BlockBuilder<Primitives = N, Executor: alloy_evm::block::BlockExecutor>,
        P: reth_storage_api::StateProvider,
    {
        // Assembling means rooting every transaction in the block, which is not
        // cheap, and a block that was thrown away has nothing worth saying
        // about it. A resolved one does: it is being sealed with exactly what
        // this slice describes.
        if cancel.reason() == Some(CancelReason::Abandoned) {
            debug!(
                target: "mantle::preconf::flashblocks",
                index = self.next_index,
                "build abandoned; skipping the slice",
            );
            return;
        }

        let sealed = match seal_block_so_far(
            ctx,
            builder,
            info,
            state_provider,
            self.da_footprint_gas_scalar,
        ) {
            Ok(sealed) => sealed,
            Err(err) => {
                warn!(
                    target: "mantle::preconf::flashblocks",
                    %err,
                    index = self.next_index,
                    "failed to assemble a slice; skipping it",
                );
                return;
            }
        };
        let invariants = self.invariants.get_or_insert_with(|| block_invariants(&sealed));
        let payload = build_flashblock(
            invariants,
            slice_header(&sealed),
            info.pending_slice(),
            ctx.payload_id(),
            self.next_index,
            self.previous,
        );
        let block_number = invariants.base.block_number;

        // Checked again after assembling, and this time inseparably from the
        // send: a payload resolved while this slice was being put together is
        // already being sealed, and a subscriber told about transactions the
        // sealed block will not contain has no way to unsee them. Checking and
        // then sending as two steps would leave a window for the resolve to
        // land in between — which is the very thing being guarded against.
        let sent = cancel.unless_abandoned(|| self.producer.publisher.publish(&payload));

        match sent {
            None => {
                // The build was thrown away while this slice was being put
                // together. Its transactions go back to the pool below, so
                // announcing them would name a block that will never exist.
                metrics::counter!("flashblock.dropped_after_abandon_total").increment(1);
                debug!(
                    target: "mantle::preconf::flashblocks",
                    index = self.next_index,
                    "build abandoned while the slice was assembled; dropping it",
                );
                return;
            }
            Some(Err(err)) => {
                warn!(
                    target: "mantle::preconf::flashblocks",
                    %err,
                    index = self.next_index,
                    "failed to serialize a slice; skipping it",
                );
                return;
            }
            Some(Ok(_)) => {}
        }

        // How far apart subscribers actually see slices — the tick interval is
        // what was asked for, this is what happened. The first slice of a block
        // has no predecessor to measure against and is reported separately.
        let now = std::time::Instant::now();
        match self.last_published {
            Some(previous) => metrics::histogram!("flashblock.publish_interval_ms")
                .record(now.duration_since(previous).as_secs_f64() * 1000.0),
            None => metrics::histogram!("flashblock.first_slice_offset_ms")
                .record(now.duration_since(self.opened_at).as_secs_f64() * 1000.0),
        }
        self.last_published = Some(now);

        // Only now, and only on the path where the slice actually went out.
        // Settling a slice that was dropped would leave its transactions in no
        // slice at all while the index chain still looked unbroken, so a
        // subscriber would have no way to notice the hole.
        info.advance_publish_cursor();

        self.previous = FlashblockId { block_number, index: self.next_index };
        self.next_index += 1;
    }
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
    /// 1. **Prelude** — upstream [`OpPayloadBuilderCtx`], parent-block state provider fetched twice
    ///    (owned, to keep the async future `Send`), L1 block contract preloaded into the DB cache.
    /// 2. **Stage 1** — `apply_pre_execution_changes`.
    /// 3. **Stage 2** — `execute_sequencer_transactions_watching_whitelist` (deposits + L1 info +
    ///    system txs, snapshotting any `WhitelistUpdated` delta they emit).
    /// 4. **Stage 3** — unified `select!` loop with four `biased` branches, **in this order**:
    ///    - `cancel.wait()` — exits the loop.
    ///    - `flashblock_ticker.tick()` — publishes the slice, opens the next allowance, brings the
    ///      pool up to date and takes a fresh best-tx iterator. Ahead of the pool arm because that
    ///      arm is always ready: behind it the ticker would never be polled, and a slice would go
    ///      out when gas ran out rather than when its window closed.
    ///    - `fifo_rx.recv()` — preconf-tx dispatch (`admit_and_dispatch` per hash on `Ok`; on
    ///      `Lagged` re-scan the fifo snapshot through the same gate; break on `Closed`).
    ///    - **Level-triggered pool arm** (`ready(()) if AdmissionPacer::has_headroom()`) — each
    ///      fire admits exactly one pool best-tx, then returns to `select!`. Unbounded readiness,
    ///      so it goes last; cancel, the ticker and preconf all get a preempt chance between every
    ///      pool tx via biased priority.
    ///
    ///    Before the loop, in order: a sweep of what the last build announced and the chain has
    ///    not taken (which also puts those senders' pool nonces back where the chain has them);
    ///    the **index 0 slice** (deposits and system txs only); then the **carryover preamble** —
    ///    stale in-flight / journal-restored commitments and the unlanded pool transactions the
    ///    sweep handed to the fifo, all dispatched through [`admit_and_dispatch`]. They bypass the
    ///    broadcast queue and the pool arm, so they land ahead of concurrently-queued RPC pushes
    ///    and fresh pool traffic.
    /// 5. **Stage 4** — SDM post-exec refund tx (only when `ctx.sdm_production_enabled()`).
    /// 6. **Stage 5** — `builder.finish` → seal + wrap into `OpBuiltPayload`.
    ///
    /// Both preconf-tx and best-tx apply into the same in-flight
    /// `State<DB>`, which is what makes the RPC-returned receipt
    /// byte-equal to the sealed block's receipt.
    ///
    /// `N` and `Attrs` are bound on the method, not the `impl`: this is an
    /// inherent async method rather than a
    /// [`reth_basic_payload_builder::PayloadBuilder`] impl, whose sync
    /// `try_build` cannot host the async select! loop. No struct field depends
    /// on them, so this keeps `PhantomData<(N, Attrs)>` off the struct.
    ///
    /// [`OpPayloadBuilderCtx`]: reth_optimism_payload_builder::builder::OpPayloadBuilderCtx
    #[allow(clippy::unused_async)]
    pub async fn build_payload<N, Attrs>(
        self,
        args: BuildArguments<Attrs, OpBuiltPayload<N>>,
        cancel: JobCancel,
    ) -> Result<OpBuiltPayload<N>, PayloadBuilderError>
    where
        Pool: reth_transaction_pool::TransactionPoolExt<
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
        //
        // Not `ctx.execute_sequencer_transactions` — see the replicated
        // function for why the per-transaction execution result is needed here.
        let (mut info, whitelist_deltas) = execute_sequencer_transactions_watching_whitelist::<N, _>(
            ctx.attributes().sequencer_transactions(),
            &mut builder,
            crate::whitelist::wants_whitelist(&self.cfg),
        )?;

        // The allowlist this block is judged against — see `barred_by_allowlist`
        // for why every entry is checked against it. Pinned rather than read
        // live per transaction: `update_whitelist` swaps the shared `Arc`
        // wholesale, so a watcher landing mid-build would otherwise judge two
        // transactions in one block against different policies. The snapshot is
        // a refcount bump.
        //
        // A governance update carried by *this* block is layered on top (the
        // only case that pays for a copy) and deliberately never written back:
        // the watcher reloads from the chain (`whitelist::should_reload`), so a
        // build that is superseded or never sealed must not leave its delta
        // visible to the RPC admission path.
        let mut whitelist = self.classifier.whitelist_snapshot();
        if !whitelist_deltas.is_empty() {
            let mut updated = (*whitelist).clone();
            for delta in &whitelist_deltas {
                crate::whitelist::apply_delta(&mut updated, delta);
            }
            whitelist = Arc::new(updated);
        }

        // ── Stage 3: unified select! loop (see method rustdoc) ────────

        // Pool iterator — one-shot snapshot at build start.
        let best_txs_iter_opt = (!ctx.attributes().no_tx_pool()).then(|| {
            let attrs = ctx.best_transaction_attributes(builder.evm_mut().block());
            BestPayloadTransactions::new(
                self.pool.best_transactions_with_attributes(attrs).without_updates(),
            )
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
        // With slicing on, the tick is what publishes, so its cadence is the
        // slice interval. With it off the cadence is what preconf always used,
        // which is what keeps turning the feature off a true rollback rather
        // than a second code path.
        let flashblock_cfg = self.flashblocks.as_ref().map(|producer| producer.cfg.as_ref());
        let tick_interval =
            crate::flashblocks::tick_interval(flashblock_cfg, self.cfg.sweep_interval);

        // Measured once and handed to both schedules unreduced; each subtracts
        // the leeway it holds back itself. Reducing it here and adding it back
        // for the other caller is a round trip that does not survive saturation.
        let time_to_deadline =
            slot_deadline.duration_since(std::time::SystemTime::now()).unwrap_or(tick_interval);

        // Divides the block across the slices still expected to fit. Also owns
        // the tick grid when slicing is on: the ticker and the budget have to
        // agree on where the tick boundaries are, and the way to guarantee that
        // is to read both from one schedule.
        let slice_schedule = flashblock_cfg.map(|cfg| {
            derive_slice_schedule(
                time_to_deadline,
                cfg.leeway_time,
                tick_interval,
                self.cfg.slot_duration,
                SliceLimits { block_gas_limit, block_da_limit, da_footprint_gas_scalar },
            )
        });

        let schedule = derive_pool_quota_schedule(
            time_to_deadline,
            tick_interval,
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

        let first_offset =
            slice_schedule.map_or(schedule.first_offset, |schedule| schedule.first_offset);
        // The grid every tick is measured against. Kept as one instant so a
        // skipped tick can be told apart from a late one.
        let tick_grid_start = tokio::time::Instant::now() + first_offset;
        let mut flashblock_ticker = tokio::time::interval_at(tick_grid_start, tick_interval);
        // A tick that arrives late has missed the window it was going to
        // publish; firing the backlog immediately afterwards would send a burst
        // of near-empty slices rather than catch anything up. Only with slicing
        // on: with it off the tick only raises a ceiling, nothing is missed by
        // firing late, and the default is what preconf has always run.
        if flashblock_cfg.is_some() {
            flashblock_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }
        let mut pool_pacer = match slice_schedule {
            Some(schedule) => AdmissionPacer::slicing(&schedule),
            // Starts with a drained quota (0) — the pool arm cannot admit
            // until the first sweep tick raises the ceiling by
            // `gas_per_batch`; its `has_headroom()` guard is a level trigger
            // that self-disables once the current allocation is drained.
            None => AdmissionPacer::sweeping(schedule.gas_per_batch, block_gas_limit),
        };
        // Skipping a tick would otherwise skip the allowance it carried, so the
        // budget is opened by where the clock is rather than by how many ticks
        // were delivered: one allowance per grid point already passed. Slicing
        // only — with it off tokio delivers every missed tick itself, and
        // counting them again here would open the same allowance twice.
        let catch_up_budget = flashblock_cfg.is_some();
        let tick_interval_ms = tick_interval.as_millis().max(1);
        let mut allowances_opened: u128 = 0;

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

        // The same question, asked of what slicing announced from the pool:
        // did it land? Nothing else answers it — the block never entered the
        // canonical chain, so no reorg happened and the pool's own reinjection
        // never sees it. Asking the chain at the start of the next build is the
        // race-free way, and it replaces asking why the last build ended:
        // `Resolved` is not proof the consensus layer used the payload.
        //
        // Survivors go to the fifo rather than back to the pool. A transaction
        // subscribers were already shown has a stronger claim on the block than
        // a new one, and the fifo is where must-land work already lives: from
        // there `replay_fifo_carryover` dispatches it ahead of the pool arm, and
        // `ordered_for_dispatch` puts it in nonce order against any commitment
        // from the same sender — a pairing the allowlist makes possible, since
        // it is keyed on `(from, to)` and one sender can hold both kinds.
        //
        // Not gated on slicing. This only reads state and moves bookkeeping, and
        // skipping it on a derivation build would leave the staging list behind
        // a sealed hash no later parent matches, so the next real build would
        // sweep accounts for transactions the chain had long since taken.
        if let Some(journal) = self.journal.as_deref() {
            if journal.parent_is_ours(parent_hash) {
                // The chain built on what this node sealed, so everything staged
                // went with it. One comparison, no state reads — this is the
                // healthy path, and it is why the list is worth keeping.
                journal.clear_unlanded();
            } else if !journal.unlanded_is_empty() {
                metrics::counter!("flashblock.unlanded_sweep_slow_total").increment(1);
                let balances = ProviderBalances(state_provider_for_finish.as_ref());
                let mut heads: HashMap<Address, u64> = HashMap::new();
                let mut changed: Vec<ChangedAccount> = Vec::new();
                for sender in journal.unlanded_senders() {
                    let nonce = match state_provider_for_finish.account_nonce(&sender) {
                        // An account with no state is a reading, not a failure:
                        // it sits at nonce 0, which is what both the staging
                        // list and the pool should be told.
                        Ok(nonce) => nonce.unwrap_or(0),
                        // A failed read is not a nonce, and must not be spent as
                        // one. What follows is written into the pool, where
                        // telling it a sender at chain nonce 30 is back at 0
                        // parks every pending transaction they hold until a real
                        // canonical update arrives. Skipping costs little:
                        // `take_unlanded` keeps the entries of any sender it was
                        // given no nonce for, so they are handed to the fifo all
                        // the same and asked about again next build.
                        Err(err) => {
                            metrics::counter!("flashblock.unlanded_nonce_unreadable_total")
                                .increment(1);
                            warn!(
                                target: "mantle::preconf::flashblocks",
                                %sender, ?err,
                                "chain nonce unreadable; leaving this sender's pool record as it stands",
                            );
                            continue;
                        }
                    };
                    heads.insert(sender, nonce);
                    // Put the pool's record back where the chain has it. The
                    // slice boundary advanced it on the strength of a block that
                    // did not land, and the pool discards rather than parks
                    // anything below the nonce it holds — so leaving it raised
                    // costs this sender everything they send until a real block
                    // corrects it.
                    //
                    // An unreadable balance is the one thing still guessed here,
                    // downwards, as pool maintenance guesses it: zero parks the
                    // sender for a block, where guessing upwards would admit an
                    // unfunded transaction. The nonce is the half worth having.
                    changed.push(ChangedAccount {
                        address: sender,
                        nonce,
                        balance: balances.balance_of(sender).unwrap_or(U256::ZERO),
                    });
                }
                self.pool.update_accounts(changed);

                // Hand the survivors over. `Replay` is what lets a transaction
                // the allowlist would refuse through the preconf path at all
                // (`barred_by_allowlist`), and what keeps the deadline gates off
                // an entry with no client waiting on it. `ConflictActive` means a
                // live commitment already holds that `(sender, nonce)`; the
                // fresher entry wins and this one is simply not staged again.
                let mut handed = 0u64;
                for entry in journal.take_unlanded(&heads) {
                    let Ok(envelope) = TxEnvelope::decode_2718(&mut entry.rlp.as_ref()) else {
                        // The bytes came from this process's own `encoded_2718`,
                        // so this is unreachable short of memory corruption —
                        // counted rather than ignored, because a silent `continue`
                        // here would drop a transaction with nothing to show for it.
                        metrics::counter!("flashblock.unlanded_undecodable_total").increment(1);
                        continue;
                    };
                    let _ = self
                        .fifo
                        .push_if_absent(
                            Arc::new(envelope),
                            entry.sender,
                            crate::types::PreconfSource::Replay,
                        )
                        .await;
                    handed += 1;
                }
                if handed > 0 {
                    metrics::counter!("flashblock.unlanded_replayed_total").increment(handed);
                    debug!(
                        target: "mantle::preconf::flashblocks",
                        handed,
                        "handed unlanded transactions to the fifo for replay",
                    );
                }
            }
        }

        // ── Slice publishing state ────────────────────────────────────
        //
        // Gated on `allow_preconf`: a derivation build has to reproduce what
        // other nodes derive from L1 data, and a subscriber watching a block
        // that is being replayed rather than built has nothing to gain from
        // seeing it in pieces.
        let mut slices = self
            .flashblocks
            .as_ref()
            .filter(|_| allow_preconf)
            .map(|producer| SliceState::new(producer, da_footprint_gas_scalar));

        // The first slice goes out before any preconf or pool transaction has
        // been applied, so it carries exactly the deposits and system
        // transactions that are fixed for this block, plus the block-level
        // fields a subscriber needs to reconstruct a header. Publishing it
        // after the carryover preamble would fold replayed commitments into it
        // and break that guarantee.
        if let Some(state) = slices.as_mut() {
            state
                .journal_and_publish(
                    &ctx,
                    &mut builder,
                    &mut info,
                    &state_provider_for_finish,
                    &cancel,
                    self.journal.as_deref(),
                )
                .await;
        }

        // Carryover replay preamble — apply stale in-flight / journal-
        // restored entries directly (see `replay_fifo_carryover`), including
        // the unlanded pool transactions the sweep above just handed over. The
        // block scope drops `apply_fn` so its `&mut builder` borrow is released
        // before the select! loop's arms.
        //
        // Skipped entirely when `!allow_preconf` — the fifo entries (including
        // Replay-sourced ones with `must-land` SLA) remain in the fifo and get
        // dispatched on the next normal-slot build. That is also what keeps a
        // derivation build from replaying anything: it has to execute exactly
        // what its attributes name, or the block it derives will not match the
        // sequencer's.
        if allow_preconf {
            // Dispatch carryover entries through the admission gate before the
            // select! loop's arms, so they land ahead of any concurrently
            // queued fresh RPC pushes. `admit_and_dispatch` builds the apply
            // closure (which folds gas/DA into `info`) internally per hash.
            for hash in replay_fifo_carryover(&self.fifo).await {
                admit_and_dispatch::<N, _>(
                    &self.fifo,
                    &self.cfg,
                    self.journal.as_deref(),
                    hash,
                    &mut loop_state,
                    &mut builder,
                    &mut info,
                    constraints,
                    &whitelist,
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
                // Publishes what has been executed since the last tick, then
                // opens the next slice's budget and refreshes the pool's view
                // of the block. The order matters: publishing first is what
                // makes a slice the delta since the previous one.
                _ = flashblock_ticker.tick() => {
                    if let Some(state) = slices.as_mut() {
                        state.journal_and_publish(
                            &ctx,
                            &mut builder,
                            &mut info,
                            &state_provider_for_finish,
                            &cancel,
                            self.journal.as_deref(),
                        ).await;

                        // Tell the pool what ran, then take a fresh iterator.
                        // Both have to happen here and in this order: the
                        // correction only survives until the next arrival
                        // overwrites it, so anything between it and the new
                        // iterator is wasted.
                        maintain_pool_at_slice_boundary(
                            &self.pool,
                            &ProviderBalances(state_provider_for_finish.as_ref()),
                            &mut info,
                        );
                        let attrs = ctx.best_transaction_attributes(builder.evm_mut().block());
                        best_txs_iter = Some(BestPayloadTransactions::new(
                            self.pool.best_transactions_with_attributes(attrs).without_updates(),
                        ));
                    }

                    if catch_up_budget {
                        let elapsed = tokio::time::Instant::now()
                            .saturating_duration_since(tick_grid_start);
                        let due = allowances_due(elapsed, tick_interval_ms);
                        for _ in allowances_opened..due {
                            pool_pacer.tick();
                        }
                        allowances_opened = due;
                    } else {
                        pool_pacer.tick();
                    }
                }
                recv = fifo_rx.recv() => {
                    match recv {
                        Ok(hash) => {
                            admit_and_dispatch::<N, _>(
                                &self.fifo, &self.cfg, self.journal.as_deref(), hash,
                                &mut loop_state,
                                &mut builder, &mut info, constraints,
                                &whitelist,
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
                                    &self.fifo, &self.cfg, self.journal.as_deref(), hash,
                                &mut loop_state,
                                    &mut builder, &mut info, constraints,
                                    &whitelist,
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
                    && pool_pacer.has_headroom() =>
                {
                    let iter = best_txs_iter.as_mut().expect("guard verified Some");
                    match apply_one_best_tx::<N, _>(
                        &self.classifier,
                        iter,
                        &mut builder,
                        &mut info,
                        &constraints,
                        &mut pool_pacer,
                    )? {
                        // The iterator has advanced either way; the next
                        // select! iteration re-fires this arm.
                        BestTxStep::Continue => {}
                        BestTxStep::Done => best_txs_iter = None,
                    }
                }
            }
        }

        if let Some(state) = slices.as_ref() {
            // What the budget divided the block into is `tick_count`; this is
            // what the block actually produced. They part company when a build
            // starts early or late, which the budget allows for by design — a
            // distribution that stops matching the configured interval is the
            // signal, not any single block.
            metrics::histogram!("flashblock.slices_per_block").record(state.next_index as f64);
        }

        // No hand-back stage here, deliberately. How a build ended is not
        // evidence about where its transactions went: `Resolved` only says the
        // consensus layer took the payload, not that it used it. What the index
        // records is settled at the start of the next build, against the one
        // thing that can answer — the chain.

        // ── Stage 4: SDM post-exec refund tx ───────────────────────────
        // `take_post_exec_entries` collects entries from ALL prior applies
        // uniformly (sequencer / pool / preconf) — preconf-tx contributions
        // are automatically included, no special handling needed.
        if ctx.sdm_production_enabled() {
            let block_number = builder.evm_mut().block().number().saturating_to();
            let entries = builder.executor_mut().take_post_exec_entries();
            try_include_post_exec_tx::<N::SignedTx, _>(block_number, entries, |tx| {
                let recorded = tx.clone();
                builder.execute_transaction(tx).map(|gas| {
                    let tx_gas_used = gas.tx_gas_used();
                    // Accounted for like every other execution. The closing
                    // slice is published after this stage, so leaving the
                    // transaction out would make the published slices stop one
                    // short of the sealed block; leaving its gas out would make
                    // `info` stop being the block's running total.
                    info.cumulative_gas_used += tx_gas_used;
                    info.record(recorded);
                    tx_gas_used
                })
            })?;
        }

        // ── Stage 4b: the closing slice ───────────────────────────────
        // The loop has no end of its own — it runs until `getPayload` cancels
        // it — so whatever executed since the last tick, the SDM refund among
        // it, is in the block and in no slice. A block whose transactions
        // arrive late is otherwise one a subscriber sees nothing of until it
        // seals.
        //
        // The reference implementation needs no such slice: its loop stops on
        // its own once the budgeted count is reached, so it has published
        // everything by the time it finalizes. A difference in shape, not in
        // policy.
        if let Some(state) = slices.as_mut() &&
            cancel.reason() == Some(CancelReason::Resolved)
        {
            state
                .journal_and_publish(
                    &ctx,
                    &mut builder,
                    &mut info,
                    &state_provider_for_finish,
                    &cancel,
                    self.journal.as_deref(),
                )
                .await;
        }

        // ── Stage 5: finalize ─────────────────────────────────────────
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } =
            builder.finish(state_provider_for_finish, None)?;

        let sealed_block = Arc::new(block.sealed_block().clone());
        // The hash the next build compares its parent against. If it matches,
        // everything this build announced is on chain and the whole group goes
        // without a single state read — which is the healthy path, and the
        // reason the sweep costs nothing on a chain that is working.
        if let Some(journal) = self.journal.as_deref() {
            journal.note_sealed(sealed_block.hash());
        }
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

        let planned = replay_fifo_carryover(&fifo).await;

        // Only Waiting + Success, in insertion order.
        assert_eq!(planned, vec![*t_wait.tx_hash(), *t_succ.tx_hash()]);
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

        let planned = replay_fifo_carryover(&fifo).await;

        assert_eq!(planned, vec![*t_rpc.tx_hash(), *t_journal.tx_hash()]);
        assert_eq!(fifo.find_by_hash(t_rpc.tx_hash()).await.unwrap().source, PreconfSource::Rpc,);
        assert_eq!(
            fifo.find_by_hash(t_journal.tx_hash()).await.unwrap().source,
            PreconfSource::Replay,
        );
    }

    /// One sender's carried-over transactions go out in nonce order, whatever
    /// order the fifo holds them in.
    ///
    /// They only execute in that order. A higher nonce reaching the builder
    /// first is rejected as invalid, and for a carried-over entry that rejection
    /// is terminal — the commitment is broken with no second attempt.
    #[tokio::test]
    async fn carryover_orders_one_senders_transactions_by_nonce() {
        let fifo = PreconfTxSet::new(16);
        let sender = Address::from([1; 20]);
        // Pushed newest-nonce-first, as two writers with different latencies
        // leave them in the journal.
        let mut by_nonce = Vec::new();
        for nonce in [2u64, 1, 0] {
            let t = tx(0xd0 + nonce as u8, nonce);
            by_nonce.push((nonce, *t.tx_hash()));
            fifo.push_if_absent(t, sender, PreconfSource::Replay).await;
        }
        by_nonce.sort_by_key(|(nonce, _)| *nonce);

        let carryover = replay_fifo_carryover(&fifo).await;

        assert_eq!(
            carryover,
            by_nonce.iter().map(|(_, hash)| *hash).collect::<Vec<_>>(),
            "a sender's transactions must reach the builder in nonce order",
        );
    }

    /// Senders keep the order they first appear in, which is the order their
    /// commitments were made — not the order their addresses happen to sort in.
    ///
    /// That order decides who lands when a block fills up, so sorting by address
    /// would hand a standing advantage to whoever holds the lower one.
    #[tokio::test]
    async fn carryover_keeps_senders_in_the_order_they_first_appear() {
        let fifo = PreconfTxSet::new(16);
        // High address first, so an address sort would swap them.
        let early = Address::from([0xee; 20]);
        let late = Address::from([0x11; 20]);
        let first = tx(0xe0, 0);
        let second = tx(0xe1, 0);
        fifo.push_if_absent(first.clone(), early, PreconfSource::Rpc).await;
        fifo.push_if_absent(second.clone(), late, PreconfSource::Rpc).await;

        let carryover = replay_fifo_carryover(&fifo).await;

        assert_eq!(
            carryover,
            vec![*first.tx_hash(), *second.tx_hash()],
            "the sender that committed first still goes first",
        );
    }

    /// Both rules at once: nonces gathered under their sender, senders in the
    /// order they first appear.
    #[tokio::test]
    async fn carryover_gathers_each_senders_nonces_without_reordering_senders() {
        let fifo = PreconfTxSet::new(16);
        let a = Address::from([0xaa; 20]);
        let b = Address::from([0x0b; 20]);
        let a1 = tx(0xf1, 1);
        let b0 = tx(0xf2, 0);
        let a0 = tx(0xf3, 0);
        fifo.push_if_absent(a1.clone(), a, PreconfSource::Replay).await;
        fifo.push_if_absent(b0.clone(), b, PreconfSource::Replay).await;
        fifo.push_if_absent(a0.clone(), a, PreconfSource::Replay).await;

        let carryover = replay_fifo_carryover(&fifo).await;

        assert_eq!(
            carryover,
            vec![*a0.tx_hash(), *a1.tx_hash(), *b0.tx_hash()],
            "sender A appeared first so it stays first, and its nonce 0 precedes its nonce 1",
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

        let planned = replay_fifo_carryover(&fifo).await;
        assert_eq!(planned, expected, "carryover must respect FIFO insertion order");
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

        let planned = replay_fifo_carryover(&fifo).await;

        assert_eq!(planned, vec![hash]);
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

    // ============ dispatch-time allowlist gate ============

    mod allowlist_gate {
        use super::*;
        use alloy_primitives::map::foldhash::HashSet;

        fn a(byte: u8) -> Address {
            Address::from([byte; 20])
        }

        /// Allowlist holding exactly the pair `(1, 2)`.
        fn only_pair_1_2() -> Whitelist {
            Whitelist {
                pairs: HashSet::from_iter([(a(1), a(2))]),
                from_wildcards: HashSet::default(),
                to_wildcards: HashSet::default(),
            }
        }

        /// On-chain allowlist mode — the configuration the gate is written for.
        fn cfg() -> PreconfConfig {
            PreconfConfig { enabled: true, ..Default::default() }
        }

        #[test]
        fn a_sender_the_allowlist_still_authorizes_passes() {
            let wl = only_pair_1_2();
            assert!(!barred_by_allowlist(&cfg(), &wl, PreconfSource::Rpc, &a(1), Some(&a(2))));
        }

        /// The gate is unconditional: an entry admitted under an earlier policy
        /// is barred as soon as the allowlist in force stops authorizing it,
        /// whether that update landed in this block or several blocks ago.
        #[test]
        fn a_sender_the_allowlist_no_longer_authorizes_is_barred() {
            let wl = only_pair_1_2();
            assert!(barred_by_allowlist(&cfg(), &wl, PreconfSource::Rpc, &a(3), Some(&a(4))));
        }

        /// The exemption that keeps a published commitment honoured: a receipt
        /// for this hash already reached its client, so policy no longer gets a
        /// say. Same sender/recipient as the barred case above.
        #[test]
        fn a_replayed_commitment_is_exempt() {
            let wl = only_pair_1_2();
            assert!(!barred_by_allowlist(&cfg(), &wl, PreconfSource::Replay, &a(3), Some(&a(4))));
        }

        /// `--preconf.all` bypasses the lists, and must be checked *before*
        /// them — see `barred_by_allowlist` for why.
        #[test]
        fn all_preconfs_bypasses_the_lists() {
            let c = PreconfConfig { all_preconfs: true, ..cfg() };
            let empty = Whitelist::default();
            assert!(!barred_by_allowlist(&c, &empty, PreconfSource::Rpc, &a(9), Some(&a(9))));
        }

        /// A creation has no recipient, so only a from-wildcard can authorize
        /// it — this pins that the gate asks the allowlist the same `to: None`
        /// question the RPC entry does.
        #[test]
        fn a_creation_falls_back_to_the_from_wildcard() {
            let barred = only_pair_1_2();
            assert!(barred_by_allowlist(&cfg(), &barred, PreconfSource::Rpc, &a(1), None));

            let allowed =
                Whitelist { from_wildcards: HashSet::from_iter([a(1)]), ..only_pair_1_2() };
            assert!(!barred_by_allowlist(&cfg(), &allowed, PreconfSource::Rpc, &a(1), None));
        }

        /// An empty allowlist bars everything — the contract accepts that state
        /// (see `whitelist::apply_whitelist`), so the gate must not read "empty"
        /// as "no policy". Distinct from `all_preconfs`, which is empty *and*
        /// authorizes everything.
        #[test]
        fn an_empty_allowlist_bars_every_non_replay_entry() {
            let wl = Whitelist::default();
            assert!(barred_by_allowlist(&cfg(), &wl, PreconfSource::Rpc, &a(1), Some(&a(2))));
        }
    }

    // ============ per-block allowlist pinning ============

    /// The snapshot must not follow a later `update_whitelist`: every
    /// transaction in one block is judged against the policy in force when that
    /// block's build began, even if governance lands mid-build.
    #[test]
    fn a_whitelist_snapshot_is_pinned_against_a_mid_build_refresh() {
        use alloy_primitives::map::foldhash::HashSet;

        let c = PreconfClassifier::new(false, std::time::Duration::from_secs(4), 128);
        let sender = Address::from([1u8; 20]);
        let to = Address::from([2u8; 20]);
        c.update_whitelist(
            HashSet::from_iter([(sender, to)]),
            HashSet::default(),
            HashSet::default(),
        );

        let pinned = c.whitelist_snapshot();
        assert!(pinned.is_eligible(&sender, Some(&to)));

        // Governance revokes while the build is still running.
        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        assert!(pinned.is_eligible(&sender, Some(&to)), "the pinned view keeps its lists");
        assert!(
            !c.whitelist_snapshot().is_eligible(&sender, Some(&to)),
            "while a fresh snapshot sees the revocation"
        );
    }

}
