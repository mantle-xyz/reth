//! Batched `eth_sendRawTransactions` for op-reth.
//!
//! Per-tx local processing (decode, sender recovery, broadcast, pool
//! insertion) runs independently; the forward step to the sequencer is
//! coalesced into a single outbound JSON-RPC batch. Per-item failures are
//! reported via [`SendRawTxBatchItem::error`] and do not abort the call.

use crate::{OpEthApi, OpEthApiError, SequencerClient};
use alloy_primitives::{Bytes, B256};
use futures::future::join_all;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use jsonrpsee_types::ErrorObjectOwned;
use reth_optimism_primitives::DepositReceipt;
use reth_rpc_eth_api::{
    FromEthApiError, FromEvmError, RpcConvert, RpcNodeCore, SendRawTxBatchItem,
};
use reth_rpc_eth_types::{utils::recover_raw_transaction, EthApiError};
use reth_storage_api::ReceiptProvider;
use reth_transaction_pool::{
    AddedTransactionOutcome, PoolTransaction, TransactionOrigin, TransactionPool,
};
use std::{sync::Arc, time::Instant};
use tracing::{info, warn};

const LOG_TARGET: &str = "rpc::eth::batch_send";

/// Capability surface required by [`OpEthBatchSendApi`].
///
/// Implemented for [`OpEthApi`] in this crate; abstracted as a trait so the
/// wrapper can be registered against the registry's associated `EthApi` type
/// without naming the concrete generics.
#[async_trait::async_trait]
pub trait OpBatchEthApi: Send + Sync + 'static {
    /// Pool transaction type.
    type PoolTx: PoolTransaction;

    /// Broadcasts a raw transaction to subscribers (mirrors single-tx path).
    fn broadcast_raw_transaction(&self, raw: Bytes);

    /// Returns the configured sequencer client, if any.
    fn sequencer_client(&self) -> Option<&SequencerClient>;

    /// Inserts a transaction into the local pool (after sequencer forward).
    async fn add_pool_transaction(
        &self,
        tx: Self::PoolTx,
    ) -> Result<AddedTransactionOutcome, EthApiError>;

    /// Inserts a transaction into the local pool with the given origin (used
    /// when no sequencer forwarder is configured).
    async fn add_local_transaction(
        &self,
        origin: TransactionOrigin,
        tx: Self::PoolTx,
    ) -> Result<AddedTransactionOutcome, EthApiError>;

    /// Recovers a raw RLP into the pool transaction type, or maps the decode
    /// failure into a per-item RPC error object.
    fn recover_pool_tx(&self, raw: &Bytes) -> Result<Self::PoolTx, ErrorObjectOwned>;
}

#[async_trait::async_trait]
impl<N, Rpc> OpBatchEthApi for OpEthApi<N, Rpc>
where
    N: RpcNodeCore<Provider: ReceiptProvider<Receipt: DepositReceipt>> + 'static,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError> + 'static,
{
    type PoolTx = <<N as RpcNodeCore>::Pool as TransactionPool>::Transaction;

    fn broadcast_raw_transaction(&self, raw: Bytes) {
        self.eth_api().broadcast_raw_transaction(raw);
    }

    fn sequencer_client(&self) -> Option<&SequencerClient> {
        Self::sequencer_client(self)
    }

    async fn add_pool_transaction(
        &self,
        tx: Self::PoolTx,
    ) -> Result<AddedTransactionOutcome, EthApiError> {
        self.eth_api().add_pool_transaction(tx).await
    }

    async fn add_local_transaction(
        &self,
        origin: TransactionOrigin,
        tx: Self::PoolTx,
    ) -> Result<AddedTransactionOutcome, EthApiError> {
        <Self as reth_rpc_eth_api::RpcNodeCore>::pool(self)
            .add_transaction(origin, tx)
            .await
            .map_err(|e| EthApiError::PoolError(e.into()))
    }

    fn recover_pool_tx(&self, raw: &Bytes) -> Result<Self::PoolTx, ErrorObjectOwned> {
        match recover_raw_transaction(raw) {
            Ok(recovered) => Ok(Self::PoolTx::from_pooled(recovered)),
            Err(err) => {
                let api_err = <OpEthApiError as FromEthApiError>::from_eth_err(err);
                Err(api_err.into())
            }
        }
    }
}

/// `eth_` namespace extension for op-reth batched raw-transaction submission.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait OpEthBatchApi {
    /// Submits multiple raw transactions in a single call. See
    /// [`SendRawTxBatchItem`] for the per-item response shape.
    #[method(name = "sendRawTransactions")]
    async fn send_raw_transactions(&self, txs: Vec<Bytes>) -> RpcResult<Vec<SendRawTxBatchItem>>;
}

/// JSON-RPC handler for [`OpEthBatchApi`], registered alongside the standard
/// eth namespace.
#[derive(Debug)]
pub struct OpEthBatchSendApi<E> {
    eth_api: Arc<E>,
}

impl<E> Clone for OpEthBatchSendApi<E> {
    fn clone(&self) -> Self {
        Self { eth_api: self.eth_api.clone() }
    }
}

impl<E> OpEthBatchSendApi<E> {
    /// Creates a new [`OpEthBatchSendApi`] wrapping the given handle.
    pub const fn new(eth_api: Arc<E>) -> Self {
        Self { eth_api }
    }
}

#[async_trait::async_trait]
impl<E> OpEthBatchApiServer for OpEthBatchSendApi<E>
where
    E: OpBatchEthApi,
{
    async fn send_raw_transactions(&self, txs: Vec<Bytes>) -> RpcResult<Vec<SendRawTxBatchItem>> {
        Ok(send_raw_transactions_impl(self.eth_api.as_ref(), txs).await)
    }
}

async fn send_raw_transactions_impl<E: OpBatchEthApi>(
    eth_api: &E,
    txs: Vec<Bytes>,
) -> Vec<SendRawTxBatchItem> {
    if txs.is_empty() {
        return Vec::new();
    }

    let t_total = Instant::now();
    let total_in = txs.len();

    struct Prepared<T> {
        raw: Bytes,
        pool_tx: T,
    }

    // Per-tx local prep: decode + sender recovery. Failed items skip the rest
    // of the pipeline and are reported as per-item errors.
    let t_recover = Instant::now();
    let prepared: Vec<Result<Prepared<E::PoolTx>, ErrorObjectOwned>> = txs
        .into_iter()
        .map(|tx| match eth_api.recover_pool_tx(&tx) {
            Ok(pool_tx) => {
                eth_api.broadcast_raw_transaction(tx.clone());
                Ok(Prepared { raw: tx, pool_tx })
            }
            Err(err) => Err(err),
        })
        .collect();
    let recover_us = t_recover.elapsed().as_micros() as u64;
    let recover_ok = prepared.iter().filter(|p| p.is_ok()).count();

    if let Some(client) = eth_api.sequencer_client() {
        let (forward_indices, forward_raws): (Vec<usize>, Vec<Bytes>) = prepared
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().ok().map(|prep| (i, prep.raw.clone())))
            .unzip();

        // Normalize to a single shape so per-item assembly is uniform whether
        // the batch envelope failed or only some entries did.
        let t_forward = Instant::now();
        let forward_results: Vec<Result<B256, ErrorObjectOwned>> = if forward_raws.is_empty() {
            Vec::new()
        } else {
            match client.forward_raw_transactions(&forward_raws).await {
                Ok(results) => results.into_iter().map(|r| r.map_err(Into::into)).collect(),
                Err(envelope_err) => {
                    warn!(
                        target: "rpc::eth",
                        %envelope_err,
                        "Batch forward envelope to sequencer failed; reporting per-item error",
                    );
                    // SequencerClientError isn't Clone; fan out a single
                    // envelope-level message to every forwarded entry.
                    let msg = envelope_err.to_string();
                    forward_raws
                        .iter()
                        .map(|_| {
                            Err(ErrorObjectOwned::owned::<()>(
                                jsonrpsee_types::error::INTERNAL_ERROR_CODE,
                                msg.clone(),
                                None,
                            ))
                        })
                        .collect()
                }
            }
        };

        let forward_us = t_forward.elapsed().as_micros() as u64;
        let forward_n = forward_indices.len();

        let t_pool = Instant::now();
        let mut forward_iter = forward_indices.into_iter().zip(forward_results);
        let mut next_forwarded = forward_iter.next();
        let mut out: Vec<SendRawTxBatchItem> = Vec::with_capacity(prepared.len());
        let mut forward_ok = 0usize;
        let mut pool_failed = 0usize;

        for (i, prep) in prepared.into_iter().enumerate() {
            match prep {
                Err(err) => out.push(SendRawTxBatchItem::err(err)),
                Ok(prep) => {
                    debug_assert_eq!(next_forwarded.as_ref().map(|(j, _)| *j), Some(i));
                    let (_, fwd_res) = next_forwarded.take().expect("forward result aligned");
                    next_forwarded = forward_iter.next();
                    match fwd_res {
                        Ok(hash) => {
                            forward_ok += 1;
                            if let Err(pool_err) = eth_api.add_pool_transaction(prep.pool_tx).await
                            {
                                pool_failed += 1;
                                warn!(
                                    target: "rpc::eth",
                                    %pool_err,
                                    %hash,
                                    "sent tx to sequencer but failed to persist locally",
                                );
                            }
                            out.push(SendRawTxBatchItem::ok(hash));
                        }
                        Err(err) => out.push(SendRawTxBatchItem::err(err)),
                    }
                }
            }
        }
        let pool_us = t_pool.elapsed().as_micros() as u64;
        info!(
            target: LOG_TARGET,
            total_in,
            recover_ok,
            forward_n,
            forward_ok,
            pool_failed,
            recover_us,
            forward_us,
            pool_us,
            total_us = t_total.elapsed().as_micros() as u64,
            "sendRawTransactions handler (forwarder path) finished"
        );
        return out;
    }

    // No sequencer forwarder: per-tx local pool insertion only.
    let t_local = Instant::now();
    let adds = prepared.into_iter().map(|prep| async move {
        match prep {
            Err(err) => SendRawTxBatchItem::err(err),
            Ok(prep) => {
                match eth_api.add_local_transaction(TransactionOrigin::Local, prep.pool_tx).await {
                    Ok(AddedTransactionOutcome { hash, .. }) => SendRawTxBatchItem::ok(hash),
                    Err(err) => {
                        let api_err = <OpEthApiError as FromEthApiError>::from_eth_err(err);
                        SendRawTxBatchItem::err(api_err.into())
                    }
                }
            }
        }
    });
    let out = join_all(adds).await;
    info!(
        target: LOG_TARGET,
        total_in,
        recover_ok,
        recover_us,
        local_pool_us = t_local.elapsed().as_micros() as u64,
        total_us = t_total.elapsed().as_micros() as u64,
        "sendRawTransactions handler (no-forwarder path) finished"
    );
    out
}
