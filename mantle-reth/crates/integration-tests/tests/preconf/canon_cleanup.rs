//! Canonical-commit driven cleanup of preconf state.
//!
//! After a preconf-eligible tx is sealed and its block is committed to
//! canonical, the sender's fifo entry no longer holds the (sender, nonce)
//! slot: the next-nonce tx from the same sender is admitted, applied and
//! lands in the following block.

use super::helpers::{send_preconf, PreconfCfgBuilder};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, B256, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

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

/// A preconf tx that lands in block N advances the sender's on-chain nonce;
/// after the block is committed to canonical, submitting the next-nonce tx
/// through the preconf RPC must succeed and land in block N+1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canon_commit_permits_next_nonce_from_same_sender() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: submit nonce=0, seal, commit to canonical ────────────
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;

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

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let event = rpc_task.await.expect("rpc join").expect("nonce=0 must succeed");
    assert!(matches!(event.status, PreconfStatus::Success));
    let hash0 = event.tx_hash;

    // Commit to canonical: submit the payload, then push forkchoice with
    // head/safe/finalized all pointing at the new block. This is what
    // triggers the canon handler's forward + clean_reclaimable.
    let new_head = node.submit_payload(payload).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");

    // Give the canon handler a beat to process the notification.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Slot 2: submit nonce=1, seal, verify it lands ────────────────
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;

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

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let event = rpc_task.await.expect("rpc join").expect("nonce=1 must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "next-nonce preconf must succeed after canonical commit; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.block_height, 2, "next-nonce tx must be predicted for block 2");

    let block = payload.block();
    assert_eq!(block.number, 2);
    let sealed: Vec<B256> = block
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(sealed.contains(&event.tx_hash), "block 2 must contain the nonce=1 tx");
    assert!(
        !sealed.contains(&hash0),
        "block 2 must not re-include the already-canon nonce=0 tx",
    );
}
