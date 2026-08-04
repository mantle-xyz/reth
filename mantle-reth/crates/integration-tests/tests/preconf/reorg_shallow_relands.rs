//! A preconf must-land commitment survives a 1-block shallow reorg.
//!
//! A preconf tx that returns `success` is a must-land promise. If the unsafe
//! block carrying it is reverted, the commitment must re-land on the new
//! canonical chain (same hash, exactly once).
//!
//! ── How the reorg is driven ──────────────────────────────────────────────
//! In production a reorg reaches reth as an `engine_forkchoiceUpdatedV3` whose
//! headBlockHash points at an ancestor, followed by a rebuild that re-derives
//! the L2 block against a NEW L1 origin (the L1 block at that height was
//! reorged). We reproduce that in-process: `reorg_to!` FCUs the head back to the
//! ancestor and rebuilds at the SAME height (hence the SAME L2 timestamp — block
//! time is height-derived, fixed 2s) but with a DIFFERENT L1 origin, injected as
//! the block's tx[0] L1-attributes deposit. So the rebuilt block differs from
//! the one it replaces ONLY by its L1 origin — a faithful, observable reorg,
//! not a timestamp artifact. `engine_*` over authrpc is only a thin wrapper over
//! the in-process engine handles these macros call, so the preconf builder /
//! replay / carryover logic exercised is identical.

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

/// A preconf commitment landed in an unsafe block re-lands after that block's
/// L1 origin is reorged out (here: an engine FCU back to the parent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_commitment_relands_after_shallow_reorg() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(fresh_journal("shallow")) // journal ON — so the reverted commitment can be replayed
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Block at height n=0 — the reorg base, referencing L1 origin 1.
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    // Submit the preconf tx. `send_preconf` blocks until the tx lands, so it must
    // be IN-FLIGHT while the commitment block is built: spawn it, build, join.
    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(&tx);
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx).await });

    // Commitment block (height n=1) — referencing L1 origin 100. tx[0] is the
    // L1-attributes deposit; the preconf lands right after it.
    let (_commit_head, sealed_commit) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let ev = rpc.await.expect("rpc join").expect("preconf submission must not error");
    assert_eq!(ev.status, PreconfStatus::Success, "must-land commitment; reason={:?}", ev.reason);
    assert!(
        sealed_commit.contains(&hash),
        "committed tx must land in the commitment block; sealed={sealed_commit:?}",
    );
    assert_eq!(
        sealed_commit[0],
        keccak256(l1_info_deposit(100)),
        "commitment block's tx[0] must be the L1-origin-100 deposit",
    );

    // ── Reorg: rebuild at the SAME height (n=1 → SAME timestamp) but a DIFFERENT
    //    L1 origin (200). This reverts the commitment block and re-derives it
    //    against a new L1 origin — exactly what an L1 reorg does. The commitment
    //    must re-land via the preconf carryover / journal-replay path.
    let (relanded_head, sealed_relanded) = reorg_to!(node, base, n = 1, l1 = 200);
    assert!(
        sealed_relanded.contains(&hash),
        "committed tx must RE-LAND after the reorg; sealed={sealed_relanded:?}",
    );

    // Reorg-happened guard (faithful — NOT a timestamp artifact): the rebuilt
    // block references the NEW L1 origin, so its tx[0] differs from the reverted
    // block's. This proves the commitment block was genuinely reverted and
    // rebuilt against a different L1 origin, not left canonical.
    assert_eq!(
        sealed_relanded[0],
        keccak256(l1_info_deposit(200)),
        "reorg rebuild's tx[0] must be the L1-origin-200 deposit",
    );
    assert_ne!(
        sealed_relanded[0], sealed_commit[0],
        "reorg must change the L1 origin (tx[0]) — otherwise no real reorg happened",
    );

    // No resurrection/duplicate: a following block (n=2) must not re-include the
    // now-committed tx.
    let (_next_head, sealed_next) = op_node_slot_l1!(node, on = relanded_head, n = 2, l1 = 201);
    assert!(
        !sealed_next.contains(&hash),
        "commitment must not be duplicated into a later block; sealed={sealed_next:?}",
    );
}
