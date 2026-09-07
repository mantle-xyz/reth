//! Behavior test for `--rollup.enable-tx-pool-admission` on a sequencer-forwarding reth node.
//!
//! ## What this pins down
//!
//! A reth RPC node with a configured sequencer (`--rollup.sequencer`) forwards
//! `eth_sendRawTransaction` to the sequencer (`OpEthApi::send_transaction`). Upstream op-reth then
//! *unconditionally* re-adds the forwarded tx to its local txpool ("for local RPC usage"), which
//! diverges from op-geth: op-geth gates that retention behind `--rollup.enabletxpooladmission` and
//! leaves it **off by default** for forwarding nodes (`RollupDisableTxPoolAdmission =
//! sequencerhttp != "" && !enableFlag`).
//!
//! This test asserts the op-geth-aligned behavior we add on top of op-reth:
//! - **default** (flag off): a forwarded tx is NOT retained in the local pool — `eth_getTransaction
//!   ByHash` returns null and the `pending` nonce stays at the on-chain nonce.
//! - **flag on**: the forwarded tx IS retained — queryable locally and reflected in the `pending`
//!   nonce.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p mantle-reth-integration-tests --test it \
//!     --  txpool_admission -- --nocapture
//! ```

use crate::helpers::{
    mantle_payload_attributes, mantle_test_chain_spec, with_configured_mantle_node,
};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::{
    core::client::ClientT,
    server::{RpcModule, Server, ServerHandle},
    types::ErrorObjectOwned,
};
use mantle_reth_cli::node::MantleNode;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use reth_optimism_node::args::RollupArgs;
use serde_json::{Value, json};

/// A mock sequencer that answers `eth_sendRawTransaction` with a fixed hash and echoes it back
/// regardless of the forwarded params. Returns its `http://` URL plus the handle (kept alive by the
/// caller — dropping it stops the server).
async fn start_mock_sequencer(tx_hash: B256) -> (String, ServerHandle) {
    let server = Server::builder().build("127.0.0.1:0").await.expect("mock sequencer bind");
    let addr = server.local_addr().expect("mock sequencer local_addr");

    let mut module = RpcModule::new(tx_hash);
    module
        .register_async_method("eth_sendRawTransaction", |_params, ctx, _ext| async move {
            Ok::<Value, ErrorObjectOwned>(json!(*ctx))
        })
        .expect("register mock sequencer method");

    let handle = server.start(module);
    (format!("http://{addr}"), handle)
}

/// Sign a minimal, pool-acceptable EIP-1559 tx from the default (genesis-funded) wallet. Returns
/// the 2718-encoded bytes and the tx hash.
async fn signed_tx(chain_id: u64, wallet: &Wallet, nonce: u64) -> (Bytes, B256) {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(Address::with_last_byte(0xde))),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    let signed = TransactionTestContext::sign_tx(wallet.inner.clone(), request).await;
    let raw: Bytes = signed.encoded_2718().into();
    // For every 2718 tx type the tx hash is keccak256 of the 2718 encoding.
    let hash = keccak256(&raw);
    (raw, hash)
}

/// Drives one forwarding node with the given admission flag: submits a forwarded tx and returns
/// `(getTransactionByHash is present, pending_nonce)` observed on the same node.
async fn run_case(enable_tx_pool_admission: bool) -> (bool, u64) {
    let chain_spec = mantle_test_chain_spec();
    let chain_id = chain_spec.chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);

    let (raw_tx, tx_hash) = signed_tx(chain_id, &wallet, 0).await;
    let from = wallet.inner.address();
    let (sequencer_url, _sequencer) = start_mock_sequencer(tx_hash).await;

    let node = MantleNode::new(RollupArgs {
        sequencer: Some(sequencer_url),
        enable_tx_pool_admission,
        ..Default::default()
    });

    let result = std::sync::Arc::new(std::sync::Mutex::new((false, 0u64)));
    let result_out = result.clone();

    with_configured_mantle_node(
        node,
        chain_spec,
        mantle_payload_attributes,
        TreeConfig::default(),
        move |node, client| async move {
            let _node = node;

            // Submit via the forwarding path. reth forwards to the mock sequencer (which returns
            // `tx_hash`) and returns that hash to us, regardless of local retention.
            let returned: B256 = client
                .request("eth_sendRawTransaction", vec![json!(raw_tx)])
                .await
                .expect("forwarded tx must be accepted (mock sequencer returns a hash)");
            assert_eq!(returned, tx_hash, "node must return the sequencer's tx hash");

            // Is the forwarded tx visible in the local view (pool)? Not mined, no flashblocks, so a
            // non-null result means it was retained in the local pool.
            let by_hash: Value = client
                .request("eth_getTransactionByHash", vec![json!(tx_hash)])
                .await
                .expect("eth_getTransactionByHash call");
            let present = !by_hash.is_null();

            // pending nonce: on-chain nonce (0) unless the consecutive tx sits in the local pool.
            let count: Value = client
                .request("eth_getTransactionCount", vec![json!(from), json!("pending")])
                .await
                .expect("eth_getTransactionCount call");
            let pending_nonce = u64::from_str_radix(
                count.as_str().expect("nonce hex string").trim_start_matches("0x"),
                16,
            )
            .expect("parse pending nonce");

            *result_out.lock().unwrap() = (present, pending_nonce);
        },
    )
    .await;

    *result.lock().unwrap()
}

/// Default (flag off): a forwarding node does NOT retain the forwarded tx locally — aligning with
/// op-geth's default. `getTransactionByHash` is null and the pending nonce stays on-chain.
#[tokio::test(flavor = "multi_thread")]
async fn forwarded_tx_not_retained_by_default() {
    let (present, pending_nonce) = run_case(false).await;
    assert!(
        !present,
        "by default a forwarded tx must NOT be retained in the local pool (op-geth parity)",
    );
    assert_eq!(
        pending_nonce, 0,
        "without local retention the pending nonce must stay at the on-chain nonce",
    );
}

/// Flag on: retention is opt-in — the forwarded tx is queryable locally and bumps the pending
/// nonce, preserving the pre-existing upstream op-reth behavior for operators who want it.
#[tokio::test(flavor = "multi_thread")]
async fn forwarded_tx_retained_when_admission_enabled() {
    let (present, pending_nonce) = run_case(true).await;
    assert!(
        present,
        "with --rollup.enabletxpooladmission the forwarded tx must be retained locally",
    );
    assert_eq!(pending_nonce, 1, "a retained consecutive tx must advance the pending nonce by one",);
}
