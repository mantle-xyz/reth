//! `TxPool` rejection tests: `MetaTx` and EIP-155 unprotected legacy transactions.

use crate::helpers::{mantle_payload_attributes, mantle_test_chain_spec};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_cli::node::MantleNode;
use reth_chainspec::EthChainSpec;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::{
    node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::DatadirArgs;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::Runtime;

use alloy_op_hardforks::MANTLE_META_TX_PREFIX;

fn mantle_meta_tx_input() -> Bytes {
    let mut input = MANTLE_META_TX_PREFIX.to_vec();
    input.push(0xF8); // minimal payload to exceed 32-byte prefix
    input.into()
}

async fn signed_raw_tx(chain_id: u64, wallet: &Wallet, nonce: u64, input: Bytes) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(Default::default())),
        gas: Some(100_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::from(input),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// `MetaTx` (prefix + payload) must be rejected by the txpool.
#[tokio::test]
async fn metatx_rejected_by_txpool() {
    reth_tracing::init_test_tracing();

    let chain_spec = mantle_test_chain_spec();
    let chain_id = chain_spec.chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);

    let mut config: NodeConfig<reth_optimism_chainspec::OpChainSpec> =
        NodeConfig::new(chain_spec).with_unused_ports().with_datadir_args(DatadirArgs {
            datadir: reth_db::test_utils::tempdir_path().into(),
            ..Default::default()
        });
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

    // Test 1: MetaTx must be rejected
    let raw_meta_tx = signed_raw_tx(chain_id, &wallet, 0, mantle_meta_tx_input()).await;
    let err = node.rpc.inject_tx(raw_meta_tx).await.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("meta tx") || err_str.contains("MetaTx"),
        "expected MetaTx rejection error, got: {err_str}",
    );

    // Test 2: Normal tx (empty input) must be accepted
    let raw_normal = signed_raw_tx(chain_id, &wallet, 0, Bytes::new()).await;
    let hash: B256 = node.rpc.inject_tx(raw_normal).await.expect("normal tx should be accepted");
    assert_ne!(hash, B256::ZERO);

    // Test 3: Exactly 32-byte prefix (no payload after it) should NOT be rejected
    let raw_prefix =
        signed_raw_tx(chain_id, &wallet, 1, MANTLE_META_TX_PREFIX.to_vec().into()).await;
    let hash: B256 =
        node.rpc.inject_tx(raw_prefix).await.expect("32-byte prefix only tx should be accepted");
    assert_ne!(hash, B256::ZERO);
}
