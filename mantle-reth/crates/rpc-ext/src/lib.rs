//! Mantle-specific RPC extensions.
//!
//! This crate provides Mantle-specific RPC methods that extend the standard Ethereum RPC API:
//!
//! - `eth_getBlockRange` — returns a list of blocks in a specified number range
//! - `eth_sendRawTransactionWithPreconf` — submits a raw transaction and returns a preconfirmation
//!   event from the sequencer
//!
//! # Preconfirmation types
//!
//! [`PreconfTxEvent`], [`PreconfStatus`], [`PreconfTxReceipt`], and [`PreconfLog`] are defined here
//! because they are both part of the RPC trait signature and part of the sequencer response
//! deserialization path.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{B256, Bytes, TxKind, U256};
use alloy_rpc_types_eth::TransactionRequest;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use op_revm::constants::{GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_optimism_evm::extract_l1_info;
use reth_optimism_forks::OpHardforks;
use reth_optimism_rpc::SequencerClient;
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_rpc_eth_api::{
    FullEthApiTypes,
    helpers::{EthBlocks, EthCall, EthFees},
};
use reth_rpc_server_types::result::invalid_params_rpc_err;
use reth_storage_api::{BlockIdReader, BlockReaderIdExt, StateProviderFactory};
use std::sync::Arc;
use tracing::debug;

/// Preconfirmation transaction event returned by `eth_sendRawTransactionWithPreconf`.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreconfTxEvent {
    /// Transaction hash
    pub tx_hash: B256,
    /// Preconfirmation status
    pub status: PreconfStatus,
    /// Optional failure message
    pub reason: String,
    /// Predicted L2 block number (hex-encoded quantity)
    #[serde(with = "alloy_serde::quantity")]
    pub block_height: u64,
    /// Preconfirmation transaction receipt
    pub receipt: PreconfTxReceipt,
}

/// Preconfirmation status
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreconfStatus {
    /// Preconfirmation succeeded
    #[serde(rename = "success")]
    Success,
    /// Preconfirmation failed
    #[serde(rename = "failed")]
    Failed,
    /// Preconfirmation timed out
    #[serde(rename = "timeout")]
    Timeout,
    /// Preconfirmation is waiting
    #[serde(rename = "waiting")]
    Waiting,
}

/// Preconfirmation transaction receipt
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreconfTxReceipt {
    /// Event logs
    #[serde(default)]
    pub logs: Vec<PreconfLog>,
}

/// Preconfirmation log entry
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreconfLog {
    /// Log address
    pub address: alloy_primitives::Address,
    /// Log topics
    pub topics: Vec<B256>,
    /// Log data
    pub data: Bytes,
}

// ─── RPC trait ───────────────────────────────────────────────────────────────

/// Extension trait for the `eth_` namespace providing Mantle-specific RPC methods.
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait MantleEthApiExt {
    /// Returns a list of blocks in the given range `[start, end]` (both inclusive).
    ///
    /// # Errors
    /// - `start > end`
    /// - range exceeds 1 000 blocks
    /// - `end` does not exist
    #[method(name = "getBlockRange")]
    async fn get_block_range(
        &self,
        start: BlockNumberOrTag,
        end: BlockNumberOrTag,
        full_transactions: bool,
    ) -> RpcResult<Vec<serde_json::Value>>;

    /// Sends a raw transaction with preconfirmation support.
    ///
    /// Forwards the transaction to the sequencer and returns a [`PreconfTxEvent`] that includes the
    /// predicted L2 block number and execution status.
    #[method(name = "sendRawTransactionWithPreconf")]
    async fn send_raw_transaction_with_preconf(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent>;

    /// Estimates the total fee for a transaction (L2 gas + L1 data + operator fee).
    ///
    /// Only supported on Mantle chains after the Arsia hardfork.
    #[method(name = "estimateTotalFee")]
    async fn estimate_total_fee(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

/// Mantle-specific `eth_` RPC extensions implementation.
///
/// Generic over:
/// - `Provider` — used to resolve `BlockNumberOrTag` to concrete block numbers
/// - `EthApi` — used to fetch fully-formatted RPC blocks (handles the network-specific type
///   conversion so we don't need to carry all the generic parameters here)
#[derive(Debug, Clone)]
pub struct MantleRpcExt<Provider, EthApi> {
    provider: Provider,
    eth_api: Arc<EthApi>,
    sequencer_client: Option<SequencerClient>,
}

impl<Provider, EthApi> MantleRpcExt<Provider, EthApi> {
    /// Creates a new [`MantleRpcExt`].
    pub fn new(
        provider: Provider,
        eth_api: Arc<EthApi>,
        sequencer_client: Option<SequencerClient>,
    ) -> Self {
        Self { provider, eth_api, sequencer_client }
    }

    #[inline]
    fn provider(&self) -> &Provider {
        &self.provider
    }

    #[inline]
    fn eth_api(&self) -> &EthApi {
        &self.eth_api
    }
}

/// Maximum number of blocks that may be requested in a single `eth_getBlockRange` call.
const MAX_BLOCK_RANGE: u64 = 1000;

/// geth `DefaultMantleBlockGasLimit` — used as `RPCGasCap` default in op-geth.
/// `estimateTotalFee` uses this to build a proxy tx envelope matching geth's `CallDefaults`,
/// which fills `args.Gas` with `RPCGasCap` before `ToTransaction` for L1 cost estimation.
const GETH_MANTLE_RPC_GAS_CAP: u64 = 0x4000000000000;

/// Caps gas for the L1 cost envelope, matching geth's `CallDefaults` behavior.
fn capped_gas_for_l1_envelope(request_gas: Option<u64>) -> u64 {
    request_gas.map(|gas| gas.min(GETH_MANTLE_RPC_GAS_CAP)).unwrap_or(GETH_MANTLE_RPC_GAS_CAP)
}

fn estimate_total_fee_gas_price(
    request_gas_price: Option<u128>,
    request_max_fee_per_gas: Option<u128>,
    request_max_priority_fee_per_gas: Option<u128>,
    base_fee: U256,
    suggested_tip: U256,
) -> U256 {
    match (request_gas_price, request_max_fee_per_gas) {
        (Some(gas_price), _) if gas_price > 0 => U256::from(gas_price),
        (_, Some(max_fee)) if max_fee > 0 => {
            let tip = U256::from(request_max_priority_fee_per_gas.unwrap_or(0));
            base_fee.saturating_add(tip).min(U256::from(max_fee))
        }
        _ => base_fee.saturating_add(suggested_tip),
    }
}

#[async_trait]
impl<Provider, EthApi> MantleEthApiExtServer for MantleRpcExt<Provider, EthApi>
where
    Provider: BlockIdReader
        + BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: OpHardforks + EthChainSpec>
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
    EthApi: EthBlocks + EthCall + EthFees + FullEthApiTypes + Send + Sync + 'static,
{
    async fn get_block_range(
        &self,
        start: BlockNumberOrTag,
        end: BlockNumberOrTag,
        full_transactions: bool,
    ) -> RpcResult<Vec<serde_json::Value>> {
        // Resolve symbolic tags (latest, earliest, …) to concrete block numbers.
        let start_num = self
            .provider()
            .convert_block_number(start)
            .map_err(|e| {
                ErrorObject::owned(
                    -32000,
                    format!("failed to convert start block number: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| invalid_params_rpc_err("start block number not found"))?;

        let end_num = self
            .provider()
            .convert_block_number(end)
            .map_err(|e| {
                ErrorObject::owned(
                    -32000,
                    format!("failed to convert end block number: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| invalid_params_rpc_err("end block number not found"))?;

        // Validate ordering.
        if end_num < start_num {
            return Err(invalid_params_rpc_err(format!(
                "start of block range ({start_num}) is greater than end of block range ({end_num})"
            )));
        }

        // Validate range size.
        let range_size = end_num.saturating_sub(start_num).saturating_add(1);
        if range_size > MAX_BLOCK_RANGE {
            return Err(invalid_params_rpc_err(format!(
                "requested block range is too large (max is {MAX_BLOCK_RANGE}, requested {range_size})"
            )));
        }

        // Verify that the end block actually exists by fetching it first.
        let end_block = EthBlocks::rpc_block(
            self.eth_api(),
            BlockNumberOrTag::Number(end_num).into(),
            full_transactions,
        )
        .await
        .map_err(|e| {
            ErrorObject::owned(-32000, format!("failed to fetch end block: {e}"), None::<()>)
        })?;

        if end_block.is_none() {
            return Err(invalid_params_rpc_err(format!(
                "end of requested block range ({end_num}) does not exist"
            )));
        }

        // Collect all blocks — serialise to `serde_json::Value` so that we avoid
        // carrying the network-specific `RpcBlock<EthApi::NetworkTypes>` generic
        // through the RPC trait boundary.
        let mut blocks = Vec::with_capacity(range_size as usize);

        for block_num in start_num..end_num {
            // All blocks in [start, end) — we already confirmed the end block exists.
            let block = EthBlocks::rpc_block(
                self.eth_api(),
                BlockNumberOrTag::Number(block_num).into(),
                full_transactions,
            )
            .await
            .map_err(|e| {
                ErrorObject::owned(
                    -32000,
                    format!("failed to fetch block {block_num}: {e}"),
                    None::<()>,
                )
            })?
            .ok_or_else(|| {
                ErrorObject::owned(
                    -32000,
                    format!("block {block_num} not indexed; this should never happen"),
                    None::<()>,
                )
            })?;

            let value = serde_json::to_value(block).map_err(|e| {
                ErrorObject::owned(-32000, format!("failed to serialise block: {e}"), None::<()>)
            })?;
            blocks.push(value);
        }

        // Append the end block (already fetched).
        let end_value = serde_json::to_value(end_block.unwrap()).map_err(|e| {
            ErrorObject::owned(-32000, format!("failed to serialise end block: {e}"), None::<()>)
        })?;
        blocks.push(end_value);

        Ok(blocks)
    }

    async fn send_raw_transaction_with_preconf(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        if let Some(sequencer) = self.sequencer_client.as_ref() {
            debug!(target: "rpc::eth::mantle", "forwarding raw transaction with preconf to sequencer");
            let raw: serde_json::Value = sequencer
                .forward_raw_transaction_with_preconf(bytes.as_ref())
                .await
                .map_err(|err| {
                    ErrorObject::owned(
                        -32000,
                        format!(
                            "failed to forward tx to sequencer, please try again. Error: '{err}'"
                        ),
                        None::<()>,
                    )
                })?;
            serde_json::from_value::<PreconfTxEvent>(raw).map_err(|err| {
                ErrorObject::owned(
                    -32000,
                    format!("failed to deserialise preconf event from sequencer: {err}"),
                    None::<()>,
                )
            })
        } else {
            Err(ErrorObject::owned(
                -32000,
                "sendRawTransactionWithPreconf: sequencer client not configured",
                None::<()>,
            ))
        }
    }

    async fn estimate_total_fee(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
    ) -> RpcResult<U256> {
        let block_id = block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));

        let block = self
            .provider()
            .block_by_id(block_id)
            .map_err(|e| {
                ErrorObject::owned(-32000, format!("failed to get block: {e}"), None::<()>)
            })?
            .ok_or_else(|| invalid_params_rpc_err("block not found"))?;

        // Pin symbolic block tags (safe, finalized, latest) to the resolved block number.
        // This ensures estimate_gas_at uses the same block even if a new block arrives.
        // Matches geth: `bNrOrHash = rpc.BlockNumberOrHashWithNumber(header.Number.Int64())`
        let block_id = BlockId::Number(BlockNumberOrTag::Number(block.header().number()));

        let header = block.header();
        let chain_spec = self.provider().chain_spec();

        if !chain_spec.is_mantle_arsia_active_at_timestamp(header.timestamp()) {
            return Err(ErrorObject::owned(
                -32000,
                "eth_estimateTotalFee is not supported for pre-Arsia blocks",
                None::<()>,
            ));
        }

        // Estimate L2 gas via the standard gas estimator (matches op-geth DoEstimateGas)
        let gas_estimate: U256 = EthCall::estimate_gas_at(
            self.eth_api(),
            serde_json::from_value(serde_json::to_value(&request).map_err(|e| {
                ErrorObject::owned(-32000, format!("invalid request: {e}"), None::<()>)
            })?)
            .map_err(|e| ErrorObject::owned(-32000, format!("invalid request: {e}"), None::<()>))?,
            block_id,
            None,
        )
        .await
        .map_err(|e| {
            ErrorObject::owned(-32000, format!("failed to estimate gas: {e}"), None::<()>)
        })?;

        let base_fee = U256::from(header.base_fee_per_gas().unwrap_or(0));

        // Get real suggested tip (matches op-geth SuggestGasTipCap)
        let suggested_tip =
            EthFees::suggested_priority_fee(self.eth_api()).await.unwrap_or(U256::ZERO);

        let gas_price = estimate_total_fee_gas_price(
            request.gas_price,
            request.max_fee_per_gas,
            request.max_priority_fee_per_gas,
            base_fee,
            suggested_tip,
        );
        let l2_fee = gas_estimate.saturating_mul(gas_price);

        // Calculate L1 data fee + operator fee from L1BlockInfo
        let (l1_data_fee, operator_fee) = match extract_l1_info(block.body()) {
            Ok(mut l1_block_info) => {
                // Read token_ratio from GasOracle contract state
                if let Ok(state) = self.provider().state_by_block_hash(header.parent_hash()) &&
                    let Ok(Some(ratio)) =
                        state.storage(GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT.into())
                {
                    l1_block_info.token_ratio = ratio;
                }

                // Build a proxy envelope matching geth's CallDefaults + ToTransaction:
                // - Gas = GETH_MANTLE_RPC_GAS_CAP (geth's CallDefaults fills Gas with RPCGasCap,
                //   which defaults to DefaultMantleBlockGasLimit = 0x4000000000000)
                // - ChainID = chain config chain ID
                // - When baseFee > 0 and no gasPrice → EIP-1559 tx; otherwise legacy
                let envelope_gas = U256::from(capped_gas_for_l1_envelope(request.gas));
                let chain_id = chain_spec.chain().id();
                let tx_envelope = build_unsigned_tx_envelope(
                    &request,
                    envelope_gas,
                    header.base_fee_per_gas().unwrap_or(0),
                    chain_id,
                );
                let spec_id = alloy_op_evm::spec_by_timestamp_after_bedrock(
                    chain_spec.as_ref(),
                    header.timestamp(),
                );
                let l1_data_fee =
                    l1_block_info.calculate_tx_l1_cost_for_estimate(&tx_envelope, spec_id, 80);

                // Operator fee: gas * scalar * 100 + constant
                let operator_fee = {
                    let scalar = l1_block_info.operator_fee_scalar.unwrap_or(U256::ZERO);
                    let constant = l1_block_info.operator_fee_constant.unwrap_or(U256::ZERO);
                    if scalar.is_zero() && constant.is_zero() {
                        U256::ZERO
                    } else {
                        gas_estimate
                            .saturating_mul(scalar)
                            .saturating_mul(U256::from(100))
                            .saturating_add(constant)
                    }
                };

                (l1_data_fee, operator_fee)
            }
            Err(_) => (U256::ZERO, U256::ZERO),
        };

        Ok(l2_fee.saturating_add(l1_data_fee).saturating_add(operator_fee))
    }
}

/// Builds an unsigned tx byte representation matching geth's `MarshalBinary` on an unsigned tx
/// created by `CallDefaults` + `ToTransaction(LegacyTxType)`.
///
/// geth's `ToTransaction` creates an unsigned tx (V/R/S = nil) and `MarshalBinary` encodes
/// Builds an unsigned transaction envelope matching geth's `eth_fillTransaction` output.
///
/// geth returns a signed envelope with a zero signature. We replicate this using
/// `into_signed(zero_sig)` + `encoded_2718()`.
fn build_unsigned_tx_envelope(
    request: &TransactionRequest,
    gas_estimate: U256,
    base_fee: u64,
    chain_id: u64,
) -> Vec<u8> {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::Signature;

    let gas_limit: u64 = gas_estimate.try_into().unwrap_or(u64::MAX);
    let to = request.to.unwrap_or(TxKind::Create);
    let value = request.value.unwrap_or(U256::ZERO);
    let input = request.input.input().cloned().unwrap_or_default();
    let nonce = request.nonce.unwrap_or(0);
    let zero_sig = Signature::new(U256::ZERO, U256::ZERO, false);

    if base_fee > 0 && request.gas_price.is_none() {
        TxEip1559 {
            chain_id,
            nonce,
            max_fee_per_gas: request.max_fee_per_gas.unwrap_or(0),
            max_priority_fee_per_gas: request.max_priority_fee_per_gas.unwrap_or(0),
            gas_limit,
            to,
            value,
            input,
            access_list: Default::default(),
        }
        .into_signed(zero_sig)
        .encoded_2718()
    } else {
        TxLegacy {
            chain_id: None,
            nonce,
            gas_price: request.gas_price.unwrap_or(0),
            gas_limit,
            to,
            value,
            input,
        }
        .into_signed(zero_sig)
        .encoded_2718()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── gas price selection ────────────────────────────────────────────

    #[test]
    fn gas_price_prefers_explicit() {
        let p = estimate_total_fee_gas_price(
            Some(123),
            Some(999),
            Some(7),
            U256::from(10),
            U256::from(5),
        );
        assert_eq!(p, U256::from(123));
    }

    #[test]
    fn gas_price_eip1559_cap() {
        let p =
            estimate_total_fee_gas_price(None, Some(15), Some(10), U256::from(10), U256::from(5));
        assert_eq!(p, U256::from(15));
    }

    #[test]
    fn gas_price_fallback_base_plus_tip() {
        let p = estimate_total_fee_gas_price(None, None, None, U256::from(10), U256::from(3));
        assert_eq!(p, U256::from(13));
    }

    // ─── tx envelope construction ───────────────────────────────────────

    #[test]
    fn envelope_eip1559_when_basefee_nonzero_and_no_gas_price() {
        let request =
            TransactionRequest { to: Some(TxKind::Call(Default::default())), ..Default::default() };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(21_000), 1_000_000, 1337);
        // EIP-1559 envelope starts with type byte 0x02
        assert_eq!(envelope[0], 0x02, "should be EIP-1559 (type 0x02)");
    }

    #[test]
    fn envelope_legacy_when_gas_price_specified() {
        let request = TransactionRequest {
            to: Some(TxKind::Call(Default::default())),
            gas_price: Some(10_000_000_000),
            ..Default::default()
        };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(21_000), 1_000_000, 1337);
        // Legacy envelope starts with RLP list prefix (>= 0xc0)
        assert!(envelope[0] >= 0xc0, "should be legacy RLP, got 0x{:02x}", envelope[0]);
    }

    #[test]
    fn envelope_legacy_when_basefee_zero() {
        let request =
            TransactionRequest { to: Some(TxKind::Call(Default::default())), ..Default::default() };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(21_000), 0, 1337);
        assert!(envelope[0] >= 0xc0, "baseFee=0 should produce legacy RLP");
    }

    #[test]
    fn envelope_includes_calldata() {
        let request_empty =
            TransactionRequest { to: Some(TxKind::Call(Default::default())), ..Default::default() };
        let calldata = vec![0xffu8; 256];
        let request_data = TransactionRequest {
            to: Some(TxKind::Call(Default::default())),
            input: alloy_rpc_types_eth::TransactionInput::new(calldata.into()),
            ..Default::default()
        };
        let empty = build_unsigned_tx_envelope(&request_empty, U256::from(21_000), 1_000_000, 1337);
        let with_data =
            build_unsigned_tx_envelope(&request_data, U256::from(100_000), 1_000_000, 1337);
        assert!(
            with_data.len() > empty.len() + 200,
            "256-byte calldata should add >200 bytes to envelope (empty={}, with_data={})",
            empty.len(),
            with_data.len()
        );
    }

    // ─── L1 data fee: deterministic tests with exact expected values ────

    fn test_l1_block_info() -> op_revm::L1BlockInfo {
        op_revm::L1BlockInfo {
            l1_base_fee: U256::from(30_000_000_000u64),
            l1_base_fee_scalar: U256::from(5000u64),
            l1_blob_base_fee: Some(U256::from(1_000_000u64)),
            l1_blob_base_fee_scalar: Some(U256::from(100u64)),
            token_ratio: U256::from(3000u64),
            ..Default::default()
        }
    }

    #[test]
    fn l1_cost_empty_calldata_hits_min_tx_size_floor() {
        let spec_id = op_revm::OpSpecId::ARSIA;
        let request = TransactionRequest {
            to: Some(TxKind::Call(Default::default())),
            value: Some(U256::from(1)),
            ..Default::default()
        };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(21_000), 1_000_000, 1337);
        let cost = test_l1_block_info().calculate_tx_l1_cost_for_estimate(&envelope, spec_id, 80);

        // Arsia formula: cost = max(MinTxSizeScaled, fastlz*COEF - INTERCEPT) * l1FeeScaled / 1e12
        // * tokenRatio l1FeeScaled = 30e9*16*5000 + 1e6*100 = 2_400_000_100_000_000
        // Small tx → MinTxSizeScaled = 100_000_000
        // cost = 100_000_000 * 2_400_000_100_000_000 / 1_000_000_000_000 * 3000 =
        // 720_000_030_000_000
        assert_eq!(cost, U256::from(720_000_030_000_000u64));
    }

    #[test]
    fn l1_cost_empty_input_returns_zero() {
        let cost = test_l1_block_info().calculate_tx_l1_cost_for_estimate(
            &[],
            op_revm::OpSpecId::ARSIA,
            80,
        );
        assert_eq!(cost, U256::ZERO);
    }

    #[test]
    fn l1_cost_large_calldata_exceeds_min_tx_size() {
        let spec_id = op_revm::OpSpecId::ARSIA;
        // High-entropy data defeats FastLZ compression → exceeds MinTxSizeScaled
        let data: Vec<u8> =
            (0u16..4096).map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8).collect();
        let request = TransactionRequest {
            to: Some(TxKind::Call(Default::default())),
            input: alloy_rpc_types_eth::TransactionInput::new(data.into()),
            ..Default::default()
        };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(100_000), 1_000_000, 1337);
        let cost = test_l1_block_info().calculate_tx_l1_cost_for_estimate(&envelope, spec_id, 80);

        assert_eq!(cost, U256::from(2_222_959_772_622_000u64));
    }

    #[test]
    fn l1_cost_regression_full_envelope_vs_calldata_only() {
        let spec_id = op_revm::OpSpecId::ARSIA;
        let request =
            TransactionRequest { to: Some(TxKind::Call(Default::default())), ..Default::default() };
        let envelope = build_unsigned_tx_envelope(&request, U256::from(21_000), 1_000_000, 1337);

        // Correct: full envelope → MinTxSizeScaled floor
        let cost_correct =
            test_l1_block_info().calculate_tx_l1_cost_for_estimate(&envelope, spec_id, 80);
        // Bug: empty bytes → zero
        let cost_buggy = test_l1_block_info().calculate_tx_l1_cost_for_estimate(&[], spec_id, 80);

        assert_eq!(cost_correct, U256::from(720_000_030_000_000u64));
        assert_eq!(cost_buggy, U256::ZERO);
    }
}
