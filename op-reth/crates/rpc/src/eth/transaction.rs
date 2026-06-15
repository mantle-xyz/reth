//! Loads and formats OP transaction RPC response.

use crate::{OpEthApi, OpEthApiError, SequencerClient};
use alloy_primitives::{B256, U256, Bytes};
use alloy_rpc_types_eth::TransactionInfo;
use futures::StreamExt;
use op_alloy_consensus::{
    OpTransaction,
    transaction::{OpDepositInfo, OpTransactionInfo},
};
use reth_chain_state::CanonStateSubscriptions;
use reth_optimism_primitives::DepositReceipt;
use reth_primitives_traits::{Recovered, SignedTransaction, SignerRecoverable, WithEncoded, TxTy};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, FromEvmError, RpcConvert, RpcNodeCore, RpcReceipt,
    TxInfoMapper,
    helpers::{EthApiSpec, EthTransactions, LoadBlock, LoadFee, LoadReceipt,LoadState, LoadTransaction, SpawnBlocking, estimate::EstimateCall, spec::SignersForRpc},
};
use reth_rpc_eth_types::{EthApiError, TransactionSource, block::convert_transaction_receipt, FillTransaction};
use reth_storage_api::{ProviderTx, ReceiptProvider, TransactionsProvider, errors::ProviderError, BlockReaderIdExt};
use reth_transaction_pool::{
    AddedTransactionOutcome, PoolPooledTx, PoolTransaction, TransactionOrigin, TransactionPool,
};
use reth_rpc_convert::RpcTxReq;
use alloy_eips::{BlockId, Encodable2718};
use alloy_network::{TransactionBuilder, TransactionBuilder4844};
use alloy_consensus::BlockHeader;
use std::{
    fmt::{Debug, Formatter},
    future::Future,
    time::Duration,
};
use tokio_stream::wrappers::WatchStream;

impl<N, Rpc> EthTransactions for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError>,
    <Self as EthApiTypes>::RpcConvert:
        RpcConvert<Primitives = <Self as RpcNodeCore>::Primitives>,
{
    fn signers(&self) -> &SignersForRpc<Self::Provider, Self::NetworkTypes> {
        self.inner.eth_api.signers()
    }

    fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.inner.eth_api.send_raw_transaction_sync_timeout()
    }

    async fn send_transaction(
        &self,
        origin: TransactionOrigin,
        tx: WithEncoded<Recovered<PoolPooledTx<Self::Pool>>>,
    ) -> Result<B256, Self::Error> {
        let (tx, recovered) = tx.split();

        // broadcast raw transaction to subscribers if there is any.
        self.eth_api().broadcast_raw_transaction(tx.clone());

        let pool_transaction = <Self::Pool as TransactionPool>::Transaction::from_pooled(recovered);

        // On optimism, transactions are forwarded directly to the sequencer to be included in
        // blocks that it builds.
        if let Some(client) = self.raw_tx_forwarder().as_ref() {
            tracing::debug!(target: "rpc::eth", hash = %pool_transaction.hash(), "forwarding raw transaction to sequencer");
            let hash = client.forward_raw_transaction(&tx).await.inspect_err(|err| {
                    tracing::debug!(target: "rpc::eth", %err, hash=% *pool_transaction.hash(), "failed to forward raw transaction");
                })?;

            // Retain tx in local tx pool after forwarding, for local RPC usage.
            let _ = self.inner.eth_api.add_pool_transaction(origin, pool_transaction).await.inspect_err(|err| {
                tracing::warn!(target: "rpc::eth", %err, %hash, "successfully sent tx to sequencer, but failed to persist in local tx pool");
            });

            return Ok(hash);
        }

        // submit the transaction to the pool with the given origin
        let AddedTransactionOutcome { hash, .. } = self
            .pool()
            .add_transaction(origin, pool_transaction)
            .await
            .map_err(Self::Error::from_eth_err)?;

        Ok(hash)
    }

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// And awaits the receipt, checking both canonical blocks and flashblocks for faster
    /// confirmation.
    fn send_raw_transaction_sync(
        &self,
        tx: Bytes,
    ) -> impl Future<Output = Result<RpcReceipt<Self::NetworkTypes>, Self::Error>> + Send {
        let this = self.clone();
        let timeout_duration = self.send_raw_transaction_sync_timeout();
        async move {
            let mut canonical_stream = this.provider().canonical_state_stream();
            let hash = EthTransactions::send_raw_transaction(&this, tx).await?;
            let mut flashblock_stream = this.pending_block_rx().map(WatchStream::new);

            tokio::time::timeout(timeout_duration, async {
                loop {
                    tokio::select! {
                        biased;
                        // check if the tx was preconfirmed in a new flashblock
                        flashblock = async {
                            if let Some(stream) = &mut flashblock_stream {
                                stream.next().await
                            } else {
                                futures::future::pending().await
                            }
                        } => {
                            if let Some(flashblock) = flashblock.flatten() {
                                // if flashblocks are supported, attempt to find id from the pending block
                                if let Some(receipt) = flashblock
                                .find_and_convert_transaction_receipt(hash, this.converter())
                                {
                                    return receipt;
                                }
                            }
                        }
                        // Listen for regular canonical block updates for inclusion
                        canonical_notification = canonical_stream.next() => {
                            if let Some(notification) = canonical_notification {
                                let chain = notification.committed();
                                if let Some((block, tx, receipt, all_receipts)) =
                                    chain.find_transaction_and_receipt_by_hash(hash) &&
                                    let Some(receipt) = convert_transaction_receipt(
                                        block,
                                        all_receipts,
                                        tx,
                                        receipt,
                                        this.converter(),
                                    )
                                    .transpose()?
                                {
                                    return Ok(receipt);
                                }
                            } else {
                                // Canonical stream ended
                                break;
                            }
                        }
                    }
                }
                Err(Self::Error::from_eth_err(EthApiError::TransactionConfirmationTimeout {
                    hash,
                    duration: timeout_duration,
                }))
            })
            .await
            .unwrap_or_else(|_elapsed| {
                Err(Self::Error::from_eth_err(EthApiError::TransactionConfirmationTimeout {
                    hash,
                    duration: timeout_duration,
                }))
            })
        }
    }

    /// Returns the transaction receipt for the given hash.
    ///
    /// With flashblocks, we should also lookup the pending block for the transaction
    /// because this is considered confirmed/mined.
    fn transaction_receipt(
        &self,
        hash: B256,
    ) -> impl Future<Output = Result<Option<RpcReceipt<Self::NetworkTypes>>, Self::Error>> + Send
    {
        let this = self.clone();
        async move {
            // first attempt to fetch the mined transaction receipt data
            let tx_receipt = this.load_transaction_and_receipt(hash).await?;

            if tx_receipt.is_none() {
                // if flashblocks are supported, attempt to find id from the pending block
                if let Ok(Some(pending_block)) = this.pending_flashblock().await &&
                    let Some(Ok(receipt)) = pending_block
                        .find_and_convert_transaction_receipt(hash, this.converter())
                {
                    return Ok(Some(receipt));
                }
            }
            let Some((tx, meta, receipt, all_receipts)) = tx_receipt else { return Ok(None) };
            self.build_transaction_receipt(tx, meta, receipt, all_receipts).await.map(Some)
        }
    }

    /// Fills the defaults on a given unsigned transaction.
    fn fill_transaction(
        &self,
        mut request: RpcTxReq<Self::NetworkTypes>,
    ) -> impl Future<Output = Result<FillTransaction<TxTy<Self::Primitives>>, Self::Error>> + Send
    where
        Self: EthApiSpec + LoadBlock + EstimateCall + LoadFee,
    {
        async move {
            if request.as_ref().value().is_none() {
                request.as_mut().set_value(U256::ZERO);
            }

            if request.as_ref().nonce().is_none() {
                let nonce = self.next_available_nonce_for(&request).await?;
                request.as_mut().set_nonce(nonce);
            }

            let chain_id = self.chain_id();
            request.as_mut().set_chain_id(chain_id.to());

            if request.as_ref().has_eip4844_fields() &&
                request.as_ref().max_fee_per_blob_gas().is_none()
            {
                let blob_fee = self.blob_base_fee().await?;
                request.as_mut().set_max_fee_per_blob_gas(blob_fee.to());
            }

            // Use `sidecar.is_some()` instead of `blob_sidecar().is_some()` to handle
            // both EIP-4844 (v0) and EIP-7594 (v1) sidecar formats
            if request.as_ref().sidecar.is_some() &&
                request.as_ref().blob_versioned_hashes.is_none()
            {
                request.as_mut().populate_blob_hashes();
            }

            if request.as_ref().gas_limit().is_none() {
                let estimated_gas =
                    self.estimate_gas_at(request.clone(), BlockId::pending(), None).await?;
                request.as_mut().set_gas_limit(estimated_gas.to());
            }

            if request.as_ref().gas_price().is_none() {
                let tip = if let Some(tip) = request.as_ref().max_priority_fee_per_gas() {
                    tip
                } else {
                    let tip = self.suggested_priority_fee().await?.to::<u128>();
                    request.as_mut().set_max_priority_fee_per_gas(tip);
                    tip
                };
                if request.as_ref().max_fee_per_gas().is_none() {
                    let header =
                        self.provider().latest_header().map_err(Self::Error::from_eth_err)?;
                    let base_fee = header.and_then(|h| h.base_fee_per_gas()).unwrap_or_default();
                    request.as_mut().set_max_fee_per_gas(base_fee as u128 * 2 + tip);
                }
            }

            let tx = self.converter().build_simulate_v1_transaction(request)?;

            let raw = tx.encoded_2718().into();

            Ok(FillTransaction { raw, tx })
        }
    }
}

impl<N, Rpc> LoadTransaction for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError>,
{
    async fn transaction_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<TransactionSource<ProviderTx<Self::Provider>>>, Self::Error> {
        // 1. Try to find the transaction on disk (historical blocks)
        if let Some((tx, meta)) = self
            .spawn_blocking_io(move |this| {
                this.provider()
                    .transaction_by_hash_with_meta(hash)
                    .map_err(Self::Error::from_eth_err)
            })
            .await?
        {
            let transaction = tx
                .try_into_recovered_unchecked()
                .map_err(|_| EthApiError::InvalidTransactionSignature)?;

            return Ok(Some(TransactionSource::Block {
                transaction,
                index: meta.index,
                block_hash: meta.block_hash,
                block_number: meta.block_number,
                block_timestamp: meta.timestamp,
                base_fee: meta.base_fee,
            }));
        }

        // 2. check flashblocks (sequencer preconfirmations)
        if let Ok(Some(pending_block)) = self.pending_flashblock().await &&
            let Some(indexed_tx) = pending_block.block().find_indexed(hash)
        {
            let meta = indexed_tx.meta();
            return Ok(Some(TransactionSource::Block {
                transaction: indexed_tx.recovered_tx().cloned(),
                index: meta.index,
                block_hash: meta.block_hash,
                block_number: meta.block_number,
                block_timestamp: meta.timestamp,
                base_fee: meta.base_fee,
            }));
        }

        // 3. check local pool
        if let Some(tx) = self.pool().get(&hash).map(|tx| tx.transaction.clone_into_consensus()) {
            return Ok(Some(TransactionSource::Pool(tx)));
        }

        Ok(None)
    }
}

impl<N, Rpc> OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    /// Returns the [`SequencerClient`] if one is set.
    pub fn raw_tx_forwarder(&self) -> Option<SequencerClient> {
        self.inner.sequencer_client.clone()
    }
}

/// Optimism implementation of [`TxInfoMapper`].
///
/// For deposits, receipt is fetched to extract `deposit_nonce` and `deposit_receipt_version`.
/// Otherwise, it works like regular Ethereum implementation, i.e. uses [`TransactionInfo`].
pub struct OpTxInfoMapper<Provider> {
    provider: Provider,
}

impl<Provider: Clone> Clone for OpTxInfoMapper<Provider> {
    fn clone(&self) -> Self {
        Self { provider: self.provider.clone() }
    }
}

impl<Provider> Debug for OpTxInfoMapper<Provider> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpTxInfoMapper").finish()
    }
}

impl<Provider> OpTxInfoMapper<Provider> {
    /// Creates [`OpTxInfoMapper`] that uses [`ReceiptProvider`] borrowed from given `eth_api`.
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

impl<T, Provider> TxInfoMapper<T> for OpTxInfoMapper<Provider>
where
    T: OpTransaction + SignedTransaction,
    Provider: ReceiptProvider<Receipt: DepositReceipt>,
{
    type Out = OpTransactionInfo;
    type Err = ProviderError;

    fn try_map(&self, tx: &T, tx_info: TransactionInfo) -> Result<Self::Out, ProviderError> {
        let deposit_meta = if tx.is_deposit() {
            self.provider.receipt_by_hash(*tx.tx_hash())?.and_then(|receipt| {
                receipt.as_deposit_receipt().map(|receipt| OpDepositInfo {
                    deposit_receipt_version: receipt.deposit_receipt_version,
                    deposit_nonce: receipt.deposit_nonce,
                })
            })
        } else {
            None
        }
        .unwrap_or_default();

        Ok(OpTransactionInfo::new(tx_info, deposit_meta))
    }
}
