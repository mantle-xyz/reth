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
//! 2. Whitelist check via [`PreconfConfig::is_preconf_tx`].
//! 3. Nonce-gap pre-check against `pending_nonce = max(on_chain_nonce,
//!    pool.highest_consecutive(sender).nonce() + 1)`.
//! 4. Attach a oneshot responder to [`PreconfTxSet`] **before** calling
//!    [`TransactionPool::add_transaction`] — otherwise the listener could push the entry and the
//!    builder could apply it before the responder is registered, dropping the receipt.
//! 5. Submit to the pool. The `AlreadyImported` branch is the same-hash retry path — if the fifo
//!    entry is in `Timeout`, atomically revive it back to `Waiting` and re-notify the builder.
//! 6. Wait on the responder with [`PreconfConfig::preconf_timeout`]. On elapsed, return `Ok(Timeout
//!    event)` (op-geth-aligned) and clean both the fifo entry CAS (`mark_timeout`) **and** any
//!    stale responder (`cancel_responder`). The second call is mandatory because the pool listener
//!    filters to `SubPool::Pending` — txs routed to `BaseFee`/`Queued` never produce a fifo entry,
//!    so `mark_timeout` returns `NotFound` and the responder would otherwise be stuck in
//!    `pending_responders`.

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
    PreconfConfig, PreconfJournal, PreconfTxSet,
    journal::JournalEntry,
    types::{AttachError, PreconfError, PreconfReceipt, PreconfStatus},
};

/// Generic preconf RPC handler. Constructed by the preconf `ServiceBuilder`
/// once the pool + provider are wired up.
pub struct PreconfRpcHandler<P, Pr> {
    pool: P,
    provider: Pr,
    fifo: Arc<PreconfTxSet>,
    cfg: Arc<PreconfConfig>,
    /// Optional persistence sink — when `Some`, every successful
    /// preconf commitment is appended to the journal before the
    /// `PreconfTxEvent` is returned to the client. Append failures
    /// are logged but do not block the response (best-effort
    /// durability; a crash before the next disk flush loses at most
    /// the most recent commitment).
    journal: Option<Arc<PreconfJournal>>,
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
    /// `journal` is optional; when `None`, persistence of successful
    /// commitments is silently skipped.
    pub const fn new(
        pool: P,
        provider: Pr,
        fifo: Arc<PreconfTxSet>,
        cfg: Arc<PreconfConfig>,
        journal: Option<Arc<PreconfJournal>>,
    ) -> Self {
        Self { pool, provider, fifo, cfg, journal }
    }
}

impl<P, Pr> PreconfRpcHandler<P, Pr>
where
    P: TransactionPool + 'static,
    Pr: StateProviderFactory + 'static,
{
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

        // Step 1 — whitelist.
        if !self.cfg.is_preconf_tx(&sender, to_opt.as_ref()) {
            trace!(target: "mantle::preconf::rpc", ?sender, ?to_opt, ?hash, "non-whitelisted preconf submission");
            return Err(preconf_error_to_rpc(&PreconfError::NotPreconfEligible));
        }

        // Step 2 — nonce-gap pre-check (synchronous rejection).
        //
        // Without this check, a gap-up tx would land in the pool's
        // `Queued` sub-pool; the pool listener filters to `Pending` only,
        // so no fifo entry is ever created, the client waits the full
        // `preconf_timeout` for nothing, and — worse — once the missing
        // prior-nonce tx eventually arrives, the queued tx gets promoted
        // and silently sealed without a preconf commitment. Surfacing
        // the gap here keeps the client's view and the chain's view in
        // sync; the cost is that SDKs must explicitly handle
        // `PreconfError::NonceGap` by resending in nonce order. See the
        // doc-comment on `PreconfError::NonceGap` for the full
        // rationale.
        let on_chain_nonce = self
            .provider
            .latest()
            .map_err(|e| internal_err(&e))?
            .account_nonce(&sender)
            .map_err(|e| internal_err(&e))?
            .unwrap_or(0);
        // `get_highest_consecutive_transaction_by_sender` returns the
        // highest non-nonce-gapped pending tx given the on-chain nonce;
        // if `None`, the pool has nothing executable from this sender,
        // so the next expected nonce is the on-chain nonce itself.
        // `nonce < on_chain_nonce` (stale) is intentionally not checked
        // here — the inner pool validator catches it with a precise
        // `NonceTooLow` error during `add_transaction` (step 4),
        // surfaced to the client through the catch-all `PoolRejected`
        // branch.
        let pending_nonce = self
            .pool
            .get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
            .map(|tx| tx.nonce() + 1)
            .unwrap_or(on_chain_nonce);
        if nonce > pending_nonce {
            debug!(target: "mantle::preconf::rpc", ?sender, ?nonce, ?pending_nonce, "nonce gap rejected");
            return Err(preconf_error_to_rpc(&PreconfError::NonceGap {
                tx_nonce: nonce,
                pending_nonce,
            }));
        }

        // Step 3 — attach responder BEFORE pool.add. See module-level docs.
        let (resp_tx, resp_rx) = oneshot::channel();
        if let Err(AttachError::AlreadyAttached) =
            self.fifo.attach_responder(hash, origin_instant, resp_tx).await
        {
            return Err(preconf_error_to_rpc(&PreconfError::AlreadyInProgress));
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
                if matches!(event.status, WireStatus::Success) &&
                    let Some(journal) = self.journal.as_ref()
                {
                    let entry = JournalEntry {
                        hash,
                        tx_rlp: bytes.clone(),
                        block_height: event.block_height,
                        committed_at_ms: now_unix_ms(),
                    };
                    if let Err(e) = journal.append_promised(&entry).await {
                        warn!(
                            target: "mantle::preconf::rpc",
                            ?hash,
                            ?e,
                            "journal append failed; commitment may be lost on restart"
                        );
                    }
                }
                Ok(event)
            }

            // Builder signalled an error through the responder.
            Some(Ok(Err(err))) => Err(preconf_error_to_rpc(&err)),

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
                // final status.
                let final_status = self.fifo.find_by_hash(&hash).await.map(|e| e.status);

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
                                if matches!(event.status, WireStatus::Success) &&
                                    let Some(journal) = self.journal.as_ref()
                                {
                                    let entry = JournalEntry {
                                        hash,
                                        tx_rlp: bytes.clone(),
                                        block_height: event.block_height,
                                        committed_at_ms: now_unix_ms(),
                                    };
                                    if let Err(e) = journal.append_promised(&entry).await {
                                        warn!(
                                            target: "mantle::preconf::rpc",
                                            ?hash, ?e,
                                            "journal append failed; commitment may be lost on restart"
                                        );
                                    }
                                }
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
                        // Apply never committed. Transition to Timeout
                        // under the still-held lock (or without any
                        // lock if entry was absent — mark_timeout will
                        // return `NotFound`, which is fine).
                        let _ = self.fifo.mark_timeout(&hash).await;
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
                        // deadline gate or F1 block-gas-budget gate
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
// the impl that erases the `<P, Pr>` generics so `MantleRpcExt` can hold an
// `Option<Arc<dyn DynPreconfHandler>>`.
#[async_trait]
impl<P, Pr> DynPreconfHandler for PreconfRpcHandler<P, Pr>
where
    P: TransactionPool + 'static,
    Pr: StateProviderFactory + 'static,
{
    async fn handle(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        self.handle_inner(bytes).await
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
/// wire `Canceled` variant — server pre-apply rejections (F1 gas
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
    use alloy_primitives::{Address, B256, Bytes as PrimBytes, Log, LogData, U256};
    use mantle_reth_rpc_ext::PreconfStatus as WireStatus;

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

    // Full handler-flow tests (mock pool + state provider + builder
    // fan-in) are deferred to end-to-end coverage — building a
    // `TransactionPool` impl with the right associated `Pooled` type
    // that supports `recover_raw_transaction` plus a
    // `StateProviderFactory` mock is heavyweight scaffolding. The flow
    // itself is straight-line over public `PreconfTxSet` APIs that
    // already have unit tests covering attach / cancel / mark_timeout /
    // recover_from_timeout semantics.
    //
    // `U256` is referenced only here so the e2e harness can later
    // construct receipts with numeric fields without re-touching the
    // use list.
    #[allow(dead_code)]
    fn _u256_marker() -> U256 {
        U256::ZERO
    }
}
