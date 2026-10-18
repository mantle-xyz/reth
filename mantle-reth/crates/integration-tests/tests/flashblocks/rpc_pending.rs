//! `eth_` overrides answering from the pending overlay.
//!
//! The override module is built over the launched node's own `EthApi` and the
//! test's [`FlashblocksState`], the same pair `MantleNode::add_ons` wires up, and
//! its methods are called directly rather than over JSON-RPC.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types_eth::{BlockNumberOrTag, BlockOverrides, Filter};
use mantle_reth_flashblocks::EthApiOverrideServer;

use crate::consumer_helpers::{
    DEPLOY_INIT_CODE, DEPLOYED_RUNTIME, FlashblockBuilder, LOGGER, SETTER, SETTER_SLOT,
    SETTER_UNWRITTEN_SLOT, SETTER_UNWRITTEN_VALUE, SETTER_VALUE, l1_info_deposit,
    launch_flashblocks_consumer, signed_call, signed_deploy, test_sender,
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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

/// A code-less account touched by the overlay must not gain code in the overrides.
///
/// REVM pads bytecode for jump-destination analysis, so the execution-time form of
/// empty code is a trailing `STOP`. Serialising that into the pending override makes
/// `EXTCODESIZE` report 1 for a plain wallet, which flips every `isContract()` guard.
#[tokio::test(flavor = "multi_thread")]
async fn a_touched_account_without_code_stays_without_code_in_the_overrides() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    overlay_with_user_call!(harness);

    let overrides = harness
        .pending()
        .expect("overlay")
        .get_state_overrides()
        .expect("the overlay publishes state overrides");
    let sender = overrides.get(&test_sender()).expect("the sender was touched by the call");

    assert!(
        sender.code.as_ref().is_none_or(|code| code.is_empty()),
        "a wallet must not gain code, got {:?}",
        sender.code,
    );
}

/// `eth_getLogs` at `pending` falls through to the standard implementation with no overlay.
///
/// Without an overlay the override must not answer from an empty snapshot: op-reth
/// resolves `pending` to latest, so the canonical block's logs are the right answer.
#[tokio::test(flavor = "multi_thread")]
async fn get_logs_at_pending_without_an_overlay_passes_through() {
    use crate::consumer_helpers::mine_canonical_block;

    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    node.rpc.inject_tx(raw).await.expect("the pool takes the call");
    mine_canonical_block!(node);

    assert!(harness.pending().is_none(), "no slice was sent, so there is no overlay");

    let filter = Filter::new()
        .address(LOGGER)
        .from_block(BlockNumberOrTag::Pending)
        .to_block(BlockNumberOrTag::Pending);
    let logs = api.get_logs(filter).await.expect("query");

    assert_eq!(logs.len(), 1, "the standard implementation resolves `pending` to latest");
    assert_eq!(logs[0].address(), LOGGER);
}

/// `eth_call` at `pending` executes against the overlay's state overrides.
#[tokio::test(flavor = "multi_thread")]
async fn call_at_pending_executes_against_the_overlay_state() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let request = serde_json::json!({
        "from": format!("{LOGGER:#x}"),
        "to": format!("{:#x}", Address::ZERO),
        "value": format!("{VALUE:#x}"),
    });

    let gas = api
        .estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("the overlay funds the sender");
    assert!(gas >= U256::from(21_000), "a plain transfer costs at least the intrinsic gas");

    assert!(
        api.estimate_gas(request, Some(BlockNumberOrTag::Latest.into()), None, None)
            .await
            .is_err(),
        "canonically the sender cannot cover the transfer"
    );
}

/// The override version of `eth_estimateGas` has to honour `blockOverrides`.
///
/// It is declared with three parameters where upstream takes four, so a
/// caller's fourth argument never reaches the estimate — no error, just an
/// answer computed against the unmodified block. Pinned with a block gas
/// limit below the intrinsic cost of a transfer, which can only change the
/// outcome if the override arrived.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_honours_a_block_override() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let request = serde_json::json!({
        "from": format!("{LOGGER:#x}"),
        "to": format!("{:#x}", Address::ZERO),
        "value": format!("{VALUE:#x}"),
    });

    // Sanity: without the override the estimate succeeds, so a failure below
    // is the override and not the request.
    api.estimate_gas(request.clone(), Some(BlockNumberOrTag::Pending.into()), None, None)
        .await
        .expect("premise: the overlay funds this transfer");

    let starved = BlockOverrides { gas_limit: Some(20_000), ..Default::default() };
    assert!(
        api.estimate_gas(
            request,
            Some(BlockNumberOrTag::Pending.into()),
            None,
            Some(Box::new(starved)),
        )
        .await
        .is_err(),
        "a block gas limit under the 21000 intrinsic cost must make the estimate fail; \
         succeeding means the override was dropped on the way in",
    );
}

/// `eth_sendRawTransactionSync` rejects a timeout above the configured ceiling.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_rejects_an_overlong_timeout() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_call(0, LOGGER, VALUE).await;
    let error =
        api.send_raw_transaction_sync(raw, Some(6_001)).await.expect_err("the ceiling is 6000 ms");
    assert!(error.message().contains("time out too long"), "unexpected error: {error}");
}

/// `eth_sendRawTransactionSync` returns once the transaction shows up in a slice.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_sync_returns_the_flashblock_receipt() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
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

/// Opens an overlay at block 1 whose user transaction calls [`SETTER`], so the
/// overlay — and only the overlay — holds [`SETTER_SLOT`].
macro_rules! overlay_with_storage_write {
    ($harness:expr) => {{
        let raw = signed_call(0, SETTER, U256::ZERO).await;
        $harness
            .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
            .await;
        assert!($harness.pending().is_some(), "the slice should open an overlay");
    }};
}

/// `eth_getStorageAt` sees the overlay's write under `pending` only.
#[tokio::test(flavor = "multi_thread")]
async fn get_storage_at_sees_the_overlay_only_under_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_storage_write!(harness);

    let pending = api
        .storage_at(SETTER, SETTER_SLOT.into(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");
    assert_eq!(pending, B256::with_last_byte(SETTER_VALUE));

    let latest = api
        .storage_at(SETTER, SETTER_SLOT.into(), Some(BlockNumberOrTag::Latest.into()))
        .await
        .expect("query");
    assert_eq!(latest, B256::ZERO, "canonical state is untouched");
}

/// A slot the overlay never wrote falls back to canonical rather than reading
/// `0` off an absent entry — the account is in the overlay, the slot is not.
///
/// Guards the wrong-answer case, not the wiring: canonical holds the same
/// value, so this stays green with the override removed entirely. The wiring
/// proof is `get_storage_at_sees_the_overlay_only_under_pending`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unwritten_slot_of_a_touched_account_falls_back_to_canonical() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_storage_write!(harness);

    let pending = api
        .storage_at(SETTER, SETTER_UNWRITTEN_SLOT.into(), Some(BlockNumberOrTag::Pending.into()))
        .await
        .expect("query");

    assert_eq!(
        pending,
        B256::with_last_byte(SETTER_UNWRITTEN_VALUE),
        "the overlay never wrote this slot, so canonical's value stands",
    );
}

/// `eth_getCode` at `pending` serves a contract that exists only in the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn get_code_serves_a_contract_the_overlay_deployed() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    let raw = signed_deploy(0, DEPLOY_INIT_CODE).await;
    harness
        .send(FlashblockBuilder::new(1, 0).transactions(vec![l1_info_deposit(1), raw]).build())
        .await;
    assert!(harness.pending().is_some(), "the slice should open an overlay");

    let deployed = test_sender().create(0);

    let pending =
        api.get_code(deployed, Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, DEPLOYED_RUNTIME);

    let latest =
        api.get_code(deployed, Some(BlockNumberOrTag::Latest.into())).await.expect("query");
    assert_eq!(latest, Bytes::new(), "canonical state has no such contract");
}

/// A wallet the overlay touched must still report no code.
///
/// Guards the same REVM-padding trap as
/// `a_touched_account_without_code_stays_without_code_in_the_overrides`, one
/// layer further out: there the padding would be written into the override,
/// here it would be served straight to a client by `eth_getCode`. Canonical
/// agrees with the overlay here, so this cannot fail on a missing override —
/// only on a wrong one.
#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_touched_by_the_overlay_still_has_no_code_at_pending() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);
    let api = eth_api!(node, harness);

    overlay_with_user_call!(harness);

    let pending =
        api.get_code(test_sender(), Some(BlockNumberOrTag::Pending.into())).await.expect("query");
    assert_eq!(pending, Bytes::new());
}
