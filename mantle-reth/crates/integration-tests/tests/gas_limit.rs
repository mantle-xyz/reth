//! Tx-pool gas-limit reservation: a non-deposit transaction may use at most
//! `block_gas_limit - MANTLE_L1_INFO_GAS_OVERHEAD` gas, matching op-geth's `EffectiveGasLimit`.
//!
//! Every L2 block carries an L1-info deposit that consumes part of the block gas, so a tx asking
//! for the full block gas limit could never be included. op-geth rejects such a tx at admission;
//! this test pins the same behaviour for op-reth's tx-pool.

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

/// Test genesis `gasLimit` (`assets/genesis.json` → `0x1c9c380`).
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
/// Must match `op-reth/crates/txpool/src/validator.rs::MANTLE_L1_INFO_GAS_OVERHEAD`.
const L1_INFO_GAS_OVERHEAD: u64 = 1_000_000;
/// Largest gas a normal tx may request and still be admitted: 29M on a 30M block.
const EFFECTIVE_CAP: u64 = BLOCK_GAS_LIMIT - L1_INFO_GAS_OVERHEAD;

async fn signed_raw_tx(chain_id: u64, wallet: &Wallet, nonce: u64, gas: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(Default::default())),
        gas: Some(gas),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// A tx whose gas exceeds `block_gas_limit - L1_INFO_GAS_OVERHEAD` is rejected by the pool;
/// a tx at or below that effective cap is accepted.
#[tokio::test]
async fn txpool_reserves_l1_info_gas_overhead() {
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

    // Over the effective cap → rejected at admission (would otherwise be stuck forever).
    // Both gas values are <= the full block gas limit, so upstream's `gas > block_gas_limit`
    // check (strict `>`) would NOT reject them — only the Mantle L1-info reservation does.
    // The pool's `ExceedsGasLimit` surfaces through the RPC layer as "exceeds block gas limit".
    // A rejected tx does not consume the sender's nonce, so all use nonce 0.
    for (label, gas) in
        [("block gas limit", BLOCK_GAS_LIMIT), ("effective cap + 1", EFFECTIVE_CAP + 1)]
    {
        let raw = signed_raw_tx(chain_id, &wallet, 0, gas).await;
        let err = node.rpc.inject_tx(raw).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds block gas limit"),
            "{label} ({gas}) should be rejected by the L1-info gas reservation, got: {msg}"
        );
    }

    // Exactly at the effective cap → accepted.
    let raw = signed_raw_tx(chain_id, &wallet, 0, EFFECTIVE_CAP).await;
    let hash: B256 =
        node.rpc.inject_tx(raw).await.expect("tx at the effective cap should be accepted");
    assert_ne!(hash, B256::ZERO);

    // Comfortably below the cap → accepted.
    let raw = signed_raw_tx(chain_id, &wallet, 1, EFFECTIVE_CAP - 1_000_000).await;
    let hash: B256 =
        node.rpc.inject_tx(raw).await.expect("tx below the effective cap should be accepted");
    assert_ne!(hash, B256::ZERO);
}
