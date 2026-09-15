//! `slice_journal`: what a sliced block leaves on disk for the next process.
//!
//! Once a transaction has gone out in a slice, subscribers show it as pending.
//! A sequencer that crashes and rebuilds the same height without it would make
//! a transaction users had already seen disappear, so what a slice carries is
//! written to the commitment journal before it is broadcast.
//!
//! Only pool transactions are. Deposits arrive with the payload attributes on
//! every rebuild, and a preconf commitment is journaled when its receipt goes
//! out — writing either here would put something back that does not belong.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::{JournalEntry, flashblocks::FlashblockProducerConfig};
use op_alloy_consensus::DEPOSIT_TX_TYPE_ID;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use crate::helpers::{PreconfCfgBuilder, send_preconf};

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Slice fast enough that a block produces several inside a test's patience,
/// and let the OS pick the port so concurrent tests never collide.
fn flashblocks_cfg() -> FlashblockProducerConfig {
    FlashblockProducerConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        block_time: Duration::from_millis(100),
        ..Default::default()
    }
}

/// A journal path under a fresh directory. Deliberately not pre-created: the
/// journal creates it at startup, so this exercises the first-boot path.
fn fresh_journal_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mantle-flashblocks-slice-journal-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir journal dir");
    (dir.join("preconf.journal"), dir)
}

fn read_journal(path: &std::path::Path) -> Vec<JournalEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<JournalEntry>(line).expect("valid JournalEntry line"))
        .collect()
}

async fn signed_transfer(wallet: &Wallet, chain_id: u64, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Drive one block: hand-rolled FCU, a pause long enough for the build loop to
/// tick, then resolve. `advance_block()` waits on an event pair the preconf
/// payload builder does not drive.
macro_rules! build_one_block {
    ($node:expr, $pause:expr) => {{
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

        tokio::time::sleep($pause).await;

        $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build")
    }};
}

/// The transaction a user sent is on disk; the deposits that open the block are
/// not.
///
/// Both halves matter. Without the first, a restart loses a transaction
/// subscribers were shown. With a deposit in the file, a restart would try to
/// put a system transaction back into the pool as if a user had sent it.
///
/// Asserting it of *every* ordinary transaction in the sealed block also covers
/// the slices the cancel guard dropped on the way: those were journaled before
/// the guard ran, and their transactions are carried by a later slice, so a
/// record going missing with a dropped slice would show up here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pool_transaction_is_journaled_and_the_blocks_deposits_are_not() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id, _fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    // Pool validation is asynchronous; give it a moment before the build starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(600));

    let journaled: Vec<_> = read_journal(&journal_file).iter().map(|entry| entry.hash).collect();
    let sealed: Vec<Bytes> =
        payload.block().body().transactions().map(|tx| Bytes::from(tx.encoded_2718())).collect();
    let (deposits, ordinary): (Vec<&Bytes>, Vec<&Bytes>) =
        sealed.iter().partition(|raw| raw.first().is_some_and(|tag| *tag == DEPOSIT_TX_TYPE_ID));

    assert!(!ordinary.is_empty(), "the block must contain the transaction that was injected");
    for raw in &ordinary {
        assert!(
            journaled.contains(&keccak256(raw)),
            "a sliced pool transaction must be on disk before its slice goes out",
        );
    }
    for raw in &deposits {
        assert!(
            !journaled.contains(&keccak256(raw)),
            "a deposit arrives with the attributes on every rebuild; journaling it would put a \
             system transaction back into the pool",
        );
    }

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A preconf commitment gets exactly one record, from its own apply.
///
/// Its journal entry is written by dispatch when the transaction lands. The
/// slice that goes on to carry it must not write a second one: the same hash
/// twice would be restored twice, and the count of what a restart owes would
/// stop matching the transactions it owes them for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_commitment_is_journaled_once_not_once_per_slice() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, http, wallet, chain_id, _fb_addr) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    // Hold a block open so the preconf lands mid-build and several slices go
    // out after it — each of them a chance to write the record again.
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

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let event = send_preconf(&http, raw).await.expect("the commitment is accepted");
    let hash = event.tx_hash;

    // Let the ticker fire several more times before sealing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let written = read_journal(&journal_file);
    let for_this_tx = written.iter().filter(|entry| entry.hash == hash).count();
    assert_eq!(
        for_this_tx, 1,
        "the commitment is recorded at its apply and nowhere else; got {for_this_tx} records \
         out of {written:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// With slicing off, nothing has been announced, so there is nothing to make
/// good on — and the journal keeps holding preconf commitments alone.
///
/// This is what makes turning flashblocks off a rollback rather than a second
/// code path with its own disk format.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_block_built_without_slicing_journals_nothing() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id) = crate::launch_preconf_node!(cfg).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(raw).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(600));
    assert!(
        payload
            .block()
            .body()
            .transactions()
            .any(|tx| tx.encoded_2718().first().is_some_and(|tag| *tag != DEPOSIT_TX_TYPE_ID)),
        "the block must contain the transaction, or this proves nothing",
    );

    assert!(
        read_journal(&journal_file).is_empty(),
        "a pool transaction nobody was told about does not belong in the journal",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}
/// The closing slice is journaled like any other.
///
/// It carries the block's tail, which would otherwise be the one part of a
/// block announced but not persisted — the window a crash turns into a
/// transaction a subscriber was shown and the chain never got.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_closing_slice_is_journaled_before_it_goes_out() {
    let (journal_file, journal_dir) = fresh_journal_path();
    let cfg = PreconfCfgBuilder::new().journal_path(journal_file.clone()).build();
    // Wide enough to have a tail — see `producer_e2e` for why a second.
    let fb = FlashblockProducerConfig { block_time: Duration::from_secs(1), ..flashblocks_cfg() };
    let (mut node, _http, wallet, chain_id, _fb_addr) =
        crate::launch_flashblocks_node!(cfg, fb).await;

    // Opened first, so these arrive mid-build and land in the tail.
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

    for nonce in 0..5u64 {
        let raw = signed_transfer(&wallet, chain_id, nonce).await;
        node.rpc.inject_tx(raw).await.expect("inject");
    }
    tokio::time::sleep(Duration::from_millis(1_300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let journaled: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    let ordinary: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| Bytes::from(tx.encoded_2718()))
        .filter(|raw| raw.first().is_some_and(|tag| *tag != DEPOSIT_TX_TYPE_ID))
        .map(|raw| keccak256(&raw))
        .collect();

    assert_eq!(ordinary.len(), 5, "the block was supposed to carry all five");
    for hash in &ordinary {
        assert!(
            journaled.contains(hash),
            "{hash:?} was announced in the closing slice but is not on disk; journaled {journaled:?}",
        );
    }

    drop(node);
    let _ = std::fs::remove_dir_all(journal_dir);
}
