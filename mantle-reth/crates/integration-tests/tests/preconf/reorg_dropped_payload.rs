//! A preconf commitment whose build is dropped (opened but never getPayload'd)
//! re-lands in the next build via FIFO carryover — same hash, exactly once.
//!
//! This is the END-TO-END regression for `ensure_only_one_payload`. op-node can
//! open a build, miss the slot (never call getPayload), then open a fresh build
//! on the same parent. The superseded build MUST be cancelled so it cannot
//! "steal" the committed preconf into a block that is never sealed; the fresh
//! build's carryover preamble MUST re-dispatch the surviving Success FIFO entry
//! so the commitment still lands. Unit tests of the carryover function alone did
//! not catch the superseded-build bug — it surfaced on testnet — so only
//! exercising the real open→drop→reopen engine sequence guards against it.
//!
//! Engine calls exercised (the full op-node → reth request sequence):
//!   1. `engine_forkchoiceUpdatedV3(attrs)` on H   -> build#1 opened (`payload_id_1`)
//!   2. `eth_sendRawTransactionWithPreconf`         -> preconf enters the FIFO (Success)
//!   3. `engine_forkchoiceUpdatedV3(attrs`') on H   -> build#2 opened; `ensure_only_one_payload`
//!      cancels build#1 (never getPayload'd)
//!   4. `engine_getPayloadV5(payload_id_2)`         -> carryover re-dispatches the preconf
//!   5. `engine_newPayloadV4` + forkchoiceUpdated   -> commit build#2

use super::helpers::{PreconfCfgBuilder, fresh_journal, send_preconf};
use crate::{
    fcu_v3_commit, fcu_v3_start, get_payload_v5, launch_preconf_node, new_payload_v4, op_node_slot,
};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use std::time::Duration;

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
async fn dropped_payload_relands_via_carryover_in_fresh_build() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(fresh_journal("dropped"))
        .build();
    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let parent = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;

    // 1. op-node opens build#1 on `parent` (FCU + attrs). No getPayload — it will be dropped.
    let attrs1 = node.payload.next_attributes();
    let pid1 = fcu_v3_start!(node, parent, attrs1);
    // Let build#1 subscribe to the preconf broadcast before we submit.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 2. Submit the preconf; it registers a Success FIFO entry. `send_preconf` blocks until the tx
    //    lands (in build#2 below), so spawn it.
    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(&tx);
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx).await });
    tokio::time::sleep(Duration::from_millis(600)).await;

    // build#1 was never getPayload'd/committed: the head must still be `parent`.
    assert_eq!(
        node.current_forkchoice_state().expect("forkchoice state").head_block_hash,
        parent,
        "dropped build#1 must not have advanced the head",
    );

    // 3. op-node opens a FRESH build#2 on the SAME parent with new attrs. Distinct attrs → distinct
    //    payload_id: a genuinely new build. Opening it triggers `ensure_only_one_payload`,
    //    cancelling the lingering build#1.
    let attrs2 = node.payload.next_attributes();
    let pid2 = fcu_v3_start!(node, parent, attrs2);
    assert_ne!(pid1, pid2, "build#2 must be a genuinely new build, not the cached build#1");

    // 4./5. getPayload build#2 → carryover re-dispatches the surviving Success
    //       entry; then commit build#2.
    let payload2 = get_payload_v5!(node, pid2);
    let sealed: Vec<B256> =
        payload2.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    let head2 = new_payload_v4!(node, payload2);
    fcu_v3_commit!(node, head2);

    let ev = rpc.await.expect("rpc join").expect("preconf submission must not error");
    assert_eq!(ev.status, PreconfStatus::Success, "must-land commitment; reason={:?}", ev.reason);

    // The commitment re-lands via carryover in the fresh build#2 — proving the
    // superseded build#1 neither stole it nor left it stranded.
    assert!(
        sealed.contains(&hash),
        "dropped-payload commitment must re-land via carryover in build#2; sealed={sealed:?}",
    );

    // Exactly once: a following block must not re-include the now-committed tx.
    let (_next_head, sealed_next) = op_node_slot!(node, on = head2);
    assert!(
        !sealed_next.contains(&hash),
        "commitment must not be duplicated into a later block; sealed={sealed_next:?}",
    );
}
