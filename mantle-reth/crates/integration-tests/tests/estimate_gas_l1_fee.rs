//! Regression coverage for the Arsia `eth_estimateGas` L1-data-fee funds check.
//!
//! The L1 fee helper expects an encoded transaction envelope. Passing raw request calldata makes
//! empty calldata and ordinary calldata beginning with the deposit type byte (`0x7e`) look exempt,
//! so reth can return an estimate for a balance that op-geth rejects.

use crate::helpers::with_mantle_node;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256, address, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::{core::client::ClientT, http_client::HttpClient};
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_node_api::TreeConfig;
use reth_optimism_node::payload::OpPayloadAttrs;
use std::sync::Arc;

const FROM: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const TO: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// `GasPriceOracle` predeploy; Mantle's `token_ratio` is stored in slot 0.
const GAS_ORACLE: Address = address!("420000000000000000000000000000000000000F");
/// `L1Block` predeploy; recipient of the per-block L1-attributes deposit.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

// Fee inputs captured from the Sepolia-QA3 reproduction block.
const GAS_PRICE: u128 = 50_000_100_000;
const OPERATOR_FEE_SCALAR: u64 = 100_000_000;
const TOKEN_RATIO: u64 = 4_369;
const EXPECTED_L1_DATA_FEE: u128 = 1_586_067_345_511_818;
const EMPTY_CALL_TOTAL: u128 = 2_846_069_445_511_818;

/// Arsia L1-attributes calldata with the fee fields from the fixed QA3 reproduction block.
fn arsia_l1_attributes_calldata() -> Bytes {
    let mut data = vec![0u8; 178];
    data[0..4].copy_from_slice(&hex!("49e72383")); // L1_BLOCK_ARSIA_SELECTOR
    let payload = &mut data[4..];
    payload[0..4].copy_from_slice(&169_019u32.to_be_bytes()); // base_fee_scalar
    payload[4..8].copy_from_slice(&4_544_124u32.to_be_bytes()); // blob_base_fee_scalar
    payload[32..64].copy_from_slice(&U256::from(1_225_135_484u64).to_be_bytes::<32>()); // l1_base_fee
    payload[64..96].copy_from_slice(&U256::from(69_790_495u64).to_be_bytes::<32>()); // blob fee
    payload[160..164].copy_from_slice(&(OPERATOR_FEE_SCALAR as u32).to_be_bytes());
    data.into()
}

fn l1_attributes_deposit_bytes() -> Bytes {
    TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: arsia_l1_attributes_calldata(),
        eth_value: U256::ZERO,
        eth_tx_value: None,
    }
    .encoded_2718()
    .into()
}

fn attrs_with_l1_deposit(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![l1_attributes_deposit_bytes()]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

fn chain_spec_with_token_ratio() -> Arc<reth_optimism_chainspec::OpChainSpec> {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("valid genesis JSON");
    let mut storage = std::collections::BTreeMap::new();
    storage.insert(B256::ZERO, B256::from(U256::from(TOKEN_RATIO)));
    genesis.alloc.insert(
        GAS_ORACLE,
        GenesisAccount {
            // EIP-161 would otherwise allow an account with zero nonce/balance/code to disappear.
            code: Some(Bytes::from_static(&[0x00])),
            storage: Some(storage),
            ..Default::default()
        },
    );
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

async fn estimate_gas(
    client: &HttpClient,
    calldata: &str,
    balance: u128,
) -> Result<U256, jsonrpsee::core::client::Error> {
    client
        .request(
            "eth_estimateGas",
            vec![
                serde_json::json!({
                    "from": FROM,
                    "to": TO,
                    "value": "0x0",
                    "gasPrice": format!("0x{GAS_PRICE:x}"),
                    "data": calldata,
                }),
                serde_json::json!("latest"),
                serde_json::json!({ FROM: { "balance": format!("0x{balance:x}") } }),
            ],
        )
        .await
}

async fn assert_insufficient(client: &HttpClient, calldata: &str, balance: u128, total: u128) {
    let err = estimate_gas(client, calldata, balance)
        .await
        .expect_err("balance below the complete L2 + L1 + operator total must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("insufficient funds for gas + L1 data fee + operator fee + value"),
        "unexpected error for calldata {calldata}: {message}"
    );
    assert!(
        message.contains(&format!("have {balance}, need {total}")),
        "error must expose the exact balance boundary for calldata {calldata}: {message}"
    );
}

#[tokio::test]
async fn estimate_gas_charges_l1_fee_for_calldata_edge_cases() {
    with_mantle_node(
        chain_spec_with_token_ratio(),
        attrs_with_l1_deposit,
        TreeConfig::default(),
        |mut node, client| async move {
            let head = node.advance_block().await.expect("mine L1-attributes block");
            node.sync_to(head.block().hash()).await.expect("settle L1-attributes block");

            for calldata in ["0x", "0x7e"] {
                let gas = estimate_gas(&client, calldata, u128::MAX)
                    .await
                    .expect("a fully funded caller must receive an estimate");
                let gas_limit: u64 = gas.try_into().expect("gas estimate fits u64");

                if calldata == "0x" {
                    assert_eq!(gas_limit, 21_000, "must reproduce the QA3 estimate");
                } else {
                    assert!(
                        gas_limit >= 21_016,
                        "one non-zero calldata byte must include its intrinsic gas"
                    );
                }

                let l2_cost = u128::from(gas_limit) * GAS_PRICE;
                let operator_cost = u128::from(gas_limit) * u128::from(OPERATOR_FEE_SCALAR) * 100;
                let without_l1 = l2_cost + operator_cost;
                let total = without_l1 + EXPECTED_L1_DATA_FEE;

                if calldata == "0x" {
                    assert_eq!(total, EMPTY_CALL_TOTAL, "must reproduce the QA3 boundary");
                }

                assert_insufficient(&client, calldata, without_l1, total).await;
                assert_insufficient(&client, calldata, total - 1, total).await;
                assert_eq!(
                    estimate_gas(&client, calldata, total).await.expect("exact total must pass"),
                    U256::from(gas_limit),
                    "strict total > balance boundary must allow equality for {calldata}"
                );
            }
        },
    )
    .await;
}
