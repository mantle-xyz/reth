//! Slices that arrive before the canonical block they build on.
//!
//! Such a slice cannot be executed yet — the base state is missing — so the
//! processor parks it in [`FlashblockCache`] and replays it once the canonical
//! block lands.

use crate::consumer_helpers::{
    FlashblockBuilder, base_slice, base_slice_builder, commit_submitted_block,
    launch_flashblocks_consumer, mine_canonical_block as mine, submit_uncommitted_block,
};

/// The cache window remains usable beyond the overlay's leading limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_beyond_leading_depth_replays_when_its_parent_lands() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 1);
    let block_1 = mine!(node);
    harness.send_canonical(block_1).await;
    let block_2_hash = submit_uncommitted_block!(node);

    harness.send(base_slice_builder(3).parent_hash(block_2_hash).build()).await;
    harness.send(FlashblockBuilder::new(3, 1).prev(3, 0).build()).await;
    assert!(harness.pending().is_none(), "cached slices must not extend the overlay yet");

    let block_2 = commit_submitted_block!(node, block_2_hash);
    harness.send_canonical(block_2).await;
    let pending = harness.pending().expect("slices beyond the leading limit must be cached");
    assert_eq!(pending.latest_block_number(), 3);
    assert_eq!(pending.latest_flashblock_index(), 1, "the cached continuation is replayed too");
}

/// A slice whose canonical parent has not been imported yet is parked, then
/// replayed when that parent arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_ahead_of_canonical_is_replayed_once_its_parent_lands() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

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
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

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
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

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
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(2)).await;
    assert!(harness.pending().is_none());

    // Canonical jumps straight past block 2, so the parked slice is stale.
    mine!(node);
    let block = mine!(node);
    assert_eq!(block.number, 2);
    harness.send_canonical(block).await;

    assert!(harness.pending().is_none(), "a slice at or below canonical must not be replayed");
}

/// Canonical two blocks behind the slices, with no overlay to start from: a
/// follower catching up after a restart or a dropped overlay.
///
/// A second node runs ahead as the sequencer; the follower makes its blocks
/// canonical only when the test says so. Each round: half of block `k + 2`,
/// canonical `k`, then the rest of `k + 2`. The overlay must stay on the newest
/// slice rather than be dropped by `k + 2`'s live slices.
#[tokio::test(flavor = "multi_thread")]
async fn an_overlay_keeps_up_while_canonical_trails_by_two_blocks() {
    const LAST: u64 = 3;

    let chain_spec = crate::consumer_helpers::flashblocks_importable_chain_spec;
    let (harness, follower) = launch_flashblocks_consumer!(3, 3, chain_spec());
    let (mut sequencer, _http, _wallet, _chain_id) =
        mantle_reth_integration_tests::launch_mantle_node!(
            chain_spec(),
            mantle_reth_cli::node::MantleNode::default(),
            crate::consumer_helpers::flashblocks_payload_attributes
        )
        .await;

    let mut hashes = vec![alloy_primitives::B256::ZERO];
    for _ in 1..=3 {
        let payload = sequencer.advance_block().await.expect("sequencer block");
        let status = follower
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .new_payload(payload.clone().into())
            .await
            .expect("follower imports the block");
        assert!(status.is_valid(), "the follower must accept the sequencer's block: {status:?}");
        hashes.push(payload.block().hash());
    }

    let slice = |block: u64, index: u64| {
        if index == 0 {
            base_slice_builder(block)
                .parent_hash(hashes[block as usize - 1])
                .prev(block - 1, LAST)
                .build()
        } else {
            FlashblockBuilder::new(block, index).prev(block, index - 1).build()
        }
    };
    let position =
        || harness.pending().map(|p| (p.latest_block_number(), p.latest_flashblock_index()));

    for index in 0..=LAST {
        harness.send(slice(2, index)).await;
    }
    assert!(harness.pending().is_none(), "block 2 waits for canonical block 1");

    let mut trace = Vec::new();
    for k in 1..=2u64 {
        harness.send(slice(k + 2, 0)).await;
        harness.send(slice(k + 2, 1)).await;
        let block = commit_submitted_block!(follower, hashes[k as usize]);
        harness.send_canonical(block).await;
        trace.push((format!("after canonical {k}"), position()));

        harness.send(slice(k + 2, 2)).await;
        harness.send(slice(k + 2, 3)).await;
        trace.push((format!("after live ({}, 2..=3)", k + 2), position()));
    }

    assert_eq!(
        trace,
        vec![
            ("after canonical 1".to_string(), Some((3, 1))),
            ("after live (3, 2..=3)".to_string(), Some((3, 3))),
            ("after canonical 2".to_string(), Some((4, 1))),
            ("after live (4, 2..=3)".to_string(), Some((4, 3))),
        ]
    );
}

/// A whole block missed from the stream: the next block's base names a last
/// slice the overlay never saw. The overlay is dropped, but the slices are kept
/// as a restart point and replay once their canonical parent lands.
#[tokio::test(flavor = "multi_thread")]
async fn slices_after_a_missed_block_replay_once_their_parent_lands() {
    const LAST: u64 = 3;

    let chain_spec = crate::consumer_helpers::flashblocks_importable_chain_spec;
    let (harness, follower) = launch_flashblocks_consumer!(3, 3, chain_spec());
    let (mut sequencer, _http, _wallet, _chain_id) =
        mantle_reth_integration_tests::launch_mantle_node!(
            chain_spec(),
            mantle_reth_cli::node::MantleNode::default(),
            crate::consumer_helpers::flashblocks_payload_attributes
        )
        .await;

    let mut hashes = vec![alloy_primitives::B256::ZERO];
    for _ in 1..=3 {
        let payload = sequencer.advance_block().await.expect("sequencer block");
        let status = follower
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .new_payload(payload.clone().into())
            .await
            .expect("follower imports the block");
        assert!(status.is_valid(), "the follower must accept the sequencer's block: {status:?}");
        hashes.push(payload.block().hash());
    }
    let position =
        || harness.pending().map(|p| (p.latest_block_number(), p.latest_flashblock_index()));

    harness.send(base_slice(1)).await;
    for index in 1..=LAST {
        harness.send(FlashblockBuilder::new(1, index).prev(1, index - 1).build()).await;
    }
    assert_eq!(position(), Some((1, LAST)));

    // Block 2 never arrives. Block 3 names its last slice.
    harness.send(base_slice_builder(3).parent_hash(hashes[2]).prev(2, LAST).build()).await;
    assert!(harness.pending().is_none(), "a base that does not link drops the overlay");
    for index in 1..=LAST {
        harness.send(FlashblockBuilder::new(3, index).prev(3, index - 1).build()).await;
    }

    for hash in &hashes[1..=2] {
        let block = commit_submitted_block!(follower, *hash);
        harness.send_canonical(block).await;
    }

    assert_eq!(position(), Some((3, LAST)), "block 3 replays once block 2 lands");
}
