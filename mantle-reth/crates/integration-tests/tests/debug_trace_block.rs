//! `debug_traceBlock*` reports a transaction whose trace fails as `{"error": …}` for that
//! transaction alone and traces the rest of the block, as op-geth does. Upstream reth fails the
//! whole call instead.

use crate::helpers::{mantle_payload_attributes, mantle_test_chain_spec, with_mantle_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use serde_json::{Value, json};

/// The transfer whose trace the tracer below makes fail.
const FAILING_TO: Address = address!("00000000000000000000000000000000000000aa");
/// A transfer mined after the failing one, to check the block goes on from the right state.
const LATER_TO: Address = address!("00000000000000000000000000000000000000bb");

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64, to: Address) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1_000)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// `debug_traceBlockByNumber` with `tracer`.
async fn trace_block(client: &HttpClient, number: u64, tracer: &str) -> Vec<Value> {
    client
        .request(
            "debug_traceBlockByNumber",
            rpc_params![format!("{number:#x}"), json!({ "tracer": tracer })],
        )
        .await
        .expect("debug_traceBlockByNumber must not fail the whole call")
}

/// `debug_traceBlockByHash` and `debug_traceBlock` (raw RLP) with `tracer`.
async fn trace_block_other_ways(
    client: &HttpClient,
    hash: B256,
    rlp: Bytes,
    tracer: &str,
) -> [Vec<Value>; 2] {
    let by_hash = client
        .request("debug_traceBlockByHash", rpc_params![hash, json!({ "tracer": tracer })])
        .await
        .expect("debug_traceBlockByHash must not fail the whole call");
    let raw = client
        .request("debug_traceBlock", rpc_params![rlp, json!({ "tracer": tracer })])
        .await
        .expect("debug_traceBlock must not fail the whole call");
    [by_hash, raw]
}

fn entry(traces: &[Value], hash: B256) -> &Value {
    let hash = format!("{hash:#x}");
    traces.iter().find(|t| t["txHash"] == hash).expect("every transaction has an entry")
}

#[tokio::test]
async fn trace_block_reports_a_failed_transaction_trace_per_transaction() {
    let chain_spec = mantle_test_chain_spec();
    let chain_id = chain_spec.chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);

    with_mantle_node(
        chain_spec,
        mantle_payload_attributes,
        TreeConfig::default(),
        move |node, client| async move {
            let mut node = node;
            let failing = node
                .rpc
                .inject_tx(signed_transfer(chain_id, &wallet, 0, FAILING_TO).await)
                .await
                .expect("first transfer admitted");
            let later = node
                .rpc
                .inject_tx(signed_transfer(chain_id, &wallet, 1, LATER_TO).await)
                .await
                .expect("second transfer admitted");
            let block = node.advance_block().await.expect("mine both transfers");
            node.sync_to(block.block().hash()).await.expect("settle block");
            let number = block.block().header().number;
            let rlp: Bytes = alloy_rlp::encode(block.block().clone_block()).into();

            // Each transaction reports the sender's balance after it ran; the throwing variant
            // fails the trace of the transfer to `FAILING_TO`.
            let reporting = "{step:function(){},fault:function(){},\
                result:function(ctx,db){return db.getBalance(ctx.from).toString()}}";
            let throwing = "{step:function(){},fault:function(){},\
                result:function(ctx,db){if(toHex(ctx.to)=='0x00000000000000000000000000000000000000aa')\
                throw new Error('boom');return db.getBalance(ctx.from).toString()}}";

            let reference = trace_block(&client, number, reporting).await;
            let traces = trace_block(&client, number, throwing).await;

            assert_eq!(traces.len(), reference.len(), "one entry per transaction");
            let failed = entry(&traces, failing);
            assert!(failed.get("result").is_none(), "the failed trace has no result: {failed}");
            let error = failed["error"].as_str().expect("the failed trace has an error");
            assert!(error.contains("boom"), "the error is the tracer's: {error}");

            // Every other transaction, the later transfer included, traces as if nothing failed:
            // the block went on from the state the failed transaction actually left.
            for trace in traces.iter().filter(|t| t["txHash"] != format!("{failing:#x}")) {
                let hash: B256 = serde_json::from_value(trace["txHash"].clone()).unwrap();
                assert_eq!(trace, entry(&reference, hash), "trace of {hash} is unaffected");
            }
            assert!(entry(&traces, later)["result"].is_string(), "the later transfer is traced");

            // The other two block-tracing methods behave the same.
            for other in trace_block_other_ways(&client, block.block().hash(), rlp, throwing).await
            {
                assert_eq!(other, traces);
            }
        },
    )
    .await;
}
