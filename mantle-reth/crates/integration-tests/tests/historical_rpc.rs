//! Mainnet cutoff routing through a real Mantle node over HTTP and WS.

use crate::helpers::{mantle_payload_attributes, with_configured_mantle_node_rpc_opts};
use alloy_genesis::Genesis;
use jsonrpsee::{
    core::client::{ClientT, Error as ClientError},
    rpc_params,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::args::RollupArgs;
use reth_transaction_pool::TransactionPool;
use serde_json::{Value, json, value::RawValue};
use std::sync::{Arc, Mutex};

const HISTORY_RESULT: &str = r#"{"source":"history","gasUsedRatio":[0.0015845666666666667]}"#;
const HISTORY_FILTER: &str = "0xf11e7";
const OLD_BLOCK: &str = "0x5352a66"; // C - 1
const LOCAL_BLOCK: &str = "0x5352a67"; // C = 87,370,343
const NEXT_BLOCK: &str = "0x5352a68"; // C + 1
type Requests = Arc<Mutex<Vec<Vec<Value>>>>;

/// Starts a history endpoint that records the params of every call it answers.
async fn start_history(requests: &Requests) -> (ServerHandle, String) {
    let server = Server::builder().build("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", server.local_addr().unwrap());
    let mut module = RpcModule::new(requests.clone());
    let mut answer = |method, result: Value| {
        module
            .register_method(method, move |params, requests, _| {
                requests.lock().unwrap().push(params.parse::<Vec<Value>>()?);
                Ok::<_, ErrorObjectOwned>(result.clone())
            })
            .unwrap();
    };
    answer("eth_getBlockByNumber", json!({"number":OLD_BLOCK,"tokenRatio":"0xf73"}));
    // Every transaction hash is unknown to history; raw lookups answer with empty bytes.
    answer("eth_getTransactionByHash", Value::Null);
    answer("eth_getTransactionReceipt", Value::Null);
    answer("eth_getRawTransactionByHash", json!("0x"));
    answer("debug_getRawTransaction", json!("0x"));
    answer("eth_newFilter", json!(HISTORY_FILTER));
    answer("eth_uninstallFilter", json!(true));
    for method in [
        "eth_getLogs",
        "eth_feeHistory",
        "eth_getFilterLogs",
        "eth_getFilterChanges",
        "eth_simulateV1",
        "debug_getRawHeader",
        "debug_getRawBlock",
        "debug_getRawReceipts",
    ] {
        module
            .register_method(method, |params, requests, _| {
                requests.lock().unwrap().push(params.parse::<Vec<Value>>()?);
                Ok::<_, ErrorObjectOwned>(RawValue::from_string(HISTORY_RESULT.to_owned()).unwrap())
            })
            .unwrap();
    }
    (server.start(module), url)
}

/// A Mantle mainnet chain whose genesis is block C, so C exists locally and C - 1 does not.
fn mainnet_from_cutoff() -> Arc<OpChainSpec> {
    let mut genesis: Value = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
    genesis["number"] = json!(LOCAL_BLOCK);
    genesis["extraData"] = json!("0x0100000008000000020000000000000000");
    let genesis: Genesis = serde_json::from_value(genesis).unwrap();
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// HTTP and WS with the debug namespace, so raw block reads have a local handler.
fn rpc_with_debug() -> RpcServerArgs {
    RpcServerArgs::default()
        .with_unused_ports()
        .with_http()
        .with_http_api("eth,net,web3,debug".parse().unwrap())
        .with_ws()
        .with_ws_api("eth,net,web3,debug".parse().unwrap())
}

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
        client.request("eth_getBlockByNumber", rpc_params![NEXT_BLOCK, false]).await.unwrap();
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
        ("debug_getRawHeader", json!([OLD_BLOCK]), true),
        ("debug_getRawBlock", json!([OLD_BLOCK]), true),
        ("debug_getRawReceipts", json!([OLD_BLOCK]), true),
        ("debug_getRawHeader", json!([LOCAL_BLOCK]), false),
        ("debug_getRawBlock", json!([LOCAL_BLOCK]), false),
        ("debug_getRawReceipts", json!([LOCAL_BLOCK]), false),
        ("eth_simulateV1", json!([{ "blockStateCalls": [] }, OLD_BLOCK]), true),
    ] {
        requests.lock().unwrap().clear();
        let values: Vec<Value> = serde_json::from_value(params).unwrap();
        let mut rpc_params = jsonrpsee::core::params::ArrayParams::new();
        for value in &values {
            rpc_params.insert(value).unwrap();
        }
        let result = client.request::<Box<RawValue>, _>(method, rpc_params).await;
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

/// A filter whose range starts before the cutoff lives on the history endpoint: polling and
/// removing it go there too, until it is removed.
async fn check_filters(client: &impl ClientT, requests: &Requests) {
    requests.lock().unwrap().clear();
    let range = json!({ "fromBlock": OLD_BLOCK, "toBlock": OLD_BLOCK });
    let id: String = client.request("eth_newFilter", rpc_params![range.clone()]).await.unwrap();
    assert_eq!(id, HISTORY_FILTER);
    for method in ["eth_getFilterLogs", "eth_getFilterChanges"] {
        let result: Box<RawValue> = client.request(method, rpc_params![&id]).await.unwrap();
        assert_eq!(result.get(), HISTORY_RESULT, "{method}");
    }
    let removed: bool = client.request("eth_uninstallFilter", rpc_params![&id]).await.unwrap();
    assert!(removed);
    let id_param = vec![json!(id)];
    assert_eq!(
        *requests.lock().unwrap(),
        vec![vec![range], id_param.clone(), id_param.clone(), id_param]
    );

    // Removed, so the id is now unknown everywhere this node looks.
    let error =
        client.request::<Value, _>("eth_getFilterChanges", rpc_params![&id]).await.unwrap_err();
    assert!(matches!(error, ClientError::Call(_)), "{error:?}");

    let local: String = client
        .request("eth_newFilter", rpc_params![json!({ "fromBlock": LOCAL_BLOCK })])
        .await
        .unwrap();
    assert_ne!(local, HISTORY_FILTER);
    let logs: Vec<Value> = client.request("eth_getFilterLogs", rpc_params![&local]).await.unwrap();
    assert!(logs.is_empty());
    let removed: bool = client.request("eth_uninstallFilter", rpc_params![&local]).await.unwrap();
    assert!(removed);
    assert_eq!(requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn historical_rpc_cutoff_http_and_ws() {
    let requests = Requests::default();
    let (history, history_url) = start_history(&requests).await;

    // Start at C so its block exists locally; C - 1 must come from the backend.
    let node =
        MantleNode::new(RollupArgs { historical_rpc: Some(history_url), ..Default::default() });
    with_configured_mantle_node_rpc_opts(
        node,
        mainnet_from_cutoff(),
        mantle_payload_attributes,
        TreeConfig::default(),
        None,
        rpc_with_debug(),
        move |node, http| async move {
            check_routes(&http, &requests).await;
            let ws = node.inner.rpc_server_handle().ws_client().await.unwrap();
            check_routes(&ws, &requests).await;
            check_filters(&http, &requests).await;
            check_filters(&ws, &requests).await;

            let raw =
                TransactionTestContext::transfer_tx_bytes(5000, Wallet::default().inner).await;
            let hash = node.rpc.inject_tx(raw.clone()).await.unwrap();
            assert!(node.inner.pool.get(&hash).is_some());
            requests.lock().unwrap().clear();
            let pending: Value =
                http.request("eth_getTransactionByHash", rpc_params![hash]).await.unwrap();
            assert_eq!(pending["hash"], json!(hash));
            assert!(pending["blockHash"].is_null());
            let pending_ws: Value =
                ws.request("eth_getTransactionByHash", rpc_params![hash]).await.unwrap();
            assert_eq!(pending_ws, pending);
            for (method, expected) in [
                ("eth_getRawTransactionByHash", json!(raw)),
                ("debug_getRawTransaction", json!(raw)),
                ("eth_getTransactionReceipt", Value::Null),
            ] {
                let result: Value = http.request(method, rpc_params![hash]).await.unwrap();
                assert_eq!(result, expected);
                let result: Value = ws.request(method, rpc_params![hash]).await.unwrap();
                assert_eq!(result, expected);
            }
            // History was asked once; after its miss the hash is served locally.
            assert_eq!(*requests.lock().unwrap(), vec![vec![json!(hash)]]);

            let unknown = alloy_primitives::B256::ZERO;
            for _ in 0..2 {
                let result: Value =
                    http.request("eth_getTransactionByHash", rpc_params![unknown]).await.unwrap();
                assert!(result.is_null());
            }
            assert_eq!(requests.lock().unwrap().len(), 2);
        },
    )
    .await;
    history.stop().unwrap();
    history.stopped().await;
}

/// An explicit cutoff replaces the Mantle mainnet default.
#[tokio::test]
async fn historical_rpc_configured_cutoff() {
    let requests = Requests::default();
    let (history, history_url) = start_history(&requests).await;

    let node = MantleNode::new(RollupArgs {
        historical_rpc: Some(history_url),
        historical_rpc_cutoff: Some(u64::from_str_radix(&NEXT_BLOCK[2..], 16).unwrap()),
        ..Default::default()
    });
    with_configured_mantle_node_rpc_opts(
        node,
        mainnet_from_cutoff(),
        mantle_payload_attributes,
        TreeConfig::default(),
        None,
        RpcServerArgs::default().with_unused_ports().with_http(),
        move |node, http| async move {
            // Keeps the node, and so its RPC server, alive while the requests run.
            let _node = node;
            let historical: Value = http
                .request("eth_getBlockByNumber", rpc_params![LOCAL_BLOCK, false])
                .await
                .unwrap();
            assert_eq!(historical, json!({"number":OLD_BLOCK,"tokenRatio":"0xf73"}));
            let future: Value =
                http.request("eth_getBlockByNumber", rpc_params![NEXT_BLOCK, false]).await.unwrap();
            assert!(future.is_null());
            assert_eq!(*requests.lock().unwrap(), vec![vec![json!(LOCAL_BLOCK), json!(false)]]);
        },
    )
    .await;
    history.stop().unwrap();
    history.stopped().await;
}
