//! Slices that arrive before the canonical block they build on.
//!
//! Such a slice cannot be executed yet — the base state is missing — so the
//! processor parks it in [`FlashblockCache`] and replays it once the canonical
//! block lands.

use crate::helpers::{
    FlashblockBuilder, base_slice, launch_flashblocks_node, mine_canonical_block as mine,
};

/// A slice whose canonical parent has not been imported yet is parked, then
/// replayed when that parent arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_ahead_of_canonical_is_replayed_once_its_parent_lands() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    // Canonical sits at genesis, so block 2 has no parent state to build on.
    harness.send(base_slice(2)).await;
    assert!(harness.pending().is_none(), "the slice cannot open an overlay yet");

    let block = mine!(node);
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
    // Block 2 is never announced; block 3 does not continue the tracked sequence.
    harness.send(base_slice(3)).await;

    let pending = harness.pending().expect("the block 1 overlay is left in place");
    assert_eq!(pending.latest_block_number(), 1, "the skipping slice must not extend the overlay");

    let block = mine!(node);
    harness.send_canonical(block).await;
    assert!(harness.pending().is_none(), "canonical catches up with block 1");

    let block = mine!(node);
    assert_eq!(block.number, 2);
    harness.send_canonical(block).await;

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
