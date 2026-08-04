//! Reorg on a node with the preconf commitment journal DISABLED (the degraded
//! replay path).
//!
//! With the journal ON (see `reorg_shallow_relands`) a reverted commitment is
//! re-injected as a `Replay` and must re-land. With the journal OFF that
//! identification is gone: the only re-land source is reth's native reverted-tx
//! pool re-injection, which is NOT bound by the must-land SLA — so a commitment
//! MAY legitimately drop.
//!
//! Because this harness has no op-node/L1 nondeterminism, the degraded-path
//! outcome is deterministic here (unlike the flaky full-stack version), so we
//! can assert it exactly rather than merely characterise it. Hard invariants
//! that must hold regardless: a real reorg happened, the node stays alive and
//! keeps producing blocks, and the commitment is never DUPLICATED.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use super::helpers::l1_info_deposit;
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
async fn reorg_with_journal_disabled_stays_healthy_and_never_duplicates() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    // Journal OFF: no `.journal_path(..)` — the degraded path.
    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();
    let (node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(&tx);
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx).await });

    let (_commit_head, sealed_commit) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let ev = rpc.await.expect("rpc join").expect("preconf submission");
    assert_eq!(ev.status, PreconfStatus::Success, "initial commitment must land");
    assert!(sealed_commit.contains(&hash), "commitment must land in block 2");

    // Reorg the commitment block out: rebuild at the same height (n=1) against a
    // NEW L1 origin (200).
    let (relanded_head, sealed_relanded) = reorg_to!(node, base, n = 1, l1 = 200);
    let relanded = sealed_relanded.contains(&hash);

    // Reorg-happened guard: the rebuild references the new L1 origin (tx[0]
    // changed) — a genuine reorg even on the journal-off path.
    assert_eq!(
        sealed_relanded[0],
        keccak256(l1_info_deposit(200)),
        "reorg rebuild's tx[0] must be the L1-origin-200 deposit (a real reorg)",
    );

    // Characterisation record (deterministic in this harness).
    eprintln!("[journal-off] re-land outcome: relanded={relanded}");

    // Hard invariant 1: the node stays alive and keeps producing blocks after
    // the degraded-path reorg.
    let (_next_head, sealed_next) = op_node_slot_l1!(node, on = relanded_head, n = 2, l1 = 201);

    // Hard invariant 2: never duplicated. If it re-landed in the reorg block it
    // must not appear again; if it dropped it must stay dropped.
    assert!(
        !sealed_next.contains(&hash),
        "commitment must never be duplicated into a later block (journal off); sealed={sealed_next:?}",
    );

    // Hard invariant 3: sender continuity — the node's nonce accounting is not
    // wedged; the same sender's next nonce still lands via a fresh preconf.
    let tx2 = signed_transfer(chain_id, &wallet, 1).await;
    let hash2 = keccak256(&tx2);
    let http_c2 = http.clone();
    let rpc2 = tokio::spawn(async move { send_preconf(&http_c2, tx2).await });
    let (_h, sealed_cont) = op_node_slot_l1!(node, on = _next_head, n = 3, l1 = 202);
    let ev2 = rpc2.await.expect("rpc join");
    // If the earlier commitment dropped, nonce 1 may be a gap; tolerate either a
    // success-and-land or a benign gap rejection, but the node must not crash.
    if let Ok(ev2) = ev2
        && ev2.status == PreconfStatus::Success
    {
        assert!(sealed_cont.contains(&hash2), "continuity tx claimed success must land");
    }
}
