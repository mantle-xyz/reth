//! Shared helpers for Mantle integration tests.

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::http_client::HttpClient;
use mantle_reth_cli::node::MantleNode;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_chainspec::EthChainSpec;
use reth_db::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::payload::OpPayloadAttrs;
use reth_provider::providers::BlockchainProvider;
use reth_tasks::Runtime;
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

/// Boot a Mantle test node (all hardforks active at genesis) with HTTP-RPC enabled and
/// run `test` against its RPC client.
///
/// The node is kept alive for the duration of the closure and torn down afterwards, so
/// callers don't have to repeat the (verbose) node-launch boilerplate. estimateGas /
/// estimateTotalFee work against genesis state, so no block needs to be mined.
pub(crate) async fn with_mantle_rpc_client<F, Fut>(test: F)
where
    F: FnOnce(HttpClient) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    reth_tracing::init_test_tracing();

    let chain_spec = mantle_test_chain_spec();

    let mut config: NodeConfig<OpChainSpec> = NodeConfig::new(chain_spec)
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

    test(client).await;
}
