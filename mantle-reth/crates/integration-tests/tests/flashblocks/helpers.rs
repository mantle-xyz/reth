//! Shared setup for the flashblock consumer integration tests.
//!
//! The consumer is launched without its websocket subscriber; slices are pushed
//! straight into [`FlashblocksState`] and awaited, so tests sequence on
//! acknowledgements rather than sleeps.

use std::sync::Arc;

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, bytes, hex, keccak256};
use alloy_rpc_types_engine::{PayloadAttributes, PayloadId};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_chainspec::from_mantle_genesis;
use mantle_reth_flashblocks::{FlashblocksAPI, FlashblocksState, PendingBlocks};
use mantle_reth_flashblocks_types::{MantleFlashblockMetadata, MantleFlashblockPayload};
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::{
    OpFlashblockPayloadBase, OpFlashblockPayloadDelta, OpPayloadAttributes,
};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::payload::OpPayloadAttrs;
use reth_optimism_primitives::OpBlock;
use reth_primitives_traits::RecoveredBlock;

/// Genesis timestamp. `PayloadTestContext` starts here and adds one second per
/// block, so canonical block `n` is built at `l2_ts(n)`.
pub const L2_GENESIS_TS: u64 = 1_710_338_135;

/// L2 block timestamp for height `n`.
pub const fn l2_ts(n: u64) -> u64 {
    L2_GENESIS_TS + n
}

/// Inverse of [`l2_ts`]: the height the node is building at `timestamp`.
pub const fn block_number_at(timestamp: u64) -> u64 {
    timestamp - L2_GENESIS_TS
}

/// Builds a slice for `block_number` at `index`.
///
/// `index == 0` carries the base payload; later indices carry only the delta,
/// mirroring the wire format.
#[derive(Debug)]
pub struct FlashblockBuilder {
    payload_id: PayloadId,
    block_number: u64,
    index: u64,
    parent_hash: B256,
    transactions: Vec<Bytes>,
    gas_used: u64,
    prev: mantle_reth_flashblocks_types::FlashblockId,
}

impl FlashblockBuilder {
    /// Starts a slice for the given block and index.
    pub fn new(block_number: u64, index: u64) -> Self {
        Self {
            payload_id: PayloadId::default(),
            block_number,
            index,
            parent_hash: B256::ZERO,
            transactions: Vec::new(),
            gas_used: 0,
            prev: mantle_reth_flashblocks_types::FlashblockId::NO_PREV,
        }
    }

    /// Sets the parent hash carried by the base payload.
    pub const fn parent_hash(mut self, parent_hash: B256) -> Self {
        self.parent_hash = parent_hash;
        self
    }

    /// Links this slice to its predecessor.
    pub const fn prev(mut self, block_number: u64, index: u64) -> Self {
        self.prev = mantle_reth_flashblocks_types::FlashblockId { block_number, index };
        self
    }

    /// Adds encoded transactions to this slice's delta.
    pub fn transactions(mut self, transactions: Vec<Bytes>) -> Self {
        self.transactions = transactions;
        self
    }

    /// Sets the block-cumulative gas used reported by this slice.
    pub const fn gas_used(mut self, gas_used: u64) -> Self {
        self.gas_used = gas_used;
        self
    }

    /// Produces the slice.
    pub fn build(self) -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: self.payload_id,
            index: self.index,
            base: (self.index == 0).then(|| OpFlashblockPayloadBase {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: self.parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: self.block_number,
                gas_limit: 30_000_000,
                timestamp: l2_ts(self.block_number),
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: OpFlashblockPayloadDelta {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                gas_used: self.gas_used,
                block_hash: B256::ZERO,
                transactions: self.transactions,
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: MantleFlashblockMetadata {
                block_number: self.block_number,
                prev_flashblock_id: self.prev,
                ..MantleFlashblockMetadata::default()
            },
        }
    }
}

/// Launches a node and returns a harness over a processor bound to its provider.
///
/// The node context is returned so the caller keeps it alive for the test's
/// duration; dropping it tears the provider down.
macro_rules! launch_flashblocks_node {
    ($max_trailing_depth:expr, $max_leading_depth:expr) => {{
        let (node, _http, _wallet, _chain_id) = mantle_reth_integration_tests::launch_mantle_node!(
            crate::helpers::flashblocks_test_chain_spec(),
            mantle_reth_cli::node::MantleNode::default(),
            crate::helpers::flashblocks_payload_attributes
        )
        .await;

        let state = std::sync::Arc::new(mantle_reth_flashblocks::FlashblocksState::new(
            $max_trailing_depth,
            $max_leading_depth,
            5,
        ));
        state.start(node.inner.provider.clone());

        (crate::helpers::FlashblocksHarness::new(state), node)
    }};
}

pub(crate) use launch_flashblocks_node;

/// Mines one canonical block and returns it as the consumer would see it.
macro_rules! mine_canonical_block {
    ($node:expr) => {{
        let payload = $node.advance_block().await.expect("mine canonical block");
        $node.sync_to(payload.block().hash()).await.expect("settle canonical block");
        reth_provider::BlockReader::recovered_block(
            &$node.inner.provider,
            payload.block().hash().into(),
            reth_provider::TransactionVariant::WithHash,
        )
        .expect("canonical block query")
        .expect("mined block must be canonical")
    }};
}

pub(crate) use mine_canonical_block;

/// Builder for a block's `index == 0` slice, carrying the mandatory
/// L1-attributes deposit.
///
/// Holds exactly the transaction the node forces into canonical `block_number`,
/// so an overlay built from these slices reconciles without a reorg.
pub fn base_slice_builder(block_number: u64) -> FlashblockBuilder {
    FlashblockBuilder::new(block_number, 0).transactions(vec![l1_info_deposit(block_number)])
}

/// [`base_slice_builder`] with no predecessor link.
pub fn base_slice(block_number: u64) -> MantleFlashblockPayload {
    base_slice_builder(block_number).build()
}

/// A deposit the canonical block will never contain.
///
/// Deposits execute without a funded sender, so this stands in for any
/// sequencer transaction that the overlay saw but the canonical block dropped.
pub fn filler_deposit(seed: u64) -> Bytes {
    let mut source = [0xffu8; 32];
    source[24..32].copy_from_slice(&seed.to_be_bytes());
    TxDeposit {
        source_hash: B256::from(source),
        from: Address::ZERO,
        to: TxKind::Call(Address::ZERO),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 100_000,
        is_system_transaction: false,
        input: Bytes::default(),
        eth_value: 0,
        eth_tx_value: None,
    }
    .encoded_2718()
    .into()
}

/// Convenience accessors over a running [`FlashblocksState`].
#[derive(Debug)]
pub struct FlashblocksHarness {
    state: Arc<FlashblocksState>,
}

impl FlashblocksHarness {
    /// Wraps an already-started state.
    pub const fn new(state: Arc<FlashblocksState>) -> Self {
        Self { state }
    }

    /// Pushes a slice and waits for the processor to finish with it.
    pub async fn send(&self, flashblock: MantleFlashblockPayload) {
        self.state
            .process_flashblock_for_testing(flashblock)
            .await
            .expect("flashblock processing should be acknowledged");
    }

    /// Delivers a canonical block and waits for reconciliation to finish.
    pub async fn send_canonical(&self, block: RecoveredBlock<OpBlock>) {
        self.state
            .process_canonical_block_for_testing(block)
            .await
            .expect("canonical block processing should be acknowledged");
    }

    /// Current pending snapshot, if any.
    pub fn pending(&self) -> Option<Arc<PendingBlocks>> {
        self.state.get_pending_blocks().clone()
    }

    /// Shared state handle.
    pub const fn state(&self) -> &Arc<FlashblocksState> {
        &self.state
    }
}

/// Chain id of [`flashblocks_test_chain_spec`].
pub const TEST_CHAIN_ID: u64 = 5003;

/// Predeploy that emits one empty `LOG0` and returns its own balance:
/// `PUSH1 0 PUSH1 0 LOG0 SELFBALANCE PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN`.
///
/// The log gives the overlay an observable event (plain transfers emit none) and
/// the returned balance lets `eth_call` distinguish overlay state from canonical.
pub const LOGGER: Address = address!("0000000000000000000000000000000000000c0d");

/// Sender funded in genesis, used for the overlay's user transactions.
pub fn test_sender() -> Address {
    Wallet::default().inner.address()
}

/// Signs a call from [`test_sender`] for inclusion in a slice.
pub async fn signed_call(nonce: u64, to: Address, value: U256) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(TEST_CHAIN_ID),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(100_000),
        max_fee_per_gas: Some(20_000_000_000),
        max_priority_fee_per_gas: Some(20_000_000_000),
        value: Some(value),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(
        Wallet::default().with_chain_id(TEST_CHAIN_ID).inner.clone(),
        request,
    )
    .await
    .encoded_2718()
    .into()
}

/// Minimal Mantle chain spec for the flashblock suite.
///
/// All OP and Mantle forks are active from genesis so slice replay exercises the
/// same code paths as a live chain.
pub fn flashblocks_test_chain_spec() -> Arc<OpChainSpec> {
    let mut genesis: Genesis = serde_json::from_str(
        r#"{
            "config": {
                "chainId": 5003,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "pragueTime": 0,
                "bedrockBlock": 0,
                "regolithTime": 0,
                "mantleSkadiTime": 0,
                "mantleLimbTime": 0,
                "mantleArsiaTime": 0,
                "terminalTotalDifficulty": 0,
                "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50 }
            },
            "alloc": {},
            "gasLimit": "0x1c9c380",
            "difficulty": "0x0",
            "timestamp": "0x0"
        }"#,
    )
    .expect("valid flashblocks test genesis");
    genesis.alloc.insert(
        test_sender(),
        GenesisAccount { balance: U256::from(10u64).pow(U256::from(20u64)), ..Default::default() },
    );
    genesis.alloc.insert(
        LOGGER,
        GenesisAccount { code: Some(bytes!("60006000a04760005260206000f3")), ..Default::default() },
    );
    Arc::new(from_mantle_genesis(genesis))
}

/// Payload attributes builder for the flashblock suite.
///
/// The forced L1-attributes deposit is the same transaction an `index == 0`
/// slice for that height carries, so an overlay that saw exactly what the
/// sequencer built reconciles against the canonical block without a reorg.
pub fn flashblocks_payload_attributes(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![l1_info_deposit(block_number_at(timestamp))]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(alloy_primitives::B64::ZERO),
        min_base_fee: Some(0),
    })
}

/// `L1Block` predeploy — recipient of the per-block L1-attributes deposit.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

/// Builds the L1-attributes deposit that must lead every block.
///
/// `extract_l1_info` refuses a block without it, so every `index == 0` slice
/// carries this transaction. The Mantle `TxDeposit` layout adds `eth_value` and
/// `eth_tx_value` over the OP one, so Base's hex fixtures cannot be reused.
pub fn l1_info_deposit(origin: u64) -> Bytes {
    // Arsia setL1BlockValues calldata: 4-byte selector + 174-byte payload.
    let mut data = vec![0u8; 178];
    data[0..4].copy_from_slice(&hex!("49e72383"));
    let p = &mut data[4..];
    p[0..4].copy_from_slice(&1_000_000u32.to_be_bytes());
    p[24..32].copy_from_slice(&origin.to_be_bytes());
    p[32..64].copy_from_slice(&U256::from(1_000_000_000u64).to_be_bytes::<32>());
    p[96..128].copy_from_slice(keccak256(origin.to_be_bytes()).as_slice());

    let mut source = [0u8; 32];
    source[24..32].copy_from_slice(&origin.to_be_bytes());
    TxDeposit {
        source_hash: B256::from(source),
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        // Regolith removed system transactions; a `true` here halts the deposit.
        is_system_transaction: false,
        input: data.into(),
        eth_value: 0,
        eth_tx_value: None,
    }
    .encoded_2718()
    .into()
}
