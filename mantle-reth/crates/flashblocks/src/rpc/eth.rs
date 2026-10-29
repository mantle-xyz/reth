//! `eth_` method overrides that serve the `pending` tag from flashblock state.
//!
//! `eth_simulateV1` is deliberately absent: it is owned by `MantleEthApiExt`,
//! which injects pending overrides via [`PendingStateOverrides`], and which
//! gets them only while the preview leads the canonical tip.
//!
//! # Freshness
//!
//! `pending` reads use the preview only while its latest block is above the
//! canonical tip; otherwise they take the standard implementation, which
//! answers from `latest`. Equal counts as stale, and a failed tip read does too.
//! Lookups by hash accept only preview entries above the tip, and
//! `eth_getLogs` serves the tip and below from the canonical chain and
//! the range above it from the preview.
//!
//! # Three overlay representations, and what is not covered
//!
//! The overlay keeps the same execution in three separately maintained shapes on
//! [`PendingBlocks`], and each overridden method answers from exactly one:
//!
//! | Source | Methods served from it |
//! |---|---|
//! | [`BundleState`] | `eth_getCode`, `eth_getStorageAt`, `eth_getTransactionCount` |
//! | `state_overrides` | `eth_call`, `eth_estimateGas`, `eth_simulateV1` |
//! | `account_balances` | `eth_getBalance` |
//!
//! Nothing keeps the three in step, so do not assume they agree on a given
//! account or slot. The bundle is the widest of them — it also carries the
//! EIP-4788 and EIP-2935 pre-execution writes and the writes of a cached replay.
//!
//! Methods that read state but are **not** overridden still resolve `pending`
//! through reth's own pending block, which this consumer never feeds, so they
//! answer from canonical: `eth_getStorageValues`, `eth_getProof`,
//! `eth_getAccount`, `eth_getAccountInfo`. `eth_getStorageValues` in
//! particular can therefore disagree with `eth_getStorageAt` on the same slot.
//!
//! [`BundleState`]: reth_revm::db::BundleState
//! [`PendingBlocks`]: crate::PendingBlocks

use std::{sync::Arc, time::Duration};

use alloy_consensus::Header;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, BlockNumber, Bytes, TxHash, U256};
use alloy_rpc_types_eth::{
    BlockOverrides, Filter, FilterBlockOption, Log,
    state::{EvmOverrides, StateOverride},
};
use alloy_serde::JsonStorageKey;
use arc_swap::Guard;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use op_alloy_network::Optimism;
use reth_chain_state::CanonStateNotification;
use reth_chainspec::ChainSpecProvider;
use reth_execution_types::BlockReceipts;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::NodePrimitives;
use reth_provider::{BlockIdReader, BlockNumReader, BlockReaderIdExt, CanonStateSubscriptions};
use reth_rpc::eth::EthFilter;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, FromEthApiError, RpcReceipt,
    helpers::{EthBlocks, EthCall, EthState, EthTransactions, FullEthApi, LoadPendingBlock},
};
use reth_rpc_eth_types::EthApiError;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time,
};
use tracing::{debug, trace, warn};

use crate::{FlashblocksAPI, PendingBlocks, metrics::Metrics};

/// Max configured timeout for `eth_sendRawTransactionSync` in milliseconds.
const MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS: u64 = 6_000;

/// Supplies one coherent pending execution snapshot to RPC methods owned elsewhere,
/// only while the preview leads the canonical tip.
pub trait PendingStateOverrides: std::fmt::Debug + Send + Sync {
    /// Returns state and execution header from the same published snapshot, or
    /// `None` when there is none or it no longer leads the tip. `canonical_tip`
    /// is read after the snapshot is loaded; `None` from it counts as stale.
    fn pending_execution_context(
        &self,
        canonical_tip: &dyn Fn() -> Option<BlockNumber>,
    ) -> Option<super::PendingExecutionContext>;
}

impl<FB: FlashblocksAPI + std::fmt::Debug + Send + Sync> PendingStateOverrides for FB {
    fn pending_execution_context(
        &self,
        canonical_tip: &dyn Fn() -> Option<BlockNumber>,
    ) -> Option<super::PendingExecutionContext> {
        // Load the preview before reading the tip: a tip read first can miss a
        // block that lands before the preview is loaded, letting a stale
        // preview pass as leading.
        let pending = Guard::into_inner(self.get_pending_blocks())?;
        if !canonical_tip().is_some_and(|tip| pending.leads(tip)) {
            Metrics::pending_stale_fallback().increment(1);
            return None;
        }
        Some(super::PendingExecutionContext::from_pending(&pending))
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
                // Case-insensitive, because the delegation below is:
                // alloy's `FromStr` lowercases first, so `"LATEST"` parses
                // while `"UNSAFE"` would fall through to the hex branch and
                // fail on a missing `0x`. Two rules in one visitor.
                if v.eq_ignore_ascii_case("unsafe") {
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

    /// Returns an account's code, with flashblock support for pending state.
    #[method(name = "getCode")]
    async fn get_code(&self, address: Address, block_number: Option<BlockId>) -> RpcResult<Bytes>;

    /// Returns a storage slot's value, with flashblock support for pending state.
    #[method(name = "getStorageAt")]
    async fn storage_at(
        &self,
        address: Address,
        index: JsonStorageKey,
        block_number: Option<BlockId>,
    ) -> RpcResult<B256>;

    /// Returns transaction by hash, checking the canonical chain first.
    #[method(name = "getTransactionByHash")]
    async fn transaction_by_hash(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>>;

    /// Sends a raw transaction and waits for its receipt from either a
    /// flashblock or a canonical block, whichever answers first, until
    /// `timeout_ms` elapses. A receipt already on the chain or in the preview
    /// when the transaction is sent is returned at once.
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
    ///
    /// Takes `block_overrides` for the same reason [`Self::call`] does: the
    /// override version has to accept every parameter the method it replaces
    /// accepts, or a caller that sends one gets an answer computed without it
    /// and no indication that anything was dropped.
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
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
    Eth::Primitives: NodePrimitives<BlockHeader = Header>,
    Eth::Provider: ChainSpecProvider<ChainSpec: OpHardforks> + BlockReaderIdExt,
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
            if let Some((pending, _)) = self.fresh_pending() {
                return Ok(serde_json::to_value(pending.get_latest_block(full)).ok());
            }
            // No fresh overlay: fall through so the standard implementation serves `pending`.
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

        // Canonical first; the preview only answers for blocks ahead of the tip.
        if let Some(receipt) = self.canonical_receipt(tx_hash).await? {
            return Ok(Some(receipt));
        }

        if let Some(receipt) = self.fresh_preview_receipt(tx_hash) {
            Metrics::rpc_get_transaction_receipt().increment(1);
            return to_value(receipt).map(Some);
        }

        // The transaction may have landed between the first lookup and the tip read.
        self.canonical_receipt(tx_hash).await
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
            if let Some((pending, _)) = self.fresh_pending() &&
                let Some(balance) = pending.get_balance(address)
            {
                return Ok(balance);
            }
        }

        EthState::balance(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn get_code(&self, address: Address, block_number: Option<BlockId>) -> RpcResult<Bytes> {
        debug!(message = "rpc::get_code", address = %address);
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            Metrics::rpc_get_code().increment(1);
            // Empty bytes are an answer, not a miss; only `None` falls through.
            // See `PendingBlocks::get_code`.
            if let Some((pending, _)) = self.fresh_pending() &&
                let Some(code) = pending.get_code(address)
            {
                return Ok(code);
            }
        }

        EthState::get_code(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn storage_at(
        &self,
        address: Address,
        index: JsonStorageKey,
        block_number: Option<BlockId>,
    ) -> RpcResult<B256> {
        debug!(message = "rpc::storage_at", address = %address, index = %index.as_b256());
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            Metrics::rpc_get_storage_at().increment(1);
            if let Some((pending, _)) = self.fresh_pending() &&
                let Some(value) = pending.get_storage_at(address, index.as_b256())
            {
                return Ok(value);
            }
        }

        EthState::storage_at(&self.eth_api, address, index, block_number).await.map_err(Into::into)
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
            let base = match self.fresh_pending() {
                Some((pending, _)) => match pending.get_nonce(address) {
                    Some(nonce) => return Ok(U256::from(nonce)),
                    None => BlockId::from(pending.parent_hash()),
                },
                None => BlockId::latest(),
            };
            return EthState::transaction_count(&self.eth_api, address, Some(base))
                .await
                .map_err(Into::into);
        }

        EthState::transaction_count(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn transaction_by_hash(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>> {
        debug!(message = "rpc::transaction_by_hash", tx_hash = %tx_hash);

        if let Some(tx) = self.canonical_transaction(tx_hash).await? {
            return Ok(Some(tx));
        }

        if let Some((pending, tip)) = self.fresh_pending() &&
            let Some(tx) = pending
                .get_transaction_by_hash(tx_hash)
                .filter(|tx| tx.block_number.is_some_and(|number| number > tip))
        {
            Metrics::rpc_get_transaction_by_hash().increment(1);
            return to_value(tx).map(Some);
        }

        // The transaction may have landed between the first lookup and the tip read.
        self.canonical_transaction(tx_hash).await
    }

    async fn send_raw_transaction_sync(
        &self,
        transaction: Bytes,
        timeout_ms: Option<u64>,
    ) -> RpcResult<serde_json::Value> {
        debug!(message = "rpc::send_raw_transaction_sync");

        // `0` means "unset", as it does upstream (`.filter(|ms| *ms > 0)`).
        // Taken literally it is a `sleep(0)`, which times the caller out
        // before the transaction could possibly land — an answer that is
        // always wrong, so there is nothing to preserve in honouring it.
        let timeout_ms = match timeout_ms.filter(|ms| *ms > 0) {
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
            None => MAX_TIMEOUT_SEND_RAW_TX_SYNC_MS,
        };

        // Subscribed before sending: a receipt published from here on reaches a
        // waiter, and one published earlier is found by the lookup below.
        let previews = self.flashblocks_state.subscribe_to_flashblocks();
        let canonical = self.eth_api.provider().subscribe_to_canonical_state();

        let tx_hash = EthTransactions::send_raw_transaction(&self.eth_api, transaction).await?;

        debug!(
            message = "rpc::send_raw_transaction_sync::sent_transaction",
            tx_hash = %tx_hash,
            timeout_ms = timeout_ms,
        );

        // The transaction is already sent; a failed lookup here must not read as a failed send.
        if let Some(receipt) = self.canonical_receipt(tx_hash).await.ok().flatten() {
            return Ok(receipt);
        }
        if let Some(receipt) = self.fresh_preview_receipt(tx_hash) {
            return to_value(receipt);
        }

        let timeout = Duration::from_millis(timeout_ms);
        match first_receipt(
            self.wait_for_flashblocks_receipt(tx_hash, previews),
            self.wait_for_canonical_receipt(tx_hash, canonical),
            timeout,
        )
        .await
        {
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

        let transaction: op_alloy_rpc_types::OpTransactionRequest = from_value(transaction)?;
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            Metrics::rpc_call().increment(1);
            let context = self
                .fresh_pending()
                .map(|(pending, _)| super::PendingExecutionContext::from_pending(&pending));
            if let Some(context) = context {
                let overrides = context.overrides(state_overrides, block_overrides);
                let at = context.base_block;
                let api = super::PendingEthApi::new(self.eth_api.clone(), context);
                return EthCall::call(&api, transaction, Some(at), overrides)
                    .await
                    .map_err(Into::into);
            }
        }
        EthCall::call(
            &self.eth_api,
            transaction,
            block_number,
            EvmOverrides::new(state_overrides, block_overrides),
        )
        .await
        .map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        transaction: serde_json::Value,
        block_number: Option<BlockId>,
        overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<U256> {
        debug!(message = "rpc::estimate_gas", block_number = ?block_number);
        let transaction: op_alloy_rpc_types::OpTransactionRequest = from_value(transaction)?;
        let block_id = block_number.unwrap_or_default();
        if block_id.is_pending() {
            Metrics::rpc_estimate_gas().increment(1);
            let context = self
                .fresh_pending()
                .map(|(pending, _)| super::PendingExecutionContext::from_pending(&pending));
            if let Some(context) = context {
                let overrides = context.overrides(overrides, block_overrides);
                let at = context.base_block;
                let api = super::PendingEthApi::new(self.eth_api.clone(), context);
                let estimate =
                    EthCall::estimate_gas_at(&api, transaction.clone(), at, overrides.clone())
                        .await?;
                api.check_estimate_funds(transaction.as_ref().clone(), estimate, overrides.state)
                    .await?;
                return Ok(estimate);
            }
        }
        EthCall::estimate_gas_at(
            &self.eth_api,
            transaction,
            block_id,
            EvmOverrides::new(overrides, block_overrides),
        )
        .await
        .map_err(Into::into)
    }

    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        debug!(message = "rpc::get_logs", address = ?filter.address);

        let (from_block, to_block) = match &filter.block_option {
            FilterBlockOption::Range { from_block, to_block } => (*from_block, *to_block),
            _ => return self.eth_filter.logs(filter).await,
        };

        if !matches!(to_block, Some(BlockNumberOrTag::Pending)) {
            return self.eth_filter.logs(filter).await;
        }

        Metrics::rpc_get_logs().increment(1);
        let Some((pending, tip)) = self.fresh_pending() else {
            // No preview, or one the chain has caught up with: the standard
            // implementation resolves `pending` on its own.
            return self.eth_filter.logs(filter).await;
        };

        let end = pending.latest_block_number();
        let start = match from_block.unwrap_or(BlockNumberOrTag::Latest) {
            BlockNumberOrTag::Pending => end,
            BlockNumberOrTag::Latest => tip,
            tag => self
                .eth_api
                .provider()
                .convert_block_number(tag)
                .map_err(EthApiError::from)?
                .ok_or(EthApiError::HeaderNotFound(tag.into()))?,
        };

        // Blocks at or below the tip come from the canonical chain only, blocks
        // above it from the preview only. The canonical range must end at `tip`:
        // a later end makes reth reject the query with `BlockRangeExceedsHead`.
        let mut logs = Vec::new();
        if start <= tip {
            let mut canonical = filter.clone();
            canonical.block_option = FilterBlockOption::Range {
                from_block: Some(start.into()),
                to_block: Some(tip.into()),
            };
            logs = self.eth_filter.logs(canonical).await?;
        }
        let after = tip.max(start.saturating_sub(1));
        logs.extend(
            pending
                .get_pending_logs(&filter)
                .into_iter()
                .filter(|log| log.block_number.is_some_and(|n| n > after && n <= end)),
        );
        Ok(logs)
    }

    async fn get_block_transaction_count_by_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<U256>> {
        debug!(message = "rpc::get_block_transaction_count_by_number", block_number = ?number);

        if number.is_pending() {
            Metrics::rpc_get_block_transaction_count_by_number().increment(1);
            if let Some((pending, _)) = self.fresh_pending() {
                let count =
                    pending.get_transactions_for_block(pending.latest_block_number()).count();
                return Ok(Some(U256::from(count)));
            }
            // No fresh overlay: fall through so the standard implementation serves `pending`.
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
    Eth::Provider: BlockNumReader,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    /// The published preview and the tip it was checked against, while it still
    /// leads the chain. A failed tip read counts as stale.
    fn fresh_pending(&self) -> Option<(Arc<PendingBlocks>, BlockNumber)> {
        // Load the preview before reading the tip: a tip read first can miss a
        // block that lands before the preview is loaded, letting a stale
        // preview pass as leading.
        let pending = Guard::into_inner(self.flashblocks_state.get_pending_blocks())?;
        let tip = self.eth_api.provider().best_block_number().ok();
        match tip {
            Some(tip) if pending.leads(tip) => Some((pending, tip)),
            _ => {
                Metrics::pending_stale_fallback().increment(1);
                None
            }
        }
    }

    /// The preview's receipt for `tx_hash`, if the preview leads the tip and
    /// holds the transaction in a block above it.
    fn fresh_preview_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        let (pending, tip) = self.fresh_pending()?;
        pending
            .get_receipt(tx_hash)
            .filter(|receipt| receipt.inner.block_number.is_some_and(|number| number > tip))
            .cloned()
    }

    async fn canonical_receipt(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>> {
        match EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await? {
            Some(receipt) => to_value(receipt).map(Some),
            None => Ok(None),
        }
    }

    async fn canonical_transaction(&self, tx_hash: TxHash) -> RpcResult<Option<serde_json::Value>> {
        match EthTransactions::transaction_by_hash(&self.eth_api, tx_hash)
            .await?
            .map(|tx| tx.into_transaction(self.eth_api.converter()))
            .transpose()
            .map_err(Eth::Error::from)?
        {
            Some(tx) => to_value(tx).map(Some),
            None => Ok(None),
        }
    }

    async fn wait_for_flashblocks_receipt(
        &self,
        tx_hash: TxHash,
        mut receiver: broadcast::Receiver<Arc<PendingBlocks>>,
    ) -> Option<RpcReceipt<Optimism>> {
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

    async fn wait_for_canonical_receipt<N: NodePrimitives>(
        &self,
        tx_hash: TxHash,
        mut receiver: broadcast::Receiver<CanonStateNotification<N>>,
    ) -> Option<RpcReceipt<Optimism>> {
        crate::await_canonical_match("send_raw_transaction_sync", &mut receiver, |canon_state| {
            names_a_canonical_receipt(&canon_state.block_receipts(), tx_hash)
        })
        .await?;

        debug!(message = "found receipt in canonical state", tx_hash = %tx_hash);
        EthTransactions::transaction_receipt(&self.eth_api, tx_hash).await.ok().flatten()
    }
}

/// Whether `receipts` names `tx_hash` in a block that is still canonical.
///
/// `CanonStateNotification::block_receipts` returns the reverted side of a
/// reorg alongside the committed one, flagged by the `bool`.
fn names_a_canonical_receipt<R>(receipts: &[(BlockReceipts<R>, bool)], tx_hash: TxHash) -> bool {
    receipts.iter().any(|(block_receipt, reverted)| {
        !reverted && block_receipt.tx_receipts.iter().any(|(canonical, _)| *canonical == tx_hash)
    })
}

/// The first receipt either waiter yields within `timeout`.
///
/// A waiter that gives up (`None`: its channel closed, or the lookup after a
/// match found nothing) drops out rather than ending the wait.
async fn first_receipt<R>(
    preview: impl Future<Output = Option<R>>,
    canonical: impl Future<Output = Option<R>>,
    timeout: Duration,
) -> Option<R> {
    tokio::select! {
        Some(receipt) = preview => Some(receipt),
        Some(receipt) = canonical => Some(receipt),
        () = time::sleep(timeout) => None,
    }
}

#[cfg(test)]
mod canonical_receipt_tests {
    use alloy_eips::BlockNumHash;

    use super::*;

    fn receipts_naming(tx_hash: TxHash, reverted: bool) -> (BlockReceipts<()>, bool) {
        (
            BlockReceipts {
                block: BlockNumHash::new(1, B256::ZERO),
                tx_receipts: vec![(tx_hash, ())],
                timestamp: 0,
            },
            reverted,
        )
    }

    /// A reorg carries the receipts it took back. Reading one as a
    /// confirmation ends `eth_sendRawTransactionSync`'s wait on a transaction
    /// that is no longer on the chain — and the authoritative lookup that
    /// follows then finds nothing, so the caller is told its transaction timed
    /// out while the next block may be about to include it.
    #[test]
    fn a_reverted_receipt_is_not_a_confirmation() {
        let tx_hash = B256::from([7; 32]);

        assert!(
            !names_a_canonical_receipt(&[receipts_naming(tx_hash, true)], tx_hash),
            "a receipt the reorg took back confirms nothing",
        );

        // The premise: the same receipt on the committed side does confirm, so
        // the assertion above is about the flag and not about the lookup.
        assert!(names_a_canonical_receipt(&[receipts_naming(tx_hash, false)], tx_hash));
    }

    #[test]
    fn an_unrelated_receipt_is_not_a_confirmation() {
        let tx_hash = B256::from([7; 32]);
        let other = B256::from([8; 32]);

        assert!(!names_a_canonical_receipt(&[receipts_naming(other, false)], tx_hash));
    }
}

#[cfg(test)]
mod first_receipt_tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn a_waiter_that_gives_up_does_not_end_the_wait() {
        let canonical = async {
            time::sleep(Duration::from_millis(20)).await;
            Some(1u8)
        };
        assert_eq!(first_receipt(async { None }, canonical, Duration::from_secs(5)).await, Some(1));

        let preview = async {
            time::sleep(Duration::from_millis(20)).await;
            Some(2u8)
        };
        assert_eq!(first_receipt(preview, async { None }, Duration::from_secs(5)).await, Some(2));
    }

    #[tokio::test]
    async fn the_wait_times_out_once_both_waiters_give_up() {
        let timeout = Duration::from_millis(50);
        let started = Instant::now();

        assert_eq!(first_receipt(async { None::<u8> }, async { None }, timeout).await, None);
        assert!(started.elapsed() >= timeout, "two waiters giving up must not end the wait early");
    }
}
