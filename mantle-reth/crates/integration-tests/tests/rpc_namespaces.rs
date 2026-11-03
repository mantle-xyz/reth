//! Mantle RPC extensions must respect each transport's namespace selection.

use crate::helpers::{mantle_payload_attributes, mantle_test_chain_spec};
use jsonrpsee::{
    core::{ClientError, client::ClientT},
    rpc_params,
};
use mantle_reth_cli::node::MantleNode;
use mantle_reth_flashblocks::FlashblocksConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::args::RollupArgs;
use serde_json::Value;

async fn assert_eth_methods(client: &impl ClientT, enabled: bool, consumer: bool) {
    let mut methods =
        vec!["eth_sendRawTransactionWithPreconf", "eth_estimateTotalFee", "eth_simulateV1"];
    if consumer {
        methods.extend(["eth_call", "eth_getLogs", "eth_subscribe"]);
    }
    for method in methods {
        let result = client.request::<Value, _>(method, rpc_params![]).await;
        let ClientError::Call(error) = result.expect_err("missing arguments must fail") else {
            panic!("expected an RPC error for {method}");
        };
        if enabled {
            assert_ne!(error.code(), -32601, "{method} must be registered");
        } else {
            assert_eq!(error.code(), -32601, "{method} must not be registered");
        }
    }
    let version: String = client.request("web3_clientVersion", rpc_params![]).await.unwrap();
    assert!(!version.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn mantle_rpc_namespaces_are_selected_per_transport() {
    for consumer in [false, true] {
        for http_eth in [false, true] {
            let mut node = MantleNode::new(RollupArgs::default());
            // Keep a local endpoint bound so the consumer cannot contact an unrelated server.
            let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            if consumer {
                node = node.with_flashblocks(FlashblocksConfig::new(
                    format!("ws://{}", upstream.local_addr().unwrap()).parse().unwrap(),
                ));
            }
            let (http_api, ws_api) =
                if http_eth { ("eth,web3", "net,web3") } else { ("net,web3", "eth,web3") };
            let ws_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = ws_port.local_addr().unwrap().port();
            drop(ws_port);
            let mut rpc = RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_ws()
                .with_http_api(http_api.parse().unwrap())
                .with_ws_api(ws_api.parse().unwrap());
            rpc.ws_port = port;
            let (node, http, _, _) = mantle_reth_integration_tests::launch_mantle_node!(
                mantle_test_chain_spec(),
                node,
                mantle_payload_attributes,
                persist_each_block = false,
                rpc = rpc
            )
            .await;
            let ws = node.inner.rpc_server_handle().ws_client().await.unwrap();
            assert_eth_methods(&http, http_eth, consumer).await;
            assert_eth_methods(&ws, !http_eth, consumer).await;
        }
    }
}
