//! `eth_` method overrides that serve the `pending` tag from flashblock state.
//!
//! `eth_simulateV1` is deliberately absent: it is owned by `MantleEthApiExt`,
//! which injects pending overrides via [`PendingStateOverrides`].

use std::{sync::Arc, time::Duration};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{
    Address, Bytes, TxHash, U256,
    map::foldhash::{HashSet, HashSetExt},
};
use alloy_rpc_types_eth::{
    BlockOverrides, Filter, Log,
    state::{EvmOverrides, StateOverride, StateOverridesBuilder},
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use op_alloy_network::Optimism;
use reth_provider::CanonStateSubscriptions;
use reth_rpc::eth::EthFilter;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, FromEthApiError, RpcReceipt,
    helpers::{EthBlocks, EthCall, EthState, EthTransactions, FullEthApi, LoadPendingBlock},
};
use reth_rpc_eth_types::EthApiError;
use tokio::{sync::broadcast::error::RecvError, time};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{debug, trace, warn};

use crate::{FlashblocksAPI, PendingBlocksAPI, metrics::Metrics};

/// Max configured timeout for `eth_sendRawTransactionSync` in milliseconds.
const MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS: u64 = 6_000;

/// Supplies the pending overlay's state overrides to RPC methods owned elsewhere.
///
/// Implemented for the flashblocks state so `MantleEthApiExt::simulate_v1` can
/// prepend pending state without depending on this crate's RPC layer.
pub trait PendingStateOverrides: std::fmt::Debug + Send + Sync {
    /// Canonical block the pending overlay is built on, or `pending` when absent.
    fn pending_base_block(&self) -> BlockId;

    /// State overrides representing the pending overlay, if any.
    fn pending_state_overrides(&self) -> Option<StateOverride>;
}

impl<FB: FlashblocksAPI + std::fmt::Debug + Send + Sync> PendingStateOverrides for FB {
    fn pending_base_block(&self) -> BlockId {
        self.get_pending_blocks().get_canonical_block_number().into()
    }

    fn pending_state_overrides(&self) -> Option<StateOverride> {
        self.get_pending_blocks().get_state_overrides()
    }
}

/// A [`BlockNumberOrTag`] wrapper that also accepts `"unsafe"` as an alias for `"latest"`.
///
/// op-conductor calls `eth_getBlockByNumber("unsafe")` to read the execution-layer
/// unsafe head; this EL surfaces that state as `"latest"`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
pub struct BlockNumberOrTagExt(BlockNumberOrTag);

impl BlockNumberOrTagExt {
    const fn is_pending(&self) -> bool {
        self.0.is_pending()
    }
}

impl From<BlockNumberOrTagExt> for BlockId {
    fn from(tag: BlockNumberOrTagExt) -> Self {
        tag.0.into()
    }
}

impl<'de> serde::Deserialize<'de> for BlockNumberOrTagExt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = BlockNumberOrTagExt;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a block number or tag")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "unsafe" {
                    return Ok(BlockNumberOrTagExt(BlockNumberOrTag::Latest));
                }
                v.parse::<BlockNumberOrTag>().map(BlockNumberOrTagExt).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Eth API override trait for flashblocks integration.
///
/// Payloads and results cross the boundary as [`serde_json::Value`] so the trait
/// carries no network-specific generics, matching `MantleEthApiExt`.
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    /// Returns block by number, with flashblock support for pending blocks.
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(
        &self,
        number: BlockNumberOrTagExt,
        full: bool,
    ) -> RpcResult<Option<serde_json::Value>>;

    /// Returns transaction receipt, checking the canonical chain first.
    #[method(name = "getTransactionReceipt")]
    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<serde_json::Value>>;

    /// Returns account balance, with flashblock support for pending state.
    #[method(name = "getBalance")]
    async fn get_balance(&self, address: Address, block_number: Option<BlockId>)
    -> RpcResult<U256>;

    /// Returns transaction count for an address.
    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256>;

    /// Returns transaction by hash, checking the canonical chain first.
    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>>;

    /// Sends a raw transaction and waits for inclusion in a flashblock.
    #[method(name = "sendRawTransactionSync")]
    async fn send_raw_transaction_sync(
        &self,
        transaction: Bytes,
        timeout_ms: Option<u64>,
    ) -> RpcResult<serde_json::Value>;

    /// Executes a call with flashblock state support.
    #[method(name = "call")]
    async fn call(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes>;

    /// Estimates gas with flashblock state support.
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
    ) -> RpcResult<U256>;

    /// Returns logs matching the filter, including pending flashblock logs.
    #[method(name = "getLogs")]
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;

    /// Returns the number of transactions in a block by block number.
    #[method(name = "getBlockTransactionCountByNumber")]
    async fn get_block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>>;
}

/// Extended Eth API with flashblocks support.
#[derive(Debug)]
pub struct EthApiExt<Eth: EthApiTypes, FB> {
    eth_api: Eth,
    eth_filter: EthFilter<Eth>,
    flashblocks_state: Arc<FB>,
}

impl<Eth: EthApiTypes, FB> EthApiExt<Eth, FB> {
    /// Creates a new extended Eth API instance with flashblocks support.
    pub const fn new(eth_api: Eth, eth_filter: EthFilter<Eth>, flashblocks_state: Arc<FB>) -> Self {
        Self { eth_api, eth_filter, flashblocks_state }
    }
}

fn to_value<T: serde::Serialize>(value: T) -> RpcResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| {
        ErrorObjectOwned::owned(
            jsonrpsee_types::error::INTERNAL_ERROR_CODE,
            format!("failed to serialise response: {e}"),
            None::<()>,
        )
    })
}

fn from_value<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> RpcResult<T> {
    serde_json::from_value(value).map_err(|e| {
        ErrorObjectOwned::owned(INVALID_PARAMS_CODE, format!("invalid request: {e}"), None::<()>)
    })
}

#[async_trait]
impl<Eth, FB> EthApiOverrideServer for EthApiExt<Eth, FB>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + LoadPendingBlock + Clone + Send + Sync + 'static,
    Eth::Error: FromEthApiError,
    <Eth as reth_rpc_eth_api::RpcNodeCore>::Provider:
        reth_chainspec::ChainSpecProvider + reth_provider::BlockReaderIdExt,
    FB: FlashblocksAPI + Send + Sync + 'static,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTagExt,
        full: bool,
    ) -> RpcResult<Option<serde_json::Value>> {
        debug!(message = "rpc::block_by_number", block_number = ?number);

        if number.is_pending() {
            Metrics::rpc_get_block_by_number().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            if pending_blocks.as_ref().is_some() {
                return Ok(pending_blocks.get_block(full));
            }
            // No overlay: fall through so the standard implementation serves `pending`.
        }

        let block =
            EthBlocks::rpc_block(&self.eth_api, number.into(), full).await.map_err(Into::into)?;
        block.map(to_value).transpose()
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: TxHash,
    ) -> RpcResult<Option<serde_json::Value>> {
        debug!(message = "rpc::get_transaction_receipt", tx_hash = %tx_hash);

        // Canonical first: after a canonical commit the pending overlay may not be
        // cleared yet, and the canonical receipt is the authoritative one.
        if let Some(canonical_receipt) =
            EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await?
        {
            return to_value(canonical_receipt).map(Some);
        }

        let pending_blocks = self.flashblocks_state.get_pending_blocks();
        if let Some(fb_receipt) = pending_blocks.get_transaction_receipt(tx_hash) {
            Metrics::rpc_get_transaction_receipt().increment(1);
            return Ok(Some(fb_receipt));
        }

        Ok(None)
    }

    async fn get_balance(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!(message = "rpc::get_balance", address = %address);
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            Metrics::rpc_get_balance().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            if let Some(balance) = pending_blocks.get_balance(address) {
                return Ok(balance);
            }
        }

        EthState::balance(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        debug!(message = "rpc::get_transaction_count", address = %address);

        let block_id = block_number.unwrap_or_default();

        if block_id.is_pending() {
            Metrics::rpc_get_transaction_count().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            let canon_block = pending_blocks.get_canonical_block_number();
            let fb_count = pending_blocks.get_transaction_count(address);

            let canon_count =
                EthState::transaction_count(&self.eth_api, address, Some(canon_block.into()))
                    .await
                    .map_err(Into::into)?;

            return Ok(canon_count + fb_count);
        }

        EthState::transaction_count(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn transaction_by_hash(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>> {
        debug!(message = "rpc::transaction_by_hash", tx_hash = %tx_hash);

        if let Some(canonical_tx) = EthTransactions::transaction_by_hash(&self.eth_api, tx_hash)
            .await?
            .map(|tx| tx.into_transaction(self.eth_api.converter()))
            .transpose()
            .map_err(Eth::Error::from)?
        {
            return to_value(canonical_tx).map(Some);
        }

        let pending_blocks = self.flashblocks_state.get_pending_blocks();
        if let Some(fb_transaction) = pending_blocks.get_transaction_by_hash(tx_hash) {
            Metrics::rpc_get_transaction_by_hash().increment(1);
            return Ok(Some(fb_transaction));
        }

        Ok(None)
    }

    async fn send_raw_transaction_sync(
        &self,
        transaction: Bytes,
        timeout_ms: Option<u64>,
    ) -> RpcResult<serde_json::Value> {
        debug!(message = "rpc::send_raw_transaction_sync");

        let timeout_ms = match timeout_ms {
            Some(ms) if ms > MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS => {
                return Err(ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!(
                        "time out too long, timeout: {ms} ms, max: {MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS} ms"
                    ),
                    None::<()>,
                ));
            }
            Some(ms) => ms,
            _ => MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS,
        };

        let tx_hash = EthTransactions::send_raw_transaction(&self.eth_api, transaction).await?;

        debug!(
            message = "rpc::send_raw_transaction_sync::sent_transaction",
            tx_hash = %tx_hash,
            timeout_ms = timeout_ms,
        );

        let timeout = Duration::from_millis(timeout_ms);
        let receipt = tokio::select! {
            receipt = self.wait_for_flashblocks_receipt(tx_hash) => receipt,
            receipt = self.wait_for_canonical_receipt(tx_hash) => receipt,
            _ = time::sleep(timeout) => None,
        };

        match receipt {
            Some(receipt) => to_value(receipt),
            None => Err(EthApiError::TransactionConfirmationTimeout {
                hash: tx_hash,
                duration: timeout,
            }
            .into()),
        }
    }

    async fn call(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        debug!(message = "rpc::call", block_number = ?block_number);

        let transaction = from_value(transaction)?;
        let mut block_id = block_number.unwrap_or_default();
        let mut pending_overrides = EvmOverrides::default();
        if block_id.is_pending() {
            Metrics::rpc_call().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            block_id = pending_blocks.get_canonical_block_number().into();
            pending_overrides.state = pending_blocks.get_state_overrides();
        }

        let mut state_overrides_builder =
            StateOverridesBuilder::new(pending_overrides.state.unwrap_or_default());
        state_overrides_builder =
            state_overrides_builder.extend(state_overrides.unwrap_or_default());
        let final_overrides = state_overrides_builder.build();

        EthCall::call(
            &self.eth_api,
            transaction,
            Some(block_id),
            EvmOverrides::new(Some(final_overrides), block_overrides),
        )
        .await
        .map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
    ) -> RpcResult<U256> {
        debug!(message = "rpc::estimate_gas", block_number = ?block_number);

        let transaction = from_value(transaction)?;
        let mut block_id = block_number.unwrap_or_default();
        let mut pending_overrides = EvmOverrides::default();
        if block_id.is_pending() {
            Metrics::rpc_estimate_gas().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            block_id = pending_blocks.get_canonical_block_number().into();
            pending_overrides.state = pending_blocks.get_state_overrides();
        }

        let mut state_overrides_builder =
            StateOverridesBuilder::new(pending_overrides.state.unwrap_or_default());
        state_overrides_builder = state_overrides_builder.extend(overrides.unwrap_or_default());
        let final_overrides = state_overrides_builder.build();

        EthCall::estimate_gas_at(&self.eth_api, transaction, block_id, Some(final_overrides))
            .await
            .map_err(Into::into)
    }

    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        debug!(message = "rpc::get_logs", address = ?filter.address);

        let (from_block, to_block) = match &filter.block_option {
            alloy_rpc_types_eth::FilterBlockOption::Range { from_block, to_block } => {
                (*from_block, *to_block)
            }
            _ => return self.eth_filter.logs(filter).await,
        };

        if !matches!(to_block, Some(BlockNumberOrTag::Pending)) {
            return self.eth_filter.logs(filter).await;
        }

        Metrics::rpc_get_logs().increment(1);
        let mut all_logs = Vec::new();

        let pending_blocks = self.flashblocks_state.get_pending_blocks();

        let mut fetched_logs = HashSet::new();
        if !matches!(from_block, Some(BlockNumberOrTag::Pending)) {
            let mut historical_filter = filter.clone();
            historical_filter.block_option = alloy_rpc_types_eth::FilterBlockOption::Range {
                from_block,
                to_block: Some(BlockNumberOrTag::Latest),
            };

            let historical_logs = self.eth_filter.logs(historical_filter).await?;
            for log in &historical_logs {
                fetched_logs.insert((log.block_number, log.log_index));
            }
            all_logs.extend(historical_logs);
        }

        let pending_logs = pending_blocks.get_pending_logs(&filter);
        all_logs.extend(
            pending_logs
                .into_iter()
                .filter(|log| !fetched_logs.contains(&(log.block_number, log.log_index))),
        );

        Ok(all_logs)
    }

    async fn get_block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        debug!(message = "rpc::get_block_transaction_count_by_number", block_number = ?number);

        if number.is_pending() {
            Metrics::rpc_get_block_transaction_count_by_number().increment(1);
            let pending_blocks = self.flashblocks_state.get_pending_blocks();
            if let Some(count) = pending_blocks
                .as_ref()
                .map(|pb| pb.get_transactions_for_block(pb.latest_block_number()).count())
            {
                return Ok(Some(U256::from(count)));
            }
            // No overlay: fall through so the standard implementation serves `pending`.
        }

        EthBlocks::block_transaction_count(&self.eth_api, number.into())
            .await
            .map(|opt| opt.map(U256::from))
            .map_err(Into::into)
    }
}

impl<Eth, FB> EthApiExt<Eth, FB>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + Send + Sync + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    async fn wait_for_flashblocks_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let mut receiver = self.flashblocks_state.subscribe_to_flashblocks();

        loop {
            match receiver.recv().await {
                Ok(pending_state) if pending_state.get_receipt(tx_hash).is_some() => {
                    debug!(message = "found receipt in flashblock", tx_hash = %tx_hash);
                    return pending_state.get_receipt(tx_hash).cloned();
                }
                Ok(_) => {
                    trace!(message = "flashblock does not contain receipt", tx_hash = %tx_hash);
                }
                Err(RecvError::Closed) => {
                    debug!(message = "flashblocks receipt queue closed");
                    return None;
                }
                Err(RecvError::Lagged(_)) => {
                    warn!("flashblocks receipt queue lagged, maybe missing receipts");
                }
            }
        }
    }

    async fn wait_for_canonical_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let mut stream =
            BroadcastStream::new(self.eth_api.provider().subscribe_to_canonical_state());

        while let Some(Ok(canon_state)) = stream.next().await {
            for (block_receipt, _) in canon_state.block_receipts() {
                for (canonical_tx_hash, _) in &block_receipt.tx_receipts {
                    if *canonical_tx_hash == tx_hash {
                        debug!(message = "found receipt in canonical state", tx_hash = %tx_hash);
                        return EthTransactions::transaction_receipt(&self.eth_api, tx_hash)
                            .await
                            .ok()
                            .flatten();
                    }
                }
            }
        }
        None
    }
}
