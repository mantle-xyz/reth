//! Regression coverage for the Arsia `eth_estimateGas` L1-data-fee funds check.
//!
//! The L1 fee helper expects an encoded transaction envelope. Passing raw request calldata makes
//! empty calldata and ordinary calldata beginning with the deposit type byte (`0x7e`) look exempt,
//! so reth can return an estimate for a balance that op-geth rejects.
//! Difficult-to-compress calldata also verifies that the check does not add 80 to the FastLZ size:
//! op-geth's `Ones += 80` has no effect on the Arsia/Fjord L1 data fee.

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

// Concatenated SHA-256("pr129-review-<offset>") blocks at offsets 0, 32, ... 224.
const HIGH_ENTROPY_CALLDATA: [u8; 256] = hex!(
    "cabe4cea1cef821b7ee68317529990647ec4d437840c1bd95de394f72ec13c08
     c6560b8062de963e294a1ef51c79c8243e0466697e16fd41fc6fed75c0f2e788
     1720780fd15bc6af555760854e9d451fed98f80c14871a99809a63887a53f938
     ccb8ad313cf88b5d8e3180d9b10afd8aae89d3f5b57c2fc83490c10d83515576
     4c2c7ecdcc44b5bfd41aae7aa92ceba5bd803b9108ab6bd346a9100f434825da
     0b40fdcbcf031d4bd8455eaeec2e2238e20eb892bc0e18512a515b367e37ad6c
     f4298bc315405952d9f9dcc782c6c10ac366d9a2a0738f4611c72cd489c01680
     e1b4d04f9d938ab6c2adec320771445e881572c85d265ca23f66bdef57abccf9"
);

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

#[tokio::test]
async fn estimate_gas_matches_geth_for_high_entropy_calldata() {
    with_mantle_node(
        chain_spec_with_token_ratio(),
        attrs_with_l1_deposit,
        TreeConfig::default(),
        |mut node, client| async move {
            let head = node.advance_block().await.expect("mine L1-attributes block");
            node.sync_to(head.block().hash()).await.expect("settle L1-attributes block");

            // Independent op-geth v1.6.3 (`e33cd26cda583e42196b1be82a79b70258be49af`)
            // NewL1CostFuncArsia goldens for the zero-signature dynamic-fee proxy envelopes.
            // FastLZ sizes are 158 and 292; `Ones += 80` leaves these fees unchanged.
            let mut cases = Vec::with_capacity(2);
            for (calldata_len, geth_l1_fee) in
                [(128, 1_586_067_345_511_818u128), (256, 3_198_660_081_310_128u128)]
            {
                let calldata = format!("0x{}", hex::encode(&HIGH_ENTROPY_CALLDATA[..calldata_len]));
                let gas = estimate_gas(&client, &calldata, u128::MAX)
                    .await
                    .expect("fully funded difficult-to-compress call must estimate successfully");
                let gas_limit: u64 = gas.try_into().expect("gas estimate fits u64");
                assert!(
                    gas_limit >= 21_000 + calldata_len as u64 * 40,
                    "an EOA call with all non-zero calldata must cover the EIP-7623 floor"
                );
                eprintln!("calldata={calldata_len} bytes, estimated gas={gas_limit}");
                cases.push((calldata_len, calldata, gas_limit, geth_l1_fee));
            }

            for (calldata_len, calldata, gas_limit, geth_l1_fee) in cases {
                let l2_cost = u128::from(gas_limit) * GAS_PRICE;
                let operator_cost = u128::from(gas_limit) * u128::from(OPERATOR_FEE_SCALAR) * 100;
                let without_l1 = l2_cost + operator_cost;
                let total = without_l1 + geth_l1_fee;

                assert_eq!(
                    estimate_gas(&client, &calldata, total)
                        .await
                        .expect("geth's exact L2 + L1 + operator balance must pass"),
                    U256::from(gas_limit),
                    "Arsia must not reject the +80 FastLZ difference window for {calldata_len} bytes"
                );
                assert_insufficient(&client, &calldata, total - 1, total).await;
                assert_insufficient(&client, &calldata, without_l1, total).await;
            }
        },
    )
    .await;
}
