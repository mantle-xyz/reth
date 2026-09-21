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
use reth_storage_api::{
    BlockIdReader, BlockReaderIdExt, StateProviderBox, StateProviderFactory, errors::ProviderResult,
};
use std::sync::Arc;
use tracing::debug;

/// Preconfirmation transaction event returned by `eth_sendRawTransactionWithPreconf`.
///
/// # What a preconfirmation does and does not promise
///
/// This event is produced the moment the sequencer applies the transaction to the
/// block it is **currently building** — before that block is sealed, and long
/// before it is canonical. So it is a prediction, not a record:
///
/// - **`tx_hash` is binding.** The sequencer has committed to landing *this* transaction, and holds
///   the `(sender, nonce)` it occupies against any other transaction for as long as the commitment
///   stands. The one exception is a breach: if the transaction later proves un-appliable (its nonce
///   was consumed elsewhere, its balance spent), the sequencer releases the nonce so the sender is
///   not wedged, and logs `COMMITMENT BROKEN` / `preconf.tx.commitment_broken_total`. A client that
///   re-submits the same hash then gets `CommitmentBroken` — the only channel through which the
///   breach is reported.
/// - **Everything else is a prediction of one particular build.** If that in-flight block is
///   discarded (a competing block takes the height, the payload job is superseded, the process
///   restarts), the sequencer re-applies the transaction to a *later* block — honouring the
///   promise, but against different state. `block_height`, `status` and `receipt.logs` are all
///   recomputed then, and the client is **not** sent a corrected event: the response to this call
///   has already been delivered.
///
/// Consequently, **do not reconcile on the fields of this event**. Treat it as
/// "accepted, will land", and read the authoritative outcome from
/// `eth_getTransactionReceipt` once the transaction is on chain. In particular a
/// `Success` here can be followed by a failed on-chain receipt if the
/// transaction reverts when replayed against later state.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreconfTxEvent {
    /// Transaction hash. The one field that is binding — see the type docs.
    pub tx_hash: B256,
    /// Preconfirmation status, as evaluated against the in-flight block.
    ///
    /// **May differ from the eventual on-chain receipt.** A cross-block replay
    /// re-executes against different state, so a `Success` here can end up as a
    /// reverted on-chain receipt (and vice versa). Not a reconciliation source.
    pub status: PreconfStatus,
    /// Optional failure message
    pub reason: String,
    /// **Predicted** L2 block number (hex-encoded quantity).
    ///
    /// Computed as `parent_number + 1` for the block being built when this
    /// preconfirmation was issued. If that block is discarded the transaction
    /// lands at a **later** height and this value is never corrected. Use
    /// `eth_getTransactionReceipt` for the authoritative height.
    #[serde(with = "alloy_serde::quantity")]
    pub block_height: u64,
    /// Preconfirmation transaction receipt
    pub receipt: PreconfTxReceipt,
}

/// Preconfirmation status.
///
/// Matches op-geth's 4-variant `PreconfStatus` for cross-client SDK
/// compatibility. Server pre-apply rejection (e.g. block gas budget) is
/// collapsed into `Failed` at this wire boundary — the fine-grained
/// reason travels in [`PreconfTxEvent::reason`]. The internal fifo state
/// machine still distinguishes `Canceled` from `Failed` for replacement
/// / clean-up semantics.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreconfStatus {
    /// EVM apply succeeded; tx on chain.
    #[serde(rename = "success")]
    Success,
    /// Either (a) EVM apply revert/halt (tx on chain) OR (b) builder
    /// pre-apply reject / server-side cancel (tx NOT on chain). The
    /// [`PreconfTxEvent::reason`] field disambiguates.
    #[serde(rename = "failed")]
    Failed,
    /// Client-side deadline elapsed (`preconf_timeout`); tx NOT on chain.
    #[serde(rename = "timeout")]
    Timeout,
    /// Preconfirmation is waiting (intermediate state; typically not
    /// broadcast to subscribers).
    #[serde(rename = "waiting")]
    Waiting,
}

/// Preconfirmation transaction receipt.
///
/// `logs` is `Option` rather than a plain `Vec` so absence semantics
/// can be expressed on the wire: `null` when no EVM apply happened
/// (Timeout / server pre-apply reject), `[]` when apply happened but
/// emitted no logs. SDKs treat both as "no logs" but the wire shape
/// preserves the distinction.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreconfTxReceipt {
    /// Event logs, echoed verbatim from the sequencer.
    ///
    /// Modeled as `Option` so the sequencer's exact JSON shape round-trips. An op-geth sequencer
    /// emits `"logs": null` for a reverted tx (Go nil slice) and `"logs": []` / `[..]` otherwise.
    /// A plain `Vec<PreconfLog>` with `#[serde(default)]` both rejected an explicit `null` on the
    /// wire (`invalid type: null, expected a sequence`) *and* would re-serialize an empty result
    /// as `[]`, diverging from geth. `Option<Vec<_>>` deserializes null→None and re-serializes
    /// None→null, so a forwarding reth node returns byte-identical shape to the geth sequencer.
    /// The `null` / `[]` distinction is the one described on the type above.
    ///
    /// Like every other field of [`PreconfTxEvent`] except `tx_hash`, these are
    /// the logs of one particular in-flight execution. A cross-block replay
    /// re-executes against different state and can emit different logs, with no
    /// corrected event sent. Read `eth_getTransactionReceipt` to reconcile.
    #[serde(default)]
    pub logs: Option<Vec<PreconfLog>>,
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

// ─── Preconf handler indirection ─────────────────────────────────────────────

/// Dyn-safe entry point for the local preconf handler.
///
/// The concrete implementation in `mantle-reth-preconf::rpc::PreconfRpcHandler`
/// is generic over the pool and the state provider; this trait erases those
/// generics so `MantleRpcExt` can hold an `Option<Arc<dyn DynPreconfHandler>>`
/// without becoming generic itself.
///
/// Returning a `PreconfTxEvent` (the existing wire type) keeps the RPC trait
/// signature unchanged — clients see the same response shape regardless of
/// whether the node is acting as a sequencer (local handler) or a follower
/// (forwarding to an upstream sequencer).
#[async_trait]
pub trait DynPreconfHandler: Send + Sync + std::fmt::Debug {
    /// Process a raw transaction submission and return the preconf event.
    async fn handle(&self, bytes: Bytes) -> RpcResult<PreconfTxEvent>;
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

    /// Overrides `eth_simulateV1` to reject simulations that cross the Mantle Arsia activation
    /// boundary, then delegates to the standard implementation.
    ///
    /// A simulated block that crosses the boundary would be assembled from the pre-Arsia parent's
    /// empty `extraData` (so the EIP-1559 denominator/elasticity/minBaseFee silently fall back to
    /// chain-spec defaults) and a pre-Arsia DA-footprint basis, while its own timestamp is
    /// post-activation. The resulting block is internally inconsistent, so refuse it rather than
    /// return a plausible-looking wrong answer. Matches op-geth, which rejects the same case in
    /// `processBlock` (`internal/ethapi/simulate.go`).
    ///
    /// Payload and result cross the RPC boundary as `serde_json::Value` to avoid carrying the
    /// network-specific `RpcTxReq`/`RpcBlock` generics through the trait, matching
    /// `eth_getBlockRange` above.
    #[method(name = "simulateV1")]
    async fn simulate_v1(
        &self,
        payload: serde_json::Value,
        block_number: Option<BlockId>,
    ) -> RpcResult<serde_json::Value>;
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
    /// Local preconf handler. `Some` only when this node is acting as the
    /// sequencer with preconf enabled (wired by the preconf `ServiceBuilder`).
    /// `None` ⇒ fall back to `sequencer_client` forward / "not implemented"
    /// stub.
    preconf_handler: Option<Arc<dyn DynPreconfHandler>>,
}

impl<Provider, EthApi> MantleRpcExt<Provider, EthApi> {
    /// Creates a new [`MantleRpcExt`].
    pub fn new(
        provider: Provider,
        eth_api: Arc<EthApi>,
        sequencer_client: Option<SequencerClient>,
        preconf_handler: Option<Arc<dyn DynPreconfHandler>>,
    ) -> Self {
        Self { provider, eth_api, sequencer_client, preconf_handler }
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

/// Minimal state-source seam for [`read_token_ratio`].
///
/// A blanket impl covers every [`StateProviderFactory`], so production passes the real
/// provider unchanged; tests implement just this one method to inject provider failures
/// without standing up a full provider stack.
trait BlockState {
    fn state_at(&self, block_id: BlockId) -> ProviderResult<StateProviderBox>;
}

impl<P: StateProviderFactory> BlockState for P {
    fn state_at(&self, block_id: BlockId) -> ProviderResult<StateProviderBox> {
        self.state_by_block_id(block_id)
    }
}

/// Reads the Mantle `token_ratio` (`GasPriceOracle` slot 0) from `block_id`'s post-state.
///
/// `token_ratio` is not carried in the L1-attributes calldata, so it must be read from
/// state. It is the last (multiplicative) factor of the Arsia L1 data fee, so a silently
/// swallowed provider error would leave it at the default `0` and drop the entire L1 data
/// fee component — a large, silent under-estimate. We therefore propagate provider errors,
/// and treat a missing slot (`Ok(None)`) as the on-chain zero value (matching geth, which
/// reads the same slot from the same target-block state).
fn read_token_ratio<S: BlockState>(source: &S, block_id: BlockId) -> RpcResult<U256> {
    let state = source.state_at(block_id).map_err(|e| {
        ErrorObject::owned(-32000, format!("failed to load state for token_ratio: {e}"), None::<()>)
    })?;
    Ok(state
        .storage(GAS_ORACLE_CONTRACT, TOKEN_RATIO_SLOT.into())
        .map_err(|e| {
            ErrorObject::owned(-32000, format!("failed to read token_ratio: {e}"), None::<()>)
        })?
        .unwrap_or(U256::ZERO))
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

/// Default spacing between simulated blocks when a request does not override `time`.
///
/// Must track `OpNextBlockEnvAttributes::build_pending_env`, which derives a simulated block's
/// timestamp as `parent.timestamp() + 12`. The value comes from `eth_simulateV1`'s original
/// definition in go-ethereum (`timestampIncrement`, `internal/ethapi/simulate.go`) and is a plain
/// constant on both clients today, so 12 is what op-geth and this reth actually produce — verified
/// on devnet.
///
/// It is nonetheless *wrong for Mantle*, whose blocks are 2s apart, and upstream reth has already
/// replaced the constant with a per-chain lookup (`Chain::average_blocktime_hint()`), keeping 12
/// only as the fallback for unregistered chains. Mantle is **not** unregistered: `alloy-chains`
/// records `average_blocktime_millis = 2000` for both chain 5000 and 5003
/// (`CHAIN_DATA[95]`/`[96]` in `alloy-chains/src/generated/named.rs`).
///
/// So bumping to a rev that includes that change will switch the real increment from 12 to 2, and
/// this constant will silently disagree with it — the crossing check would compute timestamps ~6x
/// too far ahead and could reject a request that never crosses the fork. See
/// [`simulated_block_timestamps`] for the fix to apply at bump time.
const SIMULATE_DEFAULT_BLOCK_TIME_INCREMENT: u64 = 12;

/// Parses a JSON-RPC quantity into a `u64`, accepting both the canonical `"0x2a"` hex string and a
/// bare JSON number.
fn parse_quantity_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(raw) => {
            let raw = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X"))?;
            u64::from_str_radix(raw, 16).ok()
        }
        serde_json::Value::Number(raw) => raw.as_u64(),
        _ => None,
    }
}

/// Returns the timestamp of each simulated block, in order, given the base block's timestamp and
/// each entry's optional `blockOverrides.time`.
///
/// Mirrors how `eth_simulateV1` derives timestamps: default to `previous + 12`, but an explicit
/// `time` override wins. Each block's parent is the *previous simulated block*, not the base block,
/// so the timestamps must be walked forward rather than computed against the base.
///
/// This assumes one simulated block per `blockStateCalls` entry, and that consecutive blocks are
/// [`SIMULATE_DEFAULT_BLOCK_TIME_INCREMENT`] apart. Both hold today but both break on the same
/// upstream bump:
///
/// - paradigmxyz/reth#24388 adds gap-filling, so a skipped `blockOverrides.number` inserts filler
///   blocks that also consume timestamps — the entry's real timestamp then lands *later* than
///   computed here, and a crossing could be missed.
/// - the same tree replaces the fixed increment with `Chain::average_blocktime_hint()`, which
///   returns 2s for Mantle (registered in `alloy-chains`), so timestamps computed at 12s would run
///   ~6x too far ahead and a non-crossing request could be rejected.
///
/// The single fix for both: once on such a rev, derive the timestamps from `sanitize_chain`'s
/// output — it materializes an explicit `time` for every block, fillers included, at the chain's
/// real block time — instead of recomputing them here.
fn simulated_block_timestamps(base_timestamp: u64, time_overrides: &[Option<u64>]) -> Vec<u64> {
    let mut previous = base_timestamp;
    time_overrides
        .iter()
        .map(|override_time| {
            let timestamp = override_time
                .unwrap_or_else(|| previous.saturating_add(SIMULATE_DEFAULT_BLOCK_TIME_INCREMENT));
            previous = timestamp;
            timestamp
        })
        .collect()
}

/// Returns the 0-based index of the first simulated block that crosses the Arsia activation
/// boundary, i.e. whose parent is pre-Arsia while the block itself is post-Arsia.
///
/// `is_arsia` reports whether Arsia is active at a given timestamp.
///
/// The parent is the previous *simulated* block rather than the base block. For finding the *first*
/// crossing the two are equivalent (timestamps increase and `is_arsia` is a monotonic threshold, so
/// every block before the first post-activation one has a pre-Arsia parent either way), but keeping
/// the real parent makes the check mean what it says and stays correct if this is ever reused to
/// report more than the first crossing.
fn first_arsia_boundary_crossing(
    base_timestamp: u64,
    timestamps: &[u64],
    is_arsia: impl Fn(u64) -> bool,
) -> Option<usize> {
    let mut parent_timestamp = base_timestamp;
    for (index, &timestamp) in timestamps.iter().enumerate() {
        if !is_arsia(parent_timestamp) && is_arsia(timestamp) {
            return Some(index);
        }
        parent_timestamp = timestamp;
    }
    None
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
    // Lets `eth_simulateV1` surface the standard implementation's error verbatim, preserving the
    // spec-defined codes (-38010..-38026) instead of collapsing them into a generic -32000.
    ErrorObject<'static>: From<EthApi::Error>,
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
        // Path 1: local sequencer + preconf enabled → handle in-process.
        if let Some(handler) = self.preconf_handler.as_ref() {
            return handler.handle(bytes).await;
        }
        // Path 2: follower node → forward to upstream sequencer (existing behavior).
        if let Some(sequencer) = self.sequencer_client.as_ref() {
            debug!(target: "rpc::eth::mantle", "forwarding raw transaction with preconf to sequencer");
            // Follower→sequencer forward latency (success and error paths).
            // op-geth `preconf/txpool/forward` analogue; follower-only.
            let started = std::time::Instant::now();
            let forward_result =
                sequencer.forward_raw_transaction_with_preconf(bytes.as_ref()).await;
            metrics::histogram!("preconf.forward.duration_ms")
                .record(started.elapsed().as_millis() as f64);
            let raw: serde_json::Value = forward_result.map_err(|err| {
                ErrorObject::owned(
                    -32000,
                    format!("failed to forward tx to sequencer, please try again. Error: '{err}'"),
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
                // `token_ratio` (GasPriceOracle slot 0) is not in the L1-attributes calldata;
                // read it from the target block's post-state (matches geth's
                // `StateAndHeaderByNumberOrHash`). Propagate provider errors rather than
                // silently keeping the default 0 — `token_ratio` is the multiplicative factor
                // of the L1 data fee, so swallowing an error would drop the whole L1 component.
                l1_block_info.token_ratio = read_token_ratio(self.provider(), block_id)?;

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

    async fn simulate_v1(
        &self,
        payload: serde_json::Value,
        block_number: Option<BlockId>,
    ) -> RpcResult<serde_json::Value> {
        let chain_spec = self.provider().chain_spec();

        // Only Mantle chains have an Arsia fork; leave every other chain on the standard path.
        if chain_spec.is_mantle() {
            let block_id = block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest));

            // Read only the `time` overrides. Everything else is the standard implementation's
            // concern, and deserializing the full typed payload here would drag the
            // network-specific `RpcTxReq` generic through the trait boundary.
            let time_overrides = payload
                .get("blockStateCalls")
                .and_then(serde_json::Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| {
                            call.get("blockOverrides")
                                .and_then(|overrides| overrides.get("time"))
                                .and_then(parse_quantity_u64)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // An empty/absent `blockStateCalls` is the standard implementation's error to report,
            // so skip the check and let it produce the canonical message.
            if !time_overrides.is_empty() {
                let base_timestamp = self
                    .provider()
                    .block_by_id(block_id)
                    .map_err(|e| {
                        ErrorObject::owned(-32000, format!("failed to get block: {e}"), None::<()>)
                    })?
                    .ok_or_else(|| invalid_params_rpc_err("block not found"))?
                    .header()
                    .timestamp();

                let timestamps = simulated_block_timestamps(base_timestamp, &time_overrides);
                if let Some(index) =
                    first_arsia_boundary_crossing(base_timestamp, &timestamps, |timestamp| {
                        chain_spec.is_mantle_arsia_active_at_timestamp(timestamp)
                    })
                {
                    debug!(
                        target: "rpc::eth",
                        base_timestamp,
                        crossing_block_index = index,
                        crossing_block_timestamp = timestamps[index],
                        "rejecting eth_simulateV1 crossing the Mantle Arsia activation boundary"
                    );
                    return Err(ErrorObject::owned(
                        -32000,
                        "eth_simulateV1 does not support crossing the Mantle Arsia activation \
                         boundary",
                        None::<()>,
                    ));
                }
            }
        }

        // Not a boundary-crossing simulation: hand off to the standard implementation. Round-trip
        // through JSON so the network-specific payload/result generics stay behind `EthApi`.
        let typed_payload = serde_json::from_value(payload)
            .map_err(|e| invalid_params_rpc_err(format!("invalid eth_simulateV1 payload: {e}")))?;
        let blocks = EthCall::simulate_v1(self.eth_api(), typed_payload, block_number)
            .await
            .map_err(ErrorObject::from)?;
        serde_json::to_value(blocks).map_err(|e| {
            ErrorObject::owned(
                -32000,
                format!("failed to serialise eth_simulateV1 result: {e}"),
                None::<()>,
            )
        })
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

    // ─── PreconfTxReceipt.logs three-state serde ────────────────────
    //
    // R6/T7 — `logs` distinguishes "no EVM apply happened" (`null`)
    // from "apply happened but no logs" (`[]`) on the wire. This is a
    // deliberate contract that R5/D1 wire refactor introduced (changed
    // from `Vec<PreconfLog>` to `Option<Vec<PreconfLog>>`). SDKs rely
    // on the distinction to build UX around Timeout vs revert.

    #[test]
    fn preconf_tx_receipt_logs_none_serializes_as_null() {
        let r = PreconfTxReceipt { logs: None };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"logs":null}"#);
    }

    #[test]
    fn preconf_tx_receipt_logs_empty_vec_serializes_as_empty_array() {
        let r = PreconfTxReceipt { logs: Some(vec![]) };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"logs":[]}"#);
    }

    #[test]
    fn preconf_tx_receipt_logs_populated_roundtrips() {
        let addr = alloy_primitives::Address::from([0xAB; 20]);
        let r = PreconfTxReceipt {
            logs: Some(vec![PreconfLog {
                address: addr,
                topics: vec![B256::from([0xCD; 32])],
                data: Bytes::from(vec![0xEF, 0xEF]),
            }]),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: PreconfTxReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn preconf_tx_receipt_null_and_empty_are_distinct() {
        let null_r: PreconfTxReceipt = serde_json::from_str(r#"{"logs":null}"#).unwrap();
        let empty_r: PreconfTxReceipt = serde_json::from_str(r#"{"logs":[]}"#).unwrap();
        assert!(null_r.logs.is_none(), "null decodes as None");
        assert_eq!(empty_r.logs, Some(vec![]), "[] decodes as Some(empty)");
        assert_ne!(null_r, empty_r);
    }

    #[test]
    fn preconf_status_wire_serde_has_four_variants() {
        // R5/D1 decision 8B — wire enum matches op-geth's 4 variants.
        // Regression guard against silently re-introducing `Canceled`.
        for (variant, expected) in [
            (PreconfStatus::Success, r#""success""#),
            (PreconfStatus::Failed, r#""failed""#),
            (PreconfStatus::Timeout, r#""timeout""#),
            (PreconfStatus::Waiting, r#""waiting""#),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), expected);
            let back: PreconfStatus = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant);
        }
        // `"canceled"` must NOT deserialize (variant was deleted).
        assert!(serde_json::from_str::<PreconfStatus>(r#""canceled""#).is_err());
    }

    /// Regression for the cross-client `eth_estimateTotalFee` divergence at Sepolia-QA4
    /// block 597707 (PR #73). `token_ratio` (`GasPriceOracle` slot 0) changed `3231` ->
    /// `3224` *inside* that block, so reading it from the parent block (`3231`) instead of
    /// the target block's post-state (`3224`) inflated the L1 data fee. `token_ratio` is
    /// the last (multiplicative) factor in the Arsia L1 cost, so the per-client delta is
    /// exactly the L1 data fee difference between the two ratios — and it must match the
    /// divergence observed on-chain: reth `0x82759e66553fe` - geth `0x8254fc7e3b730` =
    /// `2_242_484_739_278`.
    ///
    /// This pins the formula's exact linearity in `token_ratio` and the real on-chain
    /// delta, guarding against any change that reintroduces a stale-ratio L1 cost.
    #[test]
    fn l1_cost_token_ratio_597707_divergence() {
        // Real L1BlockInfo inputs captured at block 597707 (L1Block 0x42..0015):
        //   slot 1 l1_base_fee       = 0x411e5766
        //   slot 3 baseFeeScalar     = 169019  (bytes 16..20)
        //          blobBaseFeeScalar = 4544124 (bytes 20..24)
        //   slot 7 l1_blob_base_fee  = 0x0344616e
        let info = |token_ratio: u64| op_revm::L1BlockInfo {
            l1_base_fee: U256::from(0x411e_5766u64),
            l1_base_fee_scalar: U256::from(169_019u64),
            l1_blob_base_fee: Some(U256::from(0x0344_616eu64)),
            l1_blob_base_fee_scalar: Some(U256::from(4_544_124u64)),
            token_ratio: U256::from(token_ratio),
            ..Default::default()
        };

        // The same proxy envelope `estimate_total_fee` builds for the RM-02d request:
        // legacy tx (explicit gasPrice 0xba43b7400), gas = GETH_MANTLE_RPC_GAS_CAP,
        // value 0, empty calldata.
        let request = TransactionRequest {
            to: Some(TxKind::Call("0xB287edE875C18e1F468563a77E3b9a12A7e00349".parse().unwrap())),
            value: Some(U256::ZERO),
            gas_price: Some(0xba43b7400),
            ..Default::default()
        };
        let envelope_gas = U256::from(capped_gas_for_l1_envelope(request.gas));
        let envelope = build_unsigned_tx_envelope(&request, envelope_gas, 0, 0x3569128);

        let spec_id = op_revm::OpSpecId::ARSIA;
        // 3231 = parent (pre-update, the bug); 3224 = target post-state (correct, matches geth).
        let cost_parent = info(3231).calculate_tx_l1_cost_for_estimate(&envelope, spec_id, 80);
        let cost_target = info(3224).calculate_tx_l1_cost_for_estimate(&envelope, spec_id, 80);

        assert!(cost_parent > cost_target, "a higher token_ratio must yield a higher L1 cost");
        // Exact linearity in token_ratio: cost(r) = base * r (token_ratio applied last).
        assert_eq!(
            cost_parent * U256::from(3224u64),
            cost_target * U256::from(3231u64),
            "L1 cost must be exactly linear in token_ratio"
        );
        // The reth/geth divergence equals the L1 data fee delta between the two ratios.
        assert_eq!(
            cost_parent - cost_target,
            U256::from(2_242_484_739_278u64),
            "must reproduce the on-chain 597707 divergence \
             (reth 0x82759e66553fe - geth 0x8254fc7e3b730)"
        );
    }

    // ─── token_ratio state-read error propagation ───────────────────────

    use reth_storage_api::errors::ProviderError;

    /// A [`BlockState`] whose state lookup always fails, so we can assert that
    /// [`read_token_ratio`] surfaces provider errors instead of silently falling back to
    /// `token_ratio = 0` (which would drop the entire L1 data fee).
    struct FailingState;

    impl BlockState for FailingState {
        fn state_at(&self, _: BlockId) -> ProviderResult<StateProviderBox> {
            Err(ProviderError::StateForNumberNotFound(0))
        }
    }

    #[test]
    fn read_token_ratio_propagates_state_error() {
        // Pre-hardening, the read used `if let Ok(state) = state_by_block_id(..)` and silently
        // left `token_ratio = 0` on error, zeroing the L1 data fee. The hardened helper must
        // surface the provider error instead.
        let result = read_token_ratio(&FailingState, BlockId::Number(1u64.into()));
        assert!(
            result.is_err(),
            "a provider error while reading token_ratio must propagate, not silently yield 0"
        );
    }

    // ─── preconf event deserialization ──────────────────────────────────

    #[test]
    fn preconf_receipt_accepts_null_logs() {
        // The sequencer sends `"logs": null` (not `[]`) for reverted transactions. Before the
        // `Option<Vec<_>>` change this failed with "invalid type: null, expected a sequence".
        let json = r#"{
            "txHash": "0x66199f44ede67884fa62012bde48a4e7823c2ce6a827f4c33e28d001a9c37cf3",
            "status": "failed",
            "reason": "execution reverted: ERC20: insufficient balance",
            "blockHeight": "0xe9be5",
            "receipt": { "logs": null }
        }"#;
        let event: PreconfTxEvent = serde_json::from_str(json).expect("null logs must deserialize");
        assert_eq!(event.status, PreconfStatus::Failed);
        assert_eq!(event.receipt.logs, None);
        // Byte-parity with geth: null must round-trip back to null (not []).
        let reser = serde_json::to_value(&event).unwrap();
        assert!(reser["receipt"]["logs"].is_null(), "null logs must re-serialize as null");
    }

    #[test]
    fn preconf_receipt_accepts_missing_logs() {
        let json = r#"{
            "txHash": "0x66199f44ede67884fa62012bde48a4e7823c2ce6a827f4c33e28d001a9c37cf3",
            "status": "success",
            "reason": "",
            "blockHeight": "0xe9be5",
            "receipt": {}
        }"#;
        let event: PreconfTxEvent = serde_json::from_str(json).expect("missing logs must default");
        assert_eq!(event.receipt.logs, None);
    }

    #[test]
    fn preconf_receipt_roundtrips_empty_and_populated_logs() {
        // Empty array stays an empty array (a successful, log-less tx), and a populated array is
        // preserved — reth echoes the sequencer's exact shape in every case.
        let empty: PreconfTxReceipt = serde_json::from_str(r#"{ "logs": [] }"#).unwrap();
        assert_eq!(empty.logs, Some(vec![]));
        assert_eq!(serde_json::to_value(&empty).unwrap()["logs"].to_string(), "[]");

        let json = r#"{ "logs": [{
            "address": "0x5bec7df4940345b717361664c4847bb3b794eaca",
            "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"],
            "data": "0x000000000000000000000000000000000000000000000000000000000000000a"
        }] }"#;
        let populated: PreconfTxReceipt = serde_json::from_str(json).unwrap();
        assert_eq!(populated.logs.as_ref().map(Vec::len), Some(1));
    }

    // ─── eth_simulateV1 Arsia boundary ───────────────────────────────────────

    /// Activation timestamp used by the boundary tests below.
    const ARSIA_TIME: u64 = 1_000;

    fn is_arsia(timestamp: u64) -> bool {
        timestamp >= ARSIA_TIME
    }

    #[test]
    fn simulated_timestamps_default_to_parent_plus_increment() {
        // Each block's parent is the previous simulated block, so the increments compound.
        assert_eq!(simulated_block_timestamps(100, &[None, None, None]), vec![112, 124, 136]);
    }

    #[test]
    fn simulated_timestamps_honour_time_overrides() {
        // An explicit override wins, and subsequent defaults build on it — not on the base block.
        assert_eq!(simulated_block_timestamps(100, &[None, Some(500), None]), vec![112, 500, 512]);
    }

    /// Path 1 from the cross-client report: the base block is the last pre-Arsia block, so the very
    /// first simulated block (base + 12) already crosses the activation boundary.
    #[test]
    fn detects_crossing_when_base_block_is_pre_arsia() {
        let base = ARSIA_TIME - 2;
        let timestamps = simulated_block_timestamps(base, &[None]);
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), Some(0));
    }

    /// Path 2 from the report: the first simulated block stays pre-Arsia and only the second one
    /// crosses, via an explicit `blockOverrides.time`. The parent for that comparison is the
    /// previous *simulated* block, which is why the walk cannot be done against the base block.
    #[test]
    fn detects_crossing_introduced_by_a_later_time_override() {
        let base = ARSIA_TIME - 20;
        let timestamps = simulated_block_timestamps(base, &[None, Some(ARSIA_TIME)]);
        assert_eq!(timestamps, vec![ARSIA_TIME - 8, ARSIA_TIME]);
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), Some(1));
    }

    #[test]
    fn allows_simulation_entirely_before_activation() {
        let base = 0;
        let timestamps = simulated_block_timestamps(base, &[None, None]);
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), None);
    }

    /// The boundary is crossed by the *first* simulated block when the base block is the last
    /// pre-Arsia block, even though every later block is also post-activation: only the transition
    /// counts, and it is reported once.
    #[test]
    fn reports_the_first_crossing_block_not_a_later_one() {
        let base = ARSIA_TIME - 2;
        let timestamps = simulated_block_timestamps(base, &[None, None, None]);
        // base is pre-Arsia; all three simulated blocks land after activation.
        assert!(timestamps.iter().all(|&ts| is_arsia(ts)));
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), Some(0));
    }

    /// Once activation is reached, a chain whose base is already post-activation has no transition
    /// left to report.
    #[test]
    fn does_not_re_report_crossing_after_activation_is_reached() {
        let base = ARSIA_TIME - 20;
        // Block 0 jumps past activation, block 1 defaults to block 0 + 12 (also post-activation).
        let timestamps = simulated_block_timestamps(base, &[Some(ARSIA_TIME), None]);
        assert_eq!(timestamps, vec![ARSIA_TIME, ARSIA_TIME + 12]);
        // The crossing is at index 0 only — index 1's parent is already post-activation.
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), Some(0));
        // And with the crossing block removed, the remaining chain is entirely post-activation.
        assert_eq!(first_arsia_boundary_crossing(ARSIA_TIME, &timestamps[1..], is_arsia), None);
    }

    /// The common case on a live chain: activation is already in the past, so nothing crosses.
    #[test]
    fn allows_simulation_entirely_after_activation() {
        let base = ARSIA_TIME + 100;
        let timestamps = simulated_block_timestamps(base, &[None, None]);
        assert_eq!(first_arsia_boundary_crossing(base, &timestamps, is_arsia), None);
    }

    #[test]
    fn parses_quantity_from_hex_string_and_number() {
        assert_eq!(parse_quantity_u64(&serde_json::json!("0x6a82e880")), Some(1_786_964_096));
        assert_eq!(parse_quantity_u64(&serde_json::json!("0X2a")), Some(42));
        assert_eq!(parse_quantity_u64(&serde_json::json!(42)), Some(42));
        // Malformed values must not be silently read as 0 — that would fake a pre-Arsia timestamp
        // and skip the boundary check.
        assert_eq!(parse_quantity_u64(&serde_json::json!("2a")), None);
        assert_eq!(parse_quantity_u64(&serde_json::json!("0xzz")), None);
        assert_eq!(parse_quantity_u64(&serde_json::Value::Null), None);
    }
}
