//! Regression test for the `eth_sendRawTransactionWithPreconf` sequencer-forwarding path.
//!
//! ## The bug
//!
//! A reth RPC node with a configured sequencer (`--rollup.sequencer-http`) forwards
//! `eth_sendRawTransactionWithPreconf` to the sequencer and deserializes the sequencer's JSON
//! response into `mantle_reth_rpc_ext::PreconfTxEvent` (see `MantleRpcExt`). An **op-geth**
//! sequencer serializes `receipt.logs` as JSON `null` (Go nil slice) whenever the transaction has
//! no logs — most importantly for a **reverted** transaction (`core/events.go`:
//! `Logs []*Log json:"logs"`).
//!
//! Before the fix, `PreconfTxReceipt.logs` used `#[serde(default)]`, which only substitutes a
//! default for a *missing* field, not a present-but-`null` one. So reth failed to deserialize the
//! response and returned:
//!
//! ```text
//! -32000 failed to deserialise preconf event from sequencer:
//!        invalid type: null, expected a sequence
//! ```
//!
//! The client therefore never learned the transaction had reverted — it saw an opaque node error
//! instead of the preconfirmation's `failed` status and revert reason. The fix models
//! `PreconfTxReceipt.logs` as `Option<Vec<PreconfLog>>`, which deserializes `null`→`None` and
//! re-serializes `None`→`null`, so a forwarding reth node echoes the sequencer's exact JSON shape.
//!
//! ## Why a node test (not just the rpc-ext unit test)
//!
//! `mantle-reth-rpc-ext` has a unit test that deserializes the exact `logs: null` payload. This
//! test covers the *other half*: that the real forwarding path — reth's `SequencerClient` calling
//! a live sequencer over HTTP, then `MantleRpcExt` deserializing the response inside the running
//! RPC server — surfaces the sequencer's preconf event to the caller instead of a `-32000`. We
//! stand up a mock sequencer that returns the op-geth-shaped `logs: null` response and drive the
//! reth node's real RPC.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p mantle-reth-integration-tests --test it \
//!     preconf_forward_accepts_null_logs_from_sequencer -- --nocapture
//! ```

use crate::helpers::{
    mantle_payload_attributes, mantle_test_chain_spec, with_configured_mantle_node,
};
use jsonrpsee::{
    core::client::ClientT,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use reth_node_api::TreeConfig;
use reth_optimism_node::args::RollupArgs;
use serde_json::{Value, json};

/// Starts a mock sequencer that answers `eth_sendRawTransactionWithPreconf` with a fixed JSON
/// value, and returns its `http://` URL plus the server handle (kept alive by the caller).
async fn start_mock_sequencer(response: Value) -> (String, ServerHandle) {
    let server = Server::builder().build("127.0.0.1:0").await.expect("mock sequencer bind");
    let addr = server.local_addr().expect("mock sequencer local_addr");

    // The module context IS the canned response; the handler just echoes it back regardless of the
    // forwarded raw-tx params (reth forwards the hex-encoded bytes; the mock ignores them).
    let mut module = RpcModule::new(response);
    module
        .register_async_method(
            "eth_sendRawTransactionWithPreconf",
            |_params, ctx, _ext| async move { Ok::<Value, ErrorObjectOwned>((*ctx).clone()) },
        )
        .expect("register mock sequencer method");

    let handle = server.start(module);
    (format!("http://{addr}"), handle)
}

/// A reverted tx on an op-geth sequencer yields `receipt.logs == null`. reth must forward,
/// deserialize, and return the preconfirmation `failed` status + revert reason — not a `-32000`
/// deserialization error.
#[tokio::test(flavor = "multi_thread")]
async fn preconf_forward_accepts_null_logs_from_sequencer() {
    let reason = "execution reverted: ERC20: insufficient balance";
    let seq_response = json!({
        "txHash": "0x66199f44ede67884fa62012bde48a4e7823c2ce6a827f4c33e28d001a9c37cf3",
        "status": "failed",
        "reason": reason,
        "blockHeight": "0xe9be5",
        "receipt": { "logs": null }, // <-- op-geth shape for a no-log / reverted tx
    });
    let (sequencer_url, _sequencer) = start_mock_sequencer(seq_response).await;

    let node = MantleNode::new(RollupArgs { sequencer: Some(sequencer_url), ..Default::default() });

    with_configured_mantle_node(
        node,
        mantle_test_chain_spec(),
        mantle_payload_attributes,
        TreeConfig::default(),
        move |node, client| async move {
            // Keep the node (and its RPC server) alive for the duration of the test.
            let _node = node;

            // reth's forwarding path only hex-decodes the bytes and forwards them; the mock
            // sequencer ignores the payload, so any valid hex works here.
            let raw_tx = json!("0x02f8");
            let event: Value =
                client.request("eth_sendRawTransactionWithPreconf", vec![raw_tx]).await.expect(
                    "reth must deserialize a sequencer preconf response with logs:null and return \
                     the event, not a -32000 deserialization error",
                );

            assert_eq!(event["status"], "failed", "preconf status must be forwarded");
            assert_eq!(event["reason"], reason, "revert reason must be forwarded");
            assert_eq!(event["blockHeight"], "0xe9be5", "predicted block height must be forwarded");
            assert!(
                event["receipt"]["logs"].is_null(),
                "sequencer's null logs must round-trip back to null (byte-parity with op-geth), \
                 got {}",
                event["receipt"]["logs"]
            );
        },
    )
    .await;
}

/// Starts a mock sequencer that records the value of `header` on every request and answers
/// `eth_sendRawTransactionWithPreconf` with `response`. Speaks raw HTTP so the test sees the
/// request headers exactly as they arrive on the wire.
async fn start_header_recording_sequencer(
    header: &'static str,
    response: Value,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let field = |name: &str| {
                head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
                })
            };
            let len: usize = field("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            while buf.len() < head_end + len {
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            recorded.lock().unwrap().push(field(header));
            let request: Value =
                serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
            let body =
                json!({"jsonrpc": "2.0", "id": request["id"], "result": response}).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// `--rollup.sequencer-headers` must reach the sequencer on the preconf forward too, not only on
/// the `eth_sendRawTransaction` forward that op-reth wires itself.
#[tokio::test(flavor = "multi_thread")]
async fn preconf_forward_sends_configured_sequencer_headers() {
    let seq_response = json!({
        "txHash": "0x66199f44ede67884fa62012bde48a4e7823c2ce6a827f4c33e28d001a9c37cf3",
        "status": "success",
        "reason": "",
        "blockHeight": "0x2",
        "receipt": { "logs": [] },
    });
    let (sequencer_url, seen) = start_header_recording_sequencer("x-api-key", seq_response).await;

    let node = MantleNode::new(RollupArgs {
        sequencer: Some(sequencer_url),
        sequencer_headers: vec!["X-Api-Key=secret".to_string()],
        ..Default::default()
    });

    with_configured_mantle_node(
        node,
        mantle_test_chain_spec(),
        mantle_payload_attributes,
        TreeConfig::default(),
        move |node, client| async move {
            let _node = node;
            let _: Value = client
                .request("eth_sendRawTransactionWithPreconf", vec![json!("0x02f8")])
                .await
                .expect("the mock sequencer accepts every forward");

            let seen = seen.lock().unwrap().clone();
            assert!(!seen.is_empty(), "the preconf call must reach the sequencer");
            assert!(
                seen.iter().all(|value| value.as_deref() == Some("secret")),
                "every forward must carry the configured header, got {seen:?}"
            );
        },
    )
    .await;
}
