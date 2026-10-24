//! Mantle Hoodi `HoodiBvmUpgrade`: at the activation timestamp the block executor writes the
//! `BVM_ETH` and `LegacyERC20MNT` (`BVM_MNT`) runtime code before the block's transactions run.
//!
//! A sequencer builds the blocks and a verifier imports them, so the test pins that both the
//! payload-building path and the import path apply the same irregular state transition (the
//! verifier rejects the payload on a state-root mismatch). Storage is untouched: an existing
//! `BVM_ETH` total-supply slot survives the write.

use crate::helpers::{mantle_payload_attributes, with_mantle_node};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_op_hardforks::{MANTLE_BLOCK_TIME, MANTLE_HOODI_CHAIN_ID};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, hex, keccak256};
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use mantle_reth_cli::node::MantleNode;
use op_alloy_consensus::{TxDeposit, encode_jovian_extra_data};
use reth_chainspec::BaseFeeParams;
use reth_e2e_test_utils::NodeHelperType;
use reth_node_api::TreeConfig;
use reth_optimism_node::{OpBuiltPayload, OpEngineTypes, payload::OpPayloadAttrs};
use reth_payload_primitives::PayloadTypes;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

const BVM_ETH: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead1111");
const BVM_MNT: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddead0000");
/// Code hashes pinned by `alloy-op-evm` (`block/hoodi_bvm_upgrade.rs`).
const BVM_ETH_CODEHASH: B256 =
    alloy_primitives::b256!("0xe84a8d4d3ed629a670b03e88dab806078de4b86beac8ca06addcff8dd4996290");
const BVM_MNT_CODEHASH: B256 =
    alloy_primitives::b256!("0xf165e4b851dc8d4e18dcf1d2bc59817d0129a39365c191e76bfd596b60513b45");
/// Activation time written to the test genesis.
const UPGRADE_TIME: u64 = 1_000;
/// Pre-existing `BVM_ETH` storage that the code write must preserve.
const SUPPLY_SLOT: B256 = B256::with_last_byte(2);
const SUPPLY: u64 = 15_349;

/// Block `n` (counted from 1 by the payload helper) is one Hoodi block interval apart from its
/// parent: block 1 precedes the activation block, block 2 is it, block 3 follows it.
fn block_time(n: u64) -> u64 {
    UPGRADE_TIME - MANTLE_BLOCK_TIME + (n - 1) * MANTLE_BLOCK_TIME
}

fn attributes(n: u64) -> OpPayloadAttrs {
    let timestamp = block_time(n);
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
    let mut attrs = mantle_payload_attributes(timestamp);
    attrs.0.transactions = Some(vec![l1_info.encoded_2718().into()]);
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
        .expect("sequencer builds the block");
    let hash = payload.block().hash();
    let status = verifier
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(<OpEngineTypes as PayloadTypes>::block_to_payload(payload.block().clone()))
        .await
        .expect("verifier newPayload");
    assert!(status.is_valid(), "verifier must accept the payload: {status:?}");
    verifier.update_forkchoice(hash, hash).await.expect("canonicalize verifier block");
    sequencer.sync_to(hash).await.expect("sequencer canonical head");
    verifier.sync_to(hash).await.expect("verifier canonical head");
    payload
}

async fn code_hash(client: &HttpClient, addr: Address, block: u64) -> B256 {
    let code: Bytes = client
        .request("eth_getCode", rpc_params![addr, format!("0x{block:x}")])
        .await
        .expect("eth_getCode");
    keccak256(code)
}

async fn call_string(client: &HttpClient, to: Address, selector: [u8; 4]) -> String {
    let out: Bytes = client
        .request(
            "eth_call",
            rpc_params![json!({"to": to, "data": Bytes::from(selector.to_vec())}), "latest"],
        )
        .await
        .expect("eth_call");
    // ABI string: offset (32) | length (32) | data
    let len = U256::from_be_slice(&out[32..64]).to::<usize>();
    String::from_utf8(out[64..64 + len].to_vec()).expect("utf8")
}

#[tokio::test]
async fn hoodi_bvm_upgrade_writes_code_at_activation() {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("test genesis");
    genesis.config.chain_id = MANTLE_HOODI_CHAIN_ID;
    genesis.base_fee_per_gas = Some(1_000_000_000);
    genesis.extra_data = encode_jovian_extra_data(Default::default(), BaseFeeParams::new(8, 2), 0)
        .expect("post-Arsia genesis extra data");
    genesis.config.extra_fields.insert("mantleHoodiBvmUpgradeTime".to_owned(), json!(UPGRADE_TIME));
    // Hoodi-like pre-fork state: BVM_ETH has placeholder code and a total supply, BVM_MNT absent.
    genesis.alloc.insert(
        BVM_ETH,
        GenesisAccount {
            code: Some(Bytes::from_static(&[0x00])),
            storage: Some(BTreeMap::from([(SUPPLY_SLOT, B256::from(U256::from(SUPPLY)))])),
            ..Default::default()
        },
    );
    let chain_spec = Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
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
                    let pre = build_and_import(&mut sequencer, &verifier).await;
                    assert_eq!(pre.block().header().timestamp, UPGRADE_TIME - MANTLE_BLOCK_TIME);
                    let fork = build_and_import(&mut sequencer, &verifier).await;
                    assert_eq!(fork.block().header().timestamp, UPGRADE_TIME);
                    // The block after the activation block is outside the write window; that it
                    // does not write is covered by alloy-op-evm's unit tests, since rewriting the
                    // same code would not change the state.
                    let post = build_and_import(&mut sequencer, &verifier).await;
                    assert_eq!(post.block().header().timestamp, UPGRADE_TIME + MANTLE_BLOCK_TIME);

                    for client in [&seq_rpc, &ver_rpc] {
                        assert_eq!(
                            code_hash(client, BVM_ETH, 1).await,
                            keccak256([0x00]),
                            "pre-fork"
                        );
                        assert_eq!(code_hash(client, BVM_MNT, 1).await, keccak256([]), "pre-fork");
                        assert_eq!(code_hash(client, BVM_ETH, 2).await, BVM_ETH_CODEHASH);
                        assert_eq!(code_hash(client, BVM_MNT, 2).await, BVM_MNT_CODEHASH);
                        assert_eq!(code_hash(client, BVM_MNT, 3).await, BVM_MNT_CODEHASH);

                        let supply: B256 = client
                            .request(
                                "eth_getStorageAt",
                                rpc_params![BVM_ETH, SUPPLY_SLOT, "latest"],
                            )
                            .await
                            .expect("BVM_ETH storage");
                        assert_eq!(supply, B256::from(U256::from(SUPPLY)), "storage preserved");

                        // name() = 0x06fdde03, symbol() = 0x95d89b41
                        assert_eq!(call_string(client, BVM_ETH, hex!("06fdde03")).await, "Ether");
                        assert_eq!(
                            call_string(client, BVM_MNT, hex!("06fdde03")).await,
                            "Mantle Token"
                        );
                        assert_eq!(call_string(client, BVM_MNT, hex!("95d89b41")).await, "MNT");

                        let head: Value = client
                            .request("eth_getBlockByNumber", rpc_params!["latest", false])
                            .await
                            .expect("head");
                        assert_eq!(head["hash"], json!(post.block().hash()));
                    }
                },
            )
        },
    )
    .await;
}
