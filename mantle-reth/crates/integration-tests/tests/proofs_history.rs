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
};
use alloy_primitives::Address;
use jsonrpsee::{core::client::ClientT, rpc_params};
use mantle_reth_cli::node::MantleNode;
use reth_node_api::TreeConfig;
use reth_optimism_node::args::{ProofsStorageVersion, RollupArgs};
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
        proofs_history_storage_path: Some(sidecar_path.clone()),
        proofs_history_storage_version: ProofsStorageVersion::V2,
        ..Default::default()
    };

    with_configured_mantle_node_opts(
        MantleNode::default(),
        mantle_test_chain_spec(),
        mantle_payload_attributes,
        TreeConfig::default(),
        Some((args, sidecar_path)),
        async |_node, client| {
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
        },
    )
    .await;
}
