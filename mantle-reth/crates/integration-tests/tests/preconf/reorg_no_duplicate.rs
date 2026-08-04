//! A re-landed commitment is never resurrected or duplicated by a stale replay
//! entry, and the sender keeps working.
//!
//! The pure internal skip (new chain already contains the tx AND replay hits an
//! already-consumed nonce in one build) is pinned by `replay_nonce_consumed`.
//! What this test asserts end-to-end: after a real reorg re-lands the
//! commitment and the chain advances, the sender's nonce is consumed, so any
//! lingering journal/replay entry must be forwarded past and dropped on every
//! later build — the hash stays in exactly one block, and the same sender's
//! next nonces still land.

use super::helpers::{PreconfCfgBuilder, fresh_journal, l1_info_deposit, send_preconf};
use crate::{launch_preconf_node, op_node_slot_l1, reorg_to};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256, keccak256};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relanded_commitment_not_duplicated_and_sender_continues() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(fresh_journal("dedup"))
        .build();
    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    // Commit nonce 0 in the commitment block (height n=1, L1 origin 100).
    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(&tx);
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx).await });
    let (_commit_head, sealed_commit) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let ev = rpc.await.expect("rpc join").expect("preconf submission");
    assert_eq!(ev.status, PreconfStatus::Success);
    assert!(sealed_commit.contains(&hash));

    // Reorg → rebuild at the same height (n=1) against a NEW L1 origin (200);
    // the commitment re-lands.
    let (mut head, sealed_re) = reorg_to!(node, base, n = 1, l1 = 200);
    assert_eq!(
        sealed_re[0],
        keccak256(l1_info_deposit(200)),
        "reorg rebuild's tx[0] must be the L1-origin-200 deposit (a real reorg)",
    );
    assert!(sealed_re.contains(&hash), "commitment must re-land; sealed={sealed_re:?}");

    // Keep building with the SAME sender's next nonces. The original hash must
    // never reappear (no resurrection), and each new nonce must land (nonce
    // accounting is not wedged by the cleaned-up replay entry).
    for nonce in 1..=3u64 {
        let txn = signed_transfer(chain_id, &wallet, nonce).await;
        let hashn = keccak256(&txn);
        let http_c = http.clone();
        let rpc = tokio::spawn(async move { send_preconf(&http_c, txn).await });
        let (h, sealed) = op_node_slot_l1!(node, on = head, n = 1 + nonce, l1 = 200 + nonce);
        let ev = rpc.await.expect("rpc join").expect("preconf submission");
        assert_eq!(ev.status, PreconfStatus::Success, "nonce {nonce} must succeed");
        assert!(sealed.contains(&hashn), "nonce {nonce} tx must land; sealed={sealed:?}");
        assert!(
            !sealed.contains(&hash),
            "original commitment must NOT reappear in block for nonce {nonce}; sealed={sealed:?}",
        );
        head = h;
    }
}
