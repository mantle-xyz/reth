//! End-to-end checks over the flashblock consumer pipeline.
//!
//! Slices are injected straight into the state processor and awaited; the
//! websocket subscriber is not started.

use mantle_reth_flashblocks::{FlashblocksAPI, PendingBlocksAPI};

use crate::helpers::{FlashblockBuilder, base_slice, launch_flashblocks_node};

/// Default depths: three blocks of trailing history, three of leading slack.
macro_rules! launch {
    () => {{ launch_flashblocks_node!(3, 3) }};
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

/// Re-sending the current slice is ignored and leaves the overlay untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_slice_is_ignored() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;
    harness.send(base_slice(1)).await;

    let pending = harness.pending().expect("overlay should survive a duplicate");
    assert_eq!(pending.latest_flashblock_index(), 0);
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

/// The overlay reports the canonical block it was built on.
#[tokio::test(flavor = "multi_thread")]
async fn the_overlay_reports_its_canonical_base() {
    let (harness, _node) = launch!();

    harness.send(base_slice(1)).await;

    let base = harness.state().get_pending_blocks().get_canonical_block_number();
    assert_eq!(base, alloy_rpc_types_eth::BlockNumberOrTag::Number(0), "block 1 sits on genesis");
}
