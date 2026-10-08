//! Local end-to-end checks through a real Mantle node and an HTTP historical backend.

use crate::{
    helpers::with_configured_mantle_node_rpc_opts,
    historical_rpc_matrix::{
        FUTURE_BLOCK, HEAD_BLOCK, LOCAL_BLOCK, NEXT_BLOCK, OLD_BLOCK, chain_spec,
        payload_attributes,
    },
};
use jsonrpsee::{
    core::{
        client::{ClientT, Error as ClientError, SubscriptionClientT},
        params::BatchRequestBuilder,
    },
    rpc_params,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use reth_chainspec::EthChainSpec;
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::args::RollupArgs;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

type Requests = Arc<Mutex<Vec<(String, Vec<Value>)>>>;

/// All listeners use loopback and ephemeral ports. The backend records actual wire parameters.
async fn start_history() -> (String, ServerHandle, Requests) {
    let server = Server::builder().build("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let requests = Requests::default();
    let mut module = RpcModule::new(requests.clone());
    for method in ["eth_getBlockByNumber", "eth_getBalance", "eth_getLogs", "eth_feeHistory"] {
        module.register_method(method, move |params, requests, _| {
            let values: Vec<Value> = params.parse()?;
            requests.lock().unwrap().push((method.to_string(), values.clone()));
            let result = match method {
                "eth_getBlockByNumber" => json!({"number":values[0], "source":"history", "tokenRatio":"0xf73"}),
                "eth_getBalance" => json!("0x2a"),
                "eth_getLogs" => {
                    if values[0]["toBlock"] == FUTURE_BLOCK {
                        return Err(ErrorObjectOwned::owned(-32602, "invalid block range params", Some(json!({"head":HEAD_BLOCK}))));
                    }
                    json!([{"blockNumber":OLD_BLOCK, "logIndex":"0x0", "data":"0xcafe"}])
                }
                "eth_feeHistory" => json!({"oldestBlock":OLD_BLOCK, "baseFeePerGas":["0x1","0x1","0x1","0x1"], "gasUsedRatio":[0,0,0]}),
                _ => unreachable!(),
            };
            Ok::<Value, ErrorObjectOwned>(result)
        }).unwrap();
    }
    (format!("http://{addr}"), server.start(module), requests)
}

fn error_payload(error: ClientError) -> ErrorObjectOwned {
    match error {
        ClientError::Call(error) => error,
        other => panic!("expected a JSON-RPC error, got {other:?}"),
    }
}

async fn check_routes(client: &impl ClientT, requests: &Requests, protocol: &str) {
    requests.lock().unwrap().clear();
    let old: Value =
        client.request("eth_getBlockByNumber", rpc_params![OLD_BLOCK, false]).await.unwrap();
    assert_eq!(old["source"], "history", "{protocol}: C - 1 must forward");
    assert_eq!(old["tokenRatio"], "0xf73", "historical fields must pass through");
    let mut old_hash = Value::Null;
    for number in [LOCAL_BLOCK, HEAD_BLOCK] {
        let local: Value =
            client.request("eth_getBlockByNumber", rpc_params![number, false]).await.unwrap();
        assert_eq!(local["number"], number, "{protocol}: C and C + 1 must be local");
        assert!(local.get("source").is_none());
        if number == LOCAL_BLOCK {
            old_hash = local["parentHash"].clone();
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 1);

    let old_balance: Value = client
        .request(
            "eth_getBalance",
            rpc_params!["0x0000000000000000000000000000000000000000", OLD_BLOCK],
        )
        .await
        .unwrap();
    assert_eq!(old_balance, "0x2a");
    let selector = json!({"blockHash":old_hash,"requireCanonical":true});
    let balance: Value = client
        .request(
            "eth_getBalance",
            rpc_params!["0x0000000000000000000000000000000000000000", selector.clone()],
        )
        .await
        .unwrap();
    assert_eq!(balance, "0x2a");
    assert_eq!(requests.lock().unwrap().last().unwrap().1[1], selector);

    let count = requests.lock().unwrap().len();
    for filter in [
        json!({"fromBlock":OLD_BLOCK,"toBlock":"latest"}),
        json!({"fromBlock":OLD_BLOCK}),
        json!({"fromBlock":LOCAL_BLOCK,"toBlock":HEAD_BLOCK}),
    ] {
        let logs: Value = client.request("eth_getLogs", rpc_params![filter.clone()]).await.unwrap();
        assert_eq!(logs, json!([]), "{protocol}: logs are handled locally");
        assert_eq!(requests.lock().unwrap().len(), count, "{protocol}: log ranges must stay local");
    }

    let error = error_payload(
        client
            .request::<Value, _>(
                "eth_getLogs",
                rpc_params![json!({"fromBlock":OLD_BLOCK,"toBlock":FUTURE_BLOCK})],
            )
            .await
            .unwrap_err(),
    );
    assert_ne!(error.code(), -32601, "local log handler must be registered");
    assert_eq!(requests.lock().unwrap().len(), count, "invalid ranges must stay local");
    let range_error_code = error.code();

    let fees: Value = client
        .request("eth_feeHistory", rpc_params!["0x3", "latest", Vec::<f64>::new()])
        .await
        .unwrap();
    assert_eq!(fees["oldestBlock"], OLD_BLOCK);
    assert_eq!(requests.lock().unwrap().len(), count, "fee history must stay local across C");
    let fees: Value = client
        .request("eth_feeHistory", rpc_params!["0x2", HEAD_BLOCK, Vec::<f64>::new()])
        .await
        .unwrap();
    assert_eq!(fees["oldestBlock"], LOCAL_BLOCK);
    assert_eq!(requests.lock().unwrap().len(), count);

    let mut batch = BatchRequestBuilder::new();
    batch.insert("eth_getBlockByNumber", rpc_params![OLD_BLOCK, false]).unwrap();
    batch.insert("eth_getBlockByNumber", rpc_params![LOCAL_BLOCK, false]).unwrap();
    batch
        .insert("eth_getLogs", rpc_params![json!({"fromBlock":OLD_BLOCK,"toBlock":FUTURE_BLOCK})])
        .unwrap();
    let replies = client.batch_request::<Value>(batch).await.unwrap();
    let mut replies = replies.into_iter();
    assert_eq!(replies.next().unwrap().unwrap()["source"], "history");
    assert_eq!(replies.next().unwrap().unwrap()["number"], LOCAL_BLOCK);
    assert_eq!(replies.next().unwrap().unwrap_err().code(), range_error_code);
    assert!(replies.next().is_none());

    let error = error_payload(
        client
            .request::<Value, _>("eth_getBlockRange", rpc_params!["0x0", "0x1"])
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), -32601, "deprecated method must remain unavailable");
}

/// Local range errors retain the caller's request id on the HTTP wire.
async fn check_raw_responses(http_url: &str) {
    let http =
        reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    let request = json!({"jsonrpc":"2.0","id":"original-client-id","method":"eth_getLogs","params":[{"fromBlock":OLD_BLOCK,"toBlock":FUTURE_BLOCK}]}).to_string();
    let response = http
        .post(http_url)
        .header("Content-Type", "application/json")
        .body(request)
        .send()
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(response["id"], "original-client-id");
    assert!(response["error"]["code"].is_number());
    assert_ne!(response["error"]["code"], -32601);
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_rpc_real_mantle_node_http_and_ws() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let (history_url, history, requests) = start_history().await;
        let node = MantleNode::new(RollupArgs {
            historical_rpc: Some(history_url.clone()),
            ..Default::default()
        });
        let rpc = RpcServerArgs::default().with_unused_ports().with_http().with_ws();
        with_configured_mantle_node_rpc_opts(
            node, chain_spec(), payload_attributes,
            TreeConfig::default(), None, rpc,
            move |mut node, http| async move {
                let genesis = chain_spec().genesis_hash();
                node.update_forkchoice(genesis, genesis).await.unwrap();
                for _ in 0..3 {
                    let payload = node.advance_block().await.expect("mine local block");
                    let status = node.inner.add_ons_handle.beacon_engine_handle
                        .new_payload(payload.into()).await.unwrap();
                    assert!(status.is_valid(), "local Engine payload must be VALID: {status:?}");
                }
                // Canonical head and RPC cache updates are asynchronous.
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let height: Value = http.request("eth_blockNumber", rpc_params![]).await.unwrap();
                        if height == HEAD_BLOCK { break }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.expect("RPC sees the canonical head");
                eprintln!("Local Mantle node: HTTP={}, WS={}; historical backend={}",
                    node.inner.rpc_server_handle().http_url().unwrap(),
                    node.inner.rpc_server_handle().ws_url().unwrap(),
                    history_url);
                check_routes(&http, &requests, "HTTP").await;
                let ws = node.inner.rpc_server_handle().ws_client().await.expect("WS enabled");
                check_routes(&ws, &requests, "WS").await;
                check_raw_responses(
                    &node.inner.rpc_server_handle().http_url().unwrap(),
                ).await;

                let mut heads = ws.subscribe::<Value, _>("eth_subscribe", rpc_params!["newHeads"], "eth_unsubscribe").await.unwrap();
                node.advance_block().await.expect("mine subscription block");
                let head = tokio::time::timeout(Duration::from_secs(5), heads.next()).await.unwrap().unwrap().unwrap();
                assert_eq!(head["number"], NEXT_BLOCK, "WS subscriptions must follow active Reth");
                heads.unsubscribe().await.unwrap();

                history.stop().unwrap();
                history.stopped().await;
                // Backend failures fall back to local lookup. This fixture has the old block;
                // a snapshot with missing history may return null or an error.
                for reply in [http.request::<Value, _>("eth_getBlockByNumber", rpc_params![OLD_BLOCK, false]).await.unwrap(),
                    ws.request::<Value, _>("eth_getBlockByNumber", rpc_params![OLD_BLOCK, false]).await.unwrap()] {
                    assert_eq!(reply["number"], OLD_BLOCK);
                    assert!(reply.get("source").is_none());
                }
                eprintln!("PASS: HTTP/WS cutoff, historical methods, local ranges, mixed batch, subscription, backend-failure fallback");
            },
        ).await;
    }).await.expect("local historical RPC integration test completed");
}
