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
