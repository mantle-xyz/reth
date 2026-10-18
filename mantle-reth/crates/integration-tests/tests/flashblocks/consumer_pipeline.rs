//! End-to-end checks over the flashblock consumer pipeline.
//!
//! Slices are injected straight into the state processor and awaited; the
//! websocket subscriber is not started.

use mantle_reth_flashblocks::{
    FlashblocksAPI, FlashblocksReceiver, FlashblocksState, PendingBlocksAPI,
};

use alloy_primitives::keccak256;
use alloy_rpc_types_engine::PayloadId;

use crate::consumer_helpers::{
    FlashblockBuilder, base_slice, base_slice_builder, filler_deposit, l1_info_deposit,
    launch_flashblocks_consumer, mine_canonical_block as mine,
};

/// Default depths: three blocks of trailing history, three of leading slack.
macro_rules! launch {
    () => {{ launch_flashblocks_consumer!(3, 3) }};
}

/// Payload id of a second build of a block. Slices built by [`FlashblockBuilder`]
/// default to `PayloadId::default()`, so a rebuild has to say it is one: two
/// payload jobs never share an id, and a test that lets them share it is asking
/// about an input the sequencer cannot produce.
fn rebuild_id() -> PayloadId {
    PayloadId::new([0xff; 8])
}

/// Payload id of the job that builds block `n`. Distinct per block, as the
/// sequencer's are, and never equal to [`rebuild_id`].
fn job_id(n: u8) -> PayloadId {
    PayloadId::new([n; 8])
}

/// The base slice of the next block opens an overlay on the canonical tip.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_slice_on_the_canonical_tip_opens_an_overlay() {
    let (harness, _node) = launch!();

    assert!(harness.pending().is_none(), "overlay starts empty");

    harness.send(base_slice(1)).await;

    let pending = harness.pending().expect("base slice should open an overlay");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(pending.latest_flashblock_index(), 0);
}

/// A second slice for the same block extends the overlay in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_following_slice_extends_the_same_block() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;

    let pending = harness.pending().expect("overlay should still be present");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(pending.latest_flashblock_index(), 1, "index should advance in place");
}

/// A slice whose predecessor link does not match the tracked latest clears the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_predecessor_link_clears_the_overlay() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some());

    // Claims to follow (1, 5), but the overlay tracks (1, 0).
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 5).build()).await;

    assert!(harness.pending().is_none(), "a mismatched predecessor must clear the overlay");
}

/// A gap in the index sequence within one block clears the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_in_the_index_sequence_clears_the_overlay() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 3).prev(1, 0).build()).await;

    assert!(harness.pending().is_none(), "a skipped index must clear the overlay");
}

/// A rebuild of the block being built restarts at index 0, and the consumer has
/// to follow it.
///
/// A second forkchoice update on the same parent starts a fresh payload job,
/// whose slice counter begins at zero again — so the rebuild's first slice is
/// `(N, 0)` carrying a base, linked to the last slice of the attempt it
/// replaces. That is an ordinary sequencer event, not a malformed stream.
///
/// The index moving *backwards* is the signal; it cannot be read as a gap,
/// because the slice it arrives with is the one thing that can open an overlay
/// from nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_block_restarting_at_index_zero_reopens_the_overlay() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;
    harness.send(FlashblockBuilder::new(1, 2).prev(1, 1).build()).await;

    harness.send(base_slice_builder(1).prev(1, 2).payload_id(rebuild_id()).build()).await;

    let pending = harness.pending().expect("the rebuild's base slice must open an overlay");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(pending.latest_flashblock_index(), 0, "the rebuild's own index");
}

/// The cost of getting the slice above wrong, stated separately: the rest of
/// the rebuilt block has to remain usable.
///
/// A dropped base slice is not recoverable later in the same block — nothing
/// after it carries one — so the consumer would serve `latest` until the *next*
/// block's base slice arrives, a whole block of staleness for an event the
/// sequencer treats as routine.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_block_keeps_extending_after_its_base_slice() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;

    // The rebuild, then the slice that follows it. Both carry the rebuild's
    // payload id: a continuation of build B that named build A's id is an input
    // no payload job can emit, and the consumer now refuses it as such.
    harness.send(base_slice_builder(1).prev(1, 1).payload_id(rebuild_id()).build()).await;
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).payload_id(rebuild_id()).build())
        .await;

    let pending = harness.pending().expect("the rebuild must still be extendable");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(pending.latest_flashblock_index(), 1);
}

/// A rebuilt block's continuation must not extend the overlay the abandoned
/// build left behind.
///
/// Indices restart per build, so build B's `(1,3)` names `(1,2)` as its
/// predecessor — the very position build A's overlay sits at. Position and
/// link both agree; only the payload id says the two chains never met.
/// Extending here executes B's transactions on A's state and serves a pending
/// block no build ever produced, with no counter or log saying so.
///
/// Reached when a subscriber reconnects mid-rebuild: the archive no longer
/// holds the replacement's base, so the first slice it sees of build B is a
/// later index. Dropping the overlay costs the rest of this block; splicing
/// costs correctness.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_builds_continuation_does_not_extend_the_overlay() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;
    harness.send(FlashblockBuilder::new(1, 2).prev(1, 1).build()).await;

    harness.send(FlashblockBuilder::new(1, 3).prev(1, 2).payload_id(rebuild_id()).build()).await;

    assert!(
        harness.pending().is_none(),
        "a slice from another build of this height must clear the overlay, not extend it",
    );
}

/// Re-sending the current slice is ignored and leaves the overlay untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_slice_is_ignored() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(base_slice(1)).await;

    let pending = harness.pending().expect("overlay should survive a duplicate");
    assert_eq!(pending.latest_flashblock_index(), 0);
}

/// A rebuild of the overlay's latest block keeps the blocks under it.
///
/// The overlay runs ahead of canonical, so the rebuilt block's parent is
/// usually the overlay's previous block rather than anything the chain holds.
/// Rebuilding from canonical state alone cannot reach it.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_latest_block_keeps_the_block_under_it() {
    let (harness, _node) = launch!();

    harness.send(base_slice_builder(1).payload_id(job_id(1)).build()).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).payload_id(job_id(1)).build()).await;
    harness.send(base_slice_builder(2).prev(1, 1).payload_id(job_id(2)).build()).await;
    harness.send(FlashblockBuilder::new(2, 1).prev(2, 0).payload_id(job_id(2)).build()).await;

    {
        let before = harness.pending().expect("premise: two blocks are in the overlay");
        assert_eq!(before.earliest_block_number(), 1, "premise: earliest");
        assert_eq!(before.latest_block_number(), 2, "premise: latest");
        assert_eq!(before.latest_flashblock_index(), 1, "premise: index");
    }

    // Block 2 is built again, while block 1 is still only in the overlay.
    let only_in_the_rebuild = filler_deposit(9);
    let rebuilt_tx_hash = keccak256(&only_in_the_rebuild);
    harness
        .send(
            base_slice_builder(2)
                .prev(2, 1)
                .transactions(vec![l1_info_deposit(2), only_in_the_rebuild])
                .payload_id(rebuild_id())
                .build(),
        )
        .await;

    let pending = harness.pending().expect("the rebuild must produce an overlay");
    assert_eq!(pending.latest_block_number(), 2);
    assert_eq!(pending.earliest_block_number(), 1, "block 1 is only in the overlay");
    assert!(
        pending.has_transaction_hash(&rebuilt_tx_hash),
        "the overlay has to hold the rebuild's base, not the one it replaced",
    );
}

/// A rebuild that lands while the overlay still holds only the base slice is
/// not a retransmission of that slice.
///
/// The duplicate check is positional, so a second attempt's index-0 slice
/// cannot be told apart from a resend of the first attempt's. Whichever base
/// the overlay keeps is the one every later slice of that block is applied
/// against, so keeping the wrong one is not a dropped update — it is a block
/// assembled from two attempts.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_base_slice_replaces_the_one_it_supersedes() {
    let (harness, _node) = launch!();

    // Attempt 1 publishes its base slice and is then abandoned.
    harness.send(base_slice(1)).await;

    // Attempt 2 starts over at the same position with different contents.
    let only_in_the_rebuild = filler_deposit(7);
    let rebuilt_tx_hash = keccak256(&only_in_the_rebuild);
    harness
        .send(
            base_slice_builder(1)
                .transactions(vec![l1_info_deposit(1), only_in_the_rebuild])
                .payload_id(rebuild_id())
                .build(),
        )
        .await;

    let pending = harness.pending().expect("overlay should survive a rebuild");
    assert!(
        pending.has_transaction_hash(&rebuilt_tx_hash),
        "the overlay has to hold the base that the slices after it were built against",
    );
}

/// A slice leading canonical beyond `max_leading_depth` is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_beyond_the_leading_depth_is_dropped() {
    let (harness, _node) = launch!();

    // Canonical sits at genesis, so block 100 leads by far more than the
    // configured depth of 3.
    harness.send(base_slice(100)).await;

    assert!(harness.pending().is_none(), "the leading-depth guard must drop the slice");
}

/// A slice for a block the chain has already canonicalised is skipped.
///
/// Executing it cannot produce a publishable overlay — whatever it computes sits at
/// or below the tip — and doing the work anyway is what lets a backlog sustain
/// itself: superseded payloads occupy the one consumer while fresh ones wait.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_for_an_already_canonical_block_is_skipped() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    for _ in 0..3 {
        mine!(node);
    }

    harness.send(base_slice(2)).await;

    assert!(harness.pending().is_none(), "a superseded slice must not open an overlay");

    // The guard is about the tip, not about the slice being malformed: one block
    // past the tip still opens an overlay.
    harness.send(base_slice(4)).await;
    assert!(harness.pending().is_some(), "a slice ahead of the tip is still accepted");
}

/// A slice carrying an SDM post-exec transaction replays it as a no-op.
///
/// The canonical block executor recognises `POST_EXEC_TX_TYPE_ID` and returns
/// before handing the transaction to the EVM. op-reth v2.4.2 gives replay paths
/// the same treatment: `alloy_op_evm`'s `transact_raw` short-circuits the type and
/// synthesizes the consensus-defined no-op result, so the transaction joins the
/// overlay — landing in the `OpTxType::PostExec` arm of
/// `mantle-reth/crates/flashblocks/src/receipt_builder.rs` — instead of failing
/// pre-validation and taking the whole slice, L1-attributes deposit included, with it.
///
/// This covers only a post-exec transaction appended to `transactions`. The
/// wire protocol carries it out of band instead, so producing flashblocks with
/// SDM enabled stays an untested combination — see
/// `FlashblockProducerConfigError::SdmNotSupportedWithFlashblocks` for the gate
/// that refuses it at boot, and for the hole that gate leaves open.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_carrying_a_post_exec_transaction_replays_it_as_a_noop() {
    use alloy_network::eip2718::Encodable2718;
    use alloy_primitives::Sealable;
    use op_alloy_consensus::{OpTxEnvelope, SDMGasEntry, build_post_exec_tx};

    let (harness, _node) = launch!();

    let post_exec = OpTxEnvelope::PostExec(
        build_post_exec_tx(1, vec![SDMGasEntry { index: 1, gas_refund: 2_500 }]).seal_slow(),
    );
    harness
        .send(
            FlashblockBuilder::new(1, 0)
                .transactions(vec![l1_info_deposit(1), post_exec.encoded_2718().into()])
                .build(),
        )
        .await;

    let pending = harness.pending().expect("the slice must open an overlay");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(
        pending.pending_transaction_count(),
        2,
        "the deposit and the post-exec transaction both land in the overlay"
    );

    // The processor keeps running, so a clean slice for the next height is
    // still accepted and extends the same overlay.
    harness.send(base_slice(2)).await;
    let pending = harness.pending().expect("a clean slice should still be accepted");
    assert_eq!(pending.latest_block_number(), 2);
    assert_eq!(
        pending.pending_transaction_count(),
        3,
        "the overlay counts across blocks: block 1's two plus block 2's deposit"
    );
}

/// A transaction whose encoding is not canonical is rejected, taking its slice with it.
///
/// The overlay roots the wire bytes it was handed but hashes the transaction it
/// decoded out of them, and nothing here compares the two. They agree only while the
/// decoder refuses every non-canonical encoding. The sharp case is a deposit body
/// stripped of its `0x7E` type byte: were it to decode, the overlay would execute a
/// transaction, mint, and publish a receipt for something derivation will never
/// accept, so the preconfirmation would simply evaporate.
///
/// This pins behaviour we inherit from `op-alloy` rather than implement, which is
/// exactly why it is worth a test here.
#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_with_a_non_canonical_encoding_is_rejected() {
    let (harness, _node) = launch!();

    let deposit = l1_info_deposit(1);
    assert_eq!(deposit[0], 0x7e, "the fixture must be a typed deposit for this to mean anything");
    let bare_body = alloy_primitives::Bytes::from(deposit[1..].to_vec());

    harness.send(FlashblockBuilder::new(1, 0).transactions(vec![bare_body]).build()).await;

    assert!(harness.pending().is_none(), "an untyped deposit body must not open an overlay");

    // The same slice with the type byte intact is accepted, so the encoding was the
    // only thing wrong with it.
    harness.send(base_slice(1)).await;
    let pending = harness.pending().expect("the canonical encoding is accepted");
    assert_eq!(pending.pending_transaction_count(), 1);
}

/// The overlay reports the canonical block it was built on; with no overlay the base
/// stays `pending` so the standard implementation keeps serving that tag.
#[tokio::test(flavor = "multi_thread")]
async fn the_overlay_reports_its_canonical_base() {
    let (harness, _node) = launch!();

    assert_eq!(
        harness.state().get_pending_blocks().get_canonical_block_number(),
        alloy_rpc_types_eth::BlockNumberOrTag::Pending,
        "no overlay yet, so `pending` passes through"
    );

    harness.send(base_slice(1)).await;

    let base = harness.state().get_pending_blocks().get_canonical_block_number();
    assert_eq!(base, alloy_rpc_types_eth::BlockNumberOrTag::Number(0), "block 1 sits on genesis");
}

/// Updates waiting for the processor are counted.
///
/// The queue is unbounded and shared by both sources, so a backlog is invisible
/// unless it is counted: it shows up only as guards reasoning about a stale height
/// and `pending` answering from an older base.
#[tokio::test(flavor = "multi_thread")]
async fn the_queue_depth_counts_updates_the_processor_has_not_taken() {
    // Never started, so nothing drains what is queued here.
    let state = FlashblocksState::new(3, 3, 5);
    assert_eq!(state.queued_updates(), 0, "a fresh queue is empty");

    state.on_flashblock_received(base_slice(1));
    state.on_flashblock_received(base_slice_builder(2).prev(1, 0).build());

    assert_eq!(state.queued_updates(), 2, "nothing is consuming the queue");
}

/// The count drops back as the processor consumes.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumed_update_leaves_the_queue_empty() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;

    assert_eq!(
        harness.state().queued_updates(),
        0,
        "the acknowledgement means the processor is done with it"
    );
}

/// A rebuild forced by missing live state must not re-enter the live-state lock.
///
/// A slice that fails mid-execution has already taken the live state, and the error
/// path leaves the overlay in place. The next slice for that block therefore finds an
/// overlay but no live state and takes the full-rebuild fallback, which writes the
/// live state back. Holding the lock across that fallback deadlocks the single
/// consumer thread and stops the whole pipeline.
///
/// On regression this wedges the test binary rather than failing tidily: the timeout
/// below fires and panics, but runtime shutdown then waits forever on the blocked
/// worker. A CI timeout naming this test is the signal.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_without_live_state_does_not_deadlock() {
    use alloy_network::eip2718::Encodable2718;
    use alloy_primitives::Sealable;
    use op_alloy_consensus::{OpTxEnvelope, SDMGasEntry, build_post_exec_tx};

    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;

    let post_exec = OpTxEnvelope::PostExec(
        build_post_exec_tx(1, vec![SDMGasEntry { index: 1, gas_refund: 2_500 }]).seal_slow(),
    );
    harness
        .send(
            FlashblockBuilder::new(1, 1)
                .prev(1, 0)
                .transactions(vec![post_exec.encoded_2718().into()])
                .build(),
        )
        .await;
    assert!(harness.pending().is_some(), "a failed slice must leave the overlay in place");

    // Same (block, index) as the failed slice, so the sequence check still passes.
    let retry = FlashblockBuilder::new(1, 1).prev(1, 0).build();
    tokio::time::timeout(std::time::Duration::from_secs(10), harness.send(retry))
        .await
        .expect("the rebuild fallback must not hold the live-state lock");

    let pending = harness.pending().expect("the retry should rebuild the overlay");
    assert_eq!(pending.latest_flashblock_index(), 1);
}
