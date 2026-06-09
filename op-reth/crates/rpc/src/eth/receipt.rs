//! Loads and formats OP receipt RPC response.

use crate::{OpEthApi, OpEthApiError, eth::RpcNodeCore};
use alloy_consensus::{BlockHeader, Receipt, ReceiptWithBloom, TxReceipt};
use alloy_eips::{BlockHashOrNumber, eip2718::Encodable2718};
use alloy_primitives::{B256, U256, b256};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use op_alloy_consensus::{OpReceipt, OpTransaction, parse_post_exec_payload_from_transactions};
use op_alloy_rpc_types::{L1BlockInfo, OpTransactionReceipt, OpTransactionReceiptFields};
use op_revm::{constants::GAS_ORACLE_CONTRACT, estimate_tx_compressed_size};
use parking_lot::Mutex;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_node_api::NodePrimitives;
use reth_optimism_evm::RethL1BlockInfo;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::{BlockBody, Receipt as ReceiptTrait, SealedBlock};
use reth_rpc_eth_api::{
    RpcConvert,
    helpers::LoadReceipt,
    transaction::{ConvertReceiptInput, ReceiptConverter},
};
use reth_rpc_eth_types::{EthApiError, receipt::build_receipt};
use reth_storage_api::{BlockReader, ReceiptProvider, StateProviderFactory};
use schnellru::{ByLength, LruMap};
use std::{fmt::Debug, sync::Arc};

// ---- [MANTLE] Per-tx token_ratio tracking ----

const TOKEN_RATIO_UPDATED_TOPIC: U256 = U256::from_be_bytes(
    b256!("5d6ae9db2d6725497bed0302a8212c0db5fdb3bd7d14f188a83b5589089caafd").0,
);

const MAX_REASONABLE_TOKEN_RATIO: u128 = 1_000_000_000;
const TOKEN_RATIO_PREFIX_CACHE_MAX_BLOCKS: u32 = 1024;

type TokenRatioPrefixCache = Arc<Mutex<LruMap<B256, Arc<Vec<U256>>, ByLength>>>;

/// Returns `token_ratio` after applying any `TokenRatioUpdated` event in the given logs.
fn token_ratio_after_logs(mut current: U256, logs: &[alloy_primitives::Log]) -> U256 {
    for log in logs {
        if log.address != GAS_ORACLE_CONTRACT {
            continue;
        }
        let topics = log.topics();
        let is_token_ratio_updated =
            topics.first().is_some_and(|t| U256::from_be_bytes(t.0) == TOKEN_RATIO_UPDATED_TOPIC);
        if is_token_ratio_updated && let Some(new_ratio) = topics.last() {
            let new_ratio_val = U256::from_be_bytes(new_ratio.0);
            if new_ratio_val <= U256::from(MAX_REASONABLE_TOKEN_RATIO) {
                current = new_ratio_val;
            }
        }
    }
    current
}

fn has_full_block_indices(
    block_tx_count: usize,
    indices: impl ExactSizeIterator<Item = u64>,
) -> bool {
    indices.len() == block_tx_count &&
        indices.enumerate().all(|(idx, input_index)| input_index == idx as u64)
}

fn build_token_ratio_prefixes_from_logs<'a>(
    initial_ratio: U256,
    logs_by_receipt: impl IntoIterator<Item = &'a [alloy_primitives::Log]>,
) -> Vec<U256> {
    let mut ratio_before = vec![initial_ratio];
    let mut current = initial_ratio;
    for logs in logs_by_receipt {
        current = token_ratio_after_logs(current, logs);
        ratio_before.push(current);
    }
    ratio_before
}

fn get_or_insert_token_ratio_prefix(
    cache: &TokenRatioPrefixCache,
    block_hash: B256,
    build: impl FnOnce() -> Option<Arc<Vec<U256>>>,
) -> Option<Arc<Vec<U256>>> {
    if let Some(cached) = cache.lock().get(&block_hash).cloned() {
        return Some(cached);
    }
    let computed = build()?;
    cache.lock().insert(block_hash, computed.clone());
    Some(computed)
}

impl<N, Rpc> LoadReceipt for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError>,
{
}

/// Converter for OP receipts.
#[derive(Debug, Clone)]
pub struct OpReceiptConverter<Provider> {
    provider: Provider,
    /// Whether SDM is explicitly enabled for integration tests.
    sdm_enabled: bool,
    /// [MANTLE] LRU cache of per-tx `token_ratio` prefix arrays, keyed by block hash.
    token_ratio_prefix_cache: TokenRatioPrefixCache,
}

impl<Provider> OpReceiptConverter<Provider> {
    /// Creates a new [`OpReceiptConverter`].
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            sdm_enabled: false,
            token_ratio_prefix_cache: Arc::new(Mutex::new(LruMap::new(ByLength::new(
                TOKEN_RATIO_PREFIX_CACHE_MAX_BLOCKS,
            )))),
        }
    }

    /// Configures the temporary SDM integration-test override.
    #[must_use]
    pub const fn with_sdm_enabled(mut self, sdm_enabled: bool) -> Self {
        self.sdm_enabled = sdm_enabled;
        self
    }
}

impl<Provider, N> ReceiptConverter<N> for OpReceiptConverter<Provider>
where
    N: NodePrimitives<SignedTx: OpTransaction, Receipt = OpReceipt>,
    Provider: BlockReader<Block = N::Block>
        + ChainSpecProvider<ChainSpec: OpHardforks + EthChainSpec>
        + ReceiptProvider<Receipt: ReceiptTrait>
        + StateProviderFactory
        + Debug
        + 'static,
{
    type RpcReceipt = OpTransactionReceipt;
    type Error = OpEthApiError;

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, N>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let Some(block_number) = inputs.first().map(|r| r.meta.block_number) else {
            return Ok(Vec::new());
        };

        let block = self
            .provider
            .block_by_number(block_number)?
            .ok_or(EthApiError::HeaderNotFound(block_number.into()))?;

        self.convert_receipts_with_block(inputs, &SealedBlock::new_unhashed(block))
    }

    fn convert_receipts_with_block(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, N>>,
        block: &SealedBlock<N::Block>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let mut l1_block_info = match reth_optimism_evm::extract_l1_info(block.body()) {
            Ok(l1_block_info) => l1_block_info,
            Err(err) => {
                let genesis_number =
                    self.provider.chain_spec().genesis().number.unwrap_or_default();
                if block.header().number() == genesis_number {
                    return Ok(vec![]);
                }
                return Err(err.into());
            }
        };

        // [MANTLE] token_ratio is not in the L1 info calldata; read from GAS_ORACLE_CONTRACT
        // at the parent block state (= start of this block, before any tx runs).
        let is_mantle = self.provider.chain_spec().is_mantle();
        let mut token_ratio = U256::ZERO;
        if is_mantle &&
            let Ok(state) = self.provider.state_by_block_hash(block.header().parent_hash()) &&
            let Ok(Some(ratio)) =
                state.storage(GAS_ORACLE_CONTRACT, op_revm::constants::TOKEN_RATIO_SLOT.into())
        {
            token_ratio = ratio;
            l1_block_info.token_ratio = ratio;
        }

        // [MANTLE] For single-receipt requests (eth_getTransactionReceipt), precompute
        // per-tx token_ratio prefix from all block receipts so we account for any
        // TokenRatioUpdated events emitted by earlier txs in the same block.
        let block_tx_count = block.body().transactions().len();
        let block_hash = B256::from(*block.hash());
        let has_full_block_inputs =
            has_full_block_indices(block_tx_count, inputs.iter().map(|input| input.meta.index));

        let token_ratio_before_tx: Option<Arc<Vec<U256>>> = if !is_mantle || has_full_block_inputs {
            None
        } else {
            get_or_insert_token_ratio_prefix(&self.token_ratio_prefix_cache, block_hash, || {
                self.provider
                    .receipts_by_block(BlockHashOrNumber::Hash(block_hash))
                    .ok()
                    .flatten()
                    .filter(|all_receipts| all_receipts.len() == block_tx_count)
                    .map(|all_receipts| {
                        Arc::new(build_token_ratio_prefixes_from_logs(
                            token_ratio,
                            all_receipts.iter().map(|receipt| receipt.logs()),
                        ))
                    })
            })
        };

        let mut receipts = Vec::with_capacity(inputs.len());
        let post_exec_payload = parse_post_exec_payload_from_transactions(
            block.body().transactions(),
            block.header().number(),
            self.sdm_enabled,
        )?
        .map(|parsed| parsed.payload);

        for input in inputs {
            // [MANTLE] Set per-tx token_ratio before computing L1 fee
            if is_mantle {
                let ratio_for_this_tx = token_ratio_before_tx
                    .as_ref()
                    .and_then(|rb| rb.get(input.meta.index as usize).copied())
                    .unwrap_or(token_ratio);
                l1_block_info.token_ratio = ratio_for_this_tx;

                if token_ratio_before_tx.is_none() {
                    token_ratio = token_ratio_after_logs(token_ratio, input.receipt.logs());
                }
            }

            l1_block_info.clear_tx_l1_cost();

            let op_gas_refund = post_exec_payload
                .as_ref()
                .and_then(|payload| payload.gas_refund_for_idx(input.meta.index));

            receipts.push(
                OpReceiptBuilder::new(
                    &self.provider.chain_spec(),
                    input,
                    &mut l1_block_info,
                    op_gas_refund,
                )?
                .build(),
            );
        }

        Ok(receipts)
    }
}

/// L1 fee and data gas for a non-deposit transaction, or deposit nonce and receipt version for a
/// deposit transaction.
#[derive(Debug, Clone)]
pub struct OpReceiptFieldsBuilder {
    /// Block number.
    pub block_number: u64,
    /// Block timestamp.
    pub block_timestamp: u64,
    /// The L1 fee for transaction.
    pub l1_fee: Option<u128>,
    /// L1 gas used by transaction.
    pub l1_data_gas: Option<u128>,
    /// L1 fee scalar.
    pub l1_fee_scalar: Option<f64>,
    /* ---------------------------------------- Bedrock ---------------------------------------- */
    /// The base fee of the L1 origin block.
    pub l1_base_fee: Option<u128>,
    /// Post-exec block-level warming refund for this transaction.
    pub op_gas_refund: Option<u64>,
    /* --------------------------------------- Regolith ---------------------------------------- */
    /// Deposit nonce, if this is a deposit transaction.
    pub deposit_nonce: Option<u64>,
    /* ---------------------------------------- Canyon ----------------------------------------- */
    /// Deposit receipt version, if this is a deposit transaction.
    pub deposit_receipt_version: Option<u64>,
    /* ---------------------------------------- Ecotone ---------------------------------------- */
    /// The current L1 fee scalar.
    pub l1_base_fee_scalar: Option<u128>,
    /// The current L1 blob base fee.
    pub l1_blob_base_fee: Option<u128>,
    /// The current L1 blob base fee scalar.
    pub l1_blob_base_fee_scalar: Option<u128>,
    /* ---------------------------------------- Isthmus ---------------------------------------- */
    /// The current operator fee scalar.
    pub operator_fee_scalar: Option<u128>,
    /// The current L1 blob base fee scalar.
    pub operator_fee_constant: Option<u128>,
    /* ---------------------------------------- Jovian ----------------------------------------- */
    /// The current DA footprint gas scalar.
    pub da_footprint_gas_scalar: Option<u16>,
    /* ---------------------------------------- Mantle ---------------------------------------- */
    /// The MNT/ETH token ratio from `L1BlockInfo`.
    pub token_ratio: Option<u128>,
}

impl OpReceiptFieldsBuilder {
    /// Returns a new builder.
    pub const fn new(block_timestamp: u64, block_number: u64) -> Self {
        Self {
            block_number,
            block_timestamp,
            l1_fee: None,
            l1_data_gas: None,
            l1_fee_scalar: None,
            l1_base_fee: None,
            op_gas_refund: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
            l1_base_fee_scalar: None,
            l1_blob_base_fee: None,
            l1_blob_base_fee_scalar: None,
            operator_fee_scalar: None,
            operator_fee_constant: None,
            da_footprint_gas_scalar: None,
            token_ratio: None,
        }
    }

    /// Applies [`L1BlockInfo`](op_revm::L1BlockInfo).
    pub fn l1_block_info<T: Encodable2718 + OpTransaction>(
        mut self,
        chain_spec: &(impl OpHardforks + EthChainSpec),
        tx: &T,
        l1_block_info: &mut op_revm::L1BlockInfo,
    ) -> Result<Self, OpEthApiError> {
        let raw_tx = tx.encoded_2718();
        let timestamp = self.block_timestamp;

        self.l1_fee = Some(
            l1_block_info
                .l1_tx_data_fee(chain_spec, timestamp, &raw_tx, tx.is_deposit())
                .map_err(|_| OpEthApiError::L1BlockFeeError)?
                .saturating_to(),
        );

        self.l1_data_gas = Some(
            l1_block_info
                .l1_data_gas(chain_spec, timestamp, &raw_tx)
                .map_err(|_| OpEthApiError::L1BlockGasError)?
                .saturating_add(l1_block_info.l1_fee_overhead.unwrap_or_default())
                .saturating_to(),
        );

        self.l1_fee_scalar = (!chain_spec.is_ecotone_active_at_timestamp(timestamp))
            .then_some(f64::from(l1_block_info.l1_base_fee_scalar) / 1_000_000.0);

        self.l1_base_fee = Some(l1_block_info.l1_base_fee.saturating_to());
        self.l1_base_fee_scalar = Some(l1_block_info.l1_base_fee_scalar.saturating_to());
        self.l1_blob_base_fee = l1_block_info.l1_blob_base_fee.map(|fee| fee.saturating_to());
        self.l1_blob_base_fee_scalar =
            l1_block_info.l1_blob_base_fee_scalar.map(|scalar| scalar.saturating_to());

        // If the operator fee params are both set to 0, we don't add them to the receipt.
        let operator_fee_scalar_has_non_zero_value: bool =
            l1_block_info.operator_fee_scalar.is_some_and(|scalar| !scalar.is_zero());

        let operator_fee_constant_has_non_zero_value =
            l1_block_info.operator_fee_constant.is_some_and(|constant| !constant.is_zero());

        if operator_fee_scalar_has_non_zero_value || operator_fee_constant_has_non_zero_value {
            self.operator_fee_scalar =
                l1_block_info.operator_fee_scalar.map(|scalar| scalar.saturating_to());
            self.operator_fee_constant =
                l1_block_info.operator_fee_constant.map(|constant| constant.saturating_to());
        }

        self.da_footprint_gas_scalar = l1_block_info.da_footprint_gas_scalar;

        let ratio: u128 = l1_block_info.token_ratio.saturating_to();
        if ratio > 0 {
            self.token_ratio = Some(ratio);
        }

        Ok(self)
    }

    /// Applies post-exec block-level warming refund metadata.
    pub const fn op_gas_refund(mut self, op_gas_refund: Option<u64>) -> Self {
        self.op_gas_refund = op_gas_refund;
        self
    }

    /// Applies deposit transaction metadata: deposit nonce.
    pub const fn deposit_nonce(mut self, nonce: Option<u64>) -> Self {
        self.deposit_nonce = nonce;
        self
    }

    /// Applies deposit transaction metadata: deposit receipt version.
    pub const fn deposit_version(mut self, version: Option<u64>) -> Self {
        self.deposit_receipt_version = version;
        self
    }

    /// Builds the [`OpTransactionReceiptFields`] object.
    pub const fn build(self) -> OpTransactionReceiptFields {
        let Self {
            block_number: _,    // used to compute other fields
            block_timestamp: _, // used to compute other fields
            l1_fee,
            l1_data_gas: l1_gas_used,
            l1_fee_scalar,
            l1_base_fee: l1_gas_price,
            op_gas_refund,
            deposit_nonce,
            deposit_receipt_version,
            l1_base_fee_scalar,
            l1_blob_base_fee,
            l1_blob_base_fee_scalar,
            operator_fee_scalar,
            operator_fee_constant,
            da_footprint_gas_scalar,
            token_ratio,
        } = self;

        OpTransactionReceiptFields {
            l1_block_info: L1BlockInfo {
                l1_gas_price,
                l1_gas_used,
                l1_fee,
                l1_fee_scalar,
                l1_base_fee_scalar,
                l1_blob_base_fee,
                l1_blob_base_fee_scalar,
                operator_fee_scalar,
                operator_fee_constant,
                da_footprint_gas_scalar,
                token_ratio,
            },
            op_gas_refund,
            deposit_nonce,
            deposit_receipt_version,
        }
    }
}

/// Builds an [`OpTransactionReceipt`].
#[derive(Debug)]
pub struct OpReceiptBuilder {
    /// Core receipt, has all the fields of an L1 receipt and is the basis for the OP receipt.
    pub core_receipt: TransactionReceipt<ReceiptWithBloom<OpReceipt<Log>>>,
    /// Additional OP receipt fields.
    pub op_receipt_fields: OpTransactionReceiptFields,
}

impl OpReceiptBuilder {
    /// Returns a new builder.
    pub fn new<N>(
        chain_spec: &(impl OpHardforks + EthChainSpec),
        input: ConvertReceiptInput<'_, N>,
        l1_block_info: &mut op_revm::L1BlockInfo,
        op_gas_refund: Option<u64>,
    ) -> Result<Self, OpEthApiError>
    where
        N: NodePrimitives<SignedTx: OpTransaction, Receipt = OpReceipt>,
    {
        let timestamp = input.meta.timestamp;
        let block_number = input.meta.block_number;
        let tx_signed = *input.tx.inner();
        let mut core_receipt = build_receipt(input, None, |receipt, next_log_index, meta| {
            let map_logs = move |receipt: alloy_consensus::Receipt| {
                let Receipt { status, cumulative_gas_used, logs } = receipt;
                let logs = Log::collect_for_receipt(next_log_index, meta, logs);
                Receipt { status, cumulative_gas_used, logs }
            };
            let mapped_receipt: OpReceipt<Log> = match receipt {
                OpReceipt::Legacy(receipt) => OpReceipt::Legacy(map_logs(receipt)),
                OpReceipt::Eip2930(receipt) => OpReceipt::Eip2930(map_logs(receipt)),
                OpReceipt::Eip1559(receipt) => OpReceipt::Eip1559(map_logs(receipt)),
                OpReceipt::Eip7702(receipt) => OpReceipt::Eip7702(map_logs(receipt)),
                OpReceipt::PostExec(receipt) => OpReceipt::PostExec(map_logs(receipt)),
                OpReceipt::Deposit(receipt) => OpReceipt::Deposit(receipt.map_inner(map_logs)),
            };
            mapped_receipt.into_with_bloom()
        });

        // In jovian, we're using the blob gas used field to store the current da
        // footprint's value.
        // We're computing the jovian blob gas used before building the receipt since the inputs get
        // consumed by the `build_receipt` function.
        chain_spec.is_jovian_active_at_timestamp(timestamp).then(|| {
            // Estimate the size of the transaction in bytes and multiply by the DA
            // footprint gas scalar.
            // Jovian specs: `https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/jovian/exec-engine.md#da-footprint-block-limit`
            let da_size = estimate_tx_compressed_size(tx_signed.encoded_2718().as_slice())
                .saturating_div(1_000_000)
                .saturating_mul(l1_block_info.da_footprint_gas_scalar.unwrap_or_default().into());

            core_receipt.blob_gas_used = Some(da_size);
        });

        // op-geth parity: `build_receipt` derives `contract_address` from `tx.nonce()`,
        // which is hard-coded to `0` for deposit transactions. For a deposit
        // contract-creation tx (`to == nil`, `isCreation == true`), op-geth instead derives
        // the address from the sender's *actual* L2 nonce, persisted on the receipt as the
        // deposit nonce (see go-ethereum `core/types/receipt.go` `DeriveFields`:
        // `if tx.IsDepositTx() && r.DepositNonce != nil { nonce = *r.DepositNonce }`).
        //
        // Without this override `eth_getTransactionReceipt` returns `CREATE(from, 0)` while the
        // contract is actually deployed at `CREATE(from, depositNonce)`, so any client that
        // calls `eth_getCode(receipt.contractAddress)` fails whenever the deposit sender's L2
        // nonce > 0. The contract itself (and chain state) is correct; only this derived RPC
        // field was wrong. The `contract_address.is_some()` guard limits this to creation txs.
        if core_receipt.contract_address.is_some() &&
            let OpReceipt::Deposit(deposit_receipt) = &core_receipt.inner.receipt &&
            let Some(deposit_nonce) = deposit_receipt.deposit_nonce
        {
            core_receipt.contract_address = Some(core_receipt.from.create(deposit_nonce));
        }

        let op_receipt_fields = OpReceiptFieldsBuilder::new(timestamp, block_number)
            .l1_block_info(chain_spec, tx_signed, l1_block_info)?
            .op_gas_refund(op_gas_refund)
            .build();

        Ok(Self { core_receipt, op_receipt_fields })
    }

    /// Builds [`OpTransactionReceipt`] by combining core (l1) receipt fields and additional OP
    /// receipt fields.
    pub fn build(self) -> OpTransactionReceipt {
        let Self { core_receipt: inner, op_receipt_fields } = self;

        let OpTransactionReceiptFields {
            l1_block_info,
            op_gas_refund,
            deposit_nonce: _,
            deposit_receipt_version: _,
        } = op_receipt_fields;

        OpTransactionReceipt { inner, l1_block_info, op_gas_refund }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_consensus::{
        Block, BlockBody, Eip658Value, Header, Receipt, Sealable, SignableTransaction, TxEip7702,
        transaction::TransactionMeta,
    };
    use alloy_op_hardforks::{OP_MAINNET_ISTHMUS_TIMESTAMP, OP_MAINNET_JOVIAN_TIMESTAMP};
    use alloy_primitives::{Address, B256, Bytes, Signature, U256, hex};
    use op_alloy_consensus::{OpTypedTransaction, SDMGasEntry, TxDeposit, build_post_exec_tx};
    use op_alloy_network::eip2718::Decodable2718;
    use reth_optimism_chainspec::OP_MAINNET;
    use reth_optimism_forks::OpHardforks;
    use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};
    use reth_primitives_traits::{Recovered, SealedBlock};

    /// Build a Mantle-compatible `OpChainSpec` for tests without importing `mantle-reth-chainspec`.
    ///
    /// All Mantle hardforks (Skadi, Limb, Arsia) and their bundled OP hardforks are activated
    /// at timestamp 0, so every test block is post-Arsia.
    fn mantle_test_chain_spec() -> std::sync::Arc<reth_optimism_chainspec::OpChainSpec> {
        use alloy_hardforks::{ForkCondition, Hardfork};
        use alloy_op_hardforks::{MantleHardfork, OpHardfork};
        use reth_chainspec::{
            BaseFeeParams, BaseFeeParamsKind, ChainHardforks, ChainSpec, EthereumHardfork,
        };
        use reth_optimism_chainspec::OpChainSpec;

        let hardforks = ChainHardforks::new(vec![
            (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Paris.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
            (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
            (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(0)),
            (MantleHardfork::Skadi.boxed(), ForkCondition::Timestamp(0)),
            (MantleHardfork::Limb.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(0)),
            (OpHardfork::Jovian.boxed(), ForkCondition::Timestamp(0)),
            (MantleHardfork::Arsia.boxed(), ForkCondition::Timestamp(0)),
        ]);

        std::sync::Arc::new(OpChainSpec {
            inner: ChainSpec {
                chain: 5000u64.into(),
                hardforks,
                base_fee_params: BaseFeeParamsKind::Variable(
                    vec![(MantleHardfork::Arsia.boxed(), BaseFeeParams::new(8, 2))].into(),
                ),
                ..Default::default()
            },
        })
    }

    /// Construct an [`OpTransactionSigned`] deposit transaction with the given calldata.
    ///
    /// Used to avoid [`op_alloy_network::eip2718::Decodable2718`] failures caused by Mantle's
    /// extra [`TxDeposit`] fields (`eth_value`, `eth_tx_value`) when the encoded bytes were
    /// produced by an upstream (non-Mantle) node.
    fn deposit_tx_with_calldata(calldata: &[u8]) -> OpTransactionSigned {
        TxDeposit {
            source_hash: B256::ZERO,
            from: Address::ZERO,
            to: Address::ZERO.into(),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::copy_from_slice(calldata),
            eth_tx_value: None,
        }
        .into()
    }

    /// OP Mainnet transaction at index 0 in block 124665056.
    ///
    /// Only the calldata (`input`) is stored here; the full deposit envelope cannot be
    /// decoded with our Mantle-extended [`TxDeposit`] (extra `eth_value`/`eth_tx_value`
    /// fields). Use [`deposit_tx_with_calldata`] to construct the tx.
    ///
    /// <https://optimistic.etherscan.io/tx/0x312e290cf36df704a2217b015d6455396830b0ce678b860ebfcc30f41403d7b1>
    const TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056_INPUT: [u8; 164] = hex!(
        "440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
    );

    /// OP Mainnet transaction at index 1 in block 124665056 (EIP-1559).
    ///
    /// <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
    const TX_1_OP_MAINNET_BLOCK_124665056: [u8; 1176] = hex!(
        "02f904940a8303fba78401d6d2798401db2b6d830493e0943e6f4f7866654c18f536170780344aa8772950b680b904246a761202000000000000000000000000087000a300de7200382b55d40045000000e5d60e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000022482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000120000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000044095ea7b300000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c0000000000000000000000000000000000000000000000049b9ca9a6943400000000000000000000000000000000000000000000000000000000000000000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000024b6b55f250000000000000000000000000000000000000000000000049b9ca9a694340000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000415ec214a3950bea839a7e6fbb0ba1540ac2076acd50820e2d5ef83d0902cdffb24a47aff7de5190290769c4f0a9c6fabf63012986a0d590b1b571547a8c7050ea1b00000000000000000000000000000000000000000000000000000000000000c080a06db770e6e25a617fe9652f0958bd9bd6e49281a53036906386ed39ec48eadf63a07f47cf51a4a40b4494cf26efc686709a9b03939e20ee27e59682f5faa536667e"
    );

    /// Timestamp of OP mainnet block 124665056.
    ///
    /// <https://optimistic.etherscan.io/block/124665056>
    const BLOCK_124665056_TIMESTAMP: u64 = 1724928889;

    /// L1 block info for transaction at index 1 in block 124665056.
    ///
    /// Note: `l1_gas_used` and `l1_fee` differ from OP Mainnet explorer values because Mantle's
    /// `op-revm` fork routes Fjord-era OP transactions through the pre-Arsia (Bedrock-style)
    /// formula and requires `token_ratio` to be set explicitly for non-Mantle chains (default
    /// is 0 with `#[derive(Default)]`).  The values below were computed with `token_ratio = 1`.
    ///
    /// <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
    const TX_META_TX_1_OP_MAINNET_BLOCK_124665056: OpTransactionReceiptFields =
        OpTransactionReceiptFields {
            l1_block_info: L1BlockInfo {
                l1_gas_price: Some(1055991687), // since bedrock l1 base fee
                l1_gas_used: Some(8316),
                l1_fee: Some(45901563644),
                l1_fee_scalar: None,
                l1_base_fee_scalar: Some(5227),
                l1_blob_base_fee: Some(1),
                l1_blob_base_fee_scalar: Some(1014213),
                operator_fee_scalar: None,
                operator_fee_constant: None,
                da_footprint_gas_scalar: None,
                token_ratio: None,
            },
            op_gas_refund: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
        };

    #[test]
    fn op_receipt_fields_from_block_and_tx() {
        // rig
        let tx_0 = deposit_tx_with_calldata(&TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056_INPUT);

        let tx_1 =
            OpTransactionSigned::decode_2718(&mut TX_1_OP_MAINNET_BLOCK_124665056.as_slice())
                .unwrap();

        let block: Block<OpTransactionSigned> = Block {
            body: BlockBody { transactions: [tx_0, tx_1.clone()].to_vec(), ..Default::default() },
            ..Default::default()
        };

        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("should extract l1 info");
        // token_ratio is a Mantle extension; for OP Mainnet it is always 1.
        l1_block_info.token_ratio = U256::from(1);

        // test
        assert!(OP_MAINNET.is_fjord_active_at_timestamp(BLOCK_124665056_TIMESTAMP));

        let receipt_meta = OpReceiptFieldsBuilder::new(BLOCK_124665056_TIMESTAMP, 124665056)
            .l1_block_info(&*OP_MAINNET, &tx_1, &mut l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        let L1BlockInfo {
            l1_gas_price,
            l1_gas_used,
            l1_fee,
            l1_fee_scalar,
            l1_base_fee_scalar,
            l1_blob_base_fee,
            l1_blob_base_fee_scalar,
            operator_fee_scalar,
            operator_fee_constant,
            da_footprint_gas_scalar,
            token_ratio: _,
        } = receipt_meta.l1_block_info;

        assert_eq!(
            l1_gas_price, TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_gas_price,
            "incorrect l1 base fee (former gas price)"
        );
        assert_eq!(
            l1_gas_used, TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_gas_used,
            "incorrect l1 gas used"
        );
        assert_eq!(
            l1_fee, TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_fee,
            "incorrect l1 fee"
        );
        assert_eq!(
            l1_fee_scalar, TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_fee_scalar,
            "incorrect l1 fee scalar"
        );
        assert_eq!(
            l1_base_fee_scalar,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_base_fee_scalar,
            "incorrect l1 base fee scalar"
        );
        assert_eq!(
            l1_blob_base_fee,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_blob_base_fee,
            "incorrect l1 blob base fee"
        );
        assert_eq!(
            l1_blob_base_fee_scalar,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_blob_base_fee_scalar,
            "incorrect l1 blob base fee scalar"
        );
        assert_eq!(
            operator_fee_scalar,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.operator_fee_scalar,
            "incorrect operator fee scalar"
        );
        assert_eq!(
            operator_fee_constant,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.operator_fee_constant,
            "incorrect operator fee constant"
        );
        assert_eq!(
            da_footprint_gas_scalar,
            TX_META_TX_1_OP_MAINNET_BLOCK_124665056.l1_block_info.da_footprint_gas_scalar,
            "incorrect da footprint gas scalar"
        );
    }

    /// Mantle Mainnet deposit tx (index 0) in block 95483648.
    /// Uses Arsia selector `49e72383` (Jovian calldata layout).
    ///
    /// <https://explorer.mantle.xyz/tx/0x0e8f8f4d56d150bc5e30751ce245fb0b0db97ec73b366fc1a59bebb64b1421b5>
    const TX_SET_L1_BLOCK_MANTLE_BLOCK_95483648: [u8; 267] = hex!(
        "7ef90107a0ad9ed1416f03f55454c736fb50459359b5c059e9c7c5dbc5cfa36427f7c03dad94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f42408080b8b249e723830002943b000000000000000000000002000000006a0adfd700000000017f51ff0000000000000000000000000000000000000000000000000000000008b39d5a000000000000000000000000000000000000000000000000003cb55453060882b6c48a1019b05408945ea0657d8e2c880bfcd2ca190cb2033967e066408d9dea0000000000000000000000002f40d796917ffb642bd2e2bdd2c762a5e40fd74905f5e10000000000000000000190"
    );

    /// Mantle Mainnet legacy tx (index 1) in block 95483648.
    ///
    /// <https://explorer.mantle.xyz/tx/0x56d9524c4e20f863e4f1f6a780c3b963ef4149bbbc6f8d496533a6cb5e6e56ab>
    const TX_1_MANTLE_BLOCK_95483648: [u8; 284] = hex!(
        "f901198312e326850ba6f3c417832625a09488a8984f2b8507bbc1c699594e3a4ecdefed47848a02b111097614c86f1effb8a457b8249256d865729f3f4bfab79878291e948ee50aa31f4f2a85d7c0c631ce9c8d0ab20c56d865729f3f4bfab79878291e948ee50aa31f4f2a85d571d738b88845e5ac4b56d865729f3f4bfab79878291e948ee50aa31f4f2a85d7c0606f303613e4b6a356d865729f3f4bfab79878291e948ee50aa31f4f2a85d7c0aeda68a1f16059ee56d865729f3f4bfab79878291e948ee50aa31f4f2a85d5195776fa0c0c6a05aa822733a0c62f8997cc96dad21d707eb6c44987e9b12ee486a579bd95168cd3002696f583a02ffc25180f1a6f078506afea340d259a9d28f1e08f1e594d0cb902e355779503"
    );

    /// Timestamp of Mantle mainnet block 95483648.
    const BLOCK_95483648_TIMESTAMP: u64 = 1779097608;

    /// `token_ratio` at block 95483648, read from `GAS_ORACLE_CONTRACT` (0x420...00F) slot 0.
    const MANTLE_BLOCK_95483648_TOKEN_RATIO: u64 = 3358;

    /// Expected receipt fields for tx at index 1 in Mantle block 95483648.
    /// Values verified against Mantle explorer.
    const TX_META_TX_1_MANTLE_BLOCK_95483648: OpTransactionReceiptFields =
        OpTransactionReceiptFields {
            l1_block_info: L1BlockInfo {
                l1_gas_price: Some(145988954),
                l1_gas_used: Some(2196),
                l1_fee: Some(181972685946422),
                l1_fee_scalar: None,
                l1_base_fee_scalar: Some(169019),
                l1_blob_base_fee: Some(17087872377424002),
                l1_blob_base_fee_scalar: Some(0),
                operator_fee_scalar: Some(100000000),
                operator_fee_constant: Some(0),
                da_footprint_gas_scalar: Some(400),
                token_ratio: Some(3358),
            },
            op_gas_refund: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
        };

    #[test]
    fn mantle_receipt_fields_from_block_and_tx() {
        let tx_0 =
            OpTransactionSigned::decode_2718(&mut TX_SET_L1_BLOCK_MANTLE_BLOCK_95483648.as_slice())
                .unwrap();

        let tx_1 =
            OpTransactionSigned::decode_2718(&mut TX_1_MANTLE_BLOCK_95483648.as_slice()).unwrap();

        let block: Block<OpTransactionSigned> = Block {
            body: BlockBody { transactions: [tx_0, tx_1.clone()].to_vec(), ..Default::default() },
            ..Default::default()
        };

        let mut l1_block_info =
            reth_optimism_evm::extract_l1_info(&block.body).expect("should extract l1 info");
        l1_block_info.token_ratio = U256::from(MANTLE_BLOCK_95483648_TOKEN_RATIO);

        let mantle_spec = mantle_test_chain_spec();
        assert!(mantle_spec.is_mantle_arsia_active_at_timestamp(BLOCK_95483648_TIMESTAMP));

        let receipt_meta = OpReceiptFieldsBuilder::new(BLOCK_95483648_TIMESTAMP, 95483648)
            .l1_block_info(&*mantle_spec, &tx_1, &mut l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        let expected = &TX_META_TX_1_MANTLE_BLOCK_95483648.l1_block_info;
        let actual = &receipt_meta.l1_block_info;

        assert_eq!(actual.l1_gas_price, expected.l1_gas_price, "incorrect l1 base fee");
        assert_eq!(actual.l1_gas_used, expected.l1_gas_used, "incorrect l1 gas used");
        assert_eq!(actual.l1_fee, expected.l1_fee, "incorrect l1 fee");
        assert_eq!(actual.l1_fee_scalar, expected.l1_fee_scalar, "incorrect l1 fee scalar");
        assert_eq!(
            actual.l1_base_fee_scalar, expected.l1_base_fee_scalar,
            "incorrect l1 base fee scalar"
        );
        assert_eq!(
            actual.l1_blob_base_fee, expected.l1_blob_base_fee,
            "incorrect l1 blob base fee"
        );
        assert_eq!(
            actual.l1_blob_base_fee_scalar, expected.l1_blob_base_fee_scalar,
            "incorrect l1 blob base fee scalar"
        );
        assert_eq!(
            actual.operator_fee_scalar, expected.operator_fee_scalar,
            "incorrect operator fee scalar"
        );
        assert_eq!(
            actual.operator_fee_constant, expected.operator_fee_constant,
            "incorrect operator fee constant"
        );
        assert_eq!(
            actual.da_footprint_gas_scalar, expected.da_footprint_gas_scalar,
            "incorrect da footprint gas scalar"
        );
    }

    #[test]
    fn convert_receipts_extracts_post_exec_gas_refund_from_embedded_payload() {
        let tx_0 =
            OpTransactionSigned::decode_2718(&mut TX_SET_L1_BLOCK_MANTLE_BLOCK_95483648.as_slice())
                .unwrap();
        let tx_1 =
            OpTransactionSigned::decode_2718(&mut TX_1_MANTLE_BLOCK_95483648.as_slice()).unwrap();
        let post_exec = OpTransactionSigned::PostExec(
            build_post_exec_tx(95483648, vec![SDMGasEntry { index: 1, gas_refund: 77 }])
                .seal_slow(),
        );

        let block = SealedBlock::new_unhashed(Block::<OpTransactionSigned> {
            header: Header {
                number: 95483648,
                timestamp: BLOCK_95483648_TIMESTAMP,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![tx_0, tx_1.clone(), post_exec],
                ..Default::default()
            },
        });

        let converter = OpReceiptConverter::new(reth_storage_api::noop::NoopProvider::<
            _,
            OpPrimitives,
        >::new(mantle_test_chain_spec()))
        .with_sdm_enabled(true);
        let receipts =
            <OpReceiptConverter<_> as ReceiptConverter<OpPrimitives>>::convert_receipts_with_block(
                &converter,
                vec![ConvertReceiptInput::<OpPrimitives> {
                    tx: Recovered::new_unchecked(&tx_1, Address::ZERO),
                    receipt: OpReceipt::Legacy(Receipt {
                        status: Eip658Value::Eip658(true),
                        cumulative_gas_used: 100,
                        logs: vec![],
                    }),
                    gas_used: 100,
                    next_log_index: 0,
                    meta: TransactionMeta {
                        index: 1,
                        block_number: 95483648,
                        timestamp: BLOCK_95483648_TIMESTAMP,
                        ..Default::default()
                    },
                }],
                &block,
            )
            .unwrap();

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].op_gas_refund, Some(77));
    }

    #[test]
    fn op_non_zero_operator_fee_params_included_in_receipt() {
        let tx_1 =
            OpTransactionSigned::decode_2718(&mut TX_1_MANTLE_BLOCK_95483648.as_slice()).unwrap();

        let mut l1_block_info = op_revm::L1BlockInfo {
            operator_fee_scalar: Some(U256::ZERO),
            operator_fee_constant: Some(U256::from(2)),
            token_ratio: U256::from(1),
            ..Default::default()
        };

        let mantle_spec = mantle_test_chain_spec();
        let receipt_meta = OpReceiptFieldsBuilder::new(BLOCK_95483648_TIMESTAMP, 95483648)
            .l1_block_info(&*mantle_spec, &tx_1, &mut l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        let L1BlockInfo { operator_fee_scalar, operator_fee_constant, .. } =
            receipt_meta.l1_block_info;

        assert_eq!(operator_fee_scalar, Some(0), "incorrect operator fee scalar");
        assert_eq!(operator_fee_constant, Some(2), "incorrect operator fee constant");
    }

    #[test]
    fn op_zero_operator_fee_params_not_included_in_receipt() {
        let tx_1 =
            OpTransactionSigned::decode_2718(&mut TX_1_MANTLE_BLOCK_95483648.as_slice()).unwrap();

        let mut l1_block_info = op_revm::L1BlockInfo {
            operator_fee_scalar: Some(U256::ZERO),
            operator_fee_constant: Some(U256::ZERO),
            token_ratio: U256::from(1),
            ..Default::default()
        };

        let mantle_spec = mantle_test_chain_spec();
        let receipt_meta = OpReceiptFieldsBuilder::new(BLOCK_95483648_TIMESTAMP, 95483648)
            .l1_block_info(&*mantle_spec, &tx_1, &mut l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        let L1BlockInfo { operator_fee_scalar, operator_fee_constant, .. } =
            receipt_meta.l1_block_info;

        assert_eq!(operator_fee_scalar, None, "incorrect operator fee scalar");
        assert_eq!(operator_fee_constant, None, "incorrect operator fee constant");
    }

    #[test]
    fn da_footprint_gas_scalar_included_in_receipt_post_jovian() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 10;

        let tx = TxEip7702 {
            chain_id: 1u64,
            nonce: 0,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            gas_limit: 10,
            to: Address::default(),
            value: U256::from(3_u64),
            input: Bytes::from(vec![1, 2]),
            access_list: Default::default(),
            authorization_list: Default::default(),
        };

        let signature = Signature::new(U256::default(), U256::default(), true);

        let tx: OpTransactionSigned = OpTypedTransaction::Eip7702(tx).into_signed(signature).into();

        let mut l1_block_info = op_revm::L1BlockInfo {
            da_footprint_gas_scalar: Some(DA_FOOTPRINT_GAS_SCALAR),
            ..Default::default()
        };

        let op_hardforks = OP_MAINNET.as_ref();

        let receipt = OpReceiptFieldsBuilder::new(OP_MAINNET_JOVIAN_TIMESTAMP, u64::MAX)
            .l1_block_info(&op_hardforks, &tx, &mut l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        assert_eq!(receipt.l1_block_info.da_footprint_gas_scalar, Some(DA_FOOTPRINT_GAS_SCALAR));
    }

    #[test]
    fn blob_gas_used_included_in_receipt_post_jovian() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 100;
        let tx = TxEip7702 {
            chain_id: 1u64,
            nonce: 0,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            gas_limit: 10,
            to: Address::default(),
            value: U256::from(3_u64),
            access_list: Default::default(),
            authorization_list: Default::default(),
            input: Bytes::from(vec![0; 1_000_000]),
        };

        let signature = Signature::new(U256::default(), U256::default(), true);

        let tx: OpTransactionSigned = OpTypedTransaction::Eip7702(tx).into_signed(signature).into();

        let mut l1_block_info = op_revm::L1BlockInfo {
            da_footprint_gas_scalar: Some(DA_FOOTPRINT_GAS_SCALAR),
            ..Default::default()
        };

        let op_hardforks = OP_MAINNET.as_ref();

        let op_receipt = OpReceiptBuilder::new(
            &op_hardforks,
            ConvertReceiptInput::<OpPrimitives> {
                tx: Recovered::new_unchecked(&tx, Address::default()),
                receipt: OpReceipt::Eip7702(Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 100,
                    logs: vec![],
                }),
                gas_used: 100,
                next_log_index: 0,
                meta: TransactionMeta {
                    timestamp: OP_MAINNET_JOVIAN_TIMESTAMP,
                    ..Default::default()
                },
            },
            &mut l1_block_info,
            None,
        )
        .unwrap();

        let expected_blob_gas_used = estimate_tx_compressed_size(tx.encoded_2718().as_slice())
            .saturating_div(1_000_000)
            .saturating_mul(DA_FOOTPRINT_GAS_SCALAR.into());

        assert_eq!(op_receipt.core_receipt.blob_gas_used, Some(expected_blob_gas_used));
    }

    #[test]
    fn blob_gas_used_not_included_in_receipt_post_isthmus() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 100;
        let tx = TxEip7702 {
            chain_id: 1u64,
            nonce: 0,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            gas_limit: 10,
            to: Address::default(),
            value: U256::from(3_u64),
            access_list: Default::default(),
            authorization_list: Default::default(),
            input: Bytes::from(vec![0; 1_000_000]),
        };

        let signature = Signature::new(U256::default(), U256::default(), true);

        let tx: OpTransactionSigned = OpTypedTransaction::Eip7702(tx).into_signed(signature).into();

        let mut l1_block_info = op_revm::L1BlockInfo {
            da_footprint_gas_scalar: Some(DA_FOOTPRINT_GAS_SCALAR),
            ..Default::default()
        };

        let op_hardforks = OP_MAINNET.as_ref();

        let op_receipt = OpReceiptBuilder::new(
            &op_hardforks,
            ConvertReceiptInput::<OpPrimitives> {
                tx: Recovered::new_unchecked(&tx, Address::default()),
                receipt: OpReceipt::Eip7702(Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 100,
                    logs: vec![],
                }),
                gas_used: 100,
                next_log_index: 0,
                meta: TransactionMeta {
                    timestamp: OP_MAINNET_ISTHMUS_TIMESTAMP,
                    ..Default::default()
                },
            },
            &mut l1_block_info,
            None,
        )
        .unwrap();

        assert_eq!(op_receipt.core_receipt.blob_gas_used, None);
    }

    /// A deposit contract-creation receipt must derive `contractAddress` from the sender's
    /// real L2 nonce (the deposit nonce), not from the deposit tx nonce which is always 0.
    ///
    /// Regression test for the rpc-moon `eth_getTransactionReceipt` mismatch where
    /// `CREATE(from, 0)` was returned instead of `CREATE(from, depositNonce)`.
    #[test]
    fn deposit_contract_creation_uses_deposit_nonce_for_contract_address() {
        use op_alloy_consensus::OpDepositReceipt;

        const DEPOSIT_NONCE: u64 = 35_964;
        let from = Address::with_last_byte(0x42);

        // Deposit contract creation: `to == TxKind::Create`. Its `nonce()` is hard-coded to 0.
        let tx: OpTransactionSigned = TxDeposit {
            source_hash: B256::ZERO,
            from,
            to: alloy_primitives::TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            // init code: PUSH1 0x42, MSTORE, RETURN 32 bytes
            input: Bytes::from_static(&hex!("604260005260206000f3")),
            eth_tx_value: None,
        }
        .into();

        let mut l1_block_info = op_revm::L1BlockInfo::default();
        let op_hardforks = OP_MAINNET.as_ref();

        let op_receipt = OpReceiptBuilder::new(
            &op_hardforks,
            ConvertReceiptInput::<OpPrimitives> {
                tx: Recovered::new_unchecked(&tx, from),
                receipt: OpReceipt::Deposit(OpDepositReceipt {
                    inner: Receipt {
                        status: Eip658Value::Eip658(true),
                        cumulative_gas_used: 100,
                        logs: vec![],
                    },
                    deposit_nonce: Some(DEPOSIT_NONCE),
                    deposit_receipt_version: Some(1),
                }),
                gas_used: 100,
                next_log_index: 0,
                meta: TransactionMeta {
                    timestamp: OP_MAINNET_ISTHMUS_TIMESTAMP,
                    ..Default::default()
                },
            },
            &mut l1_block_info,
            None,
        )
        .unwrap();

        assert_eq!(
            op_receipt.core_receipt.contract_address,
            Some(from.create(DEPOSIT_NONCE)),
            "deposit contract creation must derive the address from the deposit nonce"
        );
        assert_ne!(
            op_receipt.core_receipt.contract_address,
            Some(from.create(0)),
            "must not derive the address from the deposit tx nonce (0)"
        );
    }

    /// A deposit *call* tx (`to == Call`, not creation) must have no `contractAddress`;
    /// the deposit-nonce override must not fabricate one.
    #[test]
    fn deposit_call_tx_has_no_contract_address() {
        use op_alloy_consensus::OpDepositReceipt;

        let from = Address::with_last_byte(0x11);
        let tx: OpTransactionSigned = TxDeposit {
            source_hash: B256::ZERO,
            from,
            to: Address::with_last_byte(0x99).into(), // TxKind::Call
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::new(),
            eth_tx_value: None,
        }
        .into();

        let mut l1_block_info = op_revm::L1BlockInfo::default();
        let op_hardforks = OP_MAINNET.as_ref();

        let op_receipt = OpReceiptBuilder::new(
            &op_hardforks,
            ConvertReceiptInput::<OpPrimitives> {
                tx: Recovered::new_unchecked(&tx, from),
                receipt: OpReceipt::Deposit(OpDepositReceipt {
                    inner: Receipt {
                        status: Eip658Value::Eip658(true),
                        cumulative_gas_used: 100,
                        logs: vec![],
                    },
                    deposit_nonce: Some(35_964),
                    deposit_receipt_version: Some(1),
                }),
                gas_used: 100,
                next_log_index: 0,
                meta: TransactionMeta {
                    timestamp: OP_MAINNET_ISTHMUS_TIMESTAMP,
                    ..Default::default()
                },
            },
            &mut l1_block_info,
            None,
        )
        .unwrap();

        assert_eq!(
            op_receipt.core_receipt.contract_address, None,
            "a deposit call tx must not have a contract address"
        );
    }

    /// A non-deposit contract creation must keep deriving its address from `tx.nonce()`;
    /// the deposit override must not touch it.
    #[test]
    fn non_deposit_contract_creation_uses_tx_nonce() {
        use alloy_consensus::TxEip1559;

        const NONCE: u64 = 7;
        let from = Address::with_last_byte(0x22);

        let tx_inner = TxEip1559 {
            chain_id: 1,
            nonce: NONCE,
            gas_limit: 1_000_000,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            to: alloy_primitives::TxKind::Create,
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::from_static(&hex!("604260005260206000f3")),
        };
        let signature = Signature::new(U256::default(), U256::default(), true);
        let tx: OpTransactionSigned =
            OpTypedTransaction::Eip1559(tx_inner).into_signed(signature).into();

        let mut l1_block_info = op_revm::L1BlockInfo::default();
        let op_hardforks = OP_MAINNET.as_ref();

        let op_receipt = OpReceiptBuilder::new(
            &op_hardforks,
            ConvertReceiptInput::<OpPrimitives> {
                tx: Recovered::new_unchecked(&tx, from),
                receipt: OpReceipt::Eip1559(Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 100,
                    logs: vec![],
                }),
                gas_used: 100,
                next_log_index: 0,
                meta: TransactionMeta {
                    timestamp: OP_MAINNET_ISTHMUS_TIMESTAMP,
                    ..Default::default()
                },
            },
            &mut l1_block_info,
            None,
        )
        .unwrap();

        assert_eq!(
            op_receipt.core_receipt.contract_address,
            Some(from.create(NONCE)),
            "non-deposit contract creation must keep using the tx nonce"
        );
    }

    #[test]
    fn token_ratio_after_logs_ignores_non_token_ratio_2topic_event() {
        let fake_event_sig = B256::from(U256::from(0xdeadbeefu64).to_be_bytes::<32>());
        let fake_value = B256::from(U256::from(42u64).to_be_bytes::<32>());
        let log = alloy_primitives::Log::new_unchecked(
            GAS_ORACLE_CONTRACT,
            vec![fake_event_sig, fake_value],
            Bytes::new(),
        );

        let initial = U256::from(1_000_000);
        let result = token_ratio_after_logs(initial, &[log]);
        assert_eq!(result, initial, "non-TokenRatioUpdated 2-topic log must not change ratio");
    }

    #[test]
    fn token_ratio_after_logs_accepts_real_event() {
        let topic0 = B256::from(TOKEN_RATIO_UPDATED_TOPIC.to_be_bytes::<32>());
        let prev = B256::from(U256::from(1_000_000u64).to_be_bytes::<32>());
        let new_val = B256::from(U256::from(500_000u64).to_be_bytes::<32>());
        let log = alloy_primitives::Log::new_unchecked(
            GAS_ORACLE_CONTRACT,
            vec![topic0, prev, new_val],
            Bytes::new(),
        );

        let result = token_ratio_after_logs(U256::from(1_000_000), &[log]);
        assert_eq!(result, U256::from(500_000));
    }

    #[test]
    fn token_ratio_after_logs_ignores_unreasonable_ratio() {
        let topic0 = B256::from(TOKEN_RATIO_UPDATED_TOPIC.to_be_bytes::<32>());
        let prev = B256::ZERO;
        let huge = B256::from(U256::from(MAX_REASONABLE_TOKEN_RATIO + 1).to_be_bytes::<32>());
        let log = alloy_primitives::Log::new_unchecked(
            GAS_ORACLE_CONTRACT,
            vec![topic0, prev, huge],
            Bytes::new(),
        );

        let initial = U256::from(1_000_000);
        let result = token_ratio_after_logs(initial, &[log]);
        assert_eq!(result, initial, "unreasonable ratio should be ignored");
    }

    #[test]
    fn token_ratio_after_logs_ignores_other_contract() {
        let topic0 = B256::from(TOKEN_RATIO_UPDATED_TOPIC.to_be_bytes::<32>());
        let prev = B256::ZERO;
        let new_val = B256::from(U256::from(500_000u64).to_be_bytes::<32>());
        let log = alloy_primitives::Log::new_unchecked(
            Address::ZERO,
            vec![topic0, prev, new_val],
            Bytes::new(),
        );

        let initial = U256::from(1_000_000);
        let result = token_ratio_after_logs(initial, &[log]);
        assert_eq!(result, initial, "events from other contracts should be ignored");
    }
}
