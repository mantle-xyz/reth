//! Journal **write** path + persistence scope.
//!
//! `restart_replay.rs` covers the *read* half of durability (hand-written
//! journal → restore → land). This module covers the *write* half — the
//! part the RPC handler owns — plus the persistence scope: **every RPC-path
//! commitment that lands on chain is journaled, whether it succeeded or
//! reverted**; an ordinary submission, which promised nobody anything, is excluded.
//!
//! - `rpc_success_appends_to_journal_file` — a live `eth_sendRawTransactionWithPreconf` that
//!   returns `Success` must leave a matching `JournalEntry` on disk. Guards `rpc.rs`'s
//!   `journal.append_promised` call + the on-disk format.
//! - `rpc_revert_but_sealed_is_journaled` — a preconf tx that **reverts** but still lands must ALSO
//!   be journaled. Regression guard: the write used to be gated on wire `Success`, which dropped
//!   reverted-but-sealed commitments.
//! - `an_ordinary_submission_is_not_journaled` — a whitelisted tx submitted through the **plain**
//!   `eth_sendRawTransaction` path lands on chain by the pool arm but must **not** be journaled.
//!   Guards the "only a commitment persists" scope: nobody was promised anything, so there is
//!   nothing to protect.
//!
//! Together these pin the write side end-to-end without needing to
//! actually restart a process (the disk file is the observable boundary).

use super::helpers::{
    PreconfCfgBuilder, mantle_chain_spec_with_reverting_contract, mantle_test_chain_spec,
    send_preconf,
};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Address carrying always-reverting runtime code (see
/// `mantle_chain_spec_with_reverting_contract`). A call here reverts.
const REVERT_CONTRACT: &str = "0x00000000000000000000000000000000000000ff";

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

/// Fresh journal path under a unique tempdir. The file is **not**
/// pre-created — `PreconfJournal::open` creates it on startup, so this
/// exercises the true first-boot append path. Returns `(file, dir)`; the
/// caller removes `dir` at the end.
fn fresh_journal_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-write-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir journal dir");
    (dir.join("preconf.journal"), dir)
}

/// Parse the on-disk JSON-Lines journal into entries. Missing file → empty
/// (the append path may never have fired).
fn read_journal(path: &std::path::Path) -> Vec<JournalEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<JournalEntry>(l).expect("valid JournalEntry line"))
        .collect()
}

/// A successful preconf RPC submission appends exactly one matching
/// `JournalEntry` to the on-disk journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_success_appends_to_journal_file() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();

    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, http, wallet, _chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash = keccak256(&raw_tx);

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

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf must succeed");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} reason={:?}",
        event.status,
        event.reason
    );

    // The RPC handler awaits `append_promised` before returning the
    // Success event, so by now the entry is on disk.
    let entries = read_journal(&journal_file);
    assert_eq!(entries.len(), 1, "exactly one commitment must be journaled");
    let entry = &entries[0];
    assert_eq!(entry.hash, tx_hash, "journaled hash must match the submitted tx");
    assert_eq!(entry.block_height, event.block_height, "journaled height matches receipt");
    assert!(!entry.tx_rlp.is_empty(), "journaled entry carries the raw tx bytes");
    assert_eq!(
        keccak256(&entry.tx_rlp),
        tx_hash,
        "journaled tx_rlp must re-hash to the committed tx hash"
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// A valid preconf tx whose execution **reverts** (call to the always-reverting
/// contract). Still lands with an EIP-658 `status = 0` receipt.
async fn signed_reverting_call(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(REVERT_CONTRACT.parse().unwrap())),
        gas: Some(100_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::ZERO),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// A preconf tx that **reverts** but still lands MUST be journaled. The write
/// used to be gated on wire `Success`, so a reverted-but-sealed commitment was
/// dropped — lost on restart (the tx is
/// `TransactionOrigin::External`, so not in reth's local-tx backup either),
/// even though the client already holds a receipt. The fix journals on *any*
/// receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_revert_but_sealed_is_journaled() {
    let contract: Address = REVERT_CONTRACT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();

    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(contract)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, http, wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_chain_spec_with_reverting_contract(chain_id, contract))
            .await;

    let raw_tx = signed_reverting_call(chain_id, &wallet, 0).await;
    let tx_hash = keccak256(&raw_tx);

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

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf call must return a receipt");

    // Reverted ⇒ Failed, but a receipt exists (it landed) — the case the old gate dropped.
    assert!(
        matches!(event.status, PreconfStatus::Failed),
        "expected revert ⇒ Failed, got {:?} reason={:?}",
        event.status,
        event.reason
    );

    // Sanity: the reverted tx actually landed (so "must journal" isn't a no-op).
    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&tx_hash),
        "reverted preconf tx must still land on chain; sealed={sealed:?}"
    );

    // The reverted-but-sealed commitment MUST be journaled.
    let entries = read_journal(&journal_file);
    assert_eq!(entries.len(), 1, "the reverted-but-sealed commitment must be journaled");
    let entry = &entries[0];
    assert_eq!(entry.hash, tx_hash, "journaled hash must match the submitted tx");
    assert_eq!(entry.block_height, event.block_height, "journaled height matches receipt");
    assert_eq!(
        keccak256(&entry.tx_rlp),
        tx_hash,
        "journaled tx_rlp must re-hash to the committed tx hash"
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// A whitelisted tx submitted through the **plain** `eth_sendRawTransaction`
/// path lands on chain by the pool arm but must NOT be journaled — only a
/// preconf commitment that reached `Success` persists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ordinary_submission_is_not_journaled() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();

    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, wallet, _chain_id) = launch_preconf_node!(cfg).await;

    // Plain sendRawTransaction — this never reaches admission, which is the
    // only journal writer. The whitelisted tx still lands, by the pool arm.
    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash: B256 =
        node.rpc.inject_tx(raw_tx.clone()).await.expect("plain sendRawTransaction accepted");

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

    // Sanity: the tx did land (so "not journaled" is a scope decision, not
    // a "tx dropped" artifact).
    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&tx_hash),
        "the whitelisted tx must land on chain by the pool arm; sealed={sealed:?}"
    );

    // Persistence scope: an ordinary submission must NOT be journaled.
    let entries = read_journal(&journal_file);
    assert!(
        !entries.iter().any(|e| e.hash == tx_hash),
        "listener-path tx must NOT be journaled (only RPC + Success is); entries={entries:?}"
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
