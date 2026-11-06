//! Reconciliation of the pending overlay against arriving canonical blocks.
//!
//! Blocks are mined through the engine API and handed to the processor the way
//! the node's canonical-state task does; the websocket subscriber is not
//! started.

use alloy_primitives::{B256, U256};
use alloy_rpc_types_engine::PayloadId;

use crate::consumer_helpers::{
    FlashblockBuilder, LOGGER, base_slice, base_slice_builder, commit_submitted_block,
    filler_deposit, l1_info_deposit, launch_flashblocks_consumer, mine_canonical_block as mine,
    signed_call, submit_uncommitted_block,
};

/// Overlay spans blocks 1 to 3, then canonical advances to block 2 and is
/// delivered. Every slice mirrors the transaction the node forces into its
/// block, so no reorg is detected and the trailing-depth limit alone decides
/// the outcome.
///
/// Mining is interleaved with sending rather than done all at once at the end:
/// a real sequencer streaming block N+1 already knows the hash of the block N
/// it just built, so each slice here declares a parent that is on-chain by the
/// time it is sent, the same way `FlashblocksHarness::send`'s backfill expects.
macro_rules! canonical_two_under_overlay_three {
    ($max_trailing_depth:expr) => {{
        let (harness, mut node) = launch_flashblocks_consumer!($max_trailing_depth, 3);

        harness.send(base_slice(1)).await;
        mine!(node);
        harness.send(base_slice_builder(2).prev(1, 0).build()).await;
        let block = mine!(node);
        assert_eq!(block.number, 2, "canonical should sit one block under the overlay tip");
        harness.send(base_slice_builder(3).prev(2, 0).build()).await;

        let pending = harness.pending().expect("overlay should span blocks 1 to 3");
        assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 3));

        harness.send_canonical(block).await;

        (harness, node)
    }};
}

/// Canonical reaching the overlay's tip retires the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn canonical_catching_up_to_the_overlay_clears_it() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some(), "the base slice should open an overlay");

    let block = mine!(node);
    assert_eq!(block.number, 1);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "canonical reaching the overlay tip must clear it");
}

/// A canonical block that drops a tracked transaction is a reorg: canonical
/// block 1 itself is discarded, but what was built on top of it is not. A
/// real sequencer seals block 1 and builds block 2 on that real hash
/// regardless of whether the overlay's own tracked transactions for block 1
/// matched what got sealed, so block 2's slice passes
/// `StateProcessor::base_is_canonical` even though canonical block 1 just
/// diverged from the overlay. `ReorgDetector` still flags the divergence and
/// `HandleReorg` rebuilds the overlay from the slices ahead of the reorg:
/// only block 2 survives.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_missing_a_tracked_transaction_rebuilds_the_overlay() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).transactions(vec![filler_deposit(1)]).build())
        .await;

    // Canonical block 1 carries only the L1-attributes deposit; the overlay also
    // tracked the filler, so the two transaction sequences diverge. Mined before
    // the block-2 slice is sent, so that slice backfills the real, now-canonical
    // hash as its parent.
    let block = mine!(node);
    harness.send(base_slice_builder(2).prev(1, 1).build()).await;

    let pending = harness.pending().expect("overlay should span blocks 1 and 2");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 2));

    harness.send_canonical(block).await;

    let pending = harness.pending().expect("the block ahead of the reorg should be replayed");
    assert_eq!(
        (pending.earliest_block_number(), pending.latest_block_number()),
        (2, 2),
        "block 1 is settled by canonical, only block 2 survives",
    );
}

/// Control for the depth limit: the same trailing distance, kept because it is
/// within the configured maximum.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_within_the_trailing_depth_keeps_the_overlay() {
    let (harness, _node) = canonical_two_under_overlay_three!(3);

    let pending = harness.pending().expect("the overlay should survive");
    assert_eq!(
        (pending.earliest_block_number(), pending.latest_block_number()),
        (1, 3),
        "a trailing depth of 1 is within the maximum of 3, so nothing is dropped"
    );
}

/// An overlay trailing canonical by more than the configured depth is rebuilt
/// from the canonical block, keeping only what leads it.
#[tokio::test(flavor = "multi_thread")]
async fn an_overlay_deeper_than_the_trailing_limit_is_rebuilt_from_canonical() {
    let (harness, _node) = canonical_two_under_overlay_three!(0);

    let pending = harness.pending().expect("blocks ahead of canonical should be replayed");
    assert_eq!(
        (pending.earliest_block_number(), pending.latest_block_number()),
        (3, 3),
        "a trailing depth of 1 exceeds the maximum of 0, so blocks 1 and 2 are dropped"
    );
}

/// A canonical block arriving with no overlay is a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_without_an_overlay_is_a_no_op() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    let block = mine!(node);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "nothing to reconcile, nothing to publish");
}

/// Staleness is measured against the node's tip, not the notification being handled.
///
/// The processor consumes canonical notifications and slices from one queue, so a
/// backlog makes the notification height lag the chain. Judging the overlay by that
/// height lets one whose blocks are all canonical survive, and `pending` then answers
/// from a historical base — which is slow enough to deepen the backlog that caused it.
#[tokio::test(flavor = "multi_thread")]
async fn a_backlogged_canonical_notification_is_judged_against_the_tip() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    harness.send(base_slice_builder(2).prev(1, 0).build()).await;
    harness.send(base_slice_builder(3).prev(2, 0).build()).await;

    let pending = harness.pending().expect("overlay should span blocks 1 to 3");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 3));

    // The chain runs to 5 while only the block-1 notification has been dequeued.
    let block = mine!(node);
    assert_eq!(block.number, 1);
    for _ in 0..4 {
        mine!(node);
    }
    harness.send_canonical(block).await;

    assert!(
        harness.pending().is_none(),
        "every overlay block is canonical at tip 5, so the overlay must retire",
    );
}

/// A snapshot stranded behind the tip is dropped when it is next read.
///
/// Every existing guard runs inside `process_canonical_block`, so a stream that
/// delivers slices but no canonical notifications — a lagging or stalled notification
/// path — leaves the overlay anchored wherever it was, indefinitely. `pending` keeps
/// answering from that anchor, which by then is historical state.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_stranded_behind_the_tip_is_dropped_without_a_notification() {
    let (harness, mut node) = launch_flashblocks_consumer!(1, 3);

    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some(), "the base slice should open an overlay");

    // The chain runs well past the overlay. No canonical notification is delivered,
    // so reconciliation never gets a chance to retire it.
    for _ in 0..5 {
        mine!(node);
    }

    // Any further work makes the processor read the snapshot, which is where the
    // staleness check lives.
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;

    assert!(
        harness.pending().is_none(),
        "a snapshot five blocks behind the tip must not survive being read"
    );
}

/// The harness stands in for a sequencer, so the slices it sends must declare
/// the parent they were really built on. Every assertion about the base hash
/// guard rests on this.
#[tokio::test(flavor = "multi_thread")]
async fn the_harness_anchors_slices_to_the_real_chain() {
    let (harness, node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;

    let pending = harness.pending().expect("the base slice should open an overlay");
    let genesis = reth_provider::BlockHashReader::block_hash(&node.inner.provider, 0)
        .expect("genesis hash query")
        .expect("genesis must exist");

    assert_eq!(
        pending.parent_hash(),
        genesis,
        "a slice for block 1 is built on genesis, so that is what it must declare",
    );
}

/// A slice declaring a parent the chain does not have is not publishable: its
/// cached execution was computed somewhere this node is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_declaring_a_foreign_parent_is_not_published() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice_builder(1).parent_hash(B256::repeat_byte(0xff)).build()).await;

    assert!(
        harness.pending().is_none(),
        "an overlay anchored outside the canonical chain must not be published",
    );
}

/// The guard must not fire on the ordinary case, where the producer built on
/// the head this node also has.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_on_the_canonical_head_is_published() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    harness.send(base_slice_builder(2).prev(1, 0).build()).await;

    let pending = harness.pending().expect("an overlay anchored to the real chain must publish");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 2));
}

/// A build that follows one rejected for a foreign parent carries only its own
/// transaction, not anything from the rejected attempt.
///
/// Pins the observable outcome only: a rejected build never contaminates the
/// one after it. It does not show the guard's own `clear_live_state()` is
/// what produces that — the rebuild classification already cleared live
/// state, and `apply_flashblock` drops the overlay for any rejected build.
#[tokio::test(flavor = "multi_thread")]
async fn a_build_after_a_rejected_build_carries_only_its_own_transaction() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    // A first build of block 1 carrying one transaction beyond the mandatory
    // L1-attributes deposit.
    harness
        .send(
            base_slice_builder(1)
                .payload_id(PayloadId::new([1; 8]))
                .transactions(vec![l1_info_deposit(1), filler_deposit(7)])
                .build(),
        )
        .await;

    let pending = harness.pending().expect("the first build should publish");
    assert_eq!(
        pending.pending_transaction_count(),
        2,
        "the overlay should carry both the attributes deposit and the filler",
    );

    // The producer restarts block 1 on a parent this node does not have.
    harness
        .send(
            base_slice_builder(1)
                .payload_id(PayloadId::new([2; 8]))
                .parent_hash(B256::repeat_byte(0xff))
                .transactions(vec![l1_info_deposit(1), filler_deposit(8)])
                .build(),
        )
        .await;

    assert!(
        harness.pending().is_none(),
        "a build anchored outside the canonical chain must not be published",
    );

    // A third build, this time on the real head, carrying only the attributes
    // deposit. Nothing from either earlier attempt may survive into it.
    harness
        .send(
            base_slice_builder(1)
                .payload_id(PayloadId::new([3; 8]))
                .transactions(vec![l1_info_deposit(1)])
                .build(),
        )
        .await;

    let rebuilt = harness.pending().expect("a build on the real head should publish");
    assert_eq!(
        rebuilt.pending_transaction_count(),
        1,
        "the rebuilt overlay must hold only its own transaction",
    );
}

/// Sends payload A for blocks 1 and 2 on top of the submitted but not yet
/// canonical `block_1_hash`, then commits block 1 and delivers it.
///
/// A's block 1 carries a transfer canonical block 1 lacks, so reconciliation
/// rebuilds A's block 2 on canonical block 1, where that block's transfer has a
/// nonce too high to execute.
macro_rules! deliver_a_block_payload_a_cannot_be_rebuilt_on {
    ($harness:expr, $node:expr, $block_1_hash:expr) => {{
        $harness
            .send(
                base_slice_builder(1)
                    .transactions(vec![
                        l1_info_deposit(1),
                        signed_call(0, LOGGER, U256::from(1)).await,
                    ])
                    .build(),
            )
            .await;
        $harness
            .send(
                base_slice_builder(2)
                    .prev(1, 0)
                    .parent_hash($block_1_hash)
                    .transactions(vec![
                        l1_info_deposit(2),
                        signed_call(1, LOGGER, U256::from(1)).await,
                    ])
                    .build(),
            )
            .await;
        let pending = $harness.pending().expect("payload A spans blocks 1 and 2");
        assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 2));
        assert_eq!(pending.payload_id(), PayloadId::default(), "premise: the overlay is payload A");

        let block_1 = commit_submitted_block!($node, $block_1_hash);
        assert_eq!(
            block_1.body().transactions.len(),
            1,
            "premise: canonical block 1 lacks the transfer"
        );
        $harness.send_canonical(block_1).await;
    }};
}

/// A canonical block the overlay cannot be rebuilt on stops the overlay being
/// served. Had the rebuild succeeded, payload A's block 2 would be served.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_that_fails_to_reconcile_drops_the_preview() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let block_1_hash = submit_uncommitted_block!(node);

    deliver_a_block_payload_a_cannot_be_rebuilt_on!(harness, node, block_1_hash);

    assert!(harness.pending().is_none(), "a preview that failed to reconcile must not be served");
}

/// A failed reconciliation still advances the cache: payload B's block 2, cached
/// while block 1 was unknown and built on the block 1 the chain has, is replayed.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_that_fails_to_reconcile_drops_the_preview_and_replays_the_cache() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let payload_b = PayloadId::new([0xb; 8]);
    let block_1_hash = submit_uncommitted_block!(node);

    harness
        .send(base_slice_builder(2).payload_id(payload_b).parent_hash(block_1_hash).build())
        .await;
    assert!(harness.pending().is_none(), "premise: block 2 waits in the cache for block 1");

    deliver_a_block_payload_a_cannot_be_rebuilt_on!(harness, node, block_1_hash);

    let pending = harness.pending().expect("the cached block 2 is replayed on canonical block 1");
    assert_eq!(pending.earliest_block_number(), 2, "payload A's block 1 must not be served");
    assert_eq!(pending.payload_id(), payload_b, "payload A's block 2 must not be served");
}

/// A reset drops the preview and the early-arrival cache: block 2's slice,
/// cached until block 1 lands, is not replayed when it does.
#[tokio::test(flavor = "multi_thread")]
async fn a_processor_reset_drops_the_preview_and_the_cache() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let payload_b = PayloadId::new([0xb; 8]);
    let block_1_hash = submit_uncommitted_block!(node);

    // Block 2's parent, block 1, is not canonical yet: the slice is cached.
    harness
        .send(base_slice_builder(2).payload_id(payload_b).parent_hash(block_1_hash).build())
        .await;
    assert!(harness.pending().is_none(), "premise: block 2 waits in the cache for block 1");
    // An unrelated preview is being served.
    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some(), "premise: a preview is being served");

    harness.reset_after_panic().await;
    assert!(harness.pending().is_none(), "the reset must drop the preview");

    let block_1 = commit_submitted_block!(node, block_1_hash);
    harness.send_canonical(block_1).await;
    assert!(
        harness.pending().is_none_or(|p| p.payload_id() != payload_b),
        "the cache was cleared, so block 2 must not be replayed"
    );
}
