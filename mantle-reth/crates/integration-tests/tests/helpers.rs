//! Shared helpers for Mantle integration tests.

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::payload::OpPayloadAttrs;
use std::sync::Arc;

/// Build a Mantle-flavoured `OpChainSpec` from the test genesis JSON.
///
/// All Mantle hardforks (Skadi, Limb, Arsia) are activated at timestamp 0 so that every
/// block mined in the test is post-Arsia.
pub(crate) fn mantle_test_chain_spec() -> Arc<OpChainSpec> {
    let genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("valid genesis JSON");
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// Payload attributes generator for Mantle (Arsia/Jovian-activated) test chains.
///
/// Compared to `optimism_payload_attributes`, this sets:
/// - `min_base_fee: Some(0)` — required by Jovian payload builder
/// - `eip_1559_params: Some(B64::ZERO)` — required by Holocene
/// - `withdrawals: Some(vec![])` — required by Shanghai
/// - `parent_beacon_block_root: Some(B256::ZERO)` — required by Cancun
pub(crate) fn mantle_payload_attributes(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: None,
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}
