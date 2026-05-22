//! Smoke tests for Mantle-specific RPC endpoints and gas estimation.
//!
//! These tests verify the RPC methods are reachable and return structurally
//! valid responses. Numerical accuracy is verified by `tests/rpc_compat`.

use crate::helpers::{mantle_payload_attributes, mantle_test_chain_spec};
use alloy_primitives::U256;
use mantle_reth_cli::node::MantleNode;
use reth_chainspec::EthChainSpec;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_provider::providers::BlockchainProvider;
use reth_tasks::Runtime;

/// `eth_estimateGas` for a simple transfer returns >= 21000 via RPC.
///
/// No block mining needed — estimateGas works against genesis state.
#[tokio::test]
async fn estimate_gas_simple_transfer_via_rpc() {
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

    // Don't mine — estimateGas works on genesis state directly.
    let _node = NodeTestContext::new(node_handle.node, mantle_payload_attributes).await.unwrap();

    let client = _node.inner.rpc_server_handle().http_client().expect("HTTP RPC enabled");

    // Pre-funded Hardhat account from genesis.json
    let from = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    use jsonrpsee::core::client::ClientT;
    let gas: U256 = client
        .request(
            "eth_estimateGas",
            vec![serde_json::json!({
                "from": from,
                "to": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
                "value": "0x1"
            })],
        )
        .await
        .expect("eth_estimateGas should succeed");

    assert!(gas >= U256::from(21_000u64), "expected >= 21000, got {gas}");
}
