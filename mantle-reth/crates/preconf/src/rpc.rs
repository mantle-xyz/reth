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
//!    [`TransactionPool::add_transaction`] — otherwise the listener could
//!    push the entry and the builder could apply it before the responder
//!    is registered, dropping the receipt.
//! 5. Submit to the pool. The `AlreadyImported` branch is the same-hash
//!    retry path — if the fifo entry is in `Timeout`, atomically revive it
//!    back to `Waiting` and re-notify the builder.
//! 6. Wait on the responder with [`PreconfConfig::preconf_timeout`].
//!    On elapsed, return `Ok(Timeout event)` (op-geth-aligned) and clean
//!    both the fifo entry CAS (`mark_timeout`) **and** any stale responder
//!    (`cancel_responder`). The second call is mandatory because the
//!    pool listener filters to `SubPool::Pending` — txs routed to
//!    `BaseFee`/`Queued` never produce a fifo entry, so `mark_timeout`
//!    returns `NotFound` and the responder would otherwise be stuck in
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
use tokio::{sync::oneshot, time::timeout};
use tracing::{debug, trace, warn};

use crate::{
    PreconfConfig, PreconfJournal, PreconfTxSet,
    journal::JournalEntry,
    types::{AttachError, PreconfError, PreconfReceipt, RecoverError},
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
    /// `journal` is optional; when `None`, the handler runs without
    /// persistence and successful commitments are lost on crash.
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
        if let Err(AttachError::AlreadyAttached) = self.fifo.attach_responder(hash, resp_tx).await {
            return Err(preconf_error_to_rpc(&PreconfError::AlreadyInProgress));
        }

        // Step 4 — submit to pool.
        match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
            Ok(_) => { /* normal — fall through to await responder */ }

            // Same-hash retry. If the fifo entry is `Timeout`, atomically
            // revive it (Timeout → Waiting + broadcast). The responder
            // attached at step 3 is reused.
            Err(e) if matches!(e.kind, PoolErrorKind::AlreadyImported) => {
                match self.fifo.recover_from_timeout(&hash).await {
                    Ok(()) => {
                        debug!(target: "mantle::preconf::rpc", ?hash, "recovered Timeout entry on AlreadyImported retry");
                    }
                    Err(RecoverError::NotTimeout(status)) => {
                        // Active commitment for this hash — refuse silently
                        // overlapping requests.
                        warn!(target: "mantle::preconf::rpc", ?hash, ?status, "AlreadyImported but entry is not Timeout");
                        self.fifo.cancel_responder(&hash, PreconfError::AlreadyInProgress).await;
                        return Err(preconf_error_to_rpc(&PreconfError::AlreadyInProgress));
                    }
                    Err(RecoverError::NotFound) => {
                        // Entry vanished between the pool add and our
                        // recover call (clean_timeout race). Treat as
                        // transient and ask the client to retry.
                        let err = PreconfError::Internal(
                            "transient: fifo entry cleaned between pool add and recover"
                                .to_string(),
                        );
                        self.fifo.cancel_responder(&hash, err.clone()).await;
                        return Err(preconf_error_to_rpc(&err));
                    }
                }
            }

            Err(e) => {
                let err = PreconfError::PoolRejected(format!("{}", e.kind));
                self.fifo.cancel_responder(&hash, err.clone()).await;
                return Err(preconf_error_to_rpc(&err));
            }
        }

        // Step 5 — await receipt with timeout.
        let preconf_timeout = self.cfg.preconf_timeout;
        match timeout(preconf_timeout, resp_rx).await {
            // Receipt arrived. On `Success`, persist the commitment so
            // it survives a crash. Failures of the append are logged
            // and do not block the client response — best-effort
            // durability; a power loss before fsync loses at most this
            // single record.
            Ok(Ok(Ok(receipt))) => {
                let event = PreconfTxEvent::from(receipt);
                if matches!(event.status, WireStatus::Success)
                    && let Some(journal) = self.journal.as_ref()
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
            Ok(Ok(Err(err))) => Err(preconf_error_to_rpc(&err)),

            // Builder dropped the responder without sending — should not
            // happen on healthy paths. Clean fifo state defensively.
            Ok(Err(_recv_err)) => {
                warn!(target: "mantle::preconf::rpc", ?hash, "responder dropped before send");
                let _ = self.fifo.mark_timeout(&hash).await;
                let err = PreconfError::Internal("responder dropped before send".to_string());
                // No-op if the responder was already taken; idempotent.
                self.fifo.cancel_responder(&hash, err.clone()).await;
                Err(preconf_error_to_rpc(&err))
            }

            // Timed out waiting for the builder. Op-geth returns
            // `Ok(Timeout event)`; we match that contract so SDKs that key
            // off the wire status keep working unchanged.
            Err(_elapsed) => {
                debug!(target: "mantle::preconf::rpc", ?hash, ?preconf_timeout, "preconf timeout");
                // Best-effort: try to flip Waiting → Timeout. Returns
                // `NotFound` when the pool listener never created an entry
                // (e.g. tx routed to BaseFee/Queued), and
                // `IllegalTransition` when the builder finished commit in
                // the same instant — both are safe to ignore here.
                let _ = self.fifo.mark_timeout(&hash).await;
                // Mandatory: clear any responder still parked in
                // `pending_responders` for the no-fifo-entry case above.
                // Without this, a same-hash retry from the client would
                // hit `AttachError::AlreadyAttached` forever.
                self.fifo
                    .cancel_responder(
                        &hash,
                        PreconfError::Timeout { timeout_ms: preconf_timeout.as_millis() as u64 },
                    )
                    .await;
                Ok(PreconfTxEvent {
                    tx_hash: hash,
                    status: WireStatus::Timeout,
                    reason: format!("preconf timeout after {preconf_timeout:?}"),
                    block_height: 0,
                    receipt: PreconfTxReceipt { logs: vec![] },
                })
            }
        }
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
/// `PreconfStatus::Success`/`Failed` is derived from the boolean success
/// bit on the receipt; `Waiting` and `Timeout` are never observed here
/// (only the builder's `mark_succeeded` / `mark_failed` reach this
/// conversion path via the responder).
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
            receipt: PreconfTxReceipt { logs },
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
        assert_eq!(event.receipt.logs.len(), 1);
    }

    #[test]
    fn from_receipt_failed_maps_to_failed_status_with_reason() {
        let event: PreconfTxEvent = sample_receipt(false).into();
        assert_eq!(event.status, WireStatus::Failed);
        assert_eq!(event.reason, "execution reverted");
        assert_eq!(event.receipt.logs.len(), 1);
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
        assert_eq!(event.receipt.logs.len(), 2);
        assert_eq!(event.receipt.logs[0].address, Address::from([7; 20]));
        assert_eq!(event.receipt.logs[0].topics, vec![B256::from([8; 32])]);
        assert_eq!(event.receipt.logs[0].data, PrimBytes::from(vec![9, 9, 9, 9]));
        assert_eq!(event.receipt.logs[1].address, Address::from([10; 20]));
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
        assert_eq!(event.receipt.logs.len(), 0);
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
