//! Isolated process: `preconf.api.timeout_total` is asserted exactly, so no
//! other node test may move it.

#![recursion_limit = "1024"]
#![allow(missing_docs)]

#[path = "preconf/helpers.rs"]
#[allow(dead_code)]
pub mod helpers;

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use helpers::{PreconfCfgBuilder, send_preconf};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_metrics::recorder::install_prometheus_recorder;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const API_TIMEOUT: &str = "reth_preconf_api_timeout_total";
const DEADLINE_SKIPPED: &str = "reth_preconf_dispatch_deadline_skipped_total";

fn counter(rendered: &str, metric: &str) -> u64 {
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(metric)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or(0)
}

/// Both paths that answer a client with `Timeout` count it: the RPC deadline,
/// and dispatch refusing to apply an entry whose deadline is too close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_timeout_a_client_receives_is_counted() {
    let metrics = install_prometheus_recorder().handle();
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Dispatch gives up once less than 400ms of the 600ms budget is left, so a
    // build started 250ms after submission always lands on its pre-apply gate.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(600)
        .safety_margin_ms(400)
        .build();
    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx: alloy_primitives::Bytes = TransactionTestContext::sign_tx(
        wallet.inner.clone(),
        TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(1u64)),
            input: TransactionInput::default(),
            ..Default::default()
        },
    )
    .await
    .encoded_2718()
    .into();

    // No build runs, so the RPC deadline answers.
    let first = send_preconf(&http, raw_tx.clone()).await.expect("a timeout is not an RPC error");
    assert!(matches!(first.status, PreconfStatus::Timeout), "got {:?}", first.status);
    let rendered = metrics.render();
    assert_eq!(counter(&rendered, API_TIMEOUT), 1, "the deadline answer is counted");
    assert_eq!(counter(&rendered, DEADLINE_SKIPPED), 0);

    // Resubmitted, then a build starts past dispatch's gate: dispatch answers.
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });
    tokio::time::sleep(Duration::from_millis(250)).await;
    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed")
        .payload_id
        .expect("payload_id present");
    tokio::time::sleep(Duration::from_millis(15)).await;
    node.inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let second = rpc_task.await.expect("rpc join").expect("a timeout is not an RPC error");
    assert!(matches!(second.status, PreconfStatus::Timeout), "got {:?}", second.status);
    let rendered = metrics.render();
    assert_eq!(counter(&rendered, DEADLINE_SKIPPED), 1, "dispatch decided this one");
    assert_eq!(
        counter(&rendered, API_TIMEOUT),
        2,
        "a timeout dispatch decided is still a timeout the client received"
    );
}
