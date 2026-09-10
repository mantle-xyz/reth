//! `eth_` overrides answering from the pending overlay.
//!
//! The override module is built over the launched node's own `EthApi` and the
//! test's [`FlashblocksState`], the same pair `MantleNode::add_ons` wires up, and
//! its methods are called directly rather than over JSON-RPC.

use alloy_primitives::{Address, U256, keccak256};
use alloy_rpc_types_eth::{BlockNumberOrTag, Filter};
use mantle_reth_flashblocks::EthApiOverrideServer;

use crate::helpers::{
    FlashblockBuilder, LOGGER, l1_info_deposit, launch_flashblocks_node, signed_call, test_sender,
};

/// Value the overlay's user transaction moves into [`LOGGER`].
const VALUE: U256 = U256::from_limbs([7_000_000_000_000_000_000, 0, 0, 0]);

/// The override module bound to the node's `EthApi` and the harness's state.
macro_rules! eth_api {
    ($node:expr, $harness:expr) => {
        mantle_reth_flashblocks::EthApiExt::new(
            $node.rpc.inner.eth_api().clone(),
            $node.rpc.inner.eth_handlers().filter.clone(),
            std::sync::Arc::clone($harness.state()),
        )
    };
}

/// `"pending"` as the override trait's block tag.
fn pending_tag() -> mantle_reth_flashblocks::BlockNumberOrTagExt {
    serde_json::from_value(serde_json::json!("pending")).expect("pending is a valid tag")
}

/// Opens an overlay at block 1 holding the L1-attributes deposit and one call to
/// [`LOGGER`] carrying [`VALUE`]; returns the call's hash.
macro_rules! overlay_with_user_call {
    ($harness:expr) => {{
        let raw = signed_call(0, LOGGER, VALUE).await;
        let tx_hash = keccak256(&raw);
        $harness
            .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
            .await;
        assert!($harness.pending().is_some(), "the slice should open an overlay");
        tx_hash
    }};
}

/// `eth_getBlockByNumber("pending")` serves the overlay, and passes `pending`
/// through to the standard implementation when there is none.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_by_number_serves_the_overlay_and_passes_pending_through() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    let fallback = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the standard implementation resolves the tag");
    assert_eq!(
        fallback["number"], "0x0",
        "without an overlay, op-reth resolves `pending` to latest, as op-geth does"
    );

    let tx_hash = overlay_with_user_call!(harness);

    let block = api
        .block_by_number(pending_tag(), false)
        .await
        .expect("query succeeds")
        .expect("the overlay is served");
    assert_eq!(block["number"], "0x1");
    let hashes = block["transactions"].as_array().expect("transaction hashes");
    assert_eq!(hashes.len(), 2, "the L1-attributes deposit and the user call");
    assert_eq!(hashes[1].as_str().expect("hash"), format!("{tx_hash:#x}"));
}

/// `eth_getBlockTransactionCountByNumber("pending")` counts the overlay's tip block.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_transaction_count_counts_the_overlay_tip() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let count = api
        .get_block_transaction_count_by_number(BlockNumberOrTag::Pending)
        .await
        .expect("query succeeds")
        .expect("the overlay is served");
    assert_eq!(count, U256::from(2));
}

/// A transaction that exists only in the overlay is served by hash, with a receipt.
#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_in_the_overlay_is_served_by_hash_and_has_a_receipt() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    let tx_hash = overlay_with_user_call!(harness);

    let transaction = api
        .transaction_by_hash(tx_hash)
        .await
        .expect("query succeeds")
        .expect("the overlay holds the transaction");
    assert_eq!(transaction["hash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
    assert_eq!(transaction["from"].as_str().expect("sender"), format!("{:#x}", test_sender()));

    let receipt = api
        .get_transaction_receipt(tx_hash)
        .await
        .expect("query succeeds")
        .expect("the overlay holds the receipt");
    assert_eq!(receipt["status"], "0x1", "the call must have succeeded");
}

/// `eth_getBalance` sees the overlay's transfer under `pending` only.
#[tokio::test(flavor = "multi_thread")]
async fn get_balance_sees_the_overlay_only_under_pending() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending =
        api.get_balance(LOGGER, Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, VALUE);

    let latest =
        api.get_balance(LOGGER, Some(BlockNumberOrTag::Latest.into())).await.expect("query");
    assert_eq!(latest, U256::ZERO, "canonical state is untouched");
}

/// `eth_getTransactionCount` adds the overlay's nonce bumps to the canonical count.
#[tokio::test(flavor = "multi_thread")]
async fn get_transaction_count_adds_the_overlay_to_canonical() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending = api
        .get_transaction_count(test_sender(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");
    assert_eq!(pending, U256::from(1));

    let latest = api
        .get_transaction_count(test_sender(), Some(BlockNumberOrTag::Latest.into()))
        .await
        .expect("query");
    assert_eq!(latest, U256::ZERO, "canonical state is untouched");
}

/// `eth_getLogs` appends the overlay's logs when the range ends at `pending`.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_appends_the_overlay_when_the_range_ends_at_pending() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending_filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(pending_filter).await.expect("query");
    assert_eq!(logs.len(), 1, "the overlay's call emits one log");
    assert_eq!(logs[0].address(), LOGGER);

    let canonical_filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Earliest)
        .to_block(BlockNumberOrTag::Latest);
    assert!(
        api.get_logs(canonical_filter).await.expect("query").is_empty(),
        "canonical state has no logs"
    );
}

/// `eth_call` at `pending` executes against the overlay's state overrides.
#[tokio::test(flavor = "multi_thread")]
async fn call_at_pending_executes_against_the_overlay_state() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    // `LOGGER` returns its own balance, which only the overlay has credited.
    let request = serde_json::json!({ "to": format!("{LOGGER:#x}") });

    let pending = api
        .call(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("call succeeds");
    assert_eq!(U256::from_be_slice(&pending), VALUE);

    let latest = api
        .call(request, Some(BlockNumberOrTag::Latest.into()), None, None)
        .await
        .expect("call succeeds");
    assert_eq!(U256::from_be_slice(&latest), U256::ZERO);
}

/// `eth_estimateGas` at `pending` resolves against the overlay, so a sender
/// funded only there can be estimated for.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_at_pending_sees_the_overlay_balance() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let request = serde_json::json!({
        "from": format!("{LOGGER:#x}"),
        "to": format!("{:#x}", Address::ZERO),
        "value": format!("{VALUE:#x}"),
    });

    let gas = api
        .estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None)
        .await
        .expect("the overlay funds the sender");
    assert!(gas >= U256::from(21_000), "a plain transfer costs at least the intrinsic gas");

    assert!(
        api.estimate_gas(request, Some(BlockNumberOrTag::Latest.into()), None).await.is_err(),
        "canonically the sender cannot cover the transfer"
    );
}

/// `eth_sendRawTransactionSync` rejects a timeout above the configured ceiling.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_rejects_an_overlong_timeout() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    let error =
        api.send_raw_transaction_sync(raw, Some(6_001)).await.expect_err("the ceiling is 6000 ms");
    assert!(error.message().contains("time out too long"), "unexpected error: {error}");
}

/// `eth_sendRawTransactionSync` returns once the transaction shows up in a slice.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_returns_the_flashblock_receipt() {
    let (harness, node) = launch_flashblocks_node!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    let tx_hash = keccak256(&raw);
    let slice =
        FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw.clone()]).build();

    let announce = async {
        // The call subscribes to the flashblock broadcast before waiting; the
        // broadcast has no replay, so the slice must not land before it does.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        harness.send(slice).await;
    };
    let (receipt, ()) = tokio::join!(api.send_raw_transaction_sync(raw, None), announce);

    let receipt = receipt.expect("the slice carries the transaction");
    assert_eq!(receipt["transactionHash"].as_str().expect("hash"), format!("{tx_hash:#x}"));
    assert_eq!(receipt["status"], "0x1");
}
