//! A build standing on a block a reorg has taken back does not replay the
//! commitments that reorg re-injected.
//!
//! The reorg here is a forkchoice update without attributes onto a sibling
//! block, which leaves the running build alive: the payload service abandons a
//! job only when a later one replaces it. The re-injected commitment is
//! announced to that build, and its nonce is already spent in the build's
//! parent. Executing it there fails nonce-too-low and ends it as broken; the
//! build has to notice its parent is no longer on the canonical chain and stop
//! instead.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use super::helpers::{PreconfCfgBuilder, fresh_journal, l1_attrs, send_preconf};
use crate::{
    canonize_built, fcu_v3_start, get_payload_v5, launch_preconf_node_with_fifo, op_node_slot_l1,
};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_on_a_reorged_parent_leaves_the_requeued_commitment_to_the_new_head() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(fresh_journal("stale-parent"))
        .build();
    let (node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    // The block the reorg will switch to, built before the commitment exists
    // so it does not carry it. Inserted, not made canonical.
    let sibling_id = fcu_v3_start!(node, base, l1_attrs(1, 200));
    let sibling = get_payload_v5!(node, sibling_id);
    node.submit_payload(sibling.clone()).await.expect("engine_newPayloadV4 submit");

    // The commitment, landed in a competing block at the same height.
    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(&tx);
    let rpc = tokio::spawn({
        let http = http.clone();
        async move { send_preconf(&http, tx).await }
    });
    let (committed, sealed) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let ev = rpc.await.expect("rpc join").expect("preconf submission must not error");
    assert_eq!(ev.status, PreconfStatus::Success, "reason={:?}", ev.reason);
    assert!(sealed.contains(&hash), "precondition: the commitment landed");

    // A build on the committed block, where the sender's nonce 0 is spent.
    let _stale_build = fcu_v3_start!(node, committed, l1_attrs(2, 101));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reorg onto the sibling, without attributes: the build above keeps running,
    // and the canon handler re-injects the commitment.
    let new_head = canonize_built!(node, sibling);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        fifo.contains(&hash).await,
        "the build on the reverted block must not have executed, and so ended, the commitment",
    );

    // A build on the new head lands it.
    let (_, sealed_next) = op_node_slot_l1!(node, on = new_head, n = 2, l1 = 201);
    assert!(sealed_next.contains(&hash), "re-landed on the new head; sealed={sealed_next:?}");
}
