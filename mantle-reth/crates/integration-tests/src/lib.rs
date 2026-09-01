//! Integration tests for Mantle op-reth node.
//!
//! Each `[[test]]` target is its own crate root, so machinery shared between
//! suites lives here rather than in any one target's `helpers`.

/// Launches a `MantleNode` on a temporary datadir and returns
/// `(NodeTestContext, http client, Wallet, chain_id)`.
///
/// `$node` is the fully-configured node to launch; `$payload_attributes` is the
/// attributes builder handed to `NodeTestContext`. Suites wrap this with their
/// own subsystem setup — see `launch_preconf_node!`.
#[macro_export]
macro_rules! launch_mantle_node {
    ($chain_spec:expr, $node:expr, $payload_attributes:expr) => {{
        async {
            use mantle_reth_cli::node::MantleNode;
            use reth_chainspec::EthChainSpec;
            use reth_db::test_utils::create_test_rw_db_with_path;
            use reth_e2e_test_utils::{node::NodeTestContext, wallet::Wallet};
            use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
            use reth_node_core::args::{DatadirArgs, RpcServerArgs};
            use reth_provider::providers::BlockchainProvider;
            use reth_tasks::Runtime;

            let chain_spec = $chain_spec;
            let chain_id = chain_spec.chain().id();
            let wallet = Wallet::default().with_chain_id(chain_id);

            let mut config: NodeConfig<reth_optimism_chainspec::OpChainSpec> =
                NodeConfig::new(chain_spec)
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

            let node_type: MantleNode = $node;

            let runtime = Runtime::test();
            let node_handle = NodeBuilder::new(config)
                .with_database(db)
                .with_types_and_provider::<MantleNode, BlockchainProvider<_>>()
                .with_components(node_type.components())
                .with_add_ons(node_type.add_ons())
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

            let http = node_handle
                .node
                .rpc_server_handle()
                .http_client()
                .expect("HTTP RPC must be enabled");

            let node_ctx =
                NodeTestContext::new(node_handle.node, $payload_attributes).await.unwrap();

            (node_ctx, http, wallet, chain_id)
        }
    }};
}
