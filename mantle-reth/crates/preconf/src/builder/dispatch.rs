//! Preconf dispatch helpers for
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload).
//!
//! The select! main loop inside `build_payload` calls these helpers
//! one hash at a time. Four invariants are enforced for every hash:
//!
//! - **Dedup**: a hash already committed in this build is short-circuited before any fifo / EVM
//!   work, so it cannot enter the same block twice.
//! - **Status gate**: only `Waiting` entries proceed; an entry already applied is skipped.
//! - **Pre-apply deadline**: when `entry.inserted_at.elapsed() + safety_margin >= preconf_timeout`,
//!   the tx is *not* applied; the commitment ends here and the client is told. This closes the race
//!   where the RPC client has already given up but the builder is about to commit a receipt.
//! - **Responder ownership**: every path that ends a commitment answers its client exactly once —
//!   `complete_failure` or the `SuccessTicket`, never both.
//!
//! ## Apply-fn injection
//!
//! The actual EVM apply is **injected as a closure** by the caller
//! (typically
//! [`PreconfPayloadBuilder::build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload)).
//! This keeps `dispatch.rs` free of EVM types and trait gymnastics
//! around the `BlockBuilder` generic — `apply_one_preconf` just
//! orchestrates the fifo state machine and responder ownership, while
//! the closure captures `&mut builder` and runs
//! [`apply_preconf_tx`](crate::apply::apply_preconf_tx) against the
//! in-flight state.
//!
//! Tests in this module pass a synthetic-receipt closure (no EVM) so
//! the state-machine invariants are exercised in isolation. End-to-end
//! EVM behaviour is covered by devnet integration tests.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alloy_consensus::TxEnvelope;
use alloy_primitives::{Address, TxHash};
use tracing::{debug, error, trace, warn};

use reth_payload_builder_primitives::PayloadBuilderError;

use crate::{
    PreconfConfig, PreconfTxSet,
    apply::{ApplyError, BuilderRejected},
    journal::{JournalEntry, PreconfJournal},
    types::{PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
};

/// Count an EVM refusal under the reason the client is told.
///
/// Its own namespace rather than the `preconf.admit.*` counters: those say a
/// request never reached the builder, and folding both into one series would
/// make "refused for funds" unanswerable as to *when*.
fn count_rejection(rejected: &BuilderRejected) {
    match rejected {
        BuilderRejected::InsufficientFunds { .. } => {
            metrics::counter!("preconf.execute.rejected_funds_total").increment(1);
        }
        BuilderRejected::BaseFeeTooLow { .. } => {
            metrics::counter!("preconf.execute.rejected_base_fee_total").increment(1);
        }
        BuilderRejected::Other(_) => {
            metrics::counter!("preconf.execute.rejected_other_total").increment(1);
        }
    }
}

/// How a sender's preconf chain is blocked for the rest of the current slot
/// once one of its txs cannot enter the in-flight block. Same-sender entries
/// at a nonce ≥ the blocked head inherit this outcome — they depend on the
/// blocked predecessor landing first, so admitting them independently would
/// only produce a spurious nonce-too-high failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    /// Predecessor was deferred (transient capacity) → successor is also kept
    /// `Waiting` and retried next slot.
    Defer,
    /// Predecessor was permanently rejected → successor can never land either
    /// (permanent nonce gap), so its commitment ends too.
    Reject,
}

/// Per-job local state for the preconf dispatch loop.
///
/// Owned by [`build_payload`](crate::builder::payload_builder::PreconfPayloadBuilder::build_payload)
/// — one per payload job. Dropped when the build completes / cancels.
#[derive(Debug)]
pub(super) struct LoopState {
    /// Hashes already committed to the in-flight block.
    committed: HashSet<TxHash>,
    /// Predicted L2 block height for this slot. Stamped onto every
    /// receipt as `PreconfReceipt::block_height`.
    predicted_height: u64,
    /// Cumulative preconf-path gas committed in this block, checked against
    /// `cfg.preconf_max_gas_per_block` by the budget gate below. Incremented by
    /// the actual `receipt.gas_used`, not `gas_limit` — reserving the limit
    /// would over-count against later txs that could still fit.
    preconf_gas_used: u64,
    /// Senders whose preconf chain is blocked this slot: `sender → (lowest
    /// blocked nonce, kind)`. Slot-local, reset each build. See [`BlockKind`]
    /// for why a successor inherits its predecessor's outcome.
    blocked_senders: HashMap<Address, (u64, BlockKind)>,
}

impl LoopState {
    /// Construct a fresh local state for a payload job targeting
    /// `predicted_height` (the parent's block number + 1).
    pub(super) fn new(predicted_height: u64) -> Self {
        Self {
            committed: HashSet::new(),
            predicted_height,
            preconf_gas_used: 0,
            blocked_senders: HashMap::new(),
        }
    }

    /// Record that `sender`'s preconf chain is blocked from `nonce` onward
    /// this slot with `kind`. Keeps the **lowest** blocked nonce (and that
    /// head's kind), so the earliest non-admitted tx governs the chain even
    /// if entries are seen slightly out of order.
    pub(super) fn block_sender(&mut self, sender: Address, nonce: u64, kind: BlockKind) {
        self.blocked_senders
            .entry(sender)
            .and_modify(|head| {
                if nonce < head.0 {
                    *head = (nonce, kind);
                }
            })
            .or_insert((nonce, kind));
    }

    /// If `sender` is blocked this slot at a head nonce ≤ `nonce`, return the
    /// [`BlockKind`] the entry should inherit. `None` when the sender is
    /// unblocked or `nonce` is below the blocked head (a predecessor that
    /// should still be attempted).
    pub(super) fn sender_blocked_at(&self, sender: &Address, nonce: u64) -> Option<BlockKind> {
        self.blocked_senders
            .get(sender)
            .and_then(|(head_nonce, kind)| (nonce >= *head_nonce).then_some(*kind))
    }

    /// Cumulative preconf gas committed in this block so far. Test-only
    /// accessor for budget-tracking assertions — production accounting
    /// reads the `preconf_gas_used` field directly in the budget gate, and
    /// the payload builder now folds gas into `ExecutionInfo` inside
    /// `apply_preconf_with_da` rather than syncing via this getter.
    #[cfg(test)]
    pub(super) fn preconf_gas_used(&self) -> u64 {
        self.preconf_gas_used
    }

    /// `true` iff this build already applied the hash into its in-flight block.
    pub(super) fn is_committed(&self, hash: &TxHash) -> bool {
        self.committed.contains(hash)
    }

    /// Mark hash as committed. Idempotent.
    pub(super) fn record_committed(&mut self, hash: TxHash) {
        self.committed.insert(hash);
    }

    /// Number of committed hashes — used by tests/metrics.
    #[cfg(test)]
    pub(super) fn committed_len(&self) -> usize {
        self.committed.len()
    }
}

/// Handle one preconf hash end-to-end: dedup → fetch → status gate → take the
/// entry's `apply_lock` → pre-apply deadline → gas budget → re-check under the
/// lock → caller-supplied apply → completion. Enforces the four invariants
/// listed in the module docs.
///
/// The lock comes before the two gates because a gate that fires is itself a
/// completion.
///
/// `apply_fn` receives `(tx, hash, predicted_height)` and is invoked at most
/// once per call — never once an earlier guard has fired. The bound is `FnMut`
/// rather than `Fn` because the caller's closure borrows the in-flight
/// `BlockBuilder` and `ExecutionInfo` mutably (see
/// `payload_builder::admit_and_dispatch`).
///
/// Returns `Err(PayloadBuilderError)` **only** when `apply_fn` reports an
/// [`ApplyError::Fatal`] — a non-tx-specific execution error (DB / header /
/// fatal precompile). The caller must then abort the whole build (mirroring the
/// pool arm); the fifo entry is left `Waiting` with its responder attached, so
/// the commitment is retried next build cycle rather than reneged on. Every
/// other path returns `Ok(())`.
pub(super) async fn apply_one_preconf<F>(
    fifo: &PreconfTxSet,
    cfg: &PreconfConfig,
    journal: Option<&PreconfJournal>,
    hash: TxHash,
    loop_state: &mut LoopState,
    mut apply_fn: F,
) -> Result<(), PayloadBuilderError>
where
    F: FnMut(Arc<TxEnvelope>, TxHash, u64) -> Result<PreconfReceipt, ApplyError>,
{
    // A hash already applied into this block must not enter it twice. Rejections
    // are not remembered here: a rejection ends its entry, so a resubmit of the
    // same hash arrives as a new entry and is judged afresh.
    if loop_state.is_committed(&hash) {
        trace!(target: "mantle::preconf::dispatch", ?hash, "dedup hit; already committed");
        return Ok(());
    }
    let Some(entry) = fifo.find_by_hash(&hash).await else {
        trace!(target: "mantle::preconf::dispatch", ?hash, "no fifo entry; skipping");
        return Ok(());
    };

    if entry.status != PreconfStatus::Waiting {
        trace!(
            target: "mantle::preconf::dispatch",
            ?hash, status = ?entry.status,
            "entry no longer Waiting; skipping"
        );
        return Ok(());
    }

    // The deadline and per-block gas budget gates below only apply to
    // RPC-sourced entries. Journal-replayed entries bypass both to
    // honor the mantle preconf SLA: "once a receipt has been returned
    // to the client, the tx must land on chain". Rejecting them here
    // would silently break that commitment. They remain subject to the
    // status / dedup gates above and to the underlying block gas limit
    // enforced by the block builder.
    let is_rpc = entry.source == PreconfSource::Rpc;

    // Pre-apply deadline check — see crate-level docs. `cfg.safety_margin`
    // (default 40ms, see `DEFAULT_SAFETY_MARGIN`) is sized to slightly
    // exceed measured p99 apply latency on the target hardware so the
    // skip only fires on genuine races rather than merely slow-but-in-
    // budget applies. Kept separate from `preconf_timeout` (the client-
    // facing SLA) so hardware tuning does not implicitly widen the client
    // contract. Setting `cfg.safety_margin = Duration::ZERO` opens the
    // full race window for tests that need to exercise `rpc.rs`'s
    // race-resolution branch.
    let margin = cfg.safety_margin;

    // Sample the elapsed-since-insertion for every RPC-sourced dispatch
    // decision (skipped and applied alike). Downstream analysis reads
    // the distribution to see how close the pipeline runs to the client
    // deadline, informing tuning of `SAFETY_MARGIN` and
    // `preconf_timeout`. Replay-sourced entries are excluded because
    // their `inserted_at` reflects a journal restore, not the client's
    // clock.
    let elapsed_at_gate = entry.inserted_at.elapsed();
    if is_rpc {
        metrics::histogram!("preconf.dispatch.elapsed_at_gate_ms")
            .record(elapsed_at_gate.as_millis() as f64);
    }

    // ── Point of no return begins here ────────────────────────────────
    //
    // Acquire the per-entry `apply_lock` before anything that can **end** this
    // commitment — the two gates below, not just `apply_fn`. Held from here
    // through the send, so `rpc`'s deadline branch can never observe a finished
    // commitment whose answer is still in flight, which is what holds
    // "wire Timeout ⇒ tx not committed to builder state".
    let Some(_apply_guard) = fifo.lock_for_apply(&hash).await else {
        // Entry vanished between the reads above and this acquisition.
        trace!(target: "mantle::preconf::dispatch", ?hash, "entry vanished before apply_lock");
        return Ok(());
    };

    if is_rpc && elapsed_at_gate + margin >= cfg.preconf_timeout {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            elapsed_ms = elapsed_at_gate.as_millis() as u64,
            "pre-apply deadline passed; aborting"
        );
        metrics::counter!("preconf.dispatch.deadline_skipped_total").increment(1);
        let reason = PreconfError::Timeout { timeout_ms: cfg.preconf_timeout.as_millis() as u64 };
        let _ = fifo.complete_failure(&hash, reason).await;
        return Ok(());
    }

    // Block-level preconf gas budget gate. Pessimistic check: if adding
    // this tx's `gas_limit` would push cumulative preconf gas past
    // `cfg.preconf_max_gas_per_block`, abort now. Sizing off `gas_limit`
    // (worst case) ensures the reservation stays sound even if the
    // closure ends up spending less than the tx claimed. Uses `>` so
    // exact-boundary hits (`used + limit == max`) are accepted.
    let tx_gas_limit = alloy_consensus::Transaction::gas_limit(entry.tx.as_ref());
    if is_rpc
        && loop_state.preconf_gas_used.saturating_add(tx_gas_limit) > cfg.preconf_max_gas_per_block
    {
        debug!(
            target: "mantle::preconf::dispatch",
            ?hash,
            used = loop_state.preconf_gas_used,
            limit = tx_gas_limit,
            max = cfg.preconf_max_gas_per_block,
            "block gas budget exhausted; aborting apply"
        );
        metrics::counter!("preconf.dispatch.gas_budget_skipped_total").increment(1);
        let reason = PreconfError::BlockGasBudgetExceeded {
            max: cfg.preconf_max_gas_per_block,
            used: loop_state.preconf_gas_used,
            limit: tx_gas_limit,
        };
        let _ = fifo.complete_failure(&hash, reason).await;
        return Ok(());
    }

    // Re-check under the lock. The RPC deadline branch may have already ended
    // the commitment in the window between our earlier gate reads and this
    // acquisition; running `apply_fn` now would violate the invariant
    // "committed to builder state ⇒ wire not Timeout".
    //
    // **Absent counts as changed.** `lock_for_apply` clones the lock's handle
    // under `inner` and then awaits outside it — it has to, or waiters would
    // hold the queue's lock — so a removal that does not consult `apply_lock`
    // (`forward_all`, whose predicate is the sender's nonce moving past this
    // entry) can take the entry in that gap and leave us holding a mutex that
    // belongs to nothing. Reading `None` as "nothing changed" applies the
    // snapshot taken before the gates, which is the one case the lock is here
    // to prevent.
    match fifo.find_by_hash(&hash).await {
        Some(re_entry) if re_entry.status == PreconfStatus::Waiting => {}
        other => {
            trace!(
                target: "mantle::preconf::dispatch",
                ?hash, status = ?other.map(|e| e.status),
                "entry changed or vanished before we acquired apply_lock; skipping apply"
            );
            return Ok(());
        }
    }

    // ── Apply via caller-supplied closure (real EVM in production,
    //    synthetic receipt in tests). ────────────────────────────────
    let apply_started = std::time::Instant::now();
    let apply_result = apply_fn(entry.tx.clone(), hash, loop_state.predicted_height);
    let apply_duration = apply_started.elapsed();
    // Distribution of EVM apply latency — feeds SAFETY_MARGIN tuning.
    // Recorded once per call regardless of outcome; success / failure
    // counters (below) provide the breakdown.
    metrics::histogram!("preconf.execute.duration_ms").record(apply_duration.as_millis() as f64);

    match apply_result {
        Ok(receipt) => {
            metrics::counter!("preconf.tx.success_total").increment(1);
            loop_state.record_committed(hash);
            loop_state.preconf_gas_used =
                loop_state.preconf_gas_used.saturating_add(receipt.gas_used);
            // Transition and responder together, before the journal write:
            // that write is an `await`, and a responder left in the entry
            // across it can be taken by a concurrent removal.
            let ticket = match fifo.begin_success(&hash).await {
                Ok(ticket) => ticket,
                Err(e) => {
                    // Lost a race — entry already gone or in a non-Waiting
                    // state. Log and continue: the transaction is in the block
                    // either way, and there is no responder left to answer.
                    trace!(
                        target: "mantle::preconf::dispatch",
                        ?hash, ?e,
                        "begin_success lost race"
                    );
                    None
                }
            };
            // Persist before the receipt goes anywhere. The commitment is made
            // by the transaction landing, not by a client hearing about it:
            // a replayed entry has no responder at all, and a client
            // that timed out has stopped listening — both are still in the
            // block, and with slicing both have been broadcast.
            //
            // A `Replay` entry came out of this file; writing it back on every
            // block it is retried in would grow it without adding anything.
            if let Some(journal) = journal
                && entry.source != PreconfSource::Replay
            {
                let record = JournalEntry::for_executed(
                    hash,
                    entry.tx.as_ref(),
                    loop_state.predicted_height,
                );
                if let Err(e) = journal.append_promised(&record).await {
                    warn!(
                        target: "mantle::preconf::dispatch",
                        ?hash, ?e,
                        "journal append failed; commitment may be lost on restart"
                    );
                }
            }
            // The journal has it; the client may now have it too.
            if let Some(ticket) = ticket {
                ticket.send(receipt);
            }
        }
        // Per-tx rejection of an entry the client is still waiting on. End the
        // commitment, hand the client the concrete error, and keep building.
        //
        // Guarded on `is_rpc`: an already-acknowledged commitment must not take
        // this path — see the `Rejected` arm below.
        Err(ApplyError::Rejected(rejected)) if is_rpc => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?rejected,
                "preconf apply rejected tx; ending the commitment"
            );
            metrics::counter!("preconf.tx.failure_total").increment(1);
            count_rejection(&rejected);
            let err = PreconfError::from(rejected);
            if let Err(e) = fifo.complete_failure(&hash, err).await {
                trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?e,
                    "completion lost race"
                );
            }
        }
        // Fatal, non-tx-specific execution error (DB / header / fatal
        // precompile). The execution environment is untrustworthy, so we
        // abort the whole build — same policy as the pool arm
        // (`payload_builder::apply_one_best_tx`). Crucially we do NOT end the
        // commitment: the entry stays `Waiting` and
        // its responder stays attached so the still-valid commitment is
        // retried on the next build cycle instead of being silently
        // dropped while a possibly-corrupt block gets sealed.
        Err(ApplyError::Fatal(e)) => {
            warn!(
                target: "mantle::preconf::dispatch",
                ?hash, ?e,
                "preconf apply hit fatal execution error; aborting build \
                 (commitment left Waiting for retry)"
            );
            metrics::counter!("preconf.tx.fatal_total").increment(1);
            return Err(e);
        }
        // This entry's receipt has already gone out (`Replay` covers journal
        // restore, reorg reinject, and stale-in-flight replay alike), so
        // reaching here means the commitment is **broken** — and this is the
        // only moment that fact is observable.
        //
        // No retry: every transient cause is already filtered out before apply,
        // so a second attempt would only re-derive the same answer. Transient
        // capacity becomes `Defer` (unbounded, not a retry budget), and a
        // a successor of a blocked predecessor is deferred or ended — both in
        // `payload_builder::admit_and_dispatch`, neither reaching apply; `Fatal`
        // returns above. What is left is permanent: a nonce or balance that
        // moved since the tx last applied cleanly, an envelope this pipeline
        // cannot convert, or a predecessor a crash lost for good (the pool is
        // in-memory; only preconf txs are journaled). So the commitment ends on
        // the first failure, which is what releases the `(sender, nonce)`.
        Err(ApplyError::Rejected(rejected)) => {
            // This line and the counter below are the only trace a breach
            // leaves: the responder went with the receipt, and the node exposes
            // no status query. Keep both.
            error!(
                target: "mantle::preconf::dispatch",
                ?hash, ?rejected,
                "COMMITMENT BROKEN: receipt was returned to the client but the tx could not be applied; releasing its nonce"
            );
            metrics::counter!("preconf.tx.commitment_broken_total").increment(1);
            count_rejection(&rejected);
            // A `Replay` entry has no responder (admission refuses to attach
            // one), so this removes it with nobody to answer — the log and
            // counter above are the only trace.
            if let Err(e) = fifo.complete_failure(&hash, PreconfError::CommitmentBroken).await {
                trace!(
                    target: "mantle::preconf::dispatch",
                    ?hash, ?e,
                    "completion lost race"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_consensus::{Signed, Transaction, TxLegacy};
    use alloy_primitives::{Address, B256, Bloom, Bytes, Signature};
    use tokio::sync::oneshot;

    use crate::types::PushResult;

    use super::*;

    fn make_tx(hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce: 0, gas_limit: 21_000, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    // ============ LoopState::blocked_senders (same-sender cascade) ============

    /// Blocking a sender at nonce `n0` makes every same-sender entry at
    /// `nonce ≥ n0` inherit the kind; lower nonces (predecessors) and other
    /// senders stay unblocked.
    #[test]
    fn blocked_senders_cascade_query() {
        let s = Address::from([9u8; 20]);
        let other = Address::from([8u8; 20]);
        let mut st = LoopState::new(1);

        assert_eq!(st.sender_blocked_at(&s, 5), None, "unblocked sender");

        st.block_sender(s, 5, BlockKind::Defer);
        assert_eq!(st.sender_blocked_at(&s, 5), Some(BlockKind::Defer), "at head nonce");
        assert_eq!(st.sender_blocked_at(&s, 6), Some(BlockKind::Defer), "above head nonce");
        assert_eq!(st.sender_blocked_at(&s, 4), None, "predecessor below head");
        assert_eq!(st.sender_blocked_at(&other, 6), None, "other sender");
    }

    /// `block_sender` keeps the lowest nonce as the chain head (and that
    /// head's kind); a later higher-nonce block must not raise the head, a
    /// lower-nonce block lowers it and its kind governs.
    #[test]
    fn block_sender_keeps_lowest_nonce_head() {
        let s = Address::from([9u8; 20]);
        let mut st = LoopState::new(1);

        st.block_sender(s, 5, BlockKind::Defer);
        st.block_sender(s, 8, BlockKind::Reject); // higher — must not move head
        assert_eq!(st.sender_blocked_at(&s, 8), Some(BlockKind::Defer), "head stays at 5/Defer");

        st.block_sender(s, 3, BlockKind::Reject); // lower — lowers head, its kind wins
        assert_eq!(st.sender_blocked_at(&s, 3), Some(BlockKind::Reject));
        assert_eq!(st.sender_blocked_at(&s, 4), Some(BlockKind::Reject));
    }

    /// Test apply closure that fabricates an always-success receipt using
    /// `tx.gas_limit()` as the reported `gas_used`, so the dispatch state
    /// machine can be exercised without standing up a real EVM.
    /// The entry can go while dispatch is between "I have the lock's handle"
    /// and "I have the lock".
    ///
    /// `lock_for_apply` clones the `Arc<Mutex<_>>` under `inner`, drops `inner`,
    /// and only then awaits — it has to, or waiters would hold the queue's lock.
    /// `forward_all` takes `inner` in that gap and removes the entry without
    /// consulting `apply_lock`, so what dispatch finally acquires is a mutex
    /// belonging to nothing.
    ///
    /// The re-read under the lock must therefore treat **absent** as a stop,
    /// not as "nothing has changed". Reading it the other way applies the
    /// snapshot taken before the gates — a transaction the client was already
    /// told had timed out, committed into the block anyway, which is exactly
    /// the invariant the lock exists to hold.
    ///
    /// Driven through `complete_failure`, which `payload_builder`'s
    /// pre-dispatch refusals — allowlist revoked, DA / block-gas capacity, a
    /// permanently rejected predecessor — call without taking the entry's
    /// `apply_lock`. That is what keeps this window open. (`forward_all` used
    /// to be the driver here; it now claims the lock before removing, because
    /// it is the one remover with no status CAS to stand in for one.)
    #[tokio::test]
    async fn an_entry_removed_while_dispatch_waited_for_the_lock_is_not_applied() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fifo = Arc::new(PreconfTxSet::new(8));
        let tx = make_tx(0x5e);
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        // Stands in for the RPC deadline branch, which holds this same lock
        // while it decides what to tell the client.
        let guard = fifo.lock_for_apply(&hash).await.expect("entry present");

        let applied = Arc::new(AtomicBool::new(false));
        let flag = applied.clone();
        let dispatch_fifo = fifo.clone();
        let task = tokio::spawn(async move {
            let mut state = LoopState::new(1);
            apply_one_preconf(
                &dispatch_fifo,
                &PreconfConfig::default(),
                None,
                hash,
                &mut state,
                move |tx, h, height| {
                    flag.store(true, Ordering::SeqCst);
                    synthetic_ok(tx, h, height)
                },
            )
            .await
        });

        // Long enough to clear the gates and block on the lock.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A pre-dispatch refusal from another in-flight build ends the
        // commitment. It takes no `apply_lock`, so nothing here waits for
        // dispatch.
        fifo.complete_failure(&hash, PreconfError::NotPreconfEligible)
            .await
            .expect("a Waiting entry may be ended");
        assert!(!fifo.contains(&hash).await, "the premise: the entry is gone");

        drop(guard);
        task.await.expect("join").expect("removal is not a fatal execution error");

        assert!(
            !applied.load(Ordering::SeqCst),
            "the entry was gone by the time the lock was acquired; applying the pre-gate \
             snapshot puts a transaction in the block that nothing is tracking",
        );
    }

    /// The pre-apply gates are completions, so they must hold the entry's
    /// `apply_lock` from the decision through the send. Without it `rpc`'s
    /// deadline branch can take the lock mid-gate, find nothing in the channel,
    /// and conclude nothing happened.
    ///
    /// Driven through the deadline gate because it fires without a build:
    /// `inserted_at` is already past `preconf_timeout` when the entry is
    /// pushed.
    #[tokio::test]
    async fn the_deadline_gate_waits_for_the_apply_lock_before_ending_the_entry() {
        let fifo = Arc::new(PreconfTxSet::new(8));
        // Zero timeout: `elapsed_at_gate + margin >= preconf_timeout` holds the
        // moment the entry exists, so the gate fires without a build running.
        let cfg = PreconfConfig { preconf_timeout: Duration::ZERO, ..Default::default() };
        let tx = make_tx(0x6a);
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));
        let (resp_tx, mut resp_rx) = oneshot::channel();
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        // Stands in for `rpc`'s deadline branch, which holds this lock while it
        // decides what to tell the client.
        let guard = fifo.lock_for_apply(&hash).await.expect("entry present");

        let dispatch_fifo = fifo.clone();
        let task = tokio::spawn(async move {
            let mut state = LoopState::new(1);
            apply_one_preconf(&dispatch_fifo, &cfg, None, hash, &mut state, synthetic_ok).await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            fifo.find_by_hash(&hash).await.expect("still queued").status,
            PreconfStatus::Waiting,
            "the gate must not end the entry while someone else holds the apply lock",
        );
        assert!(
            matches!(resp_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "and must not have answered the client either",
        );

        drop(guard);
        task.await.expect("join").expect("a deadline skip is not a fatal error");

        assert!(
            !fifo.contains(&hash).await,
            "once the lock is free the gate ends the commitment, and ending removes it",
        );
        assert!(
            matches!(resp_rx.try_recv(), Ok(Err(PreconfError::Timeout { .. }))),
            "and the client is told why",
        );
    }

    fn synthetic_ok(
        tx: Arc<TxEnvelope>,
        hash: TxHash,
        height: u64,
    ) -> Result<PreconfReceipt, ApplyError> {
        Ok(PreconfReceipt {
            tx_hash: hash,
            block_height: height,
            status: true,
            logs: Vec::new(),
            gas_used: tx.gas_limit(),
            reason: String::new(),
            revert_data: Bytes::new(),
            tx_index: 0,
            cumulative_gas_used: tx.gas_limit(),
            logs_bloom: Bloom::default(),
            log_index_base: 0,
            tx_type: 2,
            from: Address::ZERO,
            to: None,
            contract_address: None,
            effective_gas_price: 0,
            block_timestamp: 0,
            l1_fields: Default::default(),
        })
    }

    /// Test apply closure that reports a per-tx REJECTION — exercises the
    /// `ApplyError::Rejected` → `complete_failure` branch.
    fn synthetic_err(_: Arc<TxEnvelope>, _: TxHash, _: u64) -> Result<PreconfReceipt, ApplyError> {
        Err(ApplyError::Rejected(BuilderRejected::Other("synthetic error for test".into())))
    }

    /// Test apply closure that reports a FATAL execution error — exercises
    /// the `ApplyError::Fatal` → build-abort branch. `std::fmt::Error` is a
    /// convenient zero-field `Error + Send + Sync` payload.
    fn synthetic_fatal(
        _: Arc<TxEnvelope>,
        _: TxHash,
        _: u64,
    ) -> Result<PreconfReceipt, ApplyError> {
        Err(ApplyError::Fatal(PayloadBuilderError::other(std::fmt::Error)))
    }

    /// A journal under a temp dir, for the dispatch-side write.
    async fn temp_journal() -> (tempfile::TempDir, PreconfJournal) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let j =
            PreconfJournal::open(dir.path().join("preconf.jsonl"), 1 << 20).await.expect("opens");
        (dir, j)
    }

    /// Persisting a commitment hangs off the transaction landing, not off a
    /// client being there to hear about it.
    ///
    /// `begin_success` hands back no ticket for a replayed entry, and the send
    /// fails for a client that already gave up — in both cases the transaction
    /// is still in the block, and with slicing it has been broadcast.
    /// Journaling from the receipt path missed exactly those.
    #[tokio::test]
    async fn an_applied_commitment_is_journaled_even_with_no_responder_attached() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let (_dir, journal) = temp_journal().await;
        let tx = make_tx(0x21);
        let hash = *tx.tx_hash();
        // Deliberately no responder.
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        let mut state = LoopState::new(42);
        apply_one_preconf(&fifo, &cfg, Some(&journal), hash, &mut state, synthetic_ok)
            .await
            .unwrap();

        let (entries, _) = journal.load().await.unwrap();
        assert_eq!(entries.len(), 1, "the commitment landed, so it belongs on disk");
        assert_eq!(entries[0].hash, hash);
        assert_eq!(entries[0].block_height, 42);
    }

    /// A revert still produced a receipt, so the commitment was made and has
    /// to survive a restart exactly as a successful one does. Dispatch reads
    /// `Ok(receipt)`, never `receipt.status`.
    #[tokio::test]
    async fn a_reverted_commitment_is_journaled_too() {
        fn synthetic_revert(
            tx: Arc<TxEnvelope>,
            hash: TxHash,
            height: u64,
        ) -> Result<PreconfReceipt, ApplyError> {
            Ok(PreconfReceipt {
                tx_hash: hash,
                block_height: height,
                status: false,
                logs: Vec::new(),
                gas_used: tx.gas_limit(),
                reason: "execution reverted".to_string(),
                revert_data: Bytes::new(),
                tx_index: 0,
                cumulative_gas_used: tx.gas_limit(),
                logs_bloom: Bloom::default(),
                log_index_base: 0,
                tx_type: 2,
                from: Address::ZERO,
                to: None,
                contract_address: None,
                effective_gas_price: 0,
                block_timestamp: 0,
                l1_fields: Default::default(),
            })
        }

        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let (_dir, journal) = temp_journal().await;
        let tx = make_tx(0x23);
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        let mut state = LoopState::new(42);
        apply_one_preconf(&fifo, &cfg, Some(&journal), hash, &mut state, synthetic_revert)
            .await
            .unwrap();

        let (entries, _) = journal.load().await.unwrap();
        assert_eq!(entries.iter().map(|e| e.hash).collect::<Vec<_>>(), vec![hash]);
    }

    /// A replayed entry came *out* of the journal. Writing it back every block
    /// it is retried in would grow the file without adding anything.
    #[tokio::test]
    async fn a_replayed_commitment_is_not_journaled_again() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let (_dir, journal) = temp_journal().await;
        let tx = make_tx(0x22);
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Replay).await,
            PushResult::Inserted
        ));

        let mut state = LoopState::new(42);
        apply_one_preconf(&fifo, &cfg, Some(&journal), hash, &mut state, synthetic_ok)
            .await
            .unwrap();

        let (entries, _) = journal.load().await.unwrap();
        assert!(entries.is_empty(), "it is already in the file it was read from");
    }

    #[tokio::test]
    async fn apply_one_preconf_calls_closure_and_marks_succeeded() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x11);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        assert!(matches!(
            fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        let mut state = LoopState::new(42);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        // Responder got the synthetic receipt.
        let receipt = resp_rx.await.expect("responder closed").expect("synthetic ok");
        assert_eq!(receipt.tx_hash, hash);
        assert_eq!(receipt.block_height, 42);
        assert!(receipt.status);
        assert_eq!(receipt.gas_used, 21_000);

        // Loop state recorded.
        assert_eq!(state.committed_len(), 1);

        // Fifo entry transitioned to Success.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn dedup_hit_skips_second_apply() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x22);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;

        let mut state = LoopState::new(1);
        // `Cell` so the assert_eq below can read while the FnMut
        // closure still mutably borrows it (Cell uses interior
        // mutability with `&self`).
        let call_count = Cell::new(0u32);
        let mut counting_apply = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, &mut counting_apply).await.unwrap();
        assert_eq!(call_count.get(), 1);
        assert_eq!(state.committed_len(), 1);

        // Second call: dedup guard fires before apply_fn is invoked.
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, &mut counting_apply).await.unwrap();
        assert_eq!(call_count.get(), 1, "apply closure must not be called twice");
        assert_eq!(state.committed_len(), 1);
    }

    #[tokio::test]
    async fn the_deadline_gate_ends_an_rpc_commitment_and_answers_its_client() {
        // Configure a 50ms preconf_timeout so the deadline check fires
        // deterministically after a short sleep.
        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0x33);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        // Sleep past the deadline. `SAFETY_MARGIN` is a hard 40ms but the
        // sleep of 60ms also exceeds `preconf_timeout` (50ms) on its own.
        tokio::time::sleep(Duration::from_millis(60)).await;

        use std::cell::Cell;
        let mut state = LoopState::new(7);
        let apply_called = Cell::new(false);
        let mut tracking_apply = |tx, h, height| {
            apply_called.set(true);
            synthetic_ok(tx, h, height)
        };
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, &mut tracking_apply).await.unwrap();

        // apply closure must NOT have been invoked — deadline gate fires
        // earlier so the in-flight builder is untouched.
        assert!(!apply_called.get(), "apply closure must skip when deadline exceeded");

        // Responder must observe Timeout error.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));

        // The transaction was not committed.
        assert_eq!(state.committed_len(), 0);

        // The commitment is over, so the entry is gone with it.
        assert!(!fifo.contains(&hash).await);
    }

    /// Same-slot client resubmit after a timeout: the deadline gate ends the
    /// commitment, the resubmit arrives as a new entry with a fresh
    /// `inserted_at`, and the second dispatch applies it.
    ///
    /// Locks "a timeout is not a stable exclusion" — letting the first
    /// attempt's verdict reach the second submission would deny service to a
    /// re-eligible tx.
    #[tokio::test]
    async fn dedup_timeout_re_evaluates_gate_on_fresh_inserted_at() {
        use std::time::Instant;

        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0x55);
        let hash = *tx.tx_hash();

        // Step 1: initial insert; sleep past deadline so the gate fires.
        let (resp_tx1, resp_rx1) = oneshot::channel();
        fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, Instant::now(), resp_tx1).await);
        tokio::time::sleep(Duration::from_millis(60)).await;

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();
        // First dispatch: Timeout via deadline gate.
        let err = resp_rx1.await.expect("responder closed").expect_err("must be Timeout");
        assert!(matches!(err, PreconfError::Timeout { .. }));
        assert!(!fifo.contains(&hash).await, "the timed-out commitment is over and gone");

        // Step 2: the client resubmits. There is no entry left to revive, so
        // this is a fresh admission with a fresh clock.
        let (resp_tx2, resp_rx2) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, Instant::now(), resp_tx2).await);
        assert_eq!(
            fifo.find_by_hash(&hash).await.unwrap().status,
            PreconfStatus::Waiting,
            "revive must flip status back to Waiting",
        );

        // Step 3: the revived entry is judged against the fresh clock.
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "second dispatch must apply successfully");

        // Fresh responder observes the receipt from the successful apply.
        let receipt = resp_rx2.await.expect("responder closed").expect("must be Ok");
        assert!(receipt.status);
    }

    #[tokio::test]
    async fn a_resubmit_after_rejection_gets_a_fresh_result() {
        use std::time::Instant;

        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x7a);
        let hash = *tx.tx_hash();
        let (first_resp, first_rx) = oneshot::channel();
        fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, Instant::now(), first_resp).await);

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_err).await.unwrap();
        assert!(matches!(first_rx.await.unwrap(), Err(PreconfError::BuilderRejected(_))));
        assert!(!fifo.contains(&hash).await, "the rejected commitment is over and gone");

        // Nothing is left to revive, so the resubmit is a new entry, judged
        // afresh.
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));
        let (second_resp, second_rx) = oneshot::channel();
        assert!(fifo.set_responder(hash, Instant::now(), second_resp).await);

        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        let receipt = second_rx.await.unwrap().expect("the retry must get its own result");
        assert_eq!(receipt.tx_hash, hash);
        assert_eq!(fifo.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn apply_failure_ends_the_commitment_and_sends_err_to_responder() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x44);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        let mut state = LoopState::new(99);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_err).await.unwrap();

        // Responder got the apply error verbatim.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        assert!(matches!(err, PreconfError::BuilderRejected(_)));

        // The transaction was not committed.
        assert_eq!(state.committed_len(), 0);

        // The entry is gone. Both sources end here; only the reporting differs
        // — see `a_broken_commitment_ends_and_releases_its_slot`.
        assert!(!fifo.contains(&hash).await, "a rejected commitment is over and gone");
    }

    // ===== An already-acknowledged commitment that cannot be applied: broken,
    // ===== reported loudly, and its `(sender, nonce)` released

    /// Push an entry that is already in the "receipt has gone out" shape: a
    /// `Replay`-sourced `Waiting` entry with no responder, which is what
    /// `reset_success_to_waiting`, a reorg reinject, and a journal restore all
    /// produce.
    async fn push_replayed(fifo: &PreconfTxSet, tx: Arc<TxEnvelope>) -> TxHash {
        let hash = *tx.tx_hash();
        assert!(matches!(
            fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Replay).await,
            PushResult::Inserted
        ));
        hash
    }

    /// A commitment whose receipt already went out ends on the first apply
    /// rejection, and ending it releases its `(sender, nonce)`. See the breach
    /// arm in `apply_one_preconf` for why there is no retry.
    #[tokio::test]
    async fn a_broken_commitment_ends_and_releases_its_slot() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let hash = push_replayed(&fifo, make_tx(0x91)).await;

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");

        assert!(
            !fifo.contains(&hash).await,
            "terminal on the first failure, and ending it is what releases the nonce",
        );
        assert_eq!(state.committed_len(), 0);
    }

    /// A `Fatal` apply error must **not** break the commitment.
    ///
    /// The two error axes are independent: the error's *kind* (`Rejected` — this
    /// transaction is invalid — versus `Fatal` — the execution environment is
    /// untrustworthy) and the entry's *source* (`Rpc` versus an
    /// already-acknowledged replay). A DB / header / fatal-precompile error says
    /// nothing about this transaction, so it must leave the commitment live: no
    /// breach, no nonce released, responder still attached.
    ///
    /// This pins the `Fatal` arm's behaviour, not its position — `Rejected` and
    /// `Fatal` are disjoint patterns, so reordering them changes nothing. The
    /// ordering that *is* load-bearing is the `is_rpc` guard ahead of the breach
    /// arm; see there.
    #[tokio::test]
    async fn a_fatal_apply_error_does_not_break_the_commitment() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let hash = push_replayed(&fifo, make_tx(0x92)).await;

        let mut state = LoopState::new(7);
        let outcome = apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_fatal).await;

        assert!(outcome.is_err(), "a fatal execution error aborts the whole build");

        let entry = fifo.find_by_hash(&hash).await.expect("the commitment must survive");
        assert_eq!(
            entry.status,
            PreconfStatus::Waiting,
            "left Waiting, responder still attached, for the next build cycle",
        );
    }

    /// A broken commitment must not be applied by later jobs: ending it
    /// removed it, so a later job finds no entry at all.
    #[tokio::test]
    async fn a_broken_commitment_is_not_applied_again() {
        use std::cell::Cell;

        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let hash = push_replayed(&fifo, make_tx(0x94)).await;

        let mut state = LoopState::new(7);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_err)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");
        assert!(!fifo.contains(&hash).await, "the breach ended the commitment");

        // A later job: the apply closure must not run at all.
        let calls = Cell::new(0u32);
        let mut counting = |tx, h, height| {
            calls.set(calls.get() + 1);
            synthetic_ok(tx, h, height)
        };
        let mut next_state = LoopState::new(8);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut next_state, &mut counting)
            .await
            .expect("a per-tx rejection keeps the build going; no Fatal here");

        assert_eq!(calls.get(), 0, "a broken commitment must not be re-applied");
        assert_eq!(next_state.committed_len(), 0);
    }

    /// A FATAL apply error (DB / header / fatal precompile) must abort the
    /// whole build — `apply_one_preconf` returns `Err` — and, unlike the
    /// per-tx `Rejected` path, must leave the commitment intact for retry:
    /// the fifo entry stays `Waiting`, nothing is recorded in loop state, and
    /// the responder stays attached so the client keeps waiting for the next
    /// build cycle.
    /// This is the asymmetry the fix removes: the pool arm already aborts
    /// on this class; the preconf arm used to silently drop the commitment.
    #[tokio::test]
    async fn apply_fatal_aborts_build_and_leaves_entry_waiting() {
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x4f);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        let mut state = LoopState::new(7);
        let out = apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_fatal).await;

        // 1. Propagates as a build-aborting error.
        assert!(out.is_err(), "fatal apply must abort the build (return Err)");

        // 2. Entry left Waiting, so the next build retries it.
        let entry = fifo.find_by_hash(&hash).await.expect("entry must survive a fatal abort");
        assert_eq!(
            entry.status,
            PreconfStatus::Waiting,
            "fatal must not end the commitment — it stays queued for retry"
        );

        // 3. The commitment is untouched.
        assert_eq!(state.committed_len(), 0);

        // 4. Responder still attached (not consumed) so the client keeps waiting for the retry
        //    rather than receiving a spurious error.
        assert!(
            fifo.take_responder(&hash).await.is_some(),
            "fatal must leave the responder attached for the retry"
        );
        // The oneshot sender is still alive until we drop it here.
        drop(resp_rx);
    }

    /// Build a synthetic tx with a caller-chosen `gas_limit` and hash
    /// byte. Used by the block-gas-budget tests below.
    fn make_tx_with_gas(hash_byte: u8, nonce: u64, gas_limit: u64) -> Arc<TxEnvelope> {
        let inner = TxLegacy { nonce, gas_limit, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)))
    }

    /// Boundary: `used + tx.gas_limit == cfg.preconf_max_gas_per_block`
    /// must be accepted (gate uses `>`, not `>=`). Locks the corner
    /// against off-by-one drift in the future.
    #[tokio::test]
    async fn apply_one_preconf_at_exact_budget_boundary_accepts() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 21_000,
            preconf_max_gas_per_tx: 21_000, // per-tx cap must not shadow the test
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx_with_gas(0x55, 0, 21_000);
        let hash = *tx.tx_hash();

        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        let receipt = resp_rx.await.expect("responder closed").expect("must succeed at boundary");
        assert_eq!(receipt.gas_used, 21_000);
        assert_eq!(state.preconf_gas_used(), 21_000);
        assert_eq!(state.committed_len(), 1);
    }

    /// Over budget: `used + tx.gas_limit > preconf_max_gas_per_block`
    /// rejects with `BlockGasBudgetExceeded { max, used, limit }` and ends the
    /// commitment.
    #[tokio::test]
    async fn apply_one_preconf_over_budget_rejects_with_typed_error() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000, // per-tx cap must not shadow the test
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);

        // Pre-load loop_state as if a prior tx of 21_000 gas had already
        // committed — 21_000 + next tx 21_000 = 42_000 > 40_000.
        let mut state = LoopState::new(1);
        state.preconf_gas_used = 21_000;

        let tx = make_tx_with_gas(0x66, 0, 21_000);
        let hash = *tx.tx_hash();
        let (resp_tx, resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        // Responder got the typed error with all three fields.
        let err = resp_rx.await.expect("responder closed").expect_err("must be Err");
        match err {
            PreconfError::BlockGasBudgetExceeded { max, used, limit } => {
                assert_eq!(max, 40_000);
                assert_eq!(used, 21_000);
                assert_eq!(limit, 21_000);
            }
            other => panic!("expected BlockGasBudgetExceeded, got {other:?}"),
        }

        // Not committed; preconf_gas_used is unchanged.
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.preconf_gas_used(), 21_000);

        // Server pre-apply rejection — no EVM state change, the tx will NOT
        // land on chain, and the commitment is over, so nothing is left queued.
        assert!(!fifo.contains(&hash).await);
    }

    /// Cumulative tracking: successful applies increment
    /// `preconf_gas_used` by the receipt's `gas_used`. Three sequential
    /// applies must sum correctly.
    #[tokio::test]
    async fn apply_one_preconf_success_increments_preconf_gas_used() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 10_000_000,
            preconf_max_gas_per_tx: 5_000_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);

        let mut expected_total: u64 = 0;
        for (i, gas) in [21_000u64, 50_000, 30_000].into_iter().enumerate() {
            let tx = make_tx_with_gas(0x77 + i as u8, i as u64, gas);
            let hash = *tx.tx_hash();
            fifo.push_if_absent(tx, Address::from([i as u8 + 1; 20]), PreconfSource::Rpc).await;
            apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();
            expected_total += gas;
            assert_eq!(
                state.preconf_gas_used(),
                expected_total,
                "after apply {} of {}, expected preconf_gas_used {}",
                i + 1,
                3,
                expected_total,
            );
        }
        assert_eq!(state.committed_len(), 3);
    }

    /// Gate ② — `fifo.find_by_hash` returns None (the entry was removed
    /// between broadcast and pickup). `apply_one_preconf` must:
    ///
    /// - NOT invoke `apply_fn`
    /// - NOT touch responders
    /// - NOT record the hash in `loop_state` (allowing a future re-push of the same hash to proceed
    ///   normally)
    #[tokio::test]
    async fn missing_fifo_entry_is_silent_noop() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let mut state = LoopState::new(1);
        let call_count = Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };

        // hash never seen — nothing was ever queued for it.
        apply_one_preconf(&fifo, &cfg, None, TxHash::from([0xdd; 32]), &mut state, &mut apply_fn)
            .await
            .unwrap();

        assert_eq!(call_count.get(), 0, "apply_fn must not be invoked when entry missing");
        assert_eq!(state.committed_len(), 0);
        assert_eq!(state.preconf_gas_used(), 0);
    }

    /// Gate ③ — `entry.status != Waiting`. A `Success` entry reaches
    /// `apply_one_preconf` when a stale broadcast event fires after it has
    /// already been applied. It must not invoke `apply_fn` or touch the
    /// responder.
    #[tokio::test]
    async fn non_waiting_status_skips_apply_and_leaves_the_responder_alone() {
        use std::cell::Cell;
        let fifo = PreconfTxSet::new(8);
        let cfg = PreconfConfig::default();
        let tx = make_tx(0xee);
        let hash = *tx.tx_hash();

        let (resp_tx, mut resp_rx) = oneshot::channel();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Rpc).await;
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);
        fifo.mark_succeeded(&hash).await.unwrap();

        let mut state = LoopState::new(1);
        let call_count = Cell::new(0u32);
        let mut apply_fn = |tx, h, height| {
            call_count.set(call_count.get() + 1);
            synthetic_ok(tx, h, height)
        };

        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, &mut apply_fn).await.unwrap();

        assert_eq!(call_count.get(), 0, "apply_fn must not run for an applied entry");
        assert_eq!(state.committed_len(), 0);
        // Responder untouched — the first application owns it, and the dispatch
        // loop must NOT double-send.
        assert!(resp_rx.try_recv().is_err(), "responder must NOT be touched by dispatch");

        // Fifo entry still there, still Success: a successful commitment stays
        // queued until the sender's nonce moves past it.
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    // ── Source-differentiated gate tests (mantle preconf SLA: journal
    //    replay must never silently drop a promised tx). ─────────────

    /// Journal-replayed entries bypass the pre-apply deadline gate.
    /// Even after the timeout has elapsed since insertion, `apply_fn`
    /// still fires and the tx transitions to Success — the RPC-source
    /// counterpart under the same conditions is timed out (see
    /// `the_deadline_gate_ends_an_rpc_commitment_and_answers_its_client`).
    #[tokio::test]
    async fn replay_source_bypasses_deadline_gate() {
        let cfg = PreconfConfig {
            preconf_timeout: Duration::from_millis(50),
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let tx = make_tx(0xe0);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::ZERO, PreconfSource::Replay).await;

        // Sleep well past the deadline (50ms) + safety margin (40ms).
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut state = LoopState::new(1);
        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "journal-replayed tx must apply despite deadline");
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    /// Journal-replayed entries bypass the per-block gas budget gate.
    /// Pre-loading `preconf_gas_used` so an RPC entry would be rejected
    /// (see `apply_one_preconf_over_budget_rejects_with_typed_error`)
    /// still admits a Replay-sourced tx.
    #[tokio::test]
    async fn replay_source_bypasses_gas_budget_gate() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);
        state.preconf_gas_used = 21_000; // 21_000 + 21_000 = 42_000 > 40_000

        let tx = make_tx_with_gas(0xe1, 0, 21_000);
        let hash = *tx.tx_hash();
        fifo.push_if_absent(tx, Address::from([1; 20]), PreconfSource::Replay).await;

        apply_one_preconf(&fifo, &cfg, None, hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "journal tx must apply despite over-budget");
        assert_eq!(
            state.preconf_gas_used(),
            42_000,
            "gas_used still accumulates so subsequent RPC entries see the true cost",
        );
        let entry = fifo.find_by_hash(&hash).await.unwrap();
        assert_eq!(entry.status, PreconfStatus::Success);
    }

    /// Mixed sources share the `LoopState` `preconf_gas_used` accounting:
    /// a Journal tx that bypasses the gate still contributes to the
    /// running total, so a subsequent RPC tx sees the true cost and
    /// can be gated properly.
    #[tokio::test]
    async fn mixed_sources_share_gas_accounting() {
        let cfg = PreconfConfig {
            preconf_max_gas_per_block: 40_000,
            preconf_max_gas_per_tx: 30_000,
            ..PreconfConfig::default()
        };
        let fifo = PreconfTxSet::new(8);
        let mut state = LoopState::new(1);

        // Journal tx: 30_000 gas, bypasses budget gate.
        let j_tx = make_tx_with_gas(0xe2, 0, 30_000);
        let j_hash = *j_tx.tx_hash();
        fifo.push_if_absent(j_tx, Address::from([1; 20]), PreconfSource::Replay).await;
        apply_one_preconf(&fifo, &cfg, None, j_hash, &mut state, synthetic_ok).await.unwrap();
        assert_eq!(state.preconf_gas_used(), 30_000);

        // RPC tx: 21_000 gas. 30_000 + 21_000 = 51_000 > 40_000 → rejected.
        let r_tx = make_tx_with_gas(0xe3, 0, 21_000);
        let r_hash = *r_tx.tx_hash();
        fifo.push_if_absent(r_tx, Address::from([2; 20]), PreconfSource::Rpc).await;
        apply_one_preconf(&fifo, &cfg, None, r_hash, &mut state, synthetic_ok).await.unwrap();

        assert_eq!(state.committed_len(), 1, "only the journal tx committed");
        let j_entry = fifo.find_by_hash(&j_hash).await.unwrap();
        assert_eq!(j_entry.status, PreconfStatus::Success);
        assert!(!fifo.contains(&r_hash).await, "the budget-rejected one is over and gone");
    }

    /// Race regression: an RPC-side timeout deadline fires **while**
    /// `apply_one_preconf` is inside `apply_fn`. The per-entry
    /// `apply_lock` acquired by dispatch before `apply_fn` must block
    /// the RPC's `lock_for_apply` acquisition until dispatch finishes
    /// `mark_succeeded` + `resp.send(...)`. When the RPC finally
    /// acquires the lock, fifo status is `Success` and `resp_rx` has
    /// the receipt queued — so `try_recv` returns it, and the client
    /// sees `Success`, not `Timeout`.
    ///
    /// Regression guard for the SLA invariant "wire `Timeout` ⇒ tx not
    /// committed to builder state". Without the `apply_lock` scheme,
    /// the RPC deadline branch would previously flip the entry to
    /// `Timeout` while dispatch had already committed the tx to the
    /// in-flight builder, producing an on-chain landing under a
    /// Timeout wire response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn apply_lock_blocks_rpc_timeout_race_and_yields_success() {
        let fifo = Arc::new(PreconfTxSet::new(8));
        let cfg = PreconfConfig::default();
        let tx = make_tx(0x99);
        let hash = *tx.tx_hash();

        let (resp_tx, mut resp_rx) = oneshot::channel();
        assert!(matches!(
            fifo.push_if_absent(tx.clone(), Address::ZERO, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));
        assert!(fifo.set_responder(hash, std::time::Instant::now(), resp_tx).await);

        // Slow apply closure — `std::thread::sleep` blocks the worker
        // that dispatch runs on but leaves other workers free (test
        // uses multi_thread). Simulates a ~200ms EVM apply.
        const APPLY_DURATION: Duration = Duration::from_millis(200);
        let slow_apply = |tx: Arc<TxEnvelope>, h: TxHash, height: u64| {
            std::thread::sleep(APPLY_DURATION);
            Ok(PreconfReceipt {
                tx_hash: h,
                block_height: height,
                status: true,
                logs: Vec::new(),
                gas_used: alloy_consensus::Transaction::gas_limit(tx.as_ref()),
                reason: String::new(),
                revert_data: Bytes::new(),
                tx_index: 0,
                cumulative_gas_used: alloy_consensus::Transaction::gas_limit(tx.as_ref()),
                logs_bloom: Bloom::default(),
                log_index_base: 0,
                tx_type: 2,
                from: Address::ZERO,
                to: None,
                contract_address: None,
                effective_gas_price: 0,
                block_timestamp: 0,
                l1_fields: Default::default(),
            })
        };

        // Spawn dispatch — will grab apply_lock and run slow_apply for
        // ~200ms before releasing.
        let fifo_clone = fifo.clone();
        let cfg_clone = cfg.clone();
        let dispatch_task = tokio::spawn(async move {
            let mut state = LoopState::new(7);
            apply_one_preconf(&fifo_clone, &cfg_clone, None, hash, &mut state, slow_apply)
                .await
                .unwrap();
        });

        // Give dispatch time to enter `apply_fn` (holding apply_lock).
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate the RPC-side deadline branch: acquire apply_lock.
        // Dispatch is inside apply_fn → this must block until dispatch
        // finishes mark_succeeded + send + drop guard.
        let acquire_start = std::time::Instant::now();
        let guard = fifo.lock_for_apply(&hash).await;
        let acquire_duration = acquire_start.elapsed();
        assert!(guard.is_some(), "lock_for_apply must return Some for the pushed entry");

        // Must have waited for dispatch to finish (~150ms remaining
        // after our 50ms head start).
        assert!(
            acquire_duration >= Duration::from_millis(100),
            "RPC lock acquisition should have blocked on dispatch's apply_lock; \
             waited {acquire_duration:?} but expected ≥ 100ms",
        );

        // Under the lock, fifo status must be final and the receipt
        // must be queued in resp_rx (dispatch's `resp.send(...)`
        // completed inside the critical section).
        let final_status = fifo.find_by_hash(&hash).await.map(|e| e.status);
        assert_eq!(
            final_status,
            Some(PreconfStatus::Success),
            "dispatch must have finished mark_succeeded before releasing apply_lock",
        );

        match resp_rx.try_recv() {
            Ok(Ok(receipt)) => {
                assert_eq!(receipt.tx_hash, hash);
                assert!(receipt.status);
            }
            other => panic!("resp_rx must have queued receipt; got {other:?}"),
        }

        drop(guard);
        dispatch_task.await.expect("dispatch task join");
    }
}
