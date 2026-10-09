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
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::args::RollupArgs;
use reth_transaction_pool::TransactionPool;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const HISTORY_RESULT: &str = r#"{"source":"history","gasUsedRatio":[0.0015845666666666667]}"#;
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

    for (method, params, forwarded) in [
        ("eth_getLogs", json!([{ "fromBlock": OLD_BLOCK, "toBlock": OLD_BLOCK }]), true),
        ("eth_getLogs", json!([{ "fromBlock": OLD_BLOCK, "toBlock": LOCAL_BLOCK }]), true),
        ("eth_getLogs", json!([{ "fromBlock": OLD_BLOCK, "toBlock": "latest" }]), true),
        ("eth_getLogs", json!([{ "fromBlock": "earliest", "toBlock": OLD_BLOCK }]), true),
        ("eth_getLogs", json!([{ "fromBlock": LOCAL_BLOCK, "toBlock": LOCAL_BLOCK }]), false),
        ("eth_getLogs", json!([{}]), false),
        ("eth_getLogs", json!([{ "blockHash": alloy_primitives::B256::ZERO }]), true),
        ("eth_feeHistory", json!(["0x1", OLD_BLOCK, [50]]), true),
        ("eth_feeHistory", json!(["0x2", LOCAL_BLOCK, [50]]), true),
        ("eth_feeHistory", json!(["0x2", "latest"]), true),
        ("eth_feeHistory", json!(["0x2", "pending"]), true),
        ("eth_feeHistory", json!(["0x1", LOCAL_BLOCK, [50]]), false),
        ("eth_feeHistory", json!(["0x1", "latest"]), false),
        ("eth_feeHistory", json!(["0x1", "pending"]), false),
        ("eth_feeHistory", json!(["0x0", OLD_BLOCK]), false),
    ] {
        requests.lock().unwrap().clear();
        let values: Vec<Value> = serde_json::from_value(params).unwrap();
        let mut rpc_params = jsonrpsee::core::params::ArrayParams::new();
        for value in &values {
            rpc_params.insert(value).unwrap();
        }
        let result =
            client.request::<Box<serde_json::value::RawValue>, _>(method, rpc_params).await;
        let recorded = requests.lock().unwrap();
        if forwarded {
            assert_eq!(result.unwrap().get(), HISTORY_RESULT, "{method}: {values:?}");
            assert_eq!(*recorded, vec![values]);
        } else {
            assert!(result.is_ok(), "{method}: {values:?}: {result:?}");
            assert!(recorded.is_empty(), "{method}: {values:?}");
        }
    }
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
    module.register_method("eth_getTransactionByHash", |_, _, _| Value::Null).unwrap();
    for method in ["eth_getLogs", "eth_feeHistory"] {
        module
            .register_method(method, |params, requests, _| {
                requests.lock().unwrap().push(params.parse::<Vec<Value>>()?);
                Ok::<_, ErrorObjectOwned>(
                    serde_json::value::RawValue::from_string(HISTORY_RESULT.to_owned()).unwrap(),
                )
            })
            .unwrap();
    }
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
            let raw =
                TransactionTestContext::transfer_tx_bytes(5000, Wallet::default().inner).await;
            let hash = node.rpc.inject_tx(raw).await.unwrap();
            assert!(node.inner.pool.get(&hash).is_some());
            let pending: Value =
                http.request("eth_getTransactionByHash", rpc_params![hash]).await.unwrap();
            assert_eq!(pending["hash"], json!(hash));
            assert!(pending["blockHash"].is_null());
            let pending_ws: Value =
                ws.request("eth_getTransactionByHash", rpc_params![hash]).await.unwrap();
            assert_eq!(pending_ws, pending);
            let unknown: Value = http
                .request("eth_getTransactionByHash", rpc_params![alloy_primitives::B256::ZERO])
                .await
                .unwrap();
            assert!(unknown.is_null());
        },
    )
    .await;
    history.stop().unwrap();
    history.stopped().await;
}
