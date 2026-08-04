//! Journal-replay behaviour on startup.
//!
//! When a preconf commitment has been persisted to the on-disk journal but
//! its block never made it to canonical (e.g. because the previous process
//! crashed), the next process must honour the promise: it opens the
//! journal, re-injects the tx into the pool, pushes a fifo entry with
//! `PreconfSource::Replay` and lands the tx in the first block it builds.
//!
//! Coverage:
//!
//! - `journal_replay_lands_promised_tx_in_next_block` — base case: single entry, restore → apply →
//!   land.
//! - `journal_replay_multiple_entries_all_land_in_first_block` — 3 entries from same sender (nonces
//!   0/1/2) all land in block 1. Guards `restore_preconf_state`'s per-entry loop + nonce-order
//!   preservation in `replay_fifo_carryover`.
//! - `journal_replay_across_multiple_senders` — 2 senders, each 1 entry, both land in block 1.
//!   Guards per-entry independence in restore.
//! - `empty_journal_file_starts_normally` — journal file exists but is empty. Startup must not
//!   panic; a subsequent fresh RPC tx flows normally.
//!
//! Related Replay semantics (covered elsewhere):
//! - `gas_budgets::replay_source_bypasses_block_gas_budget` — F1 bypass
//! - `no_tx_pool::no_tx_pool_gates_replay_source_entry` — derivation builds still gate Replay to
//!   preserve chain safety
//!
//! Each test constructs the journal file by hand (JSON Lines, one
//! `JournalEntry` per line) and launches the node against it — this is
//! the observable end of the restart path without needing to actually
//! restart a process.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use reth_chainspec::EthChainSpec;
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
async fn journal_replay_lands_promised_tx_in_next_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    // The launch macro derives its wallet + chain id from the same chain
    // spec used here, so pre-signing against it produces bytes that match
    // whatever the launched node will see.
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash = keccak256(&raw_tx);

    // Pre-seed a JSON-Lines journal file with a single promised commitment.
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");

    let entry =
        JournalEntry { hash: tx_hash, tx_rlp: raw_tx.clone(), block_height: 1, committed_at_ms: 0 };
    let mut line = serde_json::to_vec(&entry).expect("encode JournalEntry");
    line.push(b'\n');
    std::fs::write(&journal_file, &line).expect("write journal file");

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _node_wallet, _launched_chain_id) = launch_preconf_node!(cfg).await;

    // Drive one payload build. Journal restore pushed the entry into the
    // fifo with `Replay` source during startup; the dispatch loop must
    // apply it in this block.
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

    let block = payload.block();
    let sealed: Vec<alloy_primitives::B256> =
        block.body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&tx_hash),
        "journal-restored tx must land in the first block after startup; \
         hash {tx_hash:?} not in sealed {sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Helper: create a fresh JSON-Lines journal file under `tempdir` with
/// the given entries and return the file path + tempdir handle.
fn write_journal(entries: &[JournalEntry]) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-{}",
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

/// Multi-entry journal replay: three consecutive nonces from the same
/// sender must all land in the first block after startup.
///
/// Guards `restore_preconf_state`'s per-entry loop (each entry
/// independently pushed with `PreconfSource::Replay`) and
/// `replay_fifo_carryover`'s FIFO-insertion-order preservation — a
/// bug that shuffled entry order would surface here because EVM nonce
/// enforcement would reject out-of-order applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_replay_multiple_entries_all_land_in_first_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    // Three consecutive nonces from the same sender.
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let tx2 = signed_transfer(chain_id, &wallet, 2).await;
    let hash0 = keccak256(&tx0);
    let hash1 = keccak256(&tx1);
    let hash2 = keccak256(&tx2);

    let entries = [
        JournalEntry { hash: hash0, tx_rlp: tx0.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash1, tx_rlp: tx1.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash2, tx_rlp: tx2.clone(), block_height: 1, committed_at_ms: 0 },
    ];
    let (journal_file, journal_dir) = write_journal(&entries);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _wallet_launched, _launched_chain_id) = launch_preconf_node!(cfg).await;

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

    let sealed: Vec<alloy_primitives::B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();

    for (label, h) in [("nonce=0", hash0), ("nonce=1", hash1), ("nonce=2", hash2)] {
        assert!(
            sealed.contains(&h),
            "journal-restored {label} must land in block 1; sealed={sealed:?}",
        );
    }

    // Order sanity: EVM enforces nonce order, so idx(nonce=0) <
    // idx(nonce=1) < idx(nonce=2). This is a nonce-order check, not
    // strictly a fifo-order check — but validates that entries were
    // pushed to the fifo in a way that let all three apply without
    // any nonce-mismatch rejection.
    let idx0 = sealed.iter().position(|h| *h == hash0).unwrap();
    let idx1 = sealed.iter().position(|h| *h == hash1).unwrap();
    let idx2 = sealed.iter().position(|h| *h == hash2).unwrap();
    assert!(
        idx0 < idx1 && idx1 < idx2,
        "sealed tx order must respect EVM nonce order (idx0<idx1<idx2); \
         got {idx0}/{idx1}/{idx2}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Multi-sender journal replay: two independent senders, each with a
/// single promised commitment, both land in the first block.
///
/// Guards that `restore_preconf_state` iterates entries with no
/// per-sender coupling — a bug that keyed restore state by (sender,
/// nonce) globally instead of per-entry would drop the second
/// sender's entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_replay_across_multiple_senders() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // Sender A: Hardhat[0] (the default `Wallet::default()`).
    // Sender B: signers[2] (index 1 collides with RECIPIENT — see
    //           `happy_path::multi_sender_land_in_one_block`).
    let signers = Wallet::new(3).with_chain_id(chain_id).wallet_gen();
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a_addr = wallet_a.inner.address();
    let signer_b = signers[2].clone();
    let sender_b_addr = signer_b.address();

    let tx_a = signed_transfer(chain_id, &wallet_a, 0).await;
    let hash_a = keccak256(&tx_a);

    let tx_b: alloy_primitives::Bytes = {
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
    let hash_b = keccak256(&tx_b);

    let entries = [
        JournalEntry { hash: hash_a, tx_rlp: tx_a.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash_b, tx_rlp: tx_b.clone(), block_height: 1, committed_at_ms: 0 },
    ];
    let (journal_file, journal_dir) = write_journal(&entries);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a_addr)
        .whitelist_from(sender_b_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _wallet_launched, _launched_chain_id) = launch_preconf_node!(cfg).await;

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

    let sealed: Vec<alloy_primitives::B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&hash_a),
        "sender A's journal-restored tx must land in block 1; sealed={sealed:?}",
    );
    assert!(
        sealed.contains(&hash_b),
        "sender B's journal-restored tx must land in block 1; sealed={sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// An existing but empty journal file must not crash startup, and a
/// subsequent fresh RPC preconf tx must flow normally.
///
/// Guards `PreconfJournal::load()` and `restore_preconf_state`
/// against a regression that would either panic on the empty-file
/// edge case or leave the fifo / pool in a broken state that blocks
/// subsequent RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_journal_file_starts_normally() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    // Empty journal file — no entries, just a zero-byte file at the
    // configured path.
    let (journal_file, journal_dir) = write_journal(&[]);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, http, wallet, launched_chain_id) = launch_preconf_node!(cfg).await;
    assert_eq!(launched_chain_id, chain_id);

    // Sanity-check: startup completed and the RPC still accepts a
    // fresh preconf tx. If restore-of-empty-file corrupted anything
    // (e.g. left `pending_responders` in a weird state), this would
    // fail either at the RPC layer or during dispatch.
    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let expected_hash = keccak256(&raw_tx);

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

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { super::helpers::send_preconf(&http_c, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("fresh preconf must succeed");
    assert!(
        matches!(event.status, mantle_reth_rpc_ext::PreconfStatus::Success),
        "fresh RPC preconf after empty-journal startup must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let sealed: Vec<alloy_primitives::B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&expected_hash),
        "fresh preconf tx must land in block 1 after empty-journal startup; sealed={sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
