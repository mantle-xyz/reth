//! A live-node regression for `eth_estimateTotalFee` when the header contains `baseFeePerGas=0`.
//! The proxy envelope must still use EIP-1559 and retain the access list. Helper-only tests do
//! not catch a callsite collapsing `Some(0)` into an absent base fee, so this chain starts at zero.

use crate::helpers::with_mantle_node;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256, address, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::core::client::ClientT;
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_node_api::TreeConfig;
use reth_optimism_node::payload::OpPayloadAttrs;
use std::sync::Arc;

const GAS_ORACLE: Address = address!("420000000000000000000000000000000000000F");
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

fn patterned_bytes<const N: usize>(seed: u8) -> [u8; N] {
    core::array::from_fn(|index| seed.wrapping_add((index as u8).wrapping_mul(31)))
}

// Same list as the rpc-ext unit fixture and the independent op-geth fee goldens.
fn access_list() -> serde_json::Value {
    serde_json::json!((0u8..4).map(|entry| {
        serde_json::json!({
            "address": Address::from(patterned_bytes(entry.wrapping_mul(53).wrapping_add(1))),
            "storageKeys": (0u8..4).map(|key| {
                B256::from(patterned_bytes(entry.wrapping_mul(67)
                    .wrapping_add(key.wrapping_mul(29)).wrapping_add(3)))
            }).collect::<Vec<_>>(),
        })
    }).collect::<Vec<_>>())
}

fn attributes_with_l1_deposit(timestamp: u64) -> OpPayloadAttrs {
    // Nonzero L1 fee parameters with no operator fee, matching test_l1_block_info in rpc-ext.
    let mut input = vec![0u8; 178];
    input[0..4].copy_from_slice(&hex!("49e72383"));
    let fields = &mut input[4..];
    fields[0..4].copy_from_slice(&5000u32.to_be_bytes());
    fields[4..8].copy_from_slice(&100u32.to_be_bytes());
    fields[32..64].copy_from_slice(&U256::from(30_000_000_000u64).to_be_bytes::<32>());
    fields[64..96].copy_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>());
    let deposit = TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: input.into(),
        eth_value: U256::ZERO,
        eth_tx_value: None,
    };
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![deposit.encoded_2718().into()]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

fn zero_base_fee_chain_spec() -> Arc<reth_optimism_chainspec::OpChainSpec> {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("valid genesis JSON");
    genesis.config.chain_id = 1337;
    genesis.base_fee_per_gas = Some(0);
    genesis.alloc.insert(
        GAS_ORACLE,
        GenesisAccount {
            code: Some(Bytes::from_static(&[0x00])),
            storage: Some(std::collections::BTreeMap::from([(
                B256::ZERO,
                B256::from(U256::from(3000)),
            )])),
            ..Default::default()
        },
    );
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

#[tokio::test]
async fn estimate_total_fee_preserves_access_list_when_basefee_zero() {
    with_mantle_node(
        zero_base_fee_chain_spec(),
        attributes_with_l1_deposit,
        TreeConfig::default(),
        |mut node, client| async move {
            let head = node.advance_block().await.expect("mine zero-basefee L1-attributes block");
            node.sync_to(head.block().hash()).await.expect("settle zero-basefee block");
            let header: serde_json::Value = client
                .request("eth_getBlockByNumber", jsonrpsee::rpc_params!["latest", false])
                .await
                .expect("read zero-basefee head");
            assert_eq!(header["baseFeePerGas"], "0x0", "the header must contain a zero base fee");

            // The L2 gas-price fallback stays unchanged. Remove it to check only the L1 fee.
            let tip: U256 = client
                .request("eth_maxPriorityFeePerGas", jsonrpsee::rpc_params![])
                .await
                .expect("read the suggested gas tip");
            let plain = serde_json::json!({
                "from": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
                "to": Address::ZERO,
                "gas": "0x186a0",
            });
            let mut with_list = plain.clone();
            with_list["accessList"] = access_list();
            let mut with_data = plain.clone();
            with_data["input"] =
                serde_json::json!(Bytes::from(patterned_bytes::<256>(0x11).to_vec()));

            // Independent op-geth goldens for gas=100000, chainId=1337 and this L1 fixture.
            for (request, expected_l1) in [
                (plain, 720_000_030_000_000u64),
                (with_list, 2_620_464_589_185_000),
                (with_data, 1_939_888_160_826_000),
            ] {
                let gas: U256 = client
                    .request("eth_estimateGas", jsonrpsee::rpc_params![request.clone(), "latest"])
                    .await
                    .expect("estimate L2 gas on the zero-basefee chain");
                let total: U256 = client
                    .request("eth_estimateTotalFee", jsonrpsee::rpc_params![request, "latest"])
                    .await
                    .expect("estimate total fee on the zero-basefee chain");
                let l2 = gas * tip;
                assert!(total >= l2, "the total must include the L2 fee");
                assert_eq!(
                    total - l2,
                    U256::from(expected_l1),
                    "a present zero base fee must preserve the geth-compatible L1 envelope"
                );
            }
        },
    )
    .await;
}
