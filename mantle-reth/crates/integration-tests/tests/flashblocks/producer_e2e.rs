//! `producer_e2e`: a real node, slicing a real block, seen by a real
//! subscriber.
//!
//! Everything below this point in the stack has unit tests; none of them
//! execute the slice path, because the payload builder only slices when the
//! node wiring hands it a publisher. This is the first test that does.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use futures_util::StreamExt;
use jsonrpsee::core::client::ClientT;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use mantle_reth_preconf::flashblocks::FlashblockProducerConfig;
use op_alloy_consensus::DEPOSIT_TX_TYPE_ID;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_provider::BlockNumReader;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::helpers::PreconfCfgBuilder;

type Subscriber = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Slice twice as fast as production so one block produces several of them
/// inside a test's patience, and let the OS pick the port so concurrent tests
/// never collide. Everything else is the shipped default — notably the leeway,
/// which these tests have no reason to move.
fn flashblocks_cfg() -> FlashblockProducerConfig {
    FlashblockProducerConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        block_time: Duration::from_millis(100),
        ..Default::default()
    }
}

async fn signed_transfer(wallet: &Wallet, chain_id: u64, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

async fn subscribe(addr: std::net::SocketAddr) -> Subscriber {
    let (stream, _) =
        connect_async(format!("ws://{addr}")).await.expect("subscriber connects to the publisher");
    stream
}

/// Drain whatever the publisher has sent within `window`.
///
/// Bounded by time rather than by count: how many slices a block produces is
/// what these tests measure, so demanding a fixed number up front would either
/// hang or bake in the answer.
async fn drain(subscriber: &mut Subscriber, window: Duration) -> Vec<MantleFlashblockPayload> {
    let mut slices = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, subscriber.next()).await {
        if let Message::Text(text) = message {
            slices.push(
                serde_json::from_str::<MantleFlashblockPayload>(&text)
                    .expect("the publisher emits decodable slices"),
            );
        }
    }
    slices
}

/// Drive one block the way every preconf suite does: hand-rolled FCU, a pause
/// long enough for the build loop to tick, then resolve.
///
/// `advance_block()` is not usable here — it waits on an event pair the preconf
/// payload builder does not drive, and times out.
macro_rules! build_one_block {
    ($node:expr, $pause:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present");

        // The build loop is slicing throughout this window.
        tokio::time::sleep($pause).await;

        $node
            .inner
            .payload_builder_handle
            // `PreconfPayloadJob::resolve_kind` ignores the kind: it signals
            // cancel and waits for the build task either way. Passing the
            // default keeps this in step with the preconf suites.
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build")
    }};
}

/// Send one transaction, hold a block open for six tick intervals, and collect
/// what the subscriber saw.
async fn one_sliced_block(
    pause: Duration,
) -> (Vec<MantleFlashblockPayload>, u64, reth_optimism_node::OpBuiltPayload) {
    // Not `all_preconfs`: that marks every transaction preconf-eligible, and
    // the pool arm skips those on purpose. A plain pool transaction is what
    // exercises slicing.
    // The address comes from the key, not the chain id, so no id is needed
    // here — and the harness has not been launched yet to tell us one.
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let mut subscriber = subscribe(fb_addr).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    // Pool validation is asynchronous; give it a moment before the build starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let parent = node.inner.provider.best_block_number().expect("best block number");
    let payload = build_one_block!(node, pause);
    let slices = drain(&mut subscriber, Duration::from_millis(500)).await;

    (slices, parent + 1, payload)
}

/// One block, sliced: a subscriber sees an opening slice carrying the block's
/// header fields, then more, all naming the same block and each pointing at
/// the one before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_sees_the_block_arrive_in_slices() {
    let (slices, block_number, _payload) = one_sliced_block(Duration::from_millis(600)).await;

    assert!(!slices.is_empty(), "the block was built but no slice reached the subscriber");

    // The opening slice is index 0, and it is the only one carrying `base`.
    assert_eq!(slices[0].index, 0, "the first slice a subscriber sees opens the block");
    assert!(slices[0].base.is_some(), "the opening slice carries the block-level fields");
    assert!(
        slices[1..].iter().all(|slice| slice.base.is_none()),
        "later slices would only repeat what the subscriber already has",
    );

    // Every slice names the same block, and it is the one being built.
    assert!(
        slices.iter().all(|slice| slice.metadata.block_number == block_number),
        "every slice of one block names that block; got {:?}",
        slices.iter().map(|slice| slice.metadata.block_number).collect::<Vec<_>>(),
    );

    // Indices run consecutively from zero — a subscriber counts on that to know
    // it has missed nothing.
    let indices: Vec<u64> = slices.iter().map(|slice| slice.index).collect();
    assert_eq!(
        indices,
        (0..slices.len() as u64).collect::<Vec<_>>(),
        "slice indices run consecutively from zero",
    );

    // Each slice points at its predecessor, so a gap is detectable.
    for pair in slices.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        assert_eq!(
            current.metadata.prev_flashblock_id.block_number, previous.metadata.block_number,
            "a slice points at the block its predecessor belonged to",
        );
        assert_eq!(
            current.metadata.prev_flashblock_id.index, previous.index,
            "a slice points at the index of its predecessor",
        );
    }

    // Nothing was published before this producer started, so the very first
    // slice it ever sent has no predecessor to name.
    assert!(
        slices[0].metadata.prev_flashblock_id.is_no_prev(),
        "the first slice of a freshly started producer points at no predecessor",
    );
}

/// The ticker fires on its own schedule, so a block held open long enough
/// arrives as more than an opening and a closing slice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holding_a_block_open_produces_a_slice_per_tick() {
    let (slices, _block_number, _payload) = one_sliced_block(Duration::from_millis(600)).await;

    assert!(
        slices.len() >= 4,
        "six tick intervals should yield more than an opening and a closing slice, got {}",
        slices.len(),
    );
}

/// The invariant the whole producer rests on: the transactions a subscriber
/// has been shown, concatenated in order, are a prefix of the block that gets
/// sealed. Anything else means a consumer displayed something that never
/// landed.
///
/// A **strict** prefix is allowed and expected. Every build ends by being
/// cancelled, so nothing is published after the last tick that completed
/// before the payload was resolved — up to one interval of the block's tail
/// reaches subscribers only when the block does. Asserting equality here would
/// be asserting a timing coincidence: it holds only while every transaction
/// happens to land before that last tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn published_slices_are_a_prefix_of_the_sealed_block() {
    let (slices, _block_number, payload) = one_sliced_block(Duration::from_millis(600)).await;

    let published: Vec<Bytes> =
        slices.iter().flat_map(|slice| slice.diff.transactions.iter().cloned()).collect();
    let sealed: Vec<Bytes> =
        payload.block().body().transactions().map(|tx| Bytes::from(tx.encoded_2718())).collect();

    assert!(
        !published.is_empty(),
        "the block carried transactions but the subscriber was shown none",
    );
    assert!(
        published.len() <= sealed.len(),
        "subscribers were shown {} transactions but the block sealed only {}",
        published.len(),
        sealed.len(),
    );
    assert_eq!(
        published,
        sealed[..published.len()],
        "what subscribers were shown is not a prefix of what was sealed",
    );
}

/// A tick interval wide enough to place work in the tail without racing.
///
/// Mid-build arrivals are only picked up when the pool iterator is rebuilt,
/// which happens on a tick — so the tail cannot be reached by simply silencing
/// the ticker: with no tick the transactions never execute at all, and there is
/// nothing to miss. What is needed is one tick to pick them up and a long gap
/// before the next, leaving a window in which they execute and the payload is
/// resolved. A second is enough to make that window hundreds of milliseconds
/// wide rather than a coin flip.
fn wide_enough_to_have_a_tail() -> FlashblockProducerConfig {
    FlashblockProducerConfig { block_time: Duration::from_secs(1), ..flashblocks_cfg() }
}

/// Transactions that arrive after the last tick still reach subscribers.
///
/// The build loop has no end of its own — it runs until `getPayload` cancels it
/// — so whatever executes between the final tick and that cancel would be in
/// the sealed block and in no slice. Measured before this was closed: five
/// transactions injected mid-build, five sealed, four slices carrying none.
///
/// The reference implementation has no such window, which is why it needs no
/// closing slice: its loop stops on its own once the budgeted count is reached,
/// so everything is out by the time it finalizes. Ours keeps ticking until
/// asked for the block, and settles up at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_arriving_after_the_last_tick_still_reach_subscribers() {
    let cfg = PreconfCfgBuilder::new().build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, wide_enough_to_have_a_tail()).await;
    let mut subscriber = subscribe(fb_addr).await;

    // Open the block first: only transactions injected after this point can
    // execute past the opening slice.
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

    for nonce in 0..5u64 {
        let raw = signed_transfer(&wallet, chain_id, nonce).await;
        node.rpc.inject_tx(raw).await.expect("inject");
    }
    // Past the tick that rebuilds the pool iterator and lets these execute,
    // with most of a second still to go before the next one — so they are in
    // the block, and in no slice yet.
    tokio::time::sleep(Duration::from_millis(1_300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let published: Vec<B256> = drain(&mut subscriber, Duration::from_millis(300))
        .await
        .iter()
        .flat_map(|slice| slice.diff.transactions.iter())
        .map(keccak256)
        .collect();
    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(Bytes::from(tx.encoded_2718())))
        .collect();

    // The premise: with nothing in the tail there would be nothing to miss, and
    // the assertion below would hold for the wrong reason.
    assert_eq!(sealed.len(), 5, "the block was supposed to carry all five");
    assert_eq!(
        published,
        sealed,
        "subscribers saw {} of the block's {} transactions; the tail never went out",
        published.len(),
        sealed.len(),
    );
}

/// A build nobody asked for publishes no closing slice.
///
/// The closing slice exists because the payload was resolved and its
/// transactions are therefore on their way to the chain. A build that was
/// thrown away hands them back to the pool instead, so announcing them would be
/// announcing a block that is going nowhere — the one thing the publish guard
/// exists to prevent, and the guard cannot help here because the closing slice
/// deliberately runs after the cancel.
///
/// Counts slices rather than transactions: an ordinary tick publishes the same
/// transactions quite legitimately while the build is still live, so their
/// presence says nothing. What must not appear is another slice *after* the
/// build was thrown away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_build_publishes_no_closing_slice() {
    let cfg = PreconfCfgBuilder::new().build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, wide_enough_to_have_a_tail()).await;
    let mut subscriber = subscribe(fb_addr).await;

    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    node.inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed");

    for nonce in 0..5u64 {
        let raw = signed_transfer(&wallet, chain_id, nonce).await;
        node.rpc.inject_tx(raw).await.expect("inject");
    }
    // Same window as the test above: past the tick that lets them execute, well
    // before the next one.
    tokio::time::sleep(Duration::from_millis(1_300)).await;

    // Drained and discarded: whatever ticks published while the build was live
    // is legitimate, and only what comes *after* it is thrown away is at issue.
    // The payload id is what separates them — the replacement build has its
    // own, and is free to publish these transactions again.
    let abandoned_payload = drain(&mut subscriber, Duration::from_millis(200))
        .await
        .last()
        .map(|slice| slice.payload_id)
        .expect("the live build published at least the opening slice");

    // A second job on the same parent throws the first away without ever
    // asking for its payload.
    let next = node.payload.next_attributes();
    node.inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(next))
        .await
        .expect("second FCU must succeed");

    // Generous: the abandoned build is mid-`await` when the second FCU lands,
    // and a slice that leaks out does so only once it unwinds. Too short a
    // window here and the test passes by not having looked yet.
    let leaked: Vec<u64> = drain(&mut subscriber, Duration::from_millis(1_500))
        .await
        .iter()
        .filter(|slice| slice.payload_id == abandoned_payload)
        .map(|slice| slice.index)
        .collect();

    assert!(
        leaked.is_empty(),
        "the build was thrown away, but it published slice(s) {leaked:?} afterwards",
    );
}

/// Slices have to keep coming while the pool arm has work.
///
/// That arm is level-triggered — `ready(())` guarded only by headroom — so it
/// is ready again the instant it finishes a transaction. `--preconf.all` makes
/// every pool transaction preconf-eligible, which the arm skips and then
/// immediately retries, so it is the configuration under which the arm is
/// readiest and the ticker is likeliest to be starved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticks_keep_coming_while_the_pool_arm_has_work() {
    let cfg = PreconfCfgBuilder::new().all_preconfs().build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let mut subscriber = subscribe(fb_addr).await;

    // Several senders so the pool always has a candidate to offer.
    for nonce in 0..3u64 {
        let raw = signed_transfer(&wallet, chain_id, nonce).await;
        let _ = node.rpc.inject_tx(raw).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let _payload = build_one_block!(node, Duration::from_millis(600));
    let slices = drain(&mut subscriber, Duration::from_millis(500)).await;

    assert!(
        slices.len() >= 4,
        "the ticker was starved by the pool arm: six tick intervals produced {} slices",
        slices.len(),
    );
}

/// A derivation build must publish nothing.
///
/// `no_tx_pool=true` means the block has to reproduce exactly what every other
/// node derives from L1 data. A subscriber watching a block being replayed
/// rather than built has nothing to gain from seeing it in pieces, and the
/// slices would describe a block the network already has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_derivation_build_publishes_nothing() {
    // The address comes from the key, not the chain id, so no id is needed
    // here — and the harness has not been launched yet to tell us one.
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let mut subscriber = subscribe(fb_addr).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut attrs = node.payload.next_attributes();
    attrs.0.no_tx_pool = Some(true);
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

    // Long enough that several ticks would have fired on a normal build.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let slices = drain(&mut subscriber, Duration::from_millis(500)).await;

    assert!(
        slices.is_empty(),
        "a derivation build published {} slices; it must publish none",
        slices.len(),
    );
}

/// The opening slice goes out before any pool or preconf transaction has been
/// applied, so it carries only what is fixed for the block regardless of what
/// else lands.
///
/// This is what publishing it *before* the carryover preamble buys: with the
/// order the other way round, replayed commitments would be folded into it.
/// Only the pool half is covered here — a journal-restored commitment would
/// need the restart fixture the preconf suite uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_opening_slice_predates_every_pool_transaction() {
    let (slices, _block_number, payload) = one_sliced_block(Duration::from_millis(600)).await;

    let sealed: Vec<Bytes> =
        payload.block().body().transactions().map(|tx| Bytes::from(tx.encoded_2718())).collect();
    let pool_transactions: Vec<&Bytes> = sealed
        .iter()
        .filter(|raw| raw.first().is_some_and(|tag| *tag != DEPOSIT_TX_TYPE_ID))
        .collect();

    assert!(
        !pool_transactions.is_empty(),
        "the block carried no pool transaction, so this proves nothing",
    );
    for raw in pool_transactions {
        assert!(
            !slices[0].diff.transactions.contains(raw),
            "the opening slice carries a transaction the pool arm applied after it",
        );
    }
}

/// Nothing goes out once the payload has been resolved.
///
/// A subscriber told about a slice after the block was handed to the consensus
/// layer has no way to unsee it, and by then the producer has no say in what
/// that block contains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_slice_is_published_after_the_payload_is_resolved() {
    // The address comes from the key, not the chain id, so no id is needed
    // here — and the harness has not been launched yet to tell us one.
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (mut node, _http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let mut subscriber = subscribe(fb_addr).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let _payload = build_one_block!(node, Duration::from_millis(600));

    // Everything published while the block was open.
    let during = drain(&mut subscriber, Duration::from_millis(500)).await;
    assert!(!during.is_empty(), "the block was built but nothing was published");

    // And nothing after. Several tick intervals' worth of waiting, so a slice
    // that leaked past the guard would have arrived.
    let after = drain(&mut subscriber, Duration::from_millis(500)).await;

    assert!(
        after.is_empty(),
        "{} slices were published after the payload was resolved: {:?}",
        after.len(),
        after.iter().map(|slice| slice.index).collect::<Vec<_>>(),
    );
}

/// Slicing changes what subscribers see, not what this node answers.
///
/// The producer's own `eth_*` surface is deliberately untouched: `pending`
/// still means what it meant before flashblocks existed. A consumer that wants
/// the slice view subscribes to the stream; wiring slices into the sequencer's
/// own RPC would give two different answers to the same question depending on
/// which node you asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_producers_own_rpc_does_not_answer_from_slices() {
    // The address comes from the key, not the chain id, so no id is needed
    // here — and the harness has not been launched yet to tell us one.
    let sender = Wallet::default().inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_pair(sender, RECIPIENT).build();
    let (mut node, http, wallet, chain_id, fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let mut subscriber = subscribe(fb_addr).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let injected = node.rpc.inject_tx(raw).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

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

    // Mid-build: several ticks have fired, so the transaction is in a slice
    // the subscriber has already been shown.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let receipt: Option<serde_json::Value> = http
        .request("eth_getTransactionReceipt", vec![format!("{injected:#x}")])
        .await
        .expect("receipt query answers");

    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let slices = drain(&mut subscriber, Duration::from_millis(500)).await;
    let published: Vec<Bytes> =
        slices.iter().flat_map(|slice| slice.diff.transactions.iter().cloned()).collect();

    assert!(!published.is_empty(), "the transaction never reached a slice, so this proves nothing",);
    assert!(
        receipt.is_none(),
        "the node answered with a receipt for a transaction that was only in a slice: {receipt:?}",
    );
}

/// `gas_used` on a slice is the block's running total, not the slice's own.
///
/// A consumer reads it straight into the header field it is reconstructing, so
/// it has to be non-decreasing across the sequence and has to land on the
/// sealed block's figure. Sending per-slice deltas instead would make every
/// consumer sum them itself, and any missed slice would corrupt the total
/// silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gas_used_accumulates_across_slices_and_lands_on_the_block() {
    let (slices, _block_number, payload) = one_sliced_block(Duration::from_millis(600)).await;

    let reported: Vec<u64> = slices.iter().map(|slice| slice.diff.gas_used).collect();
    assert!(
        reported.windows(2).all(|pair| pair[0] <= pair[1]),
        "gas_used went backwards across slices: {reported:?}",
    );

    let block_gas_limit = payload.block().header().gas_limit;
    assert!(
        reported.iter().all(|used| *used <= block_gas_limit),
        "a slice reported more gas than the block allows ({block_gas_limit}): {reported:?}",
    );

    // The last slice a subscriber saw is a prefix of the block, so its total is
    // at most the block's — and equal when nothing executed after that tick.
    let sealed_gas = payload.block().header().gas_used;
    assert!(
        reported.last().is_some_and(|last| *last <= sealed_gas),
        "the final slice reported more gas than the sealed block used ({sealed_gas}): {reported:?}",
    );
}
