//! RPC entry point for `eth_sendRawTransactionWithPreconf`.
//!
//! [`PreconfRpcHandler`] is the local-sequencer implementation of the
//! preconf flow. The wire-layer trait + the `MantleRpcExt::send_raw_…`
//! method live in `mantle-reth-rpc-ext`; this module supplies the concrete
//! handler that gets injected into `MantleRpcExt` when this node is acting
//! as the sequencer with preconf enabled.
//!
//! What is left here is the wait. Deciding whether a transaction may be
//! queued — its type, the allowlist, the per-tx gas ceiling, the validator
//! chain and the queue's own rules — is one call into [`crate::PreconfAdmission`],
//! and the client's channel goes in under the queue's lock as part of it, so
//! the builder can never reach an entry with nobody to answer for it.
//!
//! That leaves this module two jobs:
//!
//! * **Race the deadline.** `select!` rather than `timeout`, so the receiver outlives the elapsed
//!   branch. On elapse the per-entry apply lock is taken, which serialises with dispatch's point of
//!   no return; once held, the entry's status is definitive and a receipt dispatch already sent can
//!   still be picked up. Without that the client could be told `Timeout` for a transaction already
//!   committed to the block being built.
//! * **Record the commitment.** Every receipt is recorded, reverts included — a revert is an
//!   outcome, not a failure to keep the promise.
//!
//! Two doors hold a replaying commitment open against this path. A resubmit of
//! a hash that is mid-replay must not be able to destroy it by timing out: the
//! entry's receipt went to an earlier process, and marking it `Timeout` here
//! would make it replaceable and sweepable. Both are keyed on the entry's
//! source rather than its status, because a replaying commitment sits in
//! exactly the same `Waiting` state a fresh one does.

use std::sync::Arc;

use alloy_primitives::{Bytes, TxKind};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, types::ErrorObject};
use mantle_reth_rpc_ext::{
    DynPreconfHandler, PreconfLog, PreconfStatus as WireStatus, PreconfTxEvent, PreconfTxReceipt,
};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::{
    PreconfClassifier, PreconfConfig, PreconfTxSet,
    admission::{AdmittedTx, DynAdmission},
    types::{PreconfError, PreconfReceipt, PreconfSource, PreconfStatus},
};

/// Generic preconf RPC handler. Constructed by the preconf `ServiceBuilder`
/// once the pool + provider are wired up.
pub struct PreconfRpcHandler {
    /// Everything between "bytes arrived" and "queued", in one call.
    admission: Arc<dyn DynAdmission>,
    fifo: Arc<PreconfTxSet>,
    cfg: Arc<PreconfConfig>,
    /// Owns the allowlists and every frozen verdict. The single decider of
    /// preconf eligibility, shared with the validator and the builder.
    classifier: Arc<PreconfClassifier>,
}

impl std::fmt::Debug for PreconfRpcHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfRpcHandler")
            .field("cfg", &self.cfg)
            .field("fifo", &self.fifo)
            .finish_non_exhaustive()
    }
}

impl PreconfRpcHandler {
    /// Construct a handler bound to the given admission + fifo.
    ///
    /// No state provider and no pool: admission reads what chain state it
    /// needs, and what is left here is waiting on the client's channel.
    pub const fn new(
        admission: Arc<dyn DynAdmission>,
        fifo: Arc<PreconfTxSet>,
        cfg: Arc<PreconfConfig>,
        classifier: Arc<PreconfClassifier>,
    ) -> Self {
        Self { admission, fifo, cfg, classifier }
    }

    /// Claim the `(sender, nonce)` slot for a commitment whose receipt is going
    /// out, and record the promise with the classifier — the in-memory authority
    /// for slot retention. `mark_committed` needs that record to recognise our
    /// transactions among a whole block's hashes.
    ///
    /// **Every receipt counts, not only `Success`.** A `Failed` event on a
    /// receipt path is an EVM revert that *produced* a receipt; every
    /// not-on-chain outcome leaves through an `Err(..)` return and never reaches
    /// here. The receipt goes out before the block is sealed, so a reverted tx is
    /// owed a replay exactly as a successful one is — and `builder::dispatch`
    /// agrees, marking the fifo entry `Success` without reading `receipt.status`.
    /// Pinned by `a_reverted_receipt_is_still_recorded_as_a_commitment`.
    async fn claim_commitment_slot(
        &self,
        event: &PreconfTxEvent,
        hash: alloy_primitives::TxHash,
        sender: &alloy_primitives::Address,
        nonce: u64,
    ) {
        // Establishing the record here — at the receipt, which necessarily
        // precedes the block — is what makes it available to both later events
        // (`forward → release_unless_committed` and the canonical notification)
        // no matter which of them runs first.
        if let Err(owner) = self.classifier.mark_promised(hash, sender, nonce, event.block_height) {
            warn!(
                target: "mantle::preconf::rpc",
                ?hash, ?owner,
                "a different tx already owns this (sender, nonce) at receipt time; \
                 the commitment may not be honoured"
            );
        }
    }

    /// Process a single `eth_sendRawTransactionWithPreconf` submission.
    ///
    /// See module-level documentation for the step-by-step semantics.
    pub async fn handle_inner(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        // Anchor the SLA clock to the moment the request landed, before any
        // decode or validator latency. `TxEntry` carries this instant as
        // `inserted_at`, so dispatch's deadline gate measures the budget the
        // client is actually waiting out.
        let origin_instant = std::time::Instant::now();

        // One call decides everything: the type, the allowlist, the per-tx
        // gas ceiling, the validator chain and the queue's own rules, in that
        // order, with the client's channel installed under the queue's lock
        // so the builder cannot reach an entry that has nobody to answer.
        //
        // What used to be five steps here reached the queue through the pool
        // and a listener, which is why a transaction the pool merely parked
        // produced no refusal at all — the client waited out the timeout to
        // learn nothing had happened.
        let (resp_tx, resp_rx) = oneshot::channel();
        let admitted = self
            .admission
            .admit(bytes, origin_instant, resp_tx)
            .await
            .map_err(|e| preconf_error_to_rpc(&e))?;
        let AdmittedTx { hash, sender, nonce, .. } = admitted;

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
            // Receipt arrived within the deadline. Every receipt is recorded,
            // reverts included — see `claim_commitment_slot`.
            Some(Ok(Ok(receipt))) => {
                let event = PreconfTxEvent::from(receipt);
                self.claim_commitment_slot(&event, hash, &sender, nonce).await;
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
                // A `None` from `lock_for_apply` means no fifo entry for
                // this hash — it was swept between admission and here. Treat
                // as a genuine timeout.
                let apply_guard = self.fifo.lock_for_apply(&hash).await;

                // Under the (possibly-held) lock, read the definitive
                // final state. The whole view is kept (not just `status`) because
                // the `Waiting` arm below has to know the entry's `source` — see
                // the retention note there.
                let final_entry = self.fifo.find_by_hash(&hash).await;
                let final_status = final_entry.as_ref().map(|e| e.status);

                match final_status {
                    // **Must precede the `Success | Failed` arm below.** A
                    // `Replay`-source `Failed` is a commitment we could not
                    // honour; an `Rpc`-source one is an ordinary apply
                    // rejection. That arm assumes dispatch queued a message on
                    // `resp_rx`, but the breach path deliberately sends
                    // nothing, so falling into it would report a `Timeout` the
                    // client retries forever.
                    //
                    // This is also the **only** channel through which a client
                    // can learn its commitment was broken: a `Replay` entry's
                    // responder was consumed when the receipt went out, and the
                    // node exposes no status-query RPC.
                    Some(PreconfStatus::Failed)
                        if final_entry
                            .as_ref()
                            .is_some_and(|e| e.source == PreconfSource::Replay) =>
                    {
                        drop(apply_guard);
                        let err = PreconfError::CommitmentBroken;
                        self.fifo.cancel_responder(&hash, err.clone()).await;
                        Err(preconf_error_to_rpc(&err))
                    }
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
                                self.claim_commitment_slot(&event, hash, &sender, nonce).await;
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
                        // `mark_timeout` would make that commitment
                        // replaceable by another hash, and sweepable.
                        //
                        // Reachable because admission accepts a same-hash
                        // resubmit onto a live `Waiting` entry whose responder
                        // was already taken, which is the normal shape of a
                        // replaying commitment. This client's request does time
                        // out — but the commitment keeps being retried.
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
                            // Nothing to evict alongside it: the transaction
                            // never entered the pool, so the queue is the only
                            // place it can be. `NotFound` here means it was
                            // already swept, which is the same end state.
                            let _ = self.fifo.mark_timeout(&hash).await;
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
// the impl behind the `Arc<dyn DynPreconfHandler>` `MantleRpcExt` holds.
// `Option<Arc<dyn DynPreconfHandler>>`.
#[async_trait]
impl DynPreconfHandler for PreconfRpcHandler {
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

/// A creation has no recipient at all, which is not the same as a call to the
/// zero address — see `PreconfClassifier::evaluate_whitelist`'s docs for why
/// only a from-wildcard can authorize one. Shared with the payload builder so
/// the two ask the allowlist the same question.
pub(crate) fn tx_kind_to_address(kind: TxKind) -> Option<alloy_primitives::Address> {
    match kind {
        TxKind::Call(addr) => Some(addr),
        TxKind::Create => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes as PrimBytes, Log, LogData};
    use mantle_reth_rpc_ext::PreconfStatus as WireStatus;
    use std::{collections::HashSet, time::Duration};

    use crate::{admission::AdmittedTx, classifier::DEFAULT_VERDICT_CACHE_CAP};

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

    // --- what is left of the handler ------------------------------------
    //
    // Deciding whether a transaction may be queued moved to `admission`, and
    // its tests went with it. What stays here is the wait on the client's
    // channel and the commitment record that goes out with a receipt, so the
    // harness below needs an admission only to satisfy the type — it is never
    // called.

    const RECIPIENT: Address = Address::new([0x42; 20]);

    #[derive(Debug)]
    struct UnusedAdmission;

    #[async_trait]
    impl DynAdmission for UnusedAdmission {
        async fn admit(
            &self,
            _bytes: Bytes,
            _origin_instant: std::time::Instant,
            _responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
        ) -> Result<AdmittedTx, PreconfError> {
            unreachable!("these tests do not go through admission")
        }
    }

    struct Harness {
        handler: PreconfRpcHandler,
        classifier: Arc<PreconfClassifier>,
        sender: Address,
    }

    fn harness() -> Harness {
        let sender = Address::from([0x11; 20]);
        let classifier = Arc::new(PreconfClassifier::new(
            false,
            Duration::from_secs(3600),
            DEFAULT_VERDICT_CACHE_CAP,
        ));
        classifier.update_whitelist(
            [(sender, RECIPIENT)].into_iter().collect(),
            HashSet::default(),
            HashSet::default(),
        );

        let cfg = PreconfConfig {
            enabled: true,
            preconf_max_gas_per_tx: 1_000_000,
            ..Default::default()
        };
        let handler = PreconfRpcHandler::new(
            Arc::new(UnusedAdmission),
            Arc::new(PreconfTxSet::new(16)),
            Arc::new(cfg),
            classifier.clone(),
        );
        Harness { handler, classifier, sender }
    }

    /// An EVM revert that produced a receipt is a commitment like any other —
    /// see `claim_commitment_slot` for why the gate is not on `Success`.
    ///
    /// `builder::dispatch` marks the fifo entry `Success` regardless of
    /// `receipt.status`, so a gate here would leave the fifo saying "committed"
    /// about a transaction the classifier had never heard of.
    #[tokio::test]
    async fn a_reverted_receipt_is_still_recorded_as_a_commitment() {
        let h = harness();
        let hash = B256::from([0xab; 32]);
        let sender = h.sender;

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

        h.handler.claim_commitment_slot(&event, hash, &sender, 0).await;

        assert!(
            h.classifier.is_promised(&hash),
            "the classifier must know the commitment, or a reorg reinject is \
             re-gated as a fresh submission and `mark_committed` cannot count it",
        );
    }
}
