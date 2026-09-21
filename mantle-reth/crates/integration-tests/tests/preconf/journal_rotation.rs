//! Journal **rotation** lifecycle, end-to-end.
//!
//! `journal.rs` unit tests cover `rotate()` at the disk layer (entries the
//! `retain` predicate refuses are dropped, the rest kept, atomic rename). This
//! module chains `rotate()` with the *restore* path to pin the property
//! operators actually care about: **a commitment observed on chain and buried
//! deep enough to be rotated out is NOT replayed after a restart** — so a
//! rotated-then-restarted node never double-lands an already-canon tx.
//!
//! Rotation here is driven by calling `PreconfJournal::rotate()` directly
//! (the same call the background `run_rejournal_loop` makes on its
//! interval) so the test is deterministic — no dependency on the 60s
//! loop cadence. Size-triggered rotation is exercised separately, end-to-end
//! through a running node, in `journal_size_rotation.rs`; this module stays
//! focused on the disk-layer `rotate()` semantics + the restore-replay
//! property.
//!
//! Coverage:
//! - `rotate_drops_sealed_then_relaunch_replays_only_survivors` — three independent senders'
//!   commitments journaled; one marked sealed and rotated out; a node launched against the rotated
//!   file replays only the two survivors.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::{JournalEntry, PreconfJournal};
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Sign a nonce-0 transfer to `RECIPIENT` from an arbitrary signer.
async fn signed_nonce0_transfer(
    chain_id: u64,
    signer: alloy_signer_local::PrivateKeySigner,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(signer, request).await.encoded_2718().into()
}

fn journal_entry(raw: &alloy_primitives::Bytes) -> JournalEntry {
    JournalEntry { hash: keccak256(raw), tx_rlp: raw.clone(), block_height: 1, committed_at_ms: 0 }
}

/// Append three commitments (independent senders, each nonce 0), mark one
/// sealed, `rotate()`, then launch a node against the rotated file. The
/// sealed+dropped commitment must NOT be replayed; the two survivors must
/// land in block 1.
///
/// Independent senders (rather than sequential nonces from one sender)
/// are deliberate: the survivors must be applicable on a fresh chain
/// without the dropped entry, which a nonce chain would prevent
/// (`nonce=1` needs `nonce=0` first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotate_drops_sealed_then_relaunch_replays_only_survivors() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // Sender A = Hardhat[0]; B = signers[2]; C = signers[3]. Index 1
    // collides with RECIPIENT (see happy_path::multi_sender), so it is
    // skipped.
    let signers = Wallet::new(4).with_chain_id(chain_id).wallet_gen();
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = signers[2].clone();
    let signer_c = signers[3].clone();
    let sender_b = signer_b.address();
    let sender_c = signer_c.address();

    let tx_a = signed_nonce0_transfer(chain_id, wallet_a.inner.clone()).await;
    let tx_b = signed_nonce0_transfer(chain_id, signer_b).await;
    let tx_c = signed_nonce0_transfer(chain_id, signer_c).await;
    let (hash_a, hash_b, hash_c) = (keccak256(&tx_a), keccak256(&tx_b), keccak256(&tx_c));

    // Unique tempdir + journal path.
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-rotate-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal dir");
    let journal_file = journal_dir.join("preconf.journal");

    // ── Phase 1: build + seal + rotate the journal directly ────────────
    {
        let journal = PreconfJournal::open(&journal_file, 0).await.expect("open journal");
        journal.append_promised(&journal_entry(&tx_a)).await.expect("append A");
        journal.append_promised(&journal_entry(&tx_b)).await.expect("append B");
        journal.append_promised(&journal_entry(&tx_c)).await.expect("append C");

        // A has been observed on chain and buried deep enough that the
        // classifier has stopped tracking it, so the rotation predicate — which
        // in production is exactly "is the classifier still tracking this" —
        // says drop.
        let stats = journal.rotate(|h| *h != hash_a).await.expect("rotate");
        assert_eq!(stats.kept, 2, "B + C survive rotation");
        assert_eq!(stats.dropped, 1, "the released A is dropped");
        assert_eq!(stats.bad_lines_skipped, 0, "no corrupt lines");
    }

    // Disk-level check: the rotated file has exactly the two survivors.
    let survivors: Vec<B256> = std::fs::read_to_string(&journal_file)
        .expect("read rotated journal")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<JournalEntry>(l).expect("valid entry").hash)
        .collect();
    assert!(!survivors.contains(&hash_a), "the released A must be gone from disk");
    assert!(survivors.contains(&hash_b) && survivors.contains(&hash_c), "B, C remain on disk");

    // ── Phase 2: launch against the rotated file; only survivors replay ─
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a)
        .whitelist_from(sender_b)
        .whitelist_from(sender_c)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _wallet, _chain_id) = launch_preconf_node!(cfg).await;

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

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();

    assert!(
        !sealed.contains(&hash_a),
        "rotated-out (sealed) commitment must NOT be replayed; sealed={sealed:?}"
    );
    assert!(sealed.contains(&hash_b), "survivor B must be replayed and land; sealed={sealed:?}");
    assert!(sealed.contains(&hash_c), "survivor C must be replayed and land; sealed={sealed:?}");

    let _ = std::fs::remove_dir_all(&journal_dir);
}
