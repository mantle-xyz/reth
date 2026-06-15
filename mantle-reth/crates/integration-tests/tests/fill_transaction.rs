//! Tests for `eth_fillTransaction` (see `op-reth/crates/rpc/src/eth/transaction.rs`).
//!
//! `fill_transaction` populates the defaults on an unsigned request: value, nonce,
//! chain id, gas limit (via `estimateGas`), and the EIP-1559 fee fields. These tests
//! drive the real RPC endpoint against a freshly launched node and assert the filled
//! response is structurally complete and internally consistent.

use crate::helpers::{mantle_payload_attributes, mantle_test_chain_spec};
use alloy_primitives::U256;
use jsonrpsee::core::client::ClientT;
use mantle_reth_cli::node::MantleNode;
use reth_chainspec::EthChainSpec;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_provider::providers::BlockchainProvider;
use reth_tasks::Runtime;
use serde_json::Value;
use std::future::Future;

/// Pre-funded Hardhat account from `assets/genesis.json` (nonce 0 at genesis).
const FROM: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const TO: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Launches a Mantle node with HTTP RPC enabled, hands the test body an HTTP client, then
/// tears the node down.
///
/// The full `NodeTestContext` type is hard to name in a return position, so the node is
/// kept alive as a local for the duration of `body` instead of being returned.
async fn with_rpc_client<F, Fut>(body: F)
where
    F: FnOnce(jsonrpsee::http_client::HttpClient) -> Fut,
    Fut: Future<Output = ()>,
{
    reth_tracing::init_test_tracing();

    let chain_spec = mantle_test_chain_spec();

    let mut config: NodeConfig<reth_optimism_chainspec::OpChainSpec> = NodeConfig::new(chain_spec)
        .with_unused_ports()
        .with_datadir_args(DatadirArgs {
            datadir: reth_db::test_utils::tempdir_path().into(),
            ..Default::default()
        })
        .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());
    config.network.discovery.discv5_port = 0;
    config.network.discovery.discv5_port_ipv6 = 0;

    let db = create_test_rw_db_with_path(
        config
            .datadir
            .datadir
            .unwrap_or_chain_default(config.chain.chain(), config.datadir.clone())
            .db(),
    );
    let runtime = Runtime::test();
    let node_handle = NodeBuilder::new(config)
        .with_database(db)
        .with_types_and_provider::<MantleNode, BlockchainProvider<_>>()
        .with_components(MantleNode::default().components())
        .with_add_ons(MantleNode::default().add_ons())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                runtime.clone(),
                builder.config.datadir(),
                Default::default(),
            );
            builder.launch_with(launcher)
        })
        .await
        .expect("MantleNode failed to launch");

    let node = NodeTestContext::new(node_handle.node, mantle_payload_attributes).await.unwrap();
    let client = node.inner.rpc_server_handle().http_client().expect("HTTP RPC enabled");
    body(client).await;
}

fn hex_u128(v: &Value, key: &str) -> u128 {
    let s = v.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("missing field `{key}`"));
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .unwrap_or_else(|_| panic!("field `{key}` is not hex: {s}"))
}

/// A minimal request (only `from`/`to`) gets every default populated, and `raw` is non-empty.
#[tokio::test]
async fn fill_transaction_populates_all_defaults() {
    with_rpc_client(|client| async move {
        let resp: Value = client
            .request("eth_fillTransaction", vec![serde_json::json!({ "from": FROM, "to": TO })])
            .await
            .expect("eth_fillTransaction should succeed");

        // RLP-encoded unsigned tx is present and non-trivial.
        let raw = resp.get("raw").and_then(Value::as_str).expect("raw present");
        assert!(raw.starts_with("0x") && raw.len() > 2, "raw should be non-empty: {raw}");

        let tx = resp.get("tx").expect("tx present");

        // chain id matches the test chain (5000).
        let chain_spec = mantle_test_chain_spec();
        assert_eq!(hex_u128(tx, "chainId") as u64, chain_spec.chain().id(), "chainId filled");

        // nonce defaults to the next available (0 at genesis for the funded account).
        assert_eq!(hex_u128(tx, "nonce"), 0, "nonce filled to next available");

        // value defaults to zero when omitted.
        assert_eq!(hex_u128(tx, "value"), 0, "value defaults to 0");

        // gas estimated to at least the intrinsic cost of a transfer.
        assert!(hex_u128(tx, "gas") >= 21_000, "gas estimated >= 21000");

        // EIP-1559 fee fields are filled and consistent: maxFee >= priorityFee.
        let max_fee = hex_u128(tx, "maxFeePerGas");
        let max_prio = hex_u128(tx, "maxPriorityFeePerGas");
        assert!(
            max_fee >= max_prio * 2,
            "maxFeePerGas ({max_fee}) >= maxPriorityFeePerGas ({max_prio})"
        );
    })
    .await;
}

/// An explicitly supplied nonce is respected, not overwritten by the next-available lookup.
#[tokio::test]
async fn fill_transaction_respects_supplied_nonce() {
    with_rpc_client(|client| async move {
        let resp: Value = client
            .request(
                "eth_fillTransaction",
                vec![serde_json::json!({ "from": FROM, "to": TO, "nonce": "0x7" })],
            )
            .await
            .expect("eth_fillTransaction should succeed");

        let tx = resp.get("tx").expect("tx present");
        assert_eq!(hex_u128(tx, "nonce"), 7, "supplied nonce preserved");
    })
    .await;
}

/// When only the priority fee is given, `maxFeePerGas` is derived (base_fee * 2 + tip)
/// and must be >= the supplied tip.
#[tokio::test]
async fn fill_transaction_derives_max_fee_from_priority_fee() {
    with_rpc_client(|client| async move {
        let tip: u128 = 1_000_000_000; // 1 gwei
        let resp: Value = client
            .request(
                "eth_fillTransaction",
                vec![serde_json::json!({
                    "from": FROM,
                    "to": TO,
                    "maxPriorityFeePerGas": format!("0x{tip:x}"),
                })],
            )
            .await
            .expect("eth_fillTransaction should succeed");

        let tx = resp.get("tx").expect("tx present");
        assert_eq!(hex_u128(tx, "maxPriorityFeePerGas"), tip, "supplied tip preserved");
        assert!(hex_u128(tx, "maxFeePerGas") >= tip, "maxFeePerGas derived to cover the tip");

        // Sanity: the value default still applies alongside fee filling.
        assert_eq!(hex_u128(tx, "value"), 0, "value defaults to 0");
        let _ = U256::ZERO; // keep U256 import meaningful if assertions change
    })
    .await;
}
