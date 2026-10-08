//! Method-by-method scheme 1 checks against a second, real local archive node.

use crate::helpers::{mantle_payload_attributes, with_configured_mantle_node_rpc_opts};
use alloy_genesis::Genesis;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, TransactionInput, TransactionRequest};
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof};
use jsonrpsee::{
    core::client::{ClientT, Error as ClientError, SubscriptionClientT},
    http_client::HttpClient,
    rpc_params,
    server::{RpcModule, Server},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use op_alloy_consensus::TxDeposit;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::{args::RollupArgs, payload::OpPayloadAttrs};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

// Exercise the actual fixed mainnet cutoff, without a production test override.
pub(super) const OLD_BLOCK: &str = "0x5352a66"; // C - 1
pub(super) const LOCAL_BLOCK: &str = "0x5352a67"; // C
pub(super) const HEAD_BLOCK: &str = "0x5352a68"; // C + 1
pub(super) const NEXT_BLOCK: &str = "0x5352a69"; // C + 2
pub(super) const FUTURE_BLOCK: &str = "0x5352ac8";

const FROM: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
const CONTRACT: &str = "0x1000000000000000000000000000000000000001";
type Case = (&'static str, Vec<Value>);
type Requests = Arc<Mutex<Vec<Case>>>;

// These methods are handled locally, including requests before the cutoff.
const LOCAL_HISTORY_METHODS: &[&str] = &[
    "eth_getLogs",
    "eth_feeHistory",
    "eth_getAccount",
    "eth_getAccountInfo",
    "eth_getStorageValues",
    "eth_simulateV1",
    "eth_estimateTotalFee",
    "eth_callMany",
    "eth_callBundle",
    "eth_getBlockAccessListByBlockHash",
    "eth_getBlockAccessListByBlockNumber",
    "eth_getBlockAccessList",
    "eth_getBlockAccessListRaw",
    "debug_getRawHeader",
    "debug_getRawBlock",
    "debug_getRawReceipts",
    "debug_getRawTransactions",
    "debug_getRawTransaction",
];

fn rpc_args() -> RpcServerArgs {
    let mut args = RpcServerArgs::default()
        .with_unused_ports()
        .with_http()
        .with_ws()
        .with_api("eth,net,web3,debug,txpool,rpc".parse().unwrap());
    args.rpc_eth_proof_window = 16;
    args
}

pub(super) fn chain_spec() -> Arc<reth_optimism_chainspec::OpChainSpec> {
    let mut genesis: Value = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
    genesis["number"] = json!("0x5352a65"); // C - 2
    genesis["baseFeePerGas"] = json!("0x3b9aca00");
    // Jovian is active at genesis; its fee parameters must be present in the parent header.
    genesis["extraData"] = json!("0x0100000008000000020000000000000000");
    // Store 42 at slot 0 and emit a LOG0, so logs/storage/receipts are non-empty.
    genesis["alloc"][CONTRACT.trim_start_matches("0x")] =
        json!({"balance":"0x0", "code":"0x602a60005560006000a000", "nonce":"0x0"});
    let genesis: Genesis = serde_json::from_value(genesis).unwrap();
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

pub(super) fn payload_attributes(timestamp: u64) -> OpPayloadAttrs {
    let mut attrs = mantle_payload_attributes(timestamp);
    let mut data = vec![0u8; 178];
    data[..4].copy_from_slice(&[0x49, 0xe7, 0x23, 0x83]);
    let p = &mut data[4..];
    p[..4].copy_from_slice(&1_000_000u32.to_be_bytes());
    p[24..32].copy_from_slice(&timestamp.to_be_bytes());
    p[32..64].copy_from_slice(&U256::from(1_000_000_000u64).to_be_bytes::<32>());
    p[96..128].copy_from_slice(keccak256(timestamp.to_be_bytes()).as_slice());
    let deposit = TxDeposit {
        source_hash: keccak256(timestamp.to_be_bytes()),
        from: "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001".parse().unwrap(),
        to: TxKind::Call("0x4200000000000000000000000000000000000015".parse().unwrap()),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: false,
        input: data.into(),
        eth_value: 0,
        eth_tx_value: None,
    };
    attrs.0.transactions = Some(vec![deposit.encoded_2718().into()]);
    attrs
}

async fn signed_tx(nonce: u64) -> Bytes {
    let wallet = Wallet::default().with_chain_id(5000);
    let request = TransactionRequest {
        chain_id: Some(5000),
        nonce: Some(nonce),
        to: Some(TxKind::Call(CONTRACT.parse().unwrap())),
        gas: Some(100_000),
        max_fee_per_gas: Some(20_000_000_000),
        max_priority_fee_per_gas: Some(1_000_000_000),
        value: Some(U256::ZERO),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner, request).await.encoded_2718().into()
}

async fn outcome(client: &impl ClientT, method: &str, params: Vec<Value>) -> Value {
    match client.request::<Value, _>(method, params).await {
        Ok(result) => json!({"result":result}),
        Err(ClientError::Call(error)) => json!({"error":error}),
        Err(error) => panic!("{method}: RPC transport failed: {error}"),
    }
}

fn block_cases(number: &str, hash: &Value, tx_hash: &Value, bundle_tx: Bytes) -> Vec<Case> {
    let mut cases = Vec::new();
    for method in ["eth_getBlockByNumber", "eth_getBlockByHash"] {
        cases.push((
            method,
            vec![if method.ends_with("Hash") { hash.clone() } else { json!(number) }, json!(true)],
        ));
    }
    for method in [
        "eth_getHeaderByNumber",
        "eth_getBlockTransactionCountByNumber",
        "eth_getUncleCountByBlockNumber",
        "eth_getBlockAccessListByBlockNumber",
        "debug_getRawHeader",
        "debug_getRawBlock",
        "debug_getRawReceipts",
        "debug_getRawTransactions",
    ] {
        cases.push((method, vec![json!(number)]));
    }
    for method in [
        "eth_getHeaderByHash",
        "eth_getBlockTransactionCountByHash",
        "eth_getUncleCountByBlockHash",
        "eth_getBlockAccessListByBlockHash",
    ] {
        cases.push((method, vec![hash.clone()]));
    }
    for method in ["eth_getBlockReceipts", "eth_getBlockAccessList", "eth_getBlockAccessListRaw"] {
        cases.push((method, vec![json!(number)]));
    }
    for method in [
        "eth_getUncleByBlockNumberAndIndex",
        "eth_getTransactionByBlockNumberAndIndex",
        "eth_getRawTransactionByBlockNumberAndIndex",
    ] {
        cases.push((
            method,
            vec![json!(number), json!(if method.contains("Uncle") { "0x0" } else { "0x1" })],
        ));
    }
    for method in [
        "eth_getUncleByBlockHashAndIndex",
        "eth_getTransactionByBlockHashAndIndex",
        "eth_getRawTransactionByBlockHashAndIndex",
    ] {
        cases.push((
            method,
            vec![hash.clone(), json!(if method.contains("Uncle") { "0x0" } else { "0x1" })],
        ));
    }
    for method in [
        "eth_getTransactionByHash",
        "eth_getRawTransactionByHash",
        "eth_getTransactionReceipt",
        "debug_getRawTransaction",
    ] {
        cases.push((method, vec![tx_hash.clone()]));
    }
    for method in [
        "eth_getBalance",
        "eth_getCode",
        "eth_getTransactionCount",
        "eth_getAccount",
        "eth_getAccountInfo",
    ] {
        cases.push((method, vec![json!(CONTRACT), json!(number)]));
    }
    cases.push(("eth_getStorageAt", vec![json!(CONTRACT), json!("0x0"), json!(number)]));
    cases.push(("eth_getStorageValues", vec![json!({CONTRACT:["0x0"]}), json!(number)]));
    cases.push(("eth_getProof", vec![json!(CONTRACT), json!(["0x0", "0x1"]), json!(number)]));
    let call = json!({"from":FROM, "to":CONTRACT, "gas":"0x186a0"});
    for method in ["eth_call", "eth_estimateGas", "eth_createAccessList", "eth_estimateTotalFee"] {
        cases.push((method, vec![call.clone(), json!(number)]));
    }
    cases.push((
        "eth_simulateV1",
        vec![json!({"blockStateCalls":[{"calls":[call.clone()]}]}), json!(number)],
    ));
    cases.push((
        "eth_callMany",
        vec![json!([{ "transactions":[call.clone()] }]), json!({"blockNumber":number})],
    ));
    let next = u64::from_str_radix(number.trim_start_matches("0x"), 16).unwrap() + 1;
    cases.push(("eth_callBundle",vec![json!({"txs":[bundle_tx],"blockNumber":format!("0x{next:x}"),"stateBlockNumber":number})]));
    cases.push((
        "eth_getLogs",
        vec![json!({"fromBlock":number,"toBlock":number,"address":CONTRACT})],
    ));
    cases.push(("eth_feeHistory", vec![json!("0x1"), json!(number), json!([50.0])]));
    cases.push(("debug_traceBlockByNumber", vec![json!(number), json!({"tracer":"callTracer"})]));
    cases.push(("debug_traceBlockByHash", vec![hash.clone(), json!({"tracer":"callTracer"})]));
    cases.push(("debug_traceTransaction", vec![tx_hash.clone(), json!({"tracer":"callTracer"})]));
    cases.push(("debug_traceCall", vec![call, json!(number), json!({"tracer":"callTracer"})]));
    // Exercise JavaScript, not just the built-in native tracer.
    let js = json!({"tracer":"{steps:0,step:function(){this.steps++},fault:function(){},result:function(){return this.steps}}"});
    for (method, params) in cases.clone() {
        if method.starts_with("debug_trace") {
            let mut params = params;
            *params.last_mut().unwrap() = js.clone();
            cases.push((method, params));
        }
    }
    cases
}

fn verify_rpc_proof(proof: Value, root: B256) {
    let proof: EIP1186AccountProofResponse = serde_json::from_value(proof).unwrap();
    let account = TrieAccount {
        nonce: proof.nonce,
        balance: proof.balance,
        storage_root: proof.storage_hash,
        code_hash: proof.code_hash,
    };
    verify_proof(
        root,
        Nibbles::unpack(keccak256(proof.address)),
        Some(alloy_rlp::encode(account)),
        &proof.account_proof,
    )
    .expect("account proof matches block stateRoot");
    for storage in proof.storage_proof {
        let value = (!storage.value.is_zero()).then(|| alloy_rlp::encode(storage.value));
        verify_proof(
            proof.storage_hash,
            Nibbles::unpack(keccak256(storage.key.as_b256())),
            value,
            &storage.proof,
        )
        .expect("storage inclusion/exclusion proof matches account storageRoot");
    }
}

async fn check_cases(
    client: &impl ClientT,
    archive: &HttpClient,
    requests: &Requests,
    cases: &[Case],
    before_cutoff: bool,
    protocol: &str,
    covered: &mut BTreeSet<&'static str>,
) {
    let mut failures = Vec::new();
    for (method, params) in cases {
        let remote = before_cutoff && !LOCAL_HISTORY_METHODS.contains(method);
        requests.lock().unwrap().clear();
        let expected = outcome(archive, method, params.clone()).await;
        let actual = outcome(client, method, params.clone()).await;
        if actual != expected {
            failures.push(format!("{method}: response mismatch: {actual} != {expected}"));
        }
        if requests.lock().unwrap().is_empty() == remote {
            failures.push(format!("{method}: incorrect historical/local route"));
        }
        if *method == "eth_getProof" {
            let block: Value = archive
                .request("eth_getBlockByNumber", vec![params[2].clone(), json!(false)])
                .await
                .unwrap();
            verify_rpc_proof(
                actual["result"].clone(),
                serde_json::from_value(block["stateRoot"].clone()).unwrap(),
            );
        }
        covered.insert(method);
        eprintln!(
            "CHECK {protocol} {method} {} outcome={}",
            if remote { "historical" } else { "local" },
            expected
                .get("error")
                .map_or("success".to_string(), |error| format!("error {}", error["code"]))
        );
    }
    assert!(failures.is_empty(), "{protocol}: {failures:#?}");
}

async fn check_invalid_and_unknown(
    client: &impl ClientT,
    archive: &HttpClient,
    requests: &Requests,
    cases: &[Case],
) {
    for (method, _) in cases {
        requests.lock().unwrap().clear();
        let expected = outcome(archive, method, vec![]).await;
        assert_eq!(outcome(client, method, vec![]).await, expected, "{method}: missing parameters");
        assert!(requests.lock().unwrap().is_empty(), "invalid {method} must not reach history");
    }
    let unknown = json!(format!("0x{}", "77".repeat(32)));
    for method in [
        "eth_getTransactionByHash",
        "eth_getRawTransactionByHash",
        "eth_getTransactionReceipt",
        "debug_getRawTransaction",
    ] {
        requests.lock().unwrap().clear();
        assert_eq!(outcome(client, method, vec![unknown.clone()]).await, json!({"result":null}));
        let expected_calls = usize::from(!LOCAL_HISTORY_METHODS.contains(&method));
        assert_eq!(
            requests.lock().unwrap().len(),
            expected_calls,
            "{method}: expected history request count"
        );
    }
}

async fn check_local_methods(
    client: &impl ClientT,
    archive: &HttpClient,
    requests: &Requests,
    covered: &mut BTreeSet<&'static str>,
    protocol: &str,
) {
    let mut cases: Vec<Case> = [
        "eth_protocolVersion",
        "eth_syncing",
        "eth_coinbase",
        "eth_accounts",
        "eth_blockNumber",
        "eth_chainId",
        "eth_capabilities",
        "eth_pendingTransactions",
        "eth_gasPrice",
        "eth_maxPriorityFeePerGas",
        "eth_baseFee",
        "eth_blobBaseFee",
        "eth_mining",
        "eth_hashrate",
        "eth_getWork",
        "eth_config",
        "net_version",
        "net_peerCount",
        "net_listening",
        "web3_clientVersion",
        "rpc_modules",
        "txpool_status",
        "txpool_inspect",
        "txpool_content",
    ]
    .into_iter()
    .map(|method| (method, vec![]))
    .collect();
    cases.extend([
        ("web3_sha3", vec![json!("0x1234")]),
        ("txpool_contentFrom", vec![json!(FROM)]),
        ("eth_getTransactionBySenderAndNonce", vec![json!(FROM),json!("0x0")]),
        ("eth_fillTransaction", vec![json!({"from":FROM,"to":CONTRACT})]),
        ("eth_sign", vec![json!(FROM),json!("0x1234")]),
        ("eth_signTransaction", vec![json!({"from":FROM,"to":CONTRACT})]),
        ("eth_signTypedData", vec![json!(FROM),json!({"types":{"EIP712Domain":[]},"primaryType":"EIP712Domain","domain":{},"message":{}})]),
        ("eth_sendTransaction", vec![json!({"from":FROM,"to":CONTRACT})]),
        ("eth_sendRawTransaction", vec![json!("0x01")]),
        ("eth_sendRawTransactionSync", vec![json!("0x01"),json!(1)]),
        ("eth_sendRawTransactionWithPreconf", vec![json!("0x01")]),
        ("eth_submitHashrate", vec![json!("0x1"),json!(format!("0x{}", "00".repeat(32)))]),
        ("eth_submitWork", vec![json!("0x0000000000000000"),json!(format!("0x{}", "00".repeat(32))),json!(format!("0x{}", "00".repeat(32)))]),
    ]);
    requests.lock().unwrap().clear();
    for (method, params) in cases {
        let expected = outcome(archive, method, params.clone()).await;
        let response = outcome(client, method, params).await;
        assert_eq!(response, expected, "{protocol}: {method} local result");
        assert_ne!(response["error"]["code"], -32601, "{method} must be registered");
        assert!(requests.lock().unwrap().is_empty(), "{method} must stay local");
        covered.insert(method);
        eprintln!(
            "CHECK {protocol} {method} local outcome={}",
            response
                .get("error")
                .map_or("success".to_string(), |error| format!("error {}", error["code"]))
        );
    }
    for method in ["eth_newFilter", "eth_newBlockFilter", "eth_newPendingTransactionFilter"] {
        let params = if method == "eth_newFilter" {
            vec![json!({"fromBlock":LOCAL_BLOCK,"toBlock":HEAD_BLOCK,"address":CONTRACT})]
        } else {
            vec![]
        };
        let id: Value = client.request(method, params).await.unwrap();
        let _: Value =
            client.request("eth_getFilterChanges", rpc_params![id.clone()]).await.unwrap();
        if method == "eth_newFilter" {
            let logs: Value =
                client.request("eth_getFilterLogs", rpc_params![id.clone()]).await.unwrap();
            assert_eq!(logs.as_array().unwrap().len(), 2);
            covered.insert("eth_getFilterLogs");
        }
        let removed: bool = client.request("eth_uninstallFilter", rpc_params![id]).await.unwrap();
        assert!(removed);
        covered.extend([method, "eth_getFilterChanges", "eth_uninstallFilter"]);
        eprintln!("CHECK {protocol} {method} local outcome=success");
    }
    for method in ["eth_getFilterChanges", "eth_getFilterLogs", "eth_uninstallFilter"] {
        eprintln!("CHECK {protocol} {method} local outcome=success");
    }
    assert!(requests.lock().unwrap().is_empty(), "filter lifecycle must stay local");
    // Filters are local; this fixture has complete history.
    let filter = json!({"fromBlock":OLD_BLOCK,"toBlock":HEAD_BLOCK});
    let expected = outcome(archive, "eth_getLogs", vec![filter.clone()]).await;
    let id: Value = client.request("eth_newFilter", rpc_params![filter]).await.unwrap();
    assert_eq!(outcome(client, "eth_getFilterLogs", vec![id.clone()]).await, expected);
    assert!(client.request::<bool, _>("eth_uninstallFilter", rpc_params![id]).await.unwrap());
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_rpc_supported_methods_against_local_archive() {
    tokio::time::timeout(Duration::from_secs(120), async {
        with_configured_mantle_node_rpc_opts(MantleNode::default(), chain_spec(), payload_attributes,
            TreeConfig::default(), None, rpc_args(), move |mut source, archive| async move {
            let genesis = chain_spec().genesis_hash();
            source.update_forkchoice(genesis, genesis).await.unwrap();
            let mut payloads = Vec::new();
            for nonce in 0..3 {
                source.rpc.inject_tx(signed_tx(nonce).await).await.unwrap();
                payloads.push(source.advance_block().await.unwrap());
            }
            let old: Value = archive.request("eth_getBlockByNumber", rpc_params![OLD_BLOCK,false]).await.unwrap();
            let new: Value = archive.request("eth_getBlockByNumber", rpc_params![LOCAL_BLOCK,false]).await.unwrap();
            assert_eq!(old["transactions"].as_array().unwrap().len(), 2);
            let historical = block_cases(OLD_BLOCK, &old["hash"], &old["transactions"][1], signed_tx(1).await);
            let local = block_cases(LOCAL_BLOCK, &new["hash"], &new["transactions"][1], signed_tx(2).await);
            let requests = Requests::default();
            let server = Server::builder().build("127.0.0.1:0").await.unwrap();
            let history_url = format!("http://{}", server.local_addr().unwrap());
            let mut proxy = RpcModule::new((archive.clone(), requests.clone()));
            let mut registered = BTreeSet::new();
            for (method, _) in &historical {
                let method = *method;
                if !registered.insert(method) { continue }
                proxy.register_async_method(method, move |params, ctx, _| async move {
                    let params: Vec<Value> = params.parse()?;
                    ctx.1.lock().unwrap().push((method, params.clone()));
                    match ctx.0.request::<Value, _>(method, params).await {
                        Ok(result) => Ok(result),
                        Err(ClientError::Call(error)) => Err(error),
                        Err(error) => Err(ErrorObjectOwned::owned(-32603,error.to_string(),None::<()>)),
                    }
                }).unwrap();
            }
            let history = server.start(proxy);
            let args = RollupArgs { historical_rpc: Some(history_url), ..Default::default() };
            with_configured_mantle_node_rpc_opts(MantleNode::new(args), chain_spec(), payload_attributes,
                TreeConfig::default(), None, rpc_args(), move |mut node, http| async move {
                for payload in payloads {
                    let hash = payload.block().hash();
                    node.payload.timestamp = payload.block().header().timestamp;
                    let status = node.inner.add_ons_handle.beacon_engine_handle.new_payload(payload.into()).await.unwrap();
                    assert!(status.is_valid(), "imported Engine payload must be VALID: {status:?}");
                    node.update_forkchoice(hash, hash).await.unwrap();
                }
                let ws = node.inner.rpc_server_handle().ws_client().await.unwrap();
                let mut covered = BTreeSet::new();
                check_cases(&http, &archive, &requests, &historical, true, "HTTP", &mut covered).await;
                check_cases(&ws, &archive, &requests, &historical, true, "WS", &mut covered).await;
                check_cases(&http, &archive, &requests, &local, false, "HTTP", &mut covered).await;
                check_cases(&ws, &archive, &requests, &local, false, "WS", &mut covered).await;
                check_local_methods(&http, &archive, &requests, &mut covered, "HTTP").await;
                check_local_methods(&ws, &archive, &requests, &mut covered, "WS").await;
                check_invalid_and_unknown(&http, &archive, &requests, &historical).await;
                check_invalid_and_unknown(&ws, &archive, &requests, &historical).await;
                requests.lock().unwrap().clear();
                let raw = signed_tx(3).await;
                let pending_hash: Value = http.request("eth_sendRawTransaction",rpc_params![raw.clone()]).await.unwrap();
                assert!(requests.lock().unwrap().is_empty(), "write must stay local");
                for method in ["eth_getTransactionByHash", "eth_getRawTransactionByHash", "eth_getTransactionReceipt", "debug_traceTransaction"] {
                    requests.lock().unwrap().clear();
                    let result = outcome(&http, method, vec![pending_hash.clone()]).await;
                    // Unmined hashes for these methods are queried through history RPC.
                    let expected = outcome(&archive, method, vec![pending_hash.clone()]).await;
                    assert_eq!(result, expected, "{method}: history response for unmined hash");
                    if method != "debug_traceTransaction" {
                        assert_eq!(result, json!({"result":null}));
                    }
                    assert_eq!(requests.lock().unwrap().len(), 1, "{method}: pending hash checks history");
                    assert_eq!(outcome(&ws,method,vec![pending_hash.clone()]).await,result);
                    assert_eq!(requests.lock().unwrap().len(), 2);
                }
                requests.lock().unwrap().clear();
                let expected = json!({"result":serde_json::to_value(&raw).unwrap()});
                for client_result in [outcome(&http,"debug_getRawTransaction",vec![pending_hash.clone()]).await,
                    outcome(&ws,"debug_getRawTransaction",vec![pending_hash.clone()]).await] {
                    assert_eq!(client_result, expected, "raw debug lookup must return the pooled transaction");
                }
                assert!(requests.lock().unwrap().is_empty());
                let mut sub = ws.subscribe::<Value,_>("eth_subscribe",rpc_params!["newHeads"],"eth_unsubscribe").await.unwrap();
                node.advance_block().await.unwrap();
                let event = tokio::time::timeout(Duration::from_secs(5),sub.next()).await.unwrap().unwrap().unwrap();
                assert_eq!(event["number"],NEXT_BLOCK);
                sub.unsubscribe().await.unwrap();
                covered.extend(["eth_subscribe","eth_unsubscribe"]);
                // Verify core namespaces and the historical debug methods supported by this layer.
                let registered: BTreeSet<String> = node.inner.add_ons_handle.rpc_registry.module().method_names()
                    .map(str::to_string).collect();
                let inventory: BTreeSet<_> = registered.iter()
                    .filter(|name| name.starts_with("eth_") || name.starts_with("net_") || name.starts_with("web3_") || name.starts_with("txpool_") || *name == "rpc_modules" || matches!(name.as_str(),
                        "debug_traceBlockByNumber" | "debug_traceBlockByHash" | "debug_traceTransaction" | "debug_traceCall" |
                        "debug_getRawHeader" | "debug_getRawBlock" | "debug_getRawReceipts" | "debug_getRawTransactions" | "debug_getRawTransaction"))
                    .collect();
                let missing: Vec<_> = inventory.iter().filter(|name| !covered.contains(name.as_str())).collect();
                assert!(missing.is_empty(), "supported RPC methods missing from matrix: {missing:?}");
                let other_debug: Vec<_> = registered.iter().filter(|name| name.starts_with("debug_") && !covered.contains(name.as_str())).collect();
                eprintln!("DEBUG_OUTSIDE_MATRIX {}", serde_json::to_string(&other_debug).unwrap());
                eprintln!("PASS RPC matrix: {} methods; historical cutoff routes, local extensions and ranges, HTTP and WS", covered.len());
                history.stop().unwrap();
                history.stopped().await;
            }).await;
        }).await;
    }).await.expect("supported local RPC matrix finished");
}
