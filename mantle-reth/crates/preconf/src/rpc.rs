//! RPC entry point for `eth_sendRawTransactionWithPreconf`.
//!
//! [`PreconfRpcHandler`] is the local-sequencer implementation of the
//! preconf flow. The wire-layer trait + the `MantleRpcExt::send_raw_…`
//! method live in `mantle-reth-rpc-ext`; this module supplies the concrete
//! handler that gets injected into `MantleRpcExt` when this node is acting
//! as the sequencer with preconf enabled.
//!
//! Flow:
//!
//! 1. Decode + recover the raw transaction.
//! 2. Whitelist check via [`PreconfClassifier::preview_eligibility`].
//! 3. Nonce-gap + cumulative-balance pre-checks against a single `latest` snapshot and one pool
//!    scan (`get_pending_nonce_and_cumulative_cost`).
//! 4. Attach a oneshot responder to [`PreconfTxSet`] **before** calling
//!    [`TransactionPool::add_transaction`] — otherwise the listener could push the entry and the
//!    builder could apply it before the responder is registered, dropping the receipt.
//! 5. Submit to the pool. The `AlreadyImported` branch is the same-hash retry path — if the fifo
//!    entry is in `Timeout`, atomically revive it back to `Waiting` and re-notify the builder.
//! 6. Wait on the responder with [`PreconfConfig::preconf_timeout`]. On elapsed, return `Ok(Timeout
//!    event)` (op-geth-aligned) and clean the fifo entry (`mark_timeout`), responder
//!    (`cancel_responder`), **and** the pool. A tx routed to `BaseFee`/`Queued` never produces a
//!    fifo entry (the listener filters to `SubPool::Pending`), so `mark_timeout` returns `NotFound`
//!    and its pool-eviction callback never fires — hence the explicit `pool.remove_transactions`
//!    (else the orphan is mined once eligible) and `cancel_responder` (else it stays stuck in
//!    `pending_responders`).

use std::sync::Arc;

use alloy_consensus::Transaction;
use alloy_primitives::{Bytes, TxKind};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, types::ErrorObject};
use mantle_reth_rpc_ext::{
    DynPreconfHandler, PreconfLog, PreconfStatus as WireStatus, PreconfTxEvent, PreconfTxReceipt,
};
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    PoolPooledTx, PoolTransaction, TransactionOrigin, TransactionPool, error::PoolErrorKind,
};
use tokio::sync::oneshot;
use tracing::{debug, trace, warn};

use crate::{
    PreconfClassifier, PreconfConfig, PreconfJournal, PreconfTxSet,
    classifier::PreconfClaimError,
    journal::JournalEntry,
    types::{AttachError, PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
};

/// Generic preconf RPC handler. Constructed by the preconf `ServiceBuilder`
/// once the pool + provider are wired up.
pub struct PreconfRpcHandler<P, Pr> {
    pool: P,
    provider: Pr,
    fifo: Arc<PreconfTxSet>,
    cfg: Arc<PreconfConfig>,
    /// Owns the allowlists and every frozen verdict. The single decider of
    /// preconf eligibility, shared with the validator and the builder.
    classifier: Arc<PreconfClassifier>,
    /// Persistence sink (mandatory) — every commitment whose receipt goes
    /// out is appended to the journal before the `PreconfTxEvent` is
    /// returned to the client. Append failures are logged but do not block
    /// the response (best-effort durability; a crash before the next disk
    /// flush loses at most the most recent commitment).
    journal: Arc<PreconfJournal>,
}

// Manual Debug — `P` (TransactionPool) and `Pr` (StateProviderFactory) do
// not require `Debug`, and propagating that bound to every call-site of
// `PreconfRpcHandler::new(...)` would be intrusive. The pool/provider have
// no useful Debug representation here anyway.
impl<P, Pr> std::fmt::Debug for PreconfRpcHandler<P, Pr> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfRpcHandler")
            .field("cfg", &self.cfg)
            .field("fifo", &self.fifo)
            .finish_non_exhaustive()
    }
}

impl<P, Pr> PreconfRpcHandler<P, Pr> {
    /// Construct a handler bound to the given pool + provider + fifo.
    /// The `journal` is mandatory — every successful commitment is persisted.
    pub const fn new(
        pool: P,
        provider: Pr,
        fifo: Arc<PreconfTxSet>,
        cfg: Arc<PreconfConfig>,
        classifier: Arc<PreconfClassifier>,
        journal: Arc<PreconfJournal>,
    ) -> Self {
        Self { pool, provider, fifo, cfg, classifier, journal }
    }
}

impl<P, Pr> PreconfRpcHandler<P, Pr>
where
    P: TransactionPool + 'static,
    Pr: StateProviderFactory + 'static,
{
    /// Records a returned receipt in **both** halves of commitment tracking:
    /// the classifier (in memory, and the authority for slot retention) and the
    /// journal (on disk, for restart).
    ///
    /// The two writes live in one function on purpose. Commitment tracking rests
    /// on the two sets agreeing — the classifier's promise record is what lets
    /// `mark_committed` recognise our transactions among a whole block's hashes,
    /// and journal restore rebuilds that record from the file. Two call sites
    /// each doing two writes would make that agreement a thing to remember;
    /// here it is a thing that cannot be got wrong.
    ///
    /// # Every receipt counts, not only `Success`
    ///
    /// Both call sites are receipt arms, and a `Failed` event there is an **EVM
    /// revert that produced a receipt** — not "never executed". Every
    /// not-on-chain outcome leaves through an `Err(..)` return and never reaches
    /// this function.
    ///
    /// The receipt is handed to the client when the transaction is applied to
    /// the **in-flight** payload, before the block is sealed. So a reverted
    /// transaction sits in exactly the same position as a successful one: the
    /// client holds a receipt naming a height, and a crash before sealing makes
    /// that receipt a lie unless restore replays it. Gating on `Success` would
    /// protect one and not the other for no reason either can be told apart by.
    ///
    /// The fifo already takes this view: `builder::dispatch`'s `Ok(receipt)` arm
    /// calls `mark_succeeded` without consulting `receipt.status`, so a reverted
    /// transaction is `PreconfStatus::Success` there. A `Success`-only gate here
    /// would leave the two structures disagreeing about the same transaction —
    /// fifo says committed, the classifier and the journal have never heard of
    /// it. Two further things follow from that record existing: the pool
    /// listener routes a reorg reinject as `Replay` rather than subjecting an
    /// acknowledged commitment to the deadline and gas gates again, and
    /// `mark_committed` can count it toward the reorg-drift signal.
    ///
    /// Pinned by `a_reverted_receipt_is_still_recorded_as_a_commitment`.
    async fn record_commitment(
        &self,
        event: &PreconfTxEvent,
        hash: alloy_primitives::TxHash,
        sender: &alloy_primitives::Address,
        nonce: u64,
        tx_rlp: &Bytes,
    ) {
        // Establishing the record here — at the receipt, which necessarily
        // precedes the block — is what makes it available to both later events
        // (`forward → release_unless_committed` and the canonical notification)
        // no matter which of them runs first.
        if let Err(owner) = self.classifier.mark_promised(hash, sender, nonce) {
            warn!(
                target: "mantle::preconf::rpc",
                ?hash, ?owner,
                "a different tx already owns this (sender, nonce) at receipt time; \
                 the commitment may not be honoured"
            );
        }

        // Best-effort — a crash before flush loses at most this single record.
        let entry = JournalEntry {
            hash,
            tx_rlp: tx_rlp.clone(),
            block_height: event.block_height,
            committed_at_ms: now_unix_ms(),
        };
        if let Err(e) = self.journal.append_promised(&entry).await {
            warn!(
                target: "mantle::preconf::rpc",
                ?hash, ?e,
                "journal append failed; commitment may be lost on restart"
            );
        }
    }

    /// Process a single `eth_sendRawTransactionWithPreconf` submission.
    ///
    /// See module-level documentation for the step-by-step semantics.
    pub async fn handle_inner(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        // Anchor the SLA clock to the moment the request landed in the
        // handler, before any decode / pool-validator latency. `TxEntry`
        // eventually carries this instant as `inserted_at`, so the
        // dispatch deadline gate measures against the client-visible
        // budget rather than the pool-listener drain time.
        let origin_instant = std::time::Instant::now();

        // Step 0 — decode + recover, then convert to the pool's `Transaction`
        // type. Reading sender/nonce/hash/kind from the pool tx goes through
        // its `PoolTransaction` + `Transaction` impls and avoids importing
        // the alloy `RecoveredTx` accessor trait.
        let recovered =
            recover_raw_transaction::<PoolPooledTx<P>>(&bytes).map_err(|e| internal_err(&e))?;
        let pool_tx = <P::Transaction as PoolTransaction>::from_pooled(recovered);

        let sender = pool_tx.sender();
        let hash = *pool_tx.hash();
        let nonce = pool_tx.nonce();
        let to_opt = tx_kind_to_address(pool_tx.kind());
        let gas_limit = alloy_consensus::Transaction::gas_limit(&pool_tx);

        // Step 1 — whitelist. Non-authoritative by design: a fast rejection
        // ahead of the state snapshot, the fifo entry and the pool, recording
        // nothing. The binding decision is Step 3b's `claim_preconf`, which
        // consults the same allowlists again and freezes the verdict it
        // derives; an allowlist update landing in between is caught there.
        // Keeping the two in the same order matters — reaching Step 3b with a
        // sender that was never allowlisted would still be refused, but only
        // after a `latest()` snapshot, a pool scan and a responder attached
        // and cancelled again.
        if !self.classifier.preview_eligibility(&sender, to_opt.as_ref()) {
            trace!(target: "mantle::preconf::rpc", ?sender, ?to_opt, ?hash, "non-whitelisted preconf submission");
            return Err(preconf_error_to_rpc(&PreconfError::NotPreconfEligible));
        }

        // Step 2 — nonce-gap + balance pre-checks. A tx that is valid per-tx
        // but parked (nonce gap → `Queued`; cumulative funds short →
        // `!ENOUGH_BALANCE`) never reaches `Pending`, so it gets no fifo entry
        // and the client blocks the full timeout. Reject both synchronously.
        // One snapshot + one scan yield nonce, balance, `pending_nonce`, and
        // `committed_cost` (Σ over the gapless chain below `pending_nonce`).
        // Stale (`nonce < on_chain_nonce`) is left to the inner validator.
        let state = self.provider.latest().map_err(|e| internal_err(&e))?;
        let on_chain_nonce =
            state.account_nonce(&sender).map_err(|e| internal_err(&e))?.unwrap_or(0);
        let on_chain_balance =
            state.account_balance(&sender).map_err(|e| internal_err(&e))?.unwrap_or_default();
        let (pending_nonce, committed_cost) =
            self.pool.get_pending_nonce_and_cumulative_cost(sender, on_chain_nonce);
        if nonce > pending_nonce {
            debug!(target: "mantle::preconf::rpc", ?sender, ?nonce, ?pending_nonce, "nonce gap rejected");
            return Err(preconf_error_to_rpc(&PreconfError::NonceGap {
                tx_nonce: nonce,
                pending_nonce,
            }));
        }

        // Append case only, and only the *cumulative* shortfall the pool would
        // silently park: a tx that is individually affordable (so the inner
        // validator admits it) but whose running total across the sender's
        // pending chain exceeds the balance (`!ENOUGH_BALANCE` → non-pending).
        // A tx that alone exceeds the balance, or a replacement
        // (`nonce < pending_nonce`), is left to the inner validator / pool —
        // reth already rejects those synchronously. `saturating_add` guards the
        // `value == U256::MAX` edge. Replacements are left to the pool.
        if nonce == pending_nonce {
            let own_cost = pool_tx.cost().saturating_add(pool_tx.extra_balance_cost());
            let required = committed_cost.saturating_add(own_cost);
            if own_cost <= on_chain_balance && required > on_chain_balance {
                debug!(
                    target: "mantle::preconf::rpc",
                    ?sender, ?nonce, %required, %on_chain_balance,
                    "insufficient funds rejected"
                );
                return Err(preconf_error_to_rpc(&PreconfError::InsufficientFunds {
                    balance: on_chain_balance,
                    required,
                }));
            }
        }

        // Step 3 — attach responder BEFORE pool.add. See module-level docs.
        let (resp_tx, resp_rx) = oneshot::channel();
        if let Err(AttachError::AlreadyAttached) =
            self.fifo.attach_responder(hash, origin_instant, resp_tx).await
        {
            return Err(preconf_error_to_rpc(&PreconfError::AlreadyInProgress));
        }

        // Step 3b — claim the preconf verdict, before the pool ever sees the
        // transaction.
        //
        // **This is where eligibility is decided**, because this is the only
        // point at which the deciding fact exists: that the client called
        // `eth_sendRawTransactionWithPreconf` rather than `eth_sendRawTransaction`.
        // One layer down the two are indistinguishable — both reach the pool as
        // `TransactionOrigin::External` — so the validator can only latch
        // whatever it finds, and what it finds is what we write here.
        //
        // The per-tx gas ceiling is checked here too, for the same reason: the
        // verdict and the conditions it was granted under have to be decided
        // together. It is what stops an `Eligible` verdict existing for a
        // transaction that was never held to the ceiling.
        //
        // Because it runs *before* the verdict is written, this is in practice
        // **the** enforcement point for this RPC: the validator's copy of the
        // ceiling gates on `verdict.is_preconf()`, and no over-cap request gets
        // as far as having a verdict. The validator keeps its copy as defence in
        // depth for any future writer of an eligible verdict — see the comment
        // there, and do not restore the earlier claim that this check is merely
        // an optimisation ahead of the real one.
        //
        // Pinned by the integration test
        // `per_tx_gas_ceiling_rejected_at_rpc_not_by_the_pool`, which asserts the
        // error is *this* one and not a `PoolRejected` wrapper — the two Displays
        // overlap enough that a substring match cannot tell them apart.
        //
        // `Err` means the hash already carries a frozen non-preconf verdict —
        // the same raw transaction went in through plain `eth_sendRawTransaction`
        // or arrived over p2p first. A verdict is immutable for the life of the
        // transaction, so this request can never be satisfied; say so plainly
        // rather than letting the client wait out `preconf_timeout`.
        if gas_limit > self.cfg.preconf_max_gas_per_tx {
            let err = PreconfError::PreconfGasLimitExceeded {
                gas_limit,
                max: self.cfg.preconf_max_gas_per_tx,
            };
            self.fifo.cancel_responder(&hash, err.clone()).await;
            return Err(preconf_error_to_rpc(&err));
        }
        if let Err(rejection) = self.classifier.claim_preconf(hash, &sender, to_opt.as_ref()) {
            let err = match rejection {
                // Step 1 already previewed the allowlist, so reaching this arm
                // means governance changed it in between. Report it the same way
                // Step 1 would have.
                PreconfClaimError::NotAllowlisted => PreconfError::NotPreconfEligible,
                PreconfClaimError::AlreadyClassified(_) => {
                    PreconfError::AlreadyPooledWithoutPreconf
                }
            };
            trace!(
                target: "mantle::preconf::rpc",
                ?hash, ?rejection,
                "preconf claim refused"
            );
            self.fifo.cancel_responder(&hash, err.clone()).await;
            return Err(preconf_error_to_rpc(&err));
        }

        // Step 4 — submit to pool.
        //
        // `Ok(_)` and `Err(AlreadyImported)` are both admission successes
        // and fall through to Step 5. The pool listener will observe
        // whichever admission path took effect and call
        // `push_if_absent`, which handles the same-hash retry case
        // internally: if the fifo entry for this hash is in a
        // reclaimable terminal state (`Timeout` / `Canceled`), it is
        // revived to `Waiting` and broadcast so dispatch can pick it up.
        // Active-state resubmits are rejected earlier at Step 3
        // (`attach_responder`) — by the time we reach Step 4 we know
        // the fifo entry is either absent or in a reclaimable state.
        match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
            Ok(_) => {}
            Err(e) if matches!(e.kind, PoolErrorKind::AlreadyImported) => {}
            Err(e) => {
                // Everything the pool refuses, for any reason, lands here.
                // That includes **validator rejections** — `PoolInner::add_transaction`
                // maps a `TransactionValidationOutcome::Invalid` to `Err` just
                // as it does an insertion failure — so this branch covers the
                // per-tx gas ceiling, `ReplaceActivePreconf`, a lost handover
                // CAS and every inner-validator refusal, alongside pool limits
                // and underpriced replacements. (An earlier version of this
                // comment claimed validation had already passed by this point;
                // it had not, and the release below is load-bearing for those
                // paths.)
                //
                // Either way a verdict is frozen and the `(sender, nonce)` slot
                // may be claimed for a transaction that is not in the pool.
                // Release both: the slot would otherwise block that nonce until
                // the next sweep. The verdict here is ours — Step 3b wrote it —
                // so nothing else can be relying on it.
                //
                // A promised commitment is the exception, and the exemption is
                // `release_preconf_claim`'s to make, not this call site's: the
                // predicate is subtler than it looks (a promise does not change
                // the verdict) and the validator has to make the identical
                // judgement about the identical record. See that method.
                //
                // Pinned by `a_pool_refusal_releases_the_verdict_the_request_froze`
                // and `a_pool_refusal_must_not_drop_a_promised_commitment`.
                self.classifier.release_preconf_claim(&hash);
                let err = PreconfError::PoolRejected(format!("{}", e.kind));
                self.fifo.cancel_responder(&hash, err.clone()).await;
                return Err(preconf_error_to_rpc(&err));
            }
        }

        // Step 5 — await receipt or deadline, with race-safe handling.
        //
        // We use `tokio::select!` (not `tokio::time::timeout`) so
        // `resp_rx` outlives the deadline. On the deadline branch, we
        // acquire the per-entry `apply_lock` which serializes with
        // dispatch's "point of no return"; once we hold the lock, the
        // entry's status is definitive (either `Success`/`Failed`
        // because dispatch committed and sent the receipt into
        // `resp_rx`, or `Waiting` because dispatch never ran the
        // apply). The `try_recv()` on `resp_rx` then reliably picks
        // up any receipt that dispatch sent — closing the SLA race
        // where a client could previously see `Timeout` even though
        // the tx had already been committed to the builder state.
        let preconf_timeout = self.cfg.preconf_timeout;
        let deadline = tokio::time::sleep(preconf_timeout);
        tokio::pin!(deadline);
        let mut resp_rx = resp_rx;

        let recv_result: Option<
            Result<Result<PreconfReceipt, PreconfError>, oneshot::error::RecvError>,
        > = tokio::select! {
            biased;
            recv = &mut resp_rx => Some(recv),
            _ = &mut deadline => None,
        };

        match recv_result {
            // Receipt arrived within the deadline. Persist on Success
            // (best-effort — a crash before flush loses at most this
            // single record).
            Some(Ok(Ok(receipt))) => {
                let event = PreconfTxEvent::from(receipt);
                self.record_commitment(&event, hash, &sender, nonce, &bytes).await;
                Ok(event)
            }

            // Builder signalled an error through the responder. A
            // `Timeout` error is surfaced as an `Ok(Timeout event)`
            // (op-geth-aligned wire shape), never a JSON-RPC error —
            // matching the deadline branch's own timeout handling.
            Some(Ok(Err(err))) => {
                if matches!(err, PreconfError::Timeout { .. }) {
                    Ok(build_timeout_event(hash, preconf_timeout))
                } else {
                    Err(preconf_error_to_rpc(&err))
                }
            }

            // Builder dropped the responder without sending — should not
            // happen on healthy paths. Mark the entry `Canceled`
            // (revivable + swept by `clean_reclaimable`) to signal a
            // server-side failure with the tx never applied.
            Some(Err(_recv_err)) => {
                warn!(target: "mantle::preconf::rpc", ?hash, "responder dropped before send");
                let _ = self.fifo.mark_canceled(&hash).await;
                let err = PreconfError::Internal("responder dropped before send".to_string());
                self.fifo.cancel_responder(&hash, err.clone()).await;
                Err(preconf_error_to_rpc(&err))
            }

            // Deadline elapsed. `resp_rx` is still alive (select! did
            // not consume it) — see match arm body for the race
            // resolution.
            None => {
                debug!(target: "mantle::preconf::rpc", ?hash, ?preconf_timeout, "preconf deadline elapsed; resolving race");
                metrics::counter!("preconf.api.timeout_total").increment(1);

                // Acquire the per-entry `apply_lock`. If dispatch is
                // running `apply_fn`, this blocks until it finishes
                // mark_* + send. If dispatch never started (no active
                // build, or gates rejected), we get the lock
                // immediately.
                //
                // A `None` from `lock_for_apply` means no fifo entry
                // for this hash — typically the pool listener routed
                // the tx to `BaseFee`/`Queued`, so the fifo push never
                // happened. Treat as a genuine timeout.
                let apply_guard = self.fifo.lock_for_apply(&hash).await;

                // Under the (possibly-held) lock, read the definitive
                // final state. The whole view is kept (not just `status`) because
                // the `Waiting` arm below has to know the entry's `source` — see
                // the retention note there.
                let final_entry = self.fifo.find_by_hash(&hash).await;
                let final_status = final_entry.as_ref().map(|e| e.status);

                match final_status {
                    Some(PreconfStatus::Success | PreconfStatus::Failed) => {
                        // Apply committed to builder state between our
                        // deadline firing and lock acquisition. The
                        // receipt (or error) is already queued in
                        // `resp_rx` (dispatch sent it before releasing
                        // the apply_lock we now hold). Retrieve it
                        // non-blockingly.
                        drop(apply_guard);
                        match resp_rx.try_recv() {
                            Ok(Ok(receipt)) => {
                                let event = PreconfTxEvent::from(receipt);
                                self.record_commitment(&event, hash, &sender, nonce, &bytes).await;
                                Ok(event)
                            }
                            Ok(Err(err)) => Err(preconf_error_to_rpc(&err)),
                            Err(oneshot::error::TryRecvError::Empty) => {
                                // Status says terminal but resp_rx is
                                // empty — indicates lock-discipline
                                // regression in dispatch. Log and
                                // fall through to Timeout.
                                warn!(
                                    target: "mantle::preconf::rpc",
                                    ?hash, ?final_status,
                                    "terminal status but resp_rx empty; falling back to Timeout"
                                );
                                Ok(build_timeout_event(hash, preconf_timeout))
                            }
                            Err(oneshot::error::TryRecvError::Closed) => {
                                warn!(
                                    target: "mantle::preconf::rpc",
                                    ?hash,
                                    "resp_rx closed by dispatch without send; falling back to Timeout"
                                );
                                Ok(build_timeout_event(hash, preconf_timeout))
                            }
                        }
                    }
                    Some(PreconfStatus::Waiting) | None => {
                        // Apply never committed.
                        //
                        // Second retention door: do NOT time out an entry whose
                        // receipt has already gone out. `Replay` means exactly
                        // that (journal restore / reorg reinject / stale
                        // in-flight replay — see `PreconfSource`), and
                        // `mark_timeout` would make that commitment replaceable
                        // by another hash, sweepable, and evict it from the pool.
                        //
                        // Reachable because `attach_responder` accepts a
                        // same-hash resubmit onto a live `Waiting` entry whose
                        // responder was already taken, which is the normal shape
                        // of a replaying commitment. This client's request does
                        // time out — but the commitment keeps being retried.
                        let is_replay =
                            final_entry.as_ref().is_some_and(|e| e.source == PreconfSource::Replay);
                        if is_replay {
                            debug!(
                                target: "mantle::preconf::rpc",
                                ?hash,
                                "deadline elapsed on a replaying commitment; leaving it Waiting to retry"
                            );
                        } else {
                            // Transition under the still-held lock (or without
                            // any lock if the entry was absent — mark_timeout
                            // returns `NotFound`, which is fine).
                            //
                            // The CAS evicts the tx from the pool via its
                            // callback — but a pool-admitted tx with no fifo
                            // entry (parked in `BaseFee`/`Queued`, so the
                            // `Pending`-only listener never pushed it) returns
                            // `NotFound` and skips that callback. Evict it
                            // directly, else the orphan lingers and is mined
                            // once eligible — after the client saw `Timeout`.
                            if self.fifo.mark_timeout(&hash).await.is_err() {
                                self.pool.remove_transactions(vec![hash]);
                            }
                        }
                        drop(apply_guard);
                        self.fifo
                            .cancel_responder(
                                &hash,
                                PreconfError::Timeout {
                                    timeout_ms: preconf_timeout.as_millis() as u64,
                                },
                            )
                            .await;
                        Ok(build_timeout_event(hash, preconf_timeout))
                    }
                    Some(PreconfStatus::Timeout | PreconfStatus::Canceled) => {
                        // Some other path beat us (e.g. dispatch's
                        // deadline gate or block-gas-budget gate
                        // ran mark_* concurrently). The tx is not on
                        // chain; return Timeout to the client.
                        drop(apply_guard);
                        self.fifo
                            .cancel_responder(
                                &hash,
                                PreconfError::Timeout {
                                    timeout_ms: preconf_timeout.as_millis() as u64,
                                },
                            )
                            .await;
                        Ok(build_timeout_event(hash, preconf_timeout))
                    }
                    Some(PreconfStatus::Broken) => {
                        // This commitment was already given up on — the
                        // receipt went out in an earlier session and
                        // `preconf_max_apply_attempts` applies all failed. Tell
                        // this client the truth rather than a timeout it would
                        // retry forever.
                        let attempts =
                            final_entry.as_ref().map(|e| e.apply_failures).unwrap_or_default();
                        drop(apply_guard);
                        let err = PreconfError::CommitmentBroken { attempts };
                        self.fifo.cancel_responder(&hash, err.clone()).await;
                        Err(preconf_error_to_rpc(&err))
                    }
                }
            }
        }
    }
}

/// Construct the wire `Timeout` event returned to the client when the
/// preconf deadline expires without a committed apply.
fn build_timeout_event(
    hash: alloy_primitives::TxHash,
    preconf_timeout: std::time::Duration,
) -> PreconfTxEvent {
    PreconfTxEvent {
        tx_hash: hash,
        status: WireStatus::Timeout,
        reason: format!("preconf timeout after {preconf_timeout:?}"),
        block_height: 0,
        // No EVM apply happened → wire logs = null (tri-state).
        receipt: PreconfTxReceipt { logs: None },
    }
}

// `DynPreconfHandler` is the dyn-safe trait declared in `rpc-ext`; this is
// the impl that erases the `<P, Pr>` generics so `MantleRpcExt` can hold an
// `Option<Arc<dyn DynPreconfHandler>>`.
#[async_trait]
impl<P, Pr> DynPreconfHandler for PreconfRpcHandler<P, Pr>
where
    P: TransactionPool + 'static,
    Pr: StateProviderFactory + 'static,
{
    async fn handle(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        // Preconf-handling latency, measured around `handle_inner` to cover
        // every early-return path (reject / timeout / success).
        let started = std::time::Instant::now();
        let out = self.handle_inner(bytes).await;
        metrics::histogram!("preconf.api.handle_duration_ms")
            .record(started.elapsed().as_millis() as f64);
        out
    }
}

// ─── Conversions ────────────────────────────────────────────────────────────

/// Map the internal `PreconfReceipt` to the wire-layer `PreconfTxEvent`.
///
/// Wire-layer `Success`/`Failed` is derived from `receipt.status: bool`,
/// which reflects EVM execution outcome (`false` = revert/halt, `true` =
/// success). Both cases mean the tx **is on chain** — the receipt would
/// not exist otherwise.
///
/// Note the semantic mismatch with fifo-layer `PreconfStatus::Failed`,
/// which signals a builder pre-apply reject (nonce-too-low, block gas
/// budget, ...) — tx **NOT on chain**. That state never reaches this
/// conversion; it flows to the client through the `Ok(Ok(Err(err)))`
/// arm's `PreconfError`, not the receipt path.
///
/// `Waiting` / `Timeout` are constructed directly by the RPC handler's
/// other arms and never routed through this `From` impl. There is no
/// wire `Canceled` variant — server pre-apply rejections (the block-gas-budget gate gas
/// budget, admin action) are surfaced as wire `Failed` with the
/// specific reason in `PreconfTxEvent::reason`; the underlying fifo
/// `PreconfStatus::Canceled` is an internal-only distinction.
impl From<PreconfReceipt> for PreconfTxEvent {
    fn from(r: PreconfReceipt) -> Self {
        let status = if r.status { WireStatus::Success } else { WireStatus::Failed };
        let logs = r
            .logs
            .into_iter()
            .map(|log| PreconfLog {
                address: log.address,
                topics: log.data.topics().to_vec(),
                data: log.data.data,
            })
            .collect();
        Self {
            tx_hash: r.tx_hash,
            status,
            reason: r.reason,
            block_height: r.block_height,
            // Apply happened (via receipt path) — wrap logs in Some
            // even when empty, to signal "apply succeeded, no logs
            // emitted" (distinguished from Timeout's `None`).
            receipt: PreconfTxReceipt { logs: Some(logs) },
        }
    }
}

// ─── Error helpers ──────────────────────────────────────────────────────────

/// JSON-RPC error code used for all preconf-layer failures.
///
/// Matches the existing stub's choice in `mantle-reth-rpc-ext::lib.rs`
/// and op-geth's `MantleRpcErrCode` for cross-client SDK compatibility.
const PRECONF_RPC_ERR_CODE: i32 = -32000;

fn preconf_error_to_rpc(err: &PreconfError) -> ErrorObject<'static> {
    ErrorObject::owned(PRECONF_RPC_ERR_CODE, err.to_string(), None::<()>)
}

fn internal_err<E: std::fmt::Display>(e: &E) -> ErrorObject<'static> {
    ErrorObject::owned(PRECONF_RPC_ERR_CODE, format!("internal: {e}"), None::<()>)
}

fn tx_kind_to_address(kind: TxKind) -> Option<alloy_primitives::Address> {
    match kind {
        TxKind::Call(addr) => Some(addr),
        TxKind::Create => None,
    }
}

/// Current wall-clock milliseconds since the Unix epoch. Used to
/// stamp [`JournalEntry::committed_at_ms`]. Falls back to `0` if the
/// system clock is set before 1970, which is best treated as
/// "unset" rather than crashing the RPC path.
fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes as PrimBytes, Log, LogData, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use mantle_reth_rpc_ext::PreconfStatus as WireStatus;
    use reth_optimism_txpool::OpPooledTransaction;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::noop::NoopTransactionPool;
    use std::{collections::HashSet, time::Duration};

    use crate::classifier::{DEFAULT_VERDICT_CACHE_CAP, Verdict};

    fn sample_log(addr_byte: u8, topic_byte: u8, data_byte: u8) -> Log {
        let data = LogData::new_unchecked(
            vec![B256::from([topic_byte; 32])],
            PrimBytes::from(vec![data_byte; 4]),
        );
        Log { address: Address::from([addr_byte; 20]), data }
    }

    fn sample_receipt(success: bool) -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: B256::from([0xaa; 32]),
            block_height: 42,
            status: success,
            logs: vec![sample_log(1, 2, 3)],
            gas_used: 21_000,
            reason: if success { String::new() } else { "execution reverted".to_string() },
            revert_data: PrimBytes::new(),
        }
    }

    #[test]
    fn from_receipt_success_maps_to_success_status() {
        let event: PreconfTxEvent = sample_receipt(true).into();
        assert_eq!(event.status, WireStatus::Success);
        assert_eq!(event.tx_hash, B256::from([0xaa; 32]));
        assert_eq!(event.block_height, 42);
        assert!(event.reason.is_empty());
        assert_eq!(event.receipt.logs.as_ref().map(|l| l.len()), Some(1));
    }

    #[test]
    fn from_receipt_failed_maps_to_failed_status_with_reason() {
        let event: PreconfTxEvent = sample_receipt(false).into();
        assert_eq!(event.status, WireStatus::Failed);
        assert_eq!(event.reason, "execution reverted");
        assert_eq!(event.receipt.logs.as_ref().map(|l| l.len()), Some(1));
    }

    #[test]
    fn from_receipt_logs_preserve_address_topics_data() {
        let receipt = PreconfReceipt {
            tx_hash: B256::ZERO,
            block_height: 0,
            status: true,
            logs: vec![sample_log(7, 8, 9), sample_log(10, 11, 12)],
            gas_used: 0,
            reason: String::new(),
            revert_data: PrimBytes::new(),
        };
        let event: PreconfTxEvent = receipt.into();
        let logs = event.receipt.logs.expect("Some logs");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].address, Address::from([7; 20]));
        assert_eq!(logs[0].topics, vec![B256::from([8; 32])]);
        assert_eq!(logs[0].data, PrimBytes::from(vec![9, 9, 9, 9]));
        assert_eq!(logs[1].address, Address::from([10; 20]));
    }

    #[test]
    fn from_receipt_empty_logs() {
        let receipt = PreconfReceipt {
            tx_hash: B256::from([0xff; 32]),
            block_height: 1,
            status: true,
            logs: vec![],
            gas_used: 0,
            reason: String::new(),
            revert_data: PrimBytes::new(),
        };
        let event: PreconfTxEvent = receipt.into();
        assert_eq!(event.receipt.logs.as_ref().map(|l| l.len()), Some(0));
    }

    #[test]
    fn tx_kind_call_returns_address() {
        let addr = Address::from([1; 20]);
        assert_eq!(tx_kind_to_address(TxKind::Call(addr)), Some(addr));
    }

    #[test]
    fn tx_kind_create_returns_none() {
        assert_eq!(tx_kind_to_address(TxKind::Create), None);
    }

    #[test]
    fn preconf_error_to_rpc_uses_preconf_code() {
        let err = PreconfError::NotPreconfEligible;
        let rpc = preconf_error_to_rpc(&err);
        assert_eq!(rpc.code(), PRECONF_RPC_ERR_CODE);
        assert!(rpc.message().contains("not preconf eligible"));
    }

    #[test]
    fn preconf_error_to_rpc_nonce_gap_includes_values() {
        let err = PreconfError::NonceGap { tx_nonce: 5, pending_nonce: 3 };
        let rpc = preconf_error_to_rpc(&err);
        assert!(rpc.message().contains('5') && rpc.message().contains('3'));
    }

    #[test]
    fn internal_err_prefixes_message() {
        let inner = "boom";
        let rpc = internal_err(&inner);
        assert!(rpc.message().starts_with("internal: "));
        assert!(rpc.message().ends_with("boom"));
    }

    // --- `handle_inner`'s pool-refusal branch -----------------------------
    //
    // An earlier note here deferred these to end-to-end coverage, calling a
    // `TransactionPool` impl with a `recover_raw_transaction`-compatible
    // `Pooled` type "heavyweight scaffolding". It is not: reth's
    // `NoopTransactionPool<T>` is generic over any `EthPoolTransaction`, so
    // `NoopTransactionPool<OpPooledTransaction>` is a ready-made pool whose
    // `add_transaction` **always** returns `Err` — which is precisely the
    // branch that needs pinning — and `MockEthProvider` supplies the on-chain
    // nonce Step 2 reads.
    //
    // Why it needs pinning here and cannot be delegated elsewhere: deleting
    // the release at Step 4's `Err` arm left the entire suite green (319 unit
    // + 90 integration). `validator::tests::validate_preconf` models the
    // release by hand, so every test built on that fixture asserts against
    // the *copy*, never this call site.
    //
    // One test is enough for all of it. Every preconf rejection — inner
    // validator, `ReplaceActivePreconf`, a lost handover CAS, pool limits —
    // arrives here as one `Err` and is released by one piece of code; which
    // rejection produced it is a validator-side fact, covered by
    // `validator::tests`.

    const RECIPIENT: Address = Address::new([0x42; 20]);

    struct Harness {
        handler: PreconfRpcHandler<NoopTransactionPool<OpPooledTransaction>, MockEthProvider>,
        classifier: Arc<PreconfClassifier>,
        fifo: Arc<PreconfTxSet>,
        signer: PrivateKeySigner,
        journal: Arc<PreconfJournal>,
        /// Owns the journal file's directory: dropping it deletes the file, so
        /// it has to outlive `handler`. The journal is mandatory, so there is no
        /// "no persistence" variant of this harness to fall back on.
        _journal_dir: tempfile::TempDir,
    }

    async fn harness() -> Harness {
        let signer =
            PrivateKeySigner::from_bytes(&B256::from([0x11; 32])).expect("valid secp256k1 scalar");

        let classifier = Arc::new(PreconfClassifier::new(
            false,
            Duration::from_secs(3600),
            DEFAULT_VERDICT_CACHE_CAP,
        ));
        classifier.update_whitelist(
            [(signer.address(), RECIPIENT)].into_iter().collect(),
            HashSet::default(),
            HashSet::default(),
        );

        // Nonce 0 on chain, so a nonce-0 submission clears Step 2's gap gate
        // (`NoopTransactionPool` reports no pending tx for the sender).
        let provider = MockEthProvider::default();
        provider.add_account(signer.address(), ExtendedAccount::new(0, U256::from(1u64)));

        let fifo = Arc::new(PreconfTxSet::new(16));
        let cfg = PreconfConfig {
            enabled: true,
            preconf_max_gas_per_tx: 1_000_000,
            ..Default::default()
        };

        let journal_dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            PreconfJournal::open(journal_dir.path().join("preconf.jsonl"), cfg.journal_max_size)
                .await
                .expect("journal opens in a fresh temp dir"),
        );

        let handler = PreconfRpcHandler::new(
            NoopTransactionPool::<OpPooledTransaction>::new(),
            provider,
            fifo.clone(),
            Arc::new(cfg),
            classifier.clone(),
            journal.clone(),
        );
        Harness { handler, classifier, fifo, signer, journal, _journal_dir: journal_dir }
    }

    /// A genuinely signed EIP-1559 transfer, encoded the way the wire delivers
    /// it. Step 0 recovers the sender cryptographically, so the fabricated
    /// `Signature::test_signature()` + `Signed::new_unchecked` shape the
    /// validator fixture uses would not survive the decode.
    fn signed_raw_tx(signer: &PrivateKeySigner, nonce: u64, gas_limit: u64) -> (Bytes, B256) {
        let tx = TxEip1559 {
            chain_id: 10,
            nonce,
            gas_limit,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(RECIPIENT),
            value: U256::from(1u64),
            ..Default::default()
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("in-memory signer");
        let signed = tx.into_signed(signature);
        let hash = *signed.hash();
        (TxEnvelope::Eip1559(signed).encoded_2718().into(), hash)
    }

    /// A pool refusal drops the verdict this request froze at Step 3b.
    ///
    /// Without it the hash keeps an `Eligible` verdict for a transaction that
    /// is not in the pool, and a verdict is immutable for the life of the
    /// transaction — so the sender could never get this transaction
    /// preconfirmed again, and the grace sweep would be the only way out.
    ///
    /// The `(sender, nonce)` slot is deliberately **not** asserted: Step 3b
    /// claims no slot (that is `admit_and_claim`'s job, inside the pool's own
    /// admission, which `NoopTransactionPool` never reaches). Asserting it
    /// here would be an assertion that cannot fail.
    #[tokio::test]
    async fn a_pool_refusal_releases_the_verdict_the_request_froze() {
        let h = harness().await;
        let (raw, hash) = signed_raw_tx(&h.signer, 0, 21_000);

        let err = h.handler.handle_inner(raw).await.expect_err("the noop pool refuses everything");
        assert!(
            err.message().contains("pool rejected"),
            "the refusal must surface as PoolRejected, not some earlier gate: {}",
            err.message(),
        );

        assert_eq!(h.classifier.verdict(&hash), None, "the frozen verdict must be released");
        assert!(!h.fifo.contains(&hash).await, "and no responder may stay parked");
    }

    /// The same refusal must **not** drop a promised commitment.
    ///
    /// Reachable shape: the transaction was applied and its `Success` receipt
    /// returned, so `record_commitment` → `mark_promised` ran, but its block is
    /// **not yet canonical** — so no `committed_height` guards the record. A
    /// same-hash resubmit inside that window is re-validated (the transaction is
    /// still pooled, so nothing deduplicates it) and can be refused on account
    /// state alone: Mantle recomputes `extra_balance_cost` every validation.
    /// See `release_preconf_claim` for why the later "nonce has advanced" story
    /// is *not* the reachable one.
    ///
    /// Both preconditions are established through the production calls in
    /// production order — Step 3b's `claim_preconf`, then `record_commitment`'s
    /// `mark_promised` — with only the block application itself elided. The
    /// refusal itself comes from the pool, not from a hand-set flag.
    ///
    /// This is why the predicate is `is_promised()` and not
    /// `verdict == Promised`: `mark_promised` sets the `promised` flag on an
    /// existing record without rewriting its verdict (`classifier.rs:733-737`),
    /// so the record left by the normal flow is `Eligible` + promised. Keying
    /// on the verdict would release it here, and `release_unless_committed`
    /// would not stop it — that guard reads `committed_height`, which only
    /// `mark_committed` sets, and the canonical notification has not arrived
    /// yet. Dropping it hands back the nonce of a commitment already
    /// acknowledged to a client, inside the window the retention rule exists
    /// to protect.
    #[tokio::test]
    async fn a_pool_refusal_must_not_drop_a_promised_commitment() {
        let h = harness().await;
        let (raw, hash) = signed_raw_tx(&h.signer, 0, 21_000);
        let sender = h.signer.address();

        assert_eq!(h.classifier.claim_preconf(hash, &sender, Some(&RECIPIENT)), Ok(()));
        assert_eq!(h.classifier.mark_promised(hash, &sender, 0), Ok(()));
        assert_eq!(
            h.classifier.verdict(&hash),
            Some(Verdict::Eligible),
            "the normal flow leaves the verdict alone; only the flag is set",
        );

        h.handler.handle_inner(raw).await.expect_err("the pool refuses the resubmit");

        assert!(h.classifier.is_promised(&hash), "the commitment record must survive the refusal");
        assert_eq!(
            h.classifier.slot_owner(&sender, 0),
            Some(hash),
            "and so must the nonce it was promised against",
        );
    }

    /// An EVM revert that produced a receipt is a commitment like any other.
    ///
    /// `WireStatus::Failed` on a receipt path means "executed, reverted, receipt
    /// handed to the client" — not "never executed", which leaves through an
    /// `Err(..)` return and never reaches `record_commitment`. The block is not
    /// sealed yet either way, so a crash owes this transaction a replay exactly
    /// as it owes one to a successful transaction.
    ///
    /// Both halves are asserted, because the failure mode of gating on `Success`
    /// is precisely that they disagree: `builder::dispatch` marks the fifo entry
    /// `Success` without consulting `receipt.status`, so a gate here would leave
    /// the fifo saying "committed" while the classifier and the journal had
    /// never heard of the transaction.
    #[tokio::test]
    async fn a_reverted_receipt_is_still_recorded_as_a_commitment() {
        let h = harness().await;
        let (raw, hash) = signed_raw_tx(&h.signer, 0, 21_000);
        let sender = h.signer.address();

        let event = PreconfTxEvent::from(PreconfReceipt {
            tx_hash: hash,
            block_height: 42,
            status: false, // EVM revert — a receipt exists all the same
            logs: vec![],
            gas_used: 21_000,
            reason: "execution reverted".to_string(),
            revert_data: PrimBytes::new(),
        });
        assert_eq!(event.status, WireStatus::Failed, "precondition: this is the reverted arm");

        h.handler.record_commitment(&event, hash, &sender, 0, &raw).await;

        assert!(
            h.classifier.is_promised(&hash),
            "the classifier must know the commitment, or a reorg reinject is \
             re-gated as a fresh submission and `mark_committed` cannot count it",
        );
        let (entries, _bad) = h.journal.load().await.expect("journal reads back");
        assert_eq!(
            entries.iter().map(|e| e.hash).collect::<Vec<_>>(),
            vec![hash],
            "and it must be on disk, or a crash before sealing loses a receipt \
             the client is already holding",
        );
    }
}
