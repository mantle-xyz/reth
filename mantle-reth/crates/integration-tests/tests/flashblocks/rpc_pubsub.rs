//! `eth_subscribe` kinds served from flashblock state.
//!
//! A standalone jsonrpsee websocket server carries the module, so the
//! subscription sink and its notifications go over the wire exactly as they
//! would on the node's own RPC server.

use std::time::Duration;

use alloy_primitives::{U256, keccak256};
use jsonrpsee::{
    core::client::{Subscription, SubscriptionClientT},
    rpc_params,
    server::ServerBuilder,
    ws_client::WsClientBuilder,
};

use crate::helpers::{
    FlashblockBuilder, LOGGER, l1_info_deposit, launch_flashblocks_node, signed_call, test_sender,
};

/// Value the overlay's user transaction moves into [`LOGGER`].
const VALUE: U256 = U256::from_limbs([7_000_000_000_000_000_000, 0, 0, 0]);

/// Serves the pubsub module over a websocket and returns `(client, server handle)`.
macro_rules! pubsub_client {
    ($node:expr, $harness:expr) => {{
        let module =
            jsonrpsee::RpcModule::from(mantle_reth_flashblocks::EthPubSubApiServer::into_rpc(
                mantle_reth_flashblocks::EthPubSub::new(
                    $node.rpc.inner.eth_api().clone(),
                    $node.inner.task_executor.clone(),
                    std::sync::Arc::clone($harness.state()),
                ),
            ));
        let server = ServerBuilder::default()
            .build("127.0.0.1:0")
            .await
            .expect("bind the subscription server");
        let url = format!("ws://{}", server.local_addr().expect("server address"));
        let handle = server.start(module);
        let client = WsClientBuilder::default().build(&url).await.expect("connect");
        (client, handle)
    }};
}

/// Pushes the overlay's slice after the subscription is live, and returns the
/// user transaction's hash.
///
/// The sink is accepted before the stream subscribes to the flashblock
/// broadcast, and the broadcast has no replay, so the slice must not land in
/// that window.
macro_rules! announce_slice {
    ($harness:expr) => {{
        tokio::time::sleep(Duration::from_millis(500)).await;
        let raw = signed_call(0, LOGGER, VALUE).await;
        let tx_hash = keccak256(&raw);
        $harness
            .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
            .await;
        tx_hash
    }};
}

/// Awaits one notification, failing the test rather than hanging.
async fn next_item(subscription: &mut Subscription<serde_json::Value>) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), subscription.next())
        .await
        .expect("a notification should arrive")
        .expect("the subscription should stay open")
        .expect("the notification should deserialize")
}

/// `newFlashblocks` carries the whole pending block on every slice.
#[tokio::test(flavor = "multi_thread")]
async fn new_flashblocks_carries_the_pending_block() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let mut subscription: Subscription<serde_json::Value> = client
        .subscribe("eth_subscribe", rpc_params!["newFlashblocks"], "eth_unsubscribe")
        .await
        .expect("subscribe");

    let tx_hash = announce_slice!(harness);

    let block = next_item(&mut subscription).await;
    assert_eq!(block["number"], "0x1");
    let transactions = block["transactions"].as_array().expect("full transactions");
    assert_eq!(transactions.len(), 2);
    assert_eq!(transactions[1]["hash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
}

/// `pendingLogs` yields the latest slice's logs matching the filter.
#[tokio::test(flavor = "multi_thread")]
async fn pending_logs_yields_matching_logs() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let filter = serde_json::json!({ "address": format!("{LOGGER:#x}") });
    let mut subscription: Subscription<serde_json::Value> = client
        .subscribe("eth_subscribe", rpc_params!["pendingLogs", filter], "eth_unsubscribe")
        .await
        .expect("subscribe");

    let tx_hash = announce_slice!(harness);

    let log = next_item(&mut subscription).await;
    assert_eq!(log["address"].as_str().expect("address"), format!("{LOGGER:#x}"));
    assert_eq!(log["transactionHash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
}

/// `newFlashblockTransactions` defaults to hashes only.
#[tokio::test(flavor = "multi_thread")]
async fn new_flashblock_transactions_defaults_to_hashes() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let mut subscription: Subscription<serde_json::Value> = client
        .subscribe("eth_subscribe", rpc_params!["newFlashblockTransactions"], "eth_unsubscribe")
        .await
        .expect("subscribe");

    announce_slice!(harness);

    // The L1-attributes deposit leads the slice, so it is the first hash out.
    let hash = next_item(&mut subscription).await;
    assert!(hash.is_string(), "expected a bare hash, got {hash}");
}

/// `newFlashblockTransactions` with `true` yields full transactions with logs.
#[tokio::test(flavor = "multi_thread")]
async fn new_flashblock_transactions_with_true_yields_full_objects() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let mut subscription: Subscription<serde_json::Value> = client
        .subscribe(
            "eth_subscribe",
            rpc_params!["newFlashblockTransactions", true],
            "eth_unsubscribe",
        )
        .await
        .expect("subscribe");

    announce_slice!(harness);

    let transaction = next_item(&mut subscription).await;
    assert!(transaction["logs"].is_array(), "expected a full object, got {transaction}");
    assert_eq!(transaction["status"], "0x1");
    assert!(transaction["gasUsed"].is_string());
}

/// `newFlashblockTransactions` with a log filter selects only matching transactions.
#[tokio::test(flavor = "multi_thread")]
async fn new_flashblock_transactions_with_a_filter_selects_matching_transactions() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let filter = serde_json::json!({ "address": format!("{LOGGER:#x}") });
    let mut subscription: Subscription<serde_json::Value> = client
        .subscribe(
            "eth_subscribe",
            rpc_params!["newFlashblockTransactions", filter],
            "eth_unsubscribe",
        )
        .await
        .expect("subscribe");

    let tx_hash = announce_slice!(harness);

    // Only the user call emits a log at `LOGGER`; the deposit is filtered out.
    let transaction = next_item(&mut subscription).await;
    assert_eq!(transaction["hash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
    assert_eq!(transaction["from"].as_str().expect("sender"), format!("{:#x}", test_sender()));
    assert_eq!(transaction["logs"].as_array().expect("logs").len(), 1);
}

/// Standard kinds are delegated to reth's own pubsub implementation.
#[tokio::test(flavor = "multi_thread")]
async fn a_standard_kind_is_delegated() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let (client, _handle) = pubsub_client!(node, harness);

    let subscription: Subscription<serde_json::Value> = client
        .subscribe("eth_subscribe", rpc_params!["newHeads"], "eth_unsubscribe")
        .await
        .expect("newHeads must be accepted by the extended module");
    drop(subscription);
}
