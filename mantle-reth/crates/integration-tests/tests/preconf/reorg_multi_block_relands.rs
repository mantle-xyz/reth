//! Preconf commitments spread across several consecutive unsafe blocks all
//! re-land after those blocks are reorged out at once.
//!
//! Extends the single-block `reorg_shallow_relands` to the deeper
//! (2~3 block) reorg path: build N commitment blocks in a row, then FCU the
//! head all the way back to a common ancestor, reverting every commitment
//! block in one shot. All commitments must re-land on the new branch, none
//! dropped, none duplicated. Re-land is via the journal-replay path (each
//! committed block's entry is persisted, then re-injected as `Replay` when the
//! reorg's canonical-state notification reports its block reverted).

use super::helpers::{
    PreconfCfgBuilder, fresh_journal, l1_info_deposit, mantle_test_chain_spec, send_preconf,
};
use crate::{launch_preconf_node, op_node_slot_l1, reorg_to};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const NUM_COMMITMENTS: usize = 3;

async fn signed_transfer(
    chain_id: u64,
    signer: &PrivateKeySigner,
    nonce: u64,
) -> alloy_primitives::Bytes {
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
    TransactionTestContext::sign_tx(signer.clone(), request).await.encoded_2718().into()
}

/// N preconf commitments landed across N consecutive unsafe blocks re-land
/// after a single reorg reverts all of those blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_commitments_reland_after_multi_block_reorg() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    // A distinct funded sender per commitment keeps nonces independent. All are
    // whitelisted `from`; `RECIPIENT` is the whitelisted `to`. Signers must carry
    // the launched chain id (5000) or `sign_tx` rejects the chainId mismatch.
    let chain_id = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(NUM_COMMITMENTS).with_chain_id(chain_id).wallet_gen();
    let mut cfg = PreconfCfgBuilder::new().whitelist_to(recipient);
    for s in &signers {
        cfg = cfg.whitelist_from(s.address());
    }
    let cfg = cfg.journal_path(fresh_journal("multiblock")).build();

    let (mut node, http, _wallet, launched) = launch_preconf_node!(cfg).await;
    assert_eq!(launched, chain_id, "launched chain id must match signer chain id");

    // Height n=0 — the common ancestor we later rewind to (L1 origin 1).
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    // Heights 1..=N — one commitment each, on consecutive parents, referencing
    // L1 origins 101.. .
    let mut hashes: Vec<B256> = Vec::with_capacity(NUM_COMMITMENTS);
    let mut parent = base;
    for (i, signer) in signers.iter().enumerate() {
        let tx = signed_transfer(chain_id, signer, 0).await;
        let hash = keccak256(&tx);
        hashes.push(hash);

        let http_c = http.clone();
        let rpc = tokio::spawn(async move { send_preconf(&http_c, tx).await });
        let n = (i + 1) as u64;
        let (head, sealed) = op_node_slot_l1!(node, on = parent, n = n, l1 = 100 + n);
        let ev = rpc.await.expect("rpc join").expect("preconf submission");
        assert_eq!(ev.status, PreconfStatus::Success, "commitment {i} must be success");
        assert!(sealed.contains(&hash), "commitment {i} must land; sealed={sealed:?}");
        parent = head;
    }

    // ── Reorg all commitment blocks out in one shot: rebuild at the FIRST
    //    commitment's height (n=1 → same timestamp) against a NEW L1 origin
    //    (200). This reverts the whole commitment chain. Every commitment must
    //    re-land (via journal replay); the SLA promises eventual re-land, not
    //    same-block, so the re-injected commitments may drain across the rebuild
    //    block and a few following blocks — accumulate and count.
    use std::collections::HashMap;
    let mut counts: HashMap<B256, usize> = HashMap::new();
    let (mut head, sealed0) = reorg_to!(node, base, n = 1, l1 = 200);

    // Reorg-happened guard: the rebuilt block references the NEW L1 origin, so
    // its tx[0] differs from the reverted first commitment block (L1 origin 101).
    assert_eq!(
        sealed0[0],
        keccak256(l1_info_deposit(200)),
        "reorg rebuild's tx[0] must be the L1-origin-200 deposit (a real reorg)",
    );

    for h in &sealed0 {
        *counts.entry(*h).or_default() += 1;
    }
    let mut fn_n = 2u64;
    for _ in 0..(NUM_COMMITMENTS + 1) {
        if hashes.iter().all(|h| counts.contains_key(h)) {
            break;
        }
        let (h, s) = op_node_slot_l1!(node, on = head, n = fn_n, l1 = 200 + fn_n);
        for x in &s {
            *counts.entry(*x).or_default() += 1;
        }
        head = h;
        fn_n += 1;
    }

    // Primary invariant — must-land: every commitment must come back after the
    // reorg (no omission). This is the core preconf SLA the multi-block reorg
    // stresses.
    //
    // Secondary invariant — no duplicate: a re-landed commitment must not appear
    // twice. Deep no-duplicate (late resurrection over many blocks) is the
    // dedicated focus of `reorg_no_duplicate`; here we just guard the accumulated
    // window.
    for (i, hash) in hashes.iter().enumerate() {
        let n = counts.get(hash).copied().unwrap_or(0);
        assert!(
            n >= 1,
            "commitment {i} must RE-LAND after the multi-block reorg (must-land SLA) — \
             got 0, it was DROPPED",
        );
        assert!(n <= 1, "commitment {i} must not be duplicated after re-land — got {n}");
    }
}
