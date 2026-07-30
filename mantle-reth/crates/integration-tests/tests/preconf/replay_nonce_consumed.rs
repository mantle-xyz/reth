//! A Replay-sourced commitment whose nonce is already consumed on the (new)
//! canonical chain must be gracefully skipped — the tx stays on chain exactly
//! once and the node keeps building.
//!
//! Why this needs a hand-built harness: under a single sequencer the new chain
//! is rebuilt by preconf replay itself and replay always goes first, so you
//! can't naturally produce the "new chain already contains the tx, replay hits
//! a consumed nonce" conflict end-to-end. So this is a developer integration
//! test that constructs the state directly: land the tx first, then let a
//! matching journal entry drive a Replay against the now-consumed nonce.
//!
//! A real reorg can't be driven here (reth's `debug_setHead` is a no-op
//! stub), but the observable end of a reorg equals the restart path: the
//! reverted commitment re-enters the fifo from the journal as a `Replay`
//! source (see `restart_replay.rs` / `replay_da.rs`). So we pre-seed a
//! journal entry, let it land as a Replay in block 1 (this stands in for "the
//! commitment is back on the new canonical chain"), canonicalise, and then
//! build again — the stale Replay entry now points at a consumed nonce.
//!
//! Two layers must make that a graceful skip rather than a crash or a
//! double-inclusion (both in `payload_builder.rs`):
//!   - `sync_fifo_forward_to_head` runs at build start, before `replay_fifo_carryover`, and drops
//!     fifo entries whose nonce is below the sender's on-chain nonce — the primary skip;
//!   - the apply loop additionally tolerates `is_nonce_too_low()` without `mark_invalid`, as a
//!     second-line defense if a stale entry still reaches apply.
//!
//! The Replay path was previously untested (`nonce_too_low` skip was only
//! covered on the pool-sweep path). These tests assert the
//! observable contract — hash on chain exactly once, node healthy — rather
//! than which layer did the skipping.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> Bytes {
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

/// Write the entries as a JSON-Lines journal under a unique tempdir; returns
/// `(journal_file, journal_dir)`. Mirrors the helper in `restart_replay.rs` /
/// `replay_da.rs`.
fn write_journal(entries: &[JournalEntry]) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-reorg-nonce-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");
    let mut buf = Vec::new();
    for entry in entries {
        let mut line = serde_json::to_vec(entry).expect("encode JournalEntry");
        line.push(b'\n');
        buf.extend_from_slice(&line);
    }
    std::fs::write(&journal_file, &buf).expect("write journal file");
    (journal_file, journal_dir)
}

/// Drive one payload build to completion against the current forkchoice and
/// return the sealed tx hashes plus the built payload (so the caller can
/// canonicalise it). A successful `resolve_kind` is itself a node-health
/// assertion.
macro_rules! build_block {
    ($node:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let payload = $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build");
        let sealed: Vec<B256> =
            payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
        (payload, sealed)
    }};
}

/// Core scenario: a journal Replay entry lands sender A's tx0 (nonce 0) in
/// block 1 — this stands in for the commitment being back on the new canonical
/// chain. After block 1 is canonicalised (A's on-chain nonce → 1), a second
/// build sees the now-stale Replay entry for A's nonce 0. It must be skipped —
/// not re-applied — while an unrelated sender B's fresh tx lands normally. So
/// A's tx ends up on chain exactly once and the build stays healthy.
///
/// Sender B's fresh tx is what forces block 2 to actually build on top of the
/// canonical block 1 (an empty second build does not reliably advance the
/// head in this harness). It doubles as the "skip must not block other work"
/// check, mirroring `replay_da::replay_over_da_limit_does_not_block_other_replay`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_skip_of_consumed_nonce_does_not_block_other_sender() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // Sender A: the default wallet. Sender B: signers[2] (index 1 collides with
    // RECIPIENT — see `canon_cleanup::canon_does_not_leak_across_senders`).
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_b = signer_b.address();

    // Pre-seed a Replay commitment for A's nonce 0.
    let tx_a = signed_transfer(chain_id, &wallet_a, 0).await;
    let hash_a = keccak256(&tx_a);
    let entry =
        JournalEntry { hash: hash_a, tx_rlp: tx_a.clone(), block_height: 1, committed_at_ms: 0 };
    let (journal_file, journal_dir) = write_journal(&[entry]);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a)
        .whitelist_from(sender_b)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, http, _wallet, launched_chain_id) = launch_preconf_node!(cfg).await;
    assert_eq!(launched_chain_id, chain_id);

    // ── Block 1: the Replay entry lands A's tx0 ("re-landed on the new chain"). ──
    let (payload_1, sealed_1) = build_block!(node);
    assert!(
        sealed_1.contains(&hash_a),
        "block 1 must land the journal-Replay tx (sender A); sealed={sealed_1:?}",
    );

    // Canonicalise block 1 → A's on-chain nonce advances to 1, so the
    // still-present Replay entry for A's nonce 0 is now stale.
    let new_head = node.submit_payload(payload_1).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Block 2: B's fresh tx lands; A's stale Replay entry must be skipped. ──
    let tx_b: Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(1u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(signer_b, request).await.encoded_2718().into()
    };
    let http_c = http.clone();
    let rpc_b = tokio::spawn(async move { send_preconf(&http_c, tx_b).await });

    let (_payload_2, sealed_2) = build_block!(node);
    let event_b = rpc_b.await.expect("rpc join").expect("sender B must succeed");

    assert!(
        matches!(event_b.status, PreconfStatus::Success),
        "sender B's fresh tx must succeed alongside the Replay skip; got {:?} reason={:?}",
        event_b.status,
        event_b.reason,
    );
    assert!(
        sealed_2.contains(&event_b.tx_hash),
        "block 2 must contain sender B's tx; sealed={sealed_2:?}",
    );
    assert!(
        !sealed_2.contains(&hash_a),
        "stale Replay entry (A's consumed nonce) must be skipped, not re-included in block 2; \
         sealed={sealed_2:?}",
    );

    // A's tx is on chain exactly once: (block 1 has it) ∧ (block 2 does not);
    // reaching `resolve_kind` in build_block! proves the node did not crash on
    // the consumed-nonce entry.

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Continuity variant: after the stale Replay entry is skipped, the same
/// sender's next nonce must flow normally. Guards against the skip leaving the
/// fifo / nonce accounting wedged (which would block nonce=1 from landing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_continues_with_next_nonce_after_replay_skip() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let sender = wallet.inner.address();

    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let hash0 = keccak256(&tx0);
    let entry =
        JournalEntry { hash: hash0, tx_rlp: tx0.clone(), block_height: 1, committed_at_ms: 0 };
    let (journal_file, journal_dir) = write_journal(&[entry]);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, http, _wallet, _chain_id) = launch_preconf_node!(cfg).await;

    // Block 1: Replay lands tx0; canonicalise so nonce 0 is consumed.
    let (payload_1, sealed_1) = build_block!(node);
    assert!(sealed_1.contains(&hash0), "block 1 must land the Replay tx0; sealed={sealed_1:?}");
    let new_head = node.submit_payload(payload_1).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Block 2: submit a fresh nonce=1 via RPC. It must succeed and land even
    // though the stale nonce=0 Replay entry is being skipped in the same build.
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, tx1).await });

    let (_payload_2, sealed_2) = build_block!(node);
    let event = rpc_task.await.expect("rpc join").expect("nonce=1 must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "next-nonce preconf must succeed after the Replay skip; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert!(sealed_2.contains(&event.tx_hash), "block 2 must contain nonce=1; sealed={sealed_2:?}");
    assert!(
        !sealed_2.contains(&hash0),
        "block 2 must NOT re-include the already-consumed nonce=0 tx; sealed={sealed_2:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
