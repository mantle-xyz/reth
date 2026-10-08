//! Oversized `BVM_ETH` deposits must fail execution without stopping either node.
//!
//! Exercises the Engine API boundary with real sequencer/verifier nodes and checks the
//! canonical receipts and state over RPC. L1 derivation is represented by payload attributes.

use crate::helpers::{mantle_payload_attributes, with_mantle_node};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, hex, keccak256};
use alloy_rlp::{Encodable, Header};
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use mantle_reth_cli::node::MantleNode;
use op_alloy_consensus::{TxDeposit, encode_jovian_extra_data};
use op_revm::BvmEth;
use reth_chainspec::BaseFeeParams;
use reth_e2e_test_utils::NodeHelperType;
use reth_node_api::TreeConfig;
use reth_optimism_node::{OpBuiltPayload, OpEngineTypes, payload::OpPayloadAttrs};
use reth_payload_primitives::PayloadTypes;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const DEPOSITOR: Address = address!("1111111111111111111111111111111111111111");
const RECIPIENT: Address = address!("2222222222222222222222222222222222222222");
const DEPOSIT_GAS: u64 = 100_000;
const NATIVE_MINT: u128 = 1_000;
const ETH_MINT: u64 = 100;
const VALID_TRANSFER: u64 = 50;

/// Encode the wire fields independently of `TxDeposit.eth_tx_value`'s Rust type. The fixture
/// must also compile on the old u128 dependency, where decoding these same bytes overflows.
fn deposit_bytes(id: u8, eth_tx_value: U256) -> Bytes {
    let fields: [&dyn Encodable; 10] = [
        &B256::repeat_byte(id),
        &DEPOSITOR,
        &TxKind::Call(RECIPIENT),
        &NATIVE_MINT,
        &U256::ZERO,
        &DEPOSIT_GAS,
        &false,
        &U256::from(ETH_MINT),
        &Bytes::new(),
        &eth_tx_value,
    ];
    let mut encoded = vec![0x7e];
    Header { list: true, payload_length: fields.iter().map(|field| field.length()).sum() }
        .encode(&mut encoded);
    for field in fields {
        field.encode(&mut encoded);
    }
    encoded.into()
}

fn oversized_values() -> [U256; 2] {
    // Nonzero low bits also catch accidental truncation: transferring 1 would succeed after mint.
    [(U256::from(1u64) << 128usize) + U256::from(1u64), U256::MAX]
}

fn attributes(timestamp: u64) -> OpPayloadAttrs {
    let mut calldata = vec![0u8; 178];
    calldata[..4].copy_from_slice(&hex!("49e72383")); // Arsia L1 attributes, zero fee scalars.
    let l1_info = TxDeposit {
        source_hash: B256::from(U256::from(timestamp)),
        from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
        to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
        gas_limit: 1_000_000,
        input: calldata.into(),
        ..Default::default()
    };
    let mut transactions = vec![l1_info.encoded_2718().into()];
    match timestamp {
        1 => {
            for (id, value) in [1, 2].into_iter().zip(oversized_values()) {
                transactions.push(deposit_bytes(id, value));
            }
        }
        2 => transactions.push(deposit_bytes(3, U256::from(VALID_TRANSFER))),
        _ => {}
    }
    let mut attrs = mantle_payload_attributes(timestamp);
    attrs.0.transactions = Some(transactions);
    attrs.0.no_tx_pool = Some(true);
    attrs
}

async fn build_and_import(
    sequencer: &mut NodeHelperType<MantleNode>,
    verifier: &NodeHelperType<MantleNode>,
) -> OpBuiltPayload {
    let payload = tokio::time::timeout(Duration::from_secs(30), sequencer.advance_block())
        .await
        .expect("sequencer must keep building blocks")
        .expect("sequencer must decode and include mandatory deposits");
    let hash = payload.block().hash();
    let status = verifier
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(<OpEngineTypes as PayloadTypes>::block_to_payload(payload.block().clone()))
        .await
        .expect("verifier newPayload");
    assert!(status.is_valid(), "verifier must accept the payload: {status:?}");
    assert_eq!(status.latest_valid_hash, Some(hash));
    verifier.update_forkchoice(hash, hash).await.expect("canonicalize verifier block");
    sequencer.sync_to(hash).await.expect("sequencer canonical head");
    verifier.sync_to(hash).await.expect("verifier canonical head");
    payload
}

async fn receipt(client: &HttpClient, hash: B256) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(receipt) = client
                .request::<Option<Value>, _>("eth_getTransactionReceipt", rpc_params![hash])
                .await
                .expect("receipt RPC")
            {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("mandatory deposit must have a canonical receipt")
}

async fn assert_state(client: &HttpClient, deposits: u64, transferred: u64) {
    let nonce: U256 = client
        .request("eth_getTransactionCount", rpc_params![DEPOSITOR, "latest"])
        .await
        .expect("sender nonce");
    assert_eq!(nonce, U256::from(deposits), "failed deposits still increment the nonce");
    let balance: U256 = client
        .request("eth_getBalance", rpc_params![DEPOSITOR, "latest"])
        .await
        .expect("native balance");
    assert_eq!(balance, U256::from(deposits) * U256::from(NATIVE_MINT));

    for (slot, expected) in [
        (BvmEth::get_balance_slot(DEPOSITOR), deposits * ETH_MINT - transferred),
        (BvmEth::get_balance_slot(RECIPIENT), transferred),
        (BvmEth::get_total_supply_slot(), deposits * ETH_MINT),
    ] {
        let stored: B256 = client
            .request("eth_getStorageAt", rpc_params![BvmEth::ADDRESS, slot, "latest"])
            .await
            .expect("BVM_ETH storage");
        assert_eq!(stored, B256::from(U256::from(expected)), "BVM_ETH slot {slot}");
    }
}

#[tokio::test]
async fn oversized_deposits_fail_without_stalling_sequencer_or_verifier() {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("test genesis");
    // The verifier checks the first block against this already post-Arsia genesis parent.
    genesis.base_fee_per_gas = Some(1_000_000_000);
    genesis.extra_data = encode_jovian_extra_data(Default::default(), BaseFeeParams::new(8, 2), 0)
        .expect("post-Arsia genesis extra data");
    // Protocol bookkeeping changes this predeploy's storage without calling its bytecode.
    // Keep it nonempty, as on the real chain, so EIP-161 does not discard the minted balances.
    genesis.alloc.insert(
        BvmEth::ADDRESS,
        GenesisAccount { code: Some(Bytes::from_static(&[0x00])), ..Default::default() },
    );
    let chain_spec = Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
    // Keep these two blocks in the engine's canonical memory overlay; this harness does not
    // run staged sync or construct the historical-state indices.
    let tree_config = TreeConfig::default().with_persistence_threshold(8);
    with_mantle_node(
        chain_spec.clone(),
        attributes,
        tree_config.clone(),
        move |mut sequencer, seq_rpc| {
            with_mantle_node(
                chain_spec,
                attributes,
                tree_config,
                move |verifier, ver_rpc| async move {
                    sequencer.payload.timestamp = 0;
                    let failed = build_and_import(&mut sequencer, &verifier).await;
                    assert_eq!(failed.block().header().number, 1);

                    for client in [&seq_rpc, &ver_rpc] {
                        for (id, amount) in [1, 2].into_iter().zip(oversized_values()) {
                            let hash = keccak256(deposit_bytes(id, amount));
                            let receipt = receipt(client, hash).await;
                            assert_eq!(receipt["blockHash"], json!(failed.block().hash()));
                            assert_eq!(receipt["status"], "0x0", "insufficient BVM_ETH must fail");
                            assert_eq!(receipt["gasUsed"], json!(U256::from(DEPOSIT_GAS)));
                            let logs = receipt["logs"].as_array().expect("receipt logs");
                            assert_eq!(logs.len(), 1, "mint survives, transfer must not occur");
                            assert_eq!(logs[0]["address"], json!(BvmEth::ADDRESS));
                            assert_eq!(
                                logs[0]["topics"],
                                json!([keccak256("Mint(address,uint256)"), DEPOSITOR.into_word()])
                            );
                            assert_eq!(logs[0]["data"], json!(B256::from(U256::from(ETH_MINT))));

                            let tx: Value = client
                                .request("eth_getTransactionByHash", rpc_params![hash])
                                .await
                                .expect("included deposit RPC");
                            assert_eq!(
                                tx["ethTxValue"],
                                json!(amount),
                                "full U256 value survives inclusion"
                            );
                        }
                        assert_state(client, 2, 0).await;
                    }

                    let next = build_and_import(&mut sequencer, &verifier).await;
                    assert_eq!(next.block().header().number, 2);
                    assert_eq!(next.block().header().parent_hash, failed.block().hash());
                    let hash = keccak256(deposit_bytes(3, U256::from(VALID_TRANSFER)));
                    for client in [&seq_rpc, &ver_rpc] {
                        let receipt = receipt(client, hash).await;
                        assert_eq!(receipt["blockHash"], json!(next.block().hash()));
                        assert_eq!(
                            receipt["status"], "0x1",
                            "the next deposit must execute successfully"
                        );
                        let used: U256 =
                            serde_json::from_value(receipt["gasUsed"].clone()).unwrap();
                        assert!(used > U256::ZERO && used < U256::from(DEPOSIT_GAS));
                        assert_state(client, 3, VALID_TRANSFER).await;
                        let head: Value = client
                            .request("eth_getBlockByNumber", rpc_params!["latest", false])
                            .await
                            .expect("canonical head RPC");
                        assert_eq!(head["hash"], json!(next.block().hash()));
                        assert_eq!(head["number"], "0x2");
                    }
                },
            )
        },
    )
    .await;
}
