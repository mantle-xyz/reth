//! Slices extend the overlay only when they name its slices by position and content.
//!
//! A base slice whose parent block is in the overlay must name that block's last
//! slice; anything else restarts from the canonical parent. A later slice of the
//! tracked build must name the overlay's latest slice by content, or the overlay
//! is dropped.

use alloy_network::{ReceiptResponse, TransactionResponse};
use alloy_primitives::{B256, U256};

use crate::consumer_helpers::{
    FlashblockBuilder, LOGGER, base_slice, base_slice_builder, commit_submitted_block,
    l1_info_deposit, launch_flashblocks_consumer, mine_canonical_block, signed_call,
    signed_call_with_fees, submit_uncommitted_block, test_sender,
};

/// Block 1 is sent as two slices so that naming its *first* slice would not link:
/// the check has to find the block's last slice, not just any slice of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_naming_the_overlays_last_slice_extends_it() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;
    harness.send(base_slice_builder(2).prev(1, 1).build()).await;

    let pending = harness.pending().expect("the linked base extends the overlay");
    assert_eq!(pending.latest_block_number(), 2);
    assert_eq!(pending.earliest_block_number(), 1);
}

/// Sends block 1 as two slices, then `block_2_base`, with block 1 submitted but not
/// yet canonical. Returns once block 1 is canonical and replayed against.
macro_rules! base_refused_then_replayed_on_canonical {
    ($block_2_base:expr) => {{
        let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
        let block_1_hash = submit_uncommitted_block!(node);

        harness.send(base_slice(1)).await;
        harness.send(FlashblockBuilder::new(1, 1).prev(1, 0).build()).await;
        harness.send($block_2_base.parent_hash(block_1_hash).build()).await;

        assert!(
            harness.pending().is_none_or(|pending| pending.earliest_block_number() > 1),
            "a base that does not link must not extend the overlay over block 1"
        );

        let block_1 = commit_submitted_block!(node, block_1_hash);
        harness.send_canonical(block_1).await;

        let pending = harness.pending().expect("the refused base replays on canonical block 1");
        assert_eq!(pending.latest_block_number(), 2);
        assert_eq!(pending.earliest_block_number(), 2, "built on the canonical parent");
    }};
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_with_the_wrong_content_hash_waits_for_the_canonical_parent() {
    base_refused_then_replayed_on_canonical!(
        base_slice_builder(2).prev(1, 1).prev_hash(B256::repeat_byte(9))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_with_no_predecessor_does_not_extend_the_overlay() {
    base_refused_then_replayed_on_canonical!(base_slice_builder(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_slice_with_the_wrong_content_hash_drops_the_overlay() {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    assert!(harness.pending().is_some());
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).prev_hash(B256::repeat_byte(9)).build())
        .await;

    assert!(harness.pending().is_none(), "a slice that does not follow by content drops it");
}

/// Block 1 is canonical but its notification has not been handled, so the overlay
/// still holds block 1 and block 2's base links to it. The only thing wrong is the
/// parent hash, which names a block 1 the chain does not have.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_on_a_parent_the_chain_does_not_have_is_dropped() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);

    harness.send(base_slice(1)).await;
    mine_canonical_block!(node);
    harness
        .send(base_slice_builder(2).prev(1, 0).parent_hash(B256::repeat_byte(0x55)).build())
        .await;

    assert!(harness.pending().is_none(), "a base on a parent the chain does not have is dropped");
}

/// Canonical block 1 carries exactly the overlay's transactions for block 1, so
/// reconciliation keeps the overlay. Block 2 was built on a different block 1
/// (here, the hash of the overlay's last slice of block 1), so it must not carry
/// on: matching transactions do not make the headers match.
#[tokio::test(flavor = "multi_thread")]
async fn same_transactions_different_header_does_not_carry_on() {
    let (harness, mut node) = launch_flashblocks_consumer!(3, 3);
    let transfer = signed_call(0, LOGGER, U256::from(1)).await;
    node.rpc.inject_tx(transfer.clone()).await.expect("inject the transfer");

    harness.send(base_slice(1)).await;
    harness
        .send(FlashblockBuilder::new(1, 1).prev(1, 0).transactions(vec![transfer.clone()]).build())
        .await;
    harness
        .send(
            base_slice_builder(2)
                .prev(1, 1)
                .parent_hash(FlashblockBuilder::slice_hash(1, 1))
                .build(),
        )
        .await;
    let pending = harness.pending().expect("block 2 extends the overlay before block 1 lands");
    assert_eq!((pending.earliest_block_number(), pending.latest_block_number()), (1, 2));

    let block_1 = mine_canonical_block!(node);
    let canonical_txs: Vec<B256> =
        block_1.body().transactions.iter().map(|tx| *tx.hash()).collect();
    let overlay_txs: Vec<B256> =
        pending.get_transactions_for_block(1).map(|tx| tx.tx_hash()).collect();
    assert_eq!(
        canonical_txs,
        vec![
            alloy_primitives::keccak256(l1_info_deposit(1)),
            alloy_primitives::keccak256(&transfer)
        ],
        "premise: canonical block 1 holds the deposit and the transfer",
    );
    assert_eq!(overlay_txs, canonical_txs, "premise: the transaction lists match");
    assert_ne!(block_1.hash(), FlashblockBuilder::slice_hash(1, 1), "premise: headers differ");

    harness.send_canonical(block_1).await;

    assert!(harness.pending().is_none(), "block 2 was built on a block 1 the chain does not have");
}

/// Sender's balance and the transfer's gas used, after block 2 opens with a base
/// declaring `declared` and carries one transfer paying a 1 gwei tip.
async fn sender_balance_after_a_block_declaring(declared: U256) -> (U256, u64) {
    let (harness, _node) = launch_flashblocks_consumer!(3, 3);
    let transfer =
        signed_call_with_fees(0, LOGGER, U256::from(1), 20_000_000_000, 1_000_000_000).await;
    let transfer_hash = alloy_primitives::keccak256(&transfer);

    harness.send(base_slice(1)).await;
    harness
        .send(
            base_slice_builder(2)
                .prev(1, 0)
                .base_fee(declared)
                .transactions(vec![l1_info_deposit(2), transfer])
                .build(),
        )
        .await;

    let pending = harness.pending().expect("block 2 extends the overlay");
    assert_eq!(pending.latest_block_number(), 2);
    let gas_used = pending.get_receipt(transfer_hash).expect("a receipt").gas_used();
    (pending.get_balance(test_sender()).expect("the sender was touched"), gas_used)
}

/// Execution charges the basefee the base payload declares, not the one derived from
/// the parent block (zero on the test chain).
#[tokio::test(flavor = "multi_thread")]
async fn execution_uses_the_basefee_the_base_declares() {
    let (cheap, gas_used) =
        sender_balance_after_a_block_declaring(U256::from(3_000_000_000u64)).await;
    let (dear, same_gas) =
        sender_balance_after_a_block_declaring(U256::from(7_000_000_000u64)).await;

    assert_eq!(gas_used, same_gas, "premise: the same transfer uses the same gas");
    assert_eq!(
        cheap - dear,
        U256::from(gas_used) * U256::from(4_000_000_000u64),
        "a 4 gwei higher declared basefee must cost the sender exactly that much more"
    );
}
