//! Mainnet cutoff routing through a real Mantle node over HTTP and WS.

use crate::helpers::{mantle_payload_attributes, with_configured_mantle_node_rpc_opts};
use alloy_genesis::Genesis;
use jsonrpsee::{
    core::client::{ClientT, Error as ClientError},
    rpc_params,
    server::{RpcModule, Server},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::args::RollupArgs;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const OLD_BLOCK: &str = "0x5352a66"; // C - 1
const LOCAL_BLOCK: &str = "0x5352a67"; // C = 87,370,343
type Requests = Arc<Mutex<Vec<Vec<Value>>>>;

async fn check_routes(client: &impl ClientT, requests: &Requests) {
    requests.lock().unwrap().clear();
    let historical: Value =
        client.request("eth_getBlockByNumber", rpc_params![OLD_BLOCK, true]).await.unwrap();
    assert_eq!(historical, json!({"number":OLD_BLOCK,"tokenRatio":"0xf73"}));
    assert_eq!(*requests.lock().unwrap(), vec![vec![json!(OLD_BLOCK), json!(true)]]);

    let local: Value =
        client.request("eth_getBlockByNumber", rpc_params![LOCAL_BLOCK, false]).await.unwrap();
    assert_eq!(local["number"], LOCAL_BLOCK);
    let future: Value =
        client.request("eth_getBlockByNumber", rpc_params!["0x5352a68", false]).await.unwrap();
    assert!(future.is_null());

    let error = client
        .request::<Value, _>("eth_getBlockRange", rpc_params![OLD_BLOCK, LOCAL_BLOCK, false])
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Call(error) if error.code() == -32601));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn historical_rpc_cutoff_http_and_ws() {
    let requests = Requests::default();
    let server = Server::builder().build("127.0.0.1:0").await.unwrap();
    let history_url = format!("http://{}", server.local_addr().unwrap());
    let mut module = RpcModule::new(requests.clone());
    module
        .register_method("eth_getBlockByNumber", |params, requests, _| {
            requests.lock().unwrap().push(params.parse::<Vec<Value>>()?);
            Ok::<_, ErrorObjectOwned>(json!({"number":OLD_BLOCK,"tokenRatio":"0xf73"}))
        })
        .unwrap();
    let history = server.start(module);

    // Start at C so its block exists locally; C - 1 must come from the backend.
    let mut genesis: Value = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
    genesis["number"] = json!(LOCAL_BLOCK);
    genesis["extraData"] = json!("0x0100000008000000020000000000000000");
    let genesis: Genesis = serde_json::from_value(genesis).unwrap();
    let chain_spec = Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis));
    let node =
        MantleNode::new(RollupArgs { historical_rpc: Some(history_url), ..Default::default() });
    with_configured_mantle_node_rpc_opts(
        node,
        chain_spec,
        mantle_payload_attributes,
        TreeConfig::default(),
        None,
        RpcServerArgs::default().with_unused_ports().with_http().with_ws(),
        move |node, http| async move {
            check_routes(&http, &requests).await;
            let ws = node.inner.rpc_server_handle().ws_client().await.unwrap();
            check_routes(&ws, &requests).await;
        },
    )
    .await;
    history.stop().unwrap();
    history.stopped().await;
}
