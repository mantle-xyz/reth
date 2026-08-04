//! Reorg attribution via ordering: prove a reverted preconf commitment re-lands
//! through the PRECONF carryover path, not merely reth's native pool
//! re-injection.
//!
//! A plain re-land assertion (the tx reappears on chain) can pass vacuously: a
//! transfer would re-land via ordinary pool mechanics even if the preconf
//! must-land machinery were removed. The discriminator: the preconf builder
//! applies carryover/replay entries in a preamble BEFORE the tip-ordered pool
//! loop, and the preconf FIFO is arrival-ordered / tip-AGNOSTIC. So we co-submit
//!
//!   - P: a preconf tx with a LOW tip (whitelisted from+to), and
//!   - N: a normal tx with a HIGH tip (non-whitelisted recipient → pool path),
//!
//! and after a reorg assert P is ordered AHEAD of the higher-tip N. Under pure
//! pool ordering N would win; P winning proves the carryover path placed it.

use super::helpers::{
    PreconfCfgBuilder, fresh_journal, mantle_test_chain_spec, send_normal, send_preconf,
};
use crate::{launch_preconf_node, op_node_slot_l1, reorg_to};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"; // whitelisted (preconf)
const NORMAL_RECIPIENT: &str = "0x00000000000000000000000000000000000000B2"; // NOT whitelisted

const LOW_TIP: u128 = 1e9 as u128; // preconf tx: 1 gwei
const HIGH_TIP: u128 = 200e9 as u128; // normal tx: 200 gwei

async fn signed_transfer(
    chain_id: u64,
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    tip: u128,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(21_000),
        max_fee_per_gas: Some(HIGH_TIP), // fee cap >= any tip; basefee is 0 here
        max_priority_fee_per_gas: Some(tip),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(signer.clone(), request).await.encoded_2718().into()
}

/// Index of `hash` within a block-ordered sealed list, if present.
fn index_of(sealed: &[B256], hash: &B256) -> Option<usize> {
    sealed.iter().position(|h| h == hash)
}

/// Journal ON: after a reorg the low-tip preconf P re-lands AHEAD of the
/// high-tip normal N — a clean tip-order inversion proving carryover placement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_carryover_orders_ahead_of_higher_tip_after_reorg() {
    let preconf_to: Address = RECIPIENT.parse().unwrap();
    let normal_to: Address = NORMAL_RECIPIENT.parse().unwrap();

    let chain_id = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(3).with_chain_id(chain_id).wallet_gen();
    let preconf_signer = &signers[0]; // whitelisted from
    let normal_signer = &signers[2]; // NOT whitelisted (idx 1 collides w/ RECIPIENT)

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(preconf_signer.address())
        .whitelist_to(preconf_to)
        .journal_path(fresh_journal("ordering"))
        .build();
    let (node, http, _wallet, chain_id) = launch_preconf_node!(cfg).await;

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    // P: low-tip preconf; N: high-tip normal. Submit both before one build so
    // they co-locate: P via the preconf FIFO, N via the tip-ordered pool. (tx[0]
    // of every block is the L1-attributes deposit, so the user txs start at 1.)
    let tx_p = signed_transfer(chain_id, preconf_signer, 0, preconf_to, LOW_TIP).await;
    let hash_p = keccak256(&tx_p);
    let tx_n = signed_transfer(chain_id, normal_signer, 0, normal_to, HIGH_TIP).await;
    let hash_n = keccak256(&tx_n);

    send_normal(&http, tx_n).await.expect("normal submit"); // pool, returns immediately
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx_p).await });

    let (_commit_head, sealed) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let ev = rpc.await.expect("rpc join").expect("preconf submission");
    assert_eq!(ev.status, PreconfStatus::Success, "P must be a success commitment");

    // Live ordering: even before the reorg, the preconf FIFO places low-tip P
    // ahead of high-tip N (both after the tx[0] L1-info deposit).
    let (ip, in_) = (index_of(&sealed, &hash_p), index_of(&sealed, &hash_n));
    eprintln!("[ordering] pre-reorg  P idx={ip:?}  N idx={in_:?}  sealed_len={}", sealed.len());
    assert!(ip.is_some() && in_.is_some(), "both P and N must land in block 2; sealed={sealed:?}");
    assert!(ip < in_, "low-tip preconf P must precede high-tip normal N pre-reorg; sealed={sealed:?}");

    // ── Reorg (same height n=1, NEW L1 origin 200), then assert P re-lands
    //    through the carryover preamble: it must be the FIRST user tx (index 1,
    //    right after the L1-info deposit), ahead of any pool tx. The normal N
    //    re-lands via reth's native pool re-injection, not bound to the same
    //    block — so we only assert the ordering when the two co-locate.
    let (_relanded_head, sealed_re) = reorg_to!(node, base, n = 1, l1 = 200);
    let (ip_re, in_re) = (index_of(&sealed_re, &hash_p), index_of(&sealed_re, &hash_n));
    eprintln!("[ordering] post-reorg P idx={ip_re:?}  N idx={in_re:?}  sealed_len={}", sealed_re.len());
    assert_eq!(
        ip_re,
        Some(1),
        "re-landed preconf P must be the first user tx (after the L1-info deposit) via carryover, \
         not tip-ordered pool; sealed={sealed_re:?}",
    );
    if let (Some(ip), Some(inx)) = (ip_re, in_re) {
        assert!(ip < inx, "when co-located, low-tip P must precede high-tip N; sealed={sealed_re:?}");
    }
}

/// Journal OFF (characterisation): N (normal pool) re-lands regardless; P may
/// drop on the degraded path. The only conditional invariant: if P re-lands in
/// N's block, it must still precede N. The node must stay alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordering_journal_off_characterisation() {
    let preconf_to: Address = RECIPIENT.parse().unwrap();
    let normal_to: Address = NORMAL_RECIPIENT.parse().unwrap();

    let chain_id = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(3).with_chain_id(chain_id).wallet_gen();
    let preconf_signer = &signers[0];
    let normal_signer = &signers[2];

    // Journal OFF.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(preconf_signer.address())
        .whitelist_to(preconf_to)
        .build();
    let (node, http, _wallet, chain_id) = launch_preconf_node!(cfg).await;

    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    let tx_p = signed_transfer(chain_id, preconf_signer, 0, preconf_to, LOW_TIP).await;
    let hash_p = keccak256(&tx_p);
    let tx_n = signed_transfer(chain_id, normal_signer, 0, normal_to, HIGH_TIP).await;
    let hash_n = keccak256(&tx_n);

    send_normal(&http, tx_n).await.expect("normal submit");
    let http_c = http.clone();
    let rpc = tokio::spawn(async move { send_preconf(&http_c, tx_p).await });
    let (_commit_head, sealed) = op_node_slot_l1!(node, on = base, n = 1, l1 = 100);
    let _ = rpc.await.expect("rpc join");
    assert!(index_of(&sealed, &hash_n).is_some(), "N must land in block 2");

    let (relanded_head, sealed_re) = reorg_to!(node, base, n = 1, l1 = 200);

    // Accumulate the re-land outcome over the rebuild block and a few following
    // ones — on the degraded path re-injection is not bound to a single block.
    let mut all: Vec<B256> = sealed_re.clone();
    let mut head = relanded_head;
    let mut fn_n = 2u64;
    for _ in 0..3 {
        let (h, s) = op_node_slot_l1!(node, on = head, n = fn_n, l1 = 200 + fn_n);
        fn_n += 1;
        // When P and N happen to co-locate in a block, the carryover invariant
        // still holds: low-tip P precedes high-tip N.
        if let (Some(ip), Some(inx)) = (index_of(&s, &hash_p), index_of(&s, &hash_n)) {
            assert!(ip < inx, "co-located: low-tip P must precede high-tip N; sealed={s:?}");
        }
        all.extend(s);
        head = h;
    }

    // Characterisation (deterministic here): record each tx's fate. The hard
    // invariant is only that the node stayed alive — every build above returned
    // a payload, so reaching here proves liveness on the degraded path.
    let p_relanded = all.contains(&hash_p);
    let n_relanded = all.contains(&hash_n);
    eprintln!("[ordering journal-off] P re-landed={p_relanded}  N re-landed={n_relanded}");
}
