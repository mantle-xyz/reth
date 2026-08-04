//! Size-triggered journal rotation, end-to-end through a running node.
//!
//! `journal_rotation.rs` drives `rotate()` directly at the disk layer;
//! this module verifies the **wiring**: the launched node actually spawns
//! `run_rejournal_loop` with the configured `journal_max_size`, and a
//! commitment that crosses the cap *after* being sealed on chain is
//! dropped from the on-disk file by the size trigger — with no periodic
//! tick (the default 60s interval never fires in the sub-second window)
//! and no manual `rotate()` call.
//!
//! Coverage:
//! - `size_triggered_rotation_drops_sealed_entry` — the cap is sized between one and two journal
//!   entries. tx0 lands + is canon-sealed while the file is still under the cap (nothing rotates
//!   it). tx1's append then pushes the file over the cap, arming the size trigger for the *first*
//!   time (so the rate limit's `min_gap` is irrelevant — the first trigger is always honoured). The
//!   loop rotates, dropping the sealed tx0 and keeping the still-unsealed tx1.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use mantle_reth_rpc_ext::PreconfStatus;
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

/// Fresh journal path under a unique tempdir; the file is created by
/// `PreconfJournal::open` on startup. Returns `(file, dir)`.
fn fresh_journal_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-size-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir journal dir");
    (dir.join("preconf.journal"), dir)
}

/// Parse the on-disk JSON-Lines journal into entries. Missing file → empty.
fn read_journal(path: &std::path::Path) -> Vec<JournalEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<JournalEntry>(l).expect("valid JournalEntry line"))
        .collect()
}

/// End-to-end size-triggered rotation: a sealed commitment is dropped from
/// the on-disk journal by the node's own rejournal loop once a later append
/// pushes the file past the configured `journal_max_size`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_triggered_rotation_drops_sealed_entry() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    // Size the cap between one and two journal entries so the trigger arms
    // only on the *second* append. `line_len` is computed from a
    // representative entry that matches exactly what the RPC handler writes
    // for tx0 (same hash / tx bytes / block_height=1, and a 13-digit
    // `committed_at_ms` matching real wall-clock ms width). The ~0.5-line
    // margin dwarfs any few-byte variance in the second entry.
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let sample = JournalEntry {
        hash: keccak256(&tx0),
        tx_rlp: tx0.clone(),
        block_height: 1,
        committed_at_ms: 1_700_000_000_000,
    };
    let line_len = serde_json::to_vec(&sample).expect("encode sample").len() as u64 + 1;
    let cap = line_len + line_len / 2;

    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .journal_max_size(cap)
        .build();

    let (mut node, http, _wallet, _chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: land tx0 (nonce=0) and commit it to canonical so the canon
    //    handler marks it sealed. One entry on disk, still under the cap. ─
    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU 1")
        .payload_id
        .expect("payload_id 1");

    let http_c = http.clone();
    let tx0_c = tx0.clone();
    let rpc0 = tokio::spawn(async move { send_preconf(&http_c, tx0_c).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 1")
        .expect("payload 1");
    let event0 = rpc0.await.expect("rpc0 join").expect("tx0 must succeed");
    assert!(
        matches!(event0.status, PreconfStatus::Success),
        "tx0 must succeed; got {:?} reason={:?}",
        event0.status,
        event0.reason,
    );
    let hash0 = event0.tx_hash;

    // Commit to canonical → the canon handler marks tx0 sealed.
    let new_head = node.submit_payload(payload).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Under the cap: nothing has rotated tx0 away yet.
    let before = read_journal(&journal_file);
    assert!(
        before.iter().any(|e| e.hash == hash0),
        "tx0 must still be journaled before the cap is crossed; entries={before:?}",
    );

    // ── Slot 2: submit tx1 (nonce=1). Its append pushes the file over the
    //    cap → the size trigger fires (first trigger, no rate-limit gap)
    //    and the loop rotates, dropping the sealed tx0 and keeping the
    //    still-unsealed tx1. ────────────────────────────────────────────
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let hash1 = keccak256(&tx1);

    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU 2")
        .payload_id
        .expect("payload_id 2");

    let http_c = http.clone();
    let tx1_c = tx1.clone();
    let rpc1 = tokio::spawn(async move { send_preconf(&http_c, tx1_c).await });
    // Give the RPC handler time to append and the loop time to observe the
    // size notify + complete the rotate before we read the file back.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The block-2 payload itself is not asserted; resolving it just lets
    // tx1's preconf RPC return Success.
    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");
    let event1 = rpc1.await.expect("rpc1 join").expect("tx1 must succeed");
    assert!(
        matches!(event1.status, PreconfStatus::Success),
        "tx1 must succeed; got {:?} reason={:?}",
        event1.status,
        event1.reason,
    );

    // The size-triggered rotation must have dropped the sealed tx0 and kept
    // the unsealed tx1. tx1 is never committed to canonical here, so it is
    // not sealed and must survive.
    let after = read_journal(&journal_file);
    assert!(
        !after.iter().any(|e| e.hash == hash0),
        "size-triggered rotation must drop the sealed tx0; entries={after:?}",
    );
    assert!(
        after.iter().any(|e| e.hash == hash1),
        "unsealed tx1 must survive the size-triggered rotation; entries={after:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
