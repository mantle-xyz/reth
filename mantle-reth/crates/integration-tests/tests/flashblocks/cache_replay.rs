//! Slices that arrive before the canonical block they build on.
//!
//! Such a slice cannot be executed yet — the base state is missing — so the
//! processor parks it in [`FlashblockCache`] and replays it once the canonical
//! block lands.

use crate::consumer_helpers::{
    FlashblockBuilder, base_slice, base_slice_builder, commit_submitted_block,
    launch_flashblocks_node, mine_canonical_block as mine, submit_uncommitted_block,
};

/// A slice whose canonical parent has not been imported yet is parked, then
/// replayed when that parent arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_ahead_of_canonical_is_replayed_once_its_parent_lands() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    // Block 1 is built and submitted, so its hash is real, but it is not yet
    // canonical — mirroring a sequencer that has already built the block while
    // this follower has not yet imported it.
    let block_1_hash = submit_uncommitted_block!(node);

    harness.send(base_slice_builder(2).parent_hash(block_1_hash).build()).await;
    assert!(harness.pending().is_none(), "the slice cannot open an overlay yet");

    let block = commit_submitted_block!(node, block_1_hash);
    assert_eq!(block.number, 1);
    harness.send_canonical(block).await;

    let pending = harness.pending().expect("the parked slice should replay");
    assert_eq!(pending.latest_block_number(), 2);
    assert_eq!(pending.latest_flashblock_index(), 0);
}

/// A base slice for a skipped height is a valid restart point: it is parked
/// rather than dropped, and replays once canonical reaches its parent.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_slice_for_a_skipped_block_is_parked_and_resumes_the_overlay() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    harness.send(base_slice(1)).await;

    // Both blocks are mined up front: block 2 is never announced to the
    // processor as a flashblock, but it is real, exactly as a sequencer that
    // built it while this follower missed the flashblock update for it. That
    // is what lets the skip-slice below declare a real parent.
    let block_1 = mine!(node);
    let block_2 = mine!(node);
    assert_eq!(block_2.number, 2);

    harness.send(base_slice(3)).await;

    let pending = harness.pending().expect("the block 1 overlay is left in place");
    assert_eq!(pending.latest_block_number(), 1, "the skipping slice must not extend the overlay");

    harness.send_canonical(block_1).await;
    assert!(harness.pending().is_none(), "canonical catches up with block 1");

    harness.send_canonical(block_2).await;

    let pending = harness.pending().expect("the parked slice should replay on top of block 2");
    assert_eq!(pending.earliest_block_number(), 3);
    assert_eq!(pending.latest_block_number(), 3);
}

/// Parking a slice for a skipped block leaves the overlay usable: the next slice
/// that does continue the tracked sequence still extends it.
#[tokio::test(flavor = "multi_thread")]
async fn parking_a_skipped_block_does_not_disturb_the_overlay() {
    let (harness, _node) = launch_flashblocks_node!(3, 3);

    harness.send(base_slice(1)).await;
    harness.send(base_slice(3)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;

    let pending = harness.pending().expect("the overlay should still be present");
    assert_eq!(pending.latest_block_number(), 1);
    assert_eq!(pending.latest_flashblock_index(), 1, "the tracked sequence still advances");
}

/// A parked slice is dropped once canonical passes it: it can no longer extend
/// anything.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_slice_is_evicted_when_canonical_passes_it() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    harness.send(base_slice(2)).await;
    assert!(harness.pending().is_none());

    // Canonical jumps straight past block 2, so the parked slice is stale.
    mine!(node);
    let block = mine!(node);
    assert_eq!(block.number, 2);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "a slice at or below canonical must not be replayed");
}
