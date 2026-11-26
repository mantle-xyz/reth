//! Smoke test for the proofs-history sidecar wiring.
//!
//! This exists because the wiring is exactly the kind of thing that fails *silently*: the
//! `--proofs-history` flags parse fine, the node starts clean, and nothing warns you that the
//! ExEx and the RPC overrides were never installed — you find out when a historical
//! `eth_getProof` is slow in production. The test asserts on effects only the real wiring can
//! produce, and it goes through the same
//! [`with_proofs_history`](mantle_reth_cli::proofs_history::with_proofs_history) entry point the
//! shipped binary uses.

use crate::helpers::{
    mantle_payload_attributes, mantle_test_chain_spec, with_configured_mantle_node_opts,
    with_configured_mantle_node_rpc_opts,
};
use alloy_primitives::Address;
use jsonrpsee::{
    core::client::{ClientT, Error as ClientError},
    rpc_params,
};
use mantle_reth_cli::node::MantleNode;
use reth_node_api::TreeConfig;
use reth_node_core::args::RpcServerArgs;
use reth_optimism_node::args::{ProofsHistoryStorageArgs, ProofsStorageVersion, RollupArgs};
use serde_json::Value;

/// `debug_proofsSyncStatus` only exists when the debug override is installed, and the sidecar only
/// reports a window once the ExEx has actually written to it. Both halves of the wiring have to be
/// live for this to pass — which is precisely what regressed when the flags were a silent no-op.
#[tokio::test]
async fn proofs_history_wiring_serves_sidecar_rpc() {
    let sidecar = tempfile::tempdir().expect("tempdir");
    let sidecar_path = sidecar.path().join("proofs");

    let args = RollupArgs {
        proofs_history: true,
        history: ProofsHistoryStorageArgs {
            storage_path: Some(sidecar_path.clone()),
            storage_version: Some(ProofsStorageVersion::V2),
        },
        ..Default::default()
    };

    // As on the proof nodes since v1.7.0: debug on HTTP, only eth on WS. Different modules need
    // their own WS port.
    let ws_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut rpc = RpcServerArgs::default()
        .with_unused_ports()
        .with_http()
        .with_http_api("eth,net,web3,debug".parse().unwrap())
        .with_ws()
        .with_ws_api("eth".parse().unwrap());
    rpc.ws_port = ws_port.local_addr().unwrap().port();
    drop(ws_port);
    with_configured_mantle_node_rpc_opts(
        MantleNode::default(),
        mantle_test_chain_spec(),
        mantle_payload_attributes,
        TreeConfig::default(),
        Some((args, sidecar_path)),
        rpc,
        async |node, client| {
            // 1. The debug override is installed. Without the wiring this is `Method not found` —
            //    which is exactly how the silent no-op presented in production.
            let status: Value = client
                .request("debug_proofsSyncStatus", rpc_params![])
                .await
                .expect("debug_proofsSyncStatus should exist once the overrides are installed");

            let earliest = status["earliest"].as_u64().expect("earliest in window");
            let latest = status["latest"].as_u64().expect("latest in window");
            assert!(
                latest >= earliest,
                "sidecar window should be coherent, got earliest={earliest} latest={latest}"
            );

            // 2. `eth_getProof` inside the window is served from the sidecar.
            let proof: Value = client
                .request(
                    "eth_getProof",
                    rpc_params![Address::ZERO, Vec::<String>::new(), format!("0x{latest:x}")],
                )
                .await
                .expect("in-window eth_getProof should be served");
            assert!(proof["accountProof"].is_array(), "expected an account proof back");

            // 3. Below the window the sidecar refuses rather than silently falling back to the
            //    revert-based path. This is the assertion that proves the override is actually in
            //    the request path: the native implementation would happily answer here.
            if earliest > 0 {
                let below = format!("0x{:x}", earliest - 1);
                let out: Result<Value, _> = client
                    .request(
                        "eth_getProof",
                        rpc_params![Address::ZERO, Vec::<String>::new(), below],
                    )
                    .await;
                assert!(
                    out.is_err(),
                    "below-window eth_getProof must fail, not fall back to the native path"
                );
            }

            // 4. The debug overrides are not served on a transport without the debug namespace.
            let ws = node.inner.rpc_server_handle().ws_client().await.expect("ws client");
            let error = ws
                .request::<Value, _>("debug_proofsSyncStatus", rpc_params![])
                .await
                .expect_err("debug is not enabled on WS");
            assert!(
                matches!(&error, ClientError::Call(error) if error.code() == -32601),
                "debug_proofsSyncStatus must not be registered on WS: {error:?}"
            );
        },
    )
    .await;
}

/// Regression: enabling `--proofs-history` must not drop the Mantle RPC extensions.
///
/// reth keeps a single `extend_rpc_modules` hook slot. The proofs-history wiring installs its own
/// hook, which used to overwrite the one `MantleNode::add_ons` installed, so on every
/// proofs-history node the Mantle-only methods (`eth_estimateTotalFee`,
/// `eth_sendRawTransactionWithPreconf`) silently answered `-32601 Method not found` — with no
/// startup warning. The wiring now composes both, so these methods stay registered.
///
/// The assertion is only about registration: each method may still reject these dummy arguments,
/// but a registered method never returns `-32601`.
#[tokio::test]
async fn proofs_history_keeps_mantle_rpc_extensions() {
    const METHOD_NOT_FOUND: i32 = -32601;

    let sidecar = tempfile::tempdir().expect("tempdir");
    let sidecar_path = sidecar.path().join("proofs");

    let args = RollupArgs {
        proofs_history: true,
        history: ProofsHistoryStorageArgs {
            storage_path: Some(sidecar_path.clone()),
            storage_version: Some(ProofsStorageVersion::V2),
        },
        ..Default::default()
    };

    with_configured_mantle_node_opts(
        MantleNode::default(),
        mantle_test_chain_spec(),
        mantle_payload_attributes,
        TreeConfig::default(),
        Some((args, sidecar_path)),
        async |_node, client| {
            // `eth_estimateTotalFee(tx, block)` — Mantle-only; with the hook dropped this was
            // `-32601`.
            let fee: Result<Value, ClientError> = client
                .request(
                    "eth_estimateTotalFee",
                    rpc_params![serde_json::json!({
                        "from": Address::ZERO,
                        "to": Address::ZERO,
                        "value": "0x0",
                    })],
                )
                .await;
            assert_registered("eth_estimateTotalFee", &fee, METHOD_NOT_FOUND);

            // `eth_sendRawTransactionWithPreconf(raw)` — Mantle-only. With preconf disabled and no
            // sequencer configured it rejects with a different error, but it is registered.
            let preconf: Result<Value, ClientError> =
                client.request("eth_sendRawTransactionWithPreconf", rpc_params!["0x"]).await;
            assert_registered("eth_sendRawTransactionWithPreconf", &preconf, METHOD_NOT_FOUND);
        },
    )
    .await;
}

/// Asserts a JSON-RPC call reached a registered method: it either succeeded or failed with
/// something other than `-32601 Method not found`.
fn assert_registered(
    method: &str,
    out: &Result<Value, jsonrpsee::core::ClientError>,
    not_found: i32,
) {
    if let Err(jsonrpsee::core::ClientError::Call(err)) = out {
        assert_ne!(
            err.code(),
            not_found,
            "{method} must stay registered with --proofs-history, got -32601: {err}"
        );
    }
}
