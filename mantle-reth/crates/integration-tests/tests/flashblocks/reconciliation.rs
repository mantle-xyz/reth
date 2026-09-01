//! Reconciliation of the pending overlay against arriving canonical blocks.
//!
//! Blocks are mined through the engine API and handed to the processor the way
//! the node's canonical-state task does; the websocket subscriber is not
//! started.

use crate::helpers::{
    FlashblockBuilder, base_slice, base_slice_builder, filler_deposit, launch_flashblocks_node,
    mine_canonical_block as mine,
};

/// Overlay spans blocks 1 to 3, then canonical advances to block 2 and is
/// delivered. Every slice mirrors the transaction the node forces into its
/// block, so no reorg is detected and the trailing-depth limit alone decides
/// the outcome.
macro_rules! canonical_two_under_overlay_three {
    ($max_trailing_depth:expr) => {{
        let (harness, mut node) = launch_flashblocks_node!($max_trailing_depth, 3);

        harness.send(base_slice(1)).await;
        harness.send(base_slice_builder(2).prev(1, 0).build()).await;
        harness.send(base_slice_builder(3).prev(2, 0).build()).await;

        let pending = harness.pending().expect("overlay should span blocks 1 to 3");
        assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 3));

        mine!(node);
        let block = mine!(node);
        assert_eq!(block.number, 2, "canonical should sit one block under the overlay tip");
        harness.send_canonical(block).await;

        (harness, node)
    }};
}

/// Canonical reaching the overlay's tip retires the overlay.
#[tokio::test(flavor = "multi_thread")]
async fn canonical_catching_up_to_the_overlay_clears_it() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some(), "the base slice should open an overlay");

    let block = mine!(node);
    assert_eq!(block.number, 1);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "canonical reaching the overlay tip must clear it");
}

/// A canonical block that drops a tracked transaction is a reorg: everything at
/// or below it is discarded and the blocks ahead of it are replayed.
#[tokio::test(flavor = "multi_thread")]
async fn a_canonical_block_missing_a_tracked_transaction_rebuilds_the_overlay() {
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    harness.send(base_slice(1)).await;
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).transactions(vec![filler_deposit(1)]).build())
        .await;
    harness.send(base_slice_builder(2).prev(1, 1).build()).await;

    let pending = harness.pending().expect("overlay should span blocks 1 and 2");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 2));

    // Canonical block 1 carries only the L1-attributes deposit; the overlay also
    // tracked the filler, so the two transaction sequences diverge.
    let block = mine!(node);
    harness.send_canonical(block).await;

    let pending = harness.pending().expect("the block ahead of the reorg should be replayed");
    assert_eq!(
        (pending.earliest_block_number(), pending.latest_block_number()),
        (2, 2),
        "block 1 is settled by canonical, only block 2 survives"
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
    let (harness, mut node) = launch_flashblocks_node!(3, 3);

    let block = mine!(node);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "nothing to reconcile, nothing to publish");
}
