//! The journal keeps execution order, and a commitment never reaches it ahead
//! of an ordinary transaction it executed after.
//!
//! A commitment can depend on a pool transaction executed just before it — a
//! transfer that funds it, or here simply the previous nonce. A restart replays
//! the file in order, so the pool transaction has to be in the file, and ahead
//! of the commitment, by the time the commitment's receipt goes out.
//!
//! Both tests run with flashblocks off: no slice boundary ever writes the pool
//! transactions, so whatever is written comes from the commitment's own write
//! or from the start of the next build.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Left out of every allowlist here, so a transfer to it is an ordinary pool
/// transaction even from an allowlisted sender.
const OTHER: Address = address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

fn shared_journal_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-order-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir journal dir");
    (dir.join("preconf.journal"), dir)
}

fn read_journal(path: &std::path::Path) -> Vec<JournalEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    raw.lines().filter_map(|line| serde_json::from_str::<JournalEntry>(line).ok()).collect()
}

async fn signed_transfer(wallet: &Wallet, chain_id: u64, nonce: u64, to: Address) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Open a block and leave it open for `pause`. Never resolved: the block is
/// the one a restart has to rebuild.
macro_rules! hold_a_block_open {
    ($node:expr, $pause:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed");
        tokio::time::sleep($pause).await;
    }};
}

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

fn landed(payload: &reth_optimism_node::OpBuiltPayload) -> Vec<B256> {
    payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect()
}

/// A commitment whose nonce follows a pool transaction lands after a restart.
///
/// The pool transaction executes first and the commitment builds on it. Written
/// without it, or behind it, the commitment replays against a nonce the chain
/// has not reached and is reported broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commitment_survives_a_restart_with_the_pool_transaction_before_it() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = || {
        PreconfCfgBuilder::new()
            .whitelist_pair(sender, RECIPIENT)
            .journal_path(journal_file.clone())
            .build()
    };

    let (mut first, http, wallet, chain_id) = launch_preconf_node!(cfg()).await;

    let pooled = signed_transfer(&wallet, chain_id, 0, OTHER).await;
    let pooled_hash = keccak256(&pooled);
    first.rpc.inject_tx(pooled).await.expect("inject pool transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    hold_a_block_open!(first, Duration::from_millis(300));

    let committed = signed_transfer(&wallet, chain_id, 1, RECIPIENT).await;
    let committed_hash = keccak256(&committed);
    let event = send_preconf(&http, committed).await.expect("the commitment is accepted");
    assert_eq!(event.tx_hash, committed_hash);

    let order: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert_eq!(
        order,
        vec![pooled_hash, committed_hash],
        "the receipt is out, so both must be in the file, in execution order",
    );

    // `first` stays alive: a clean shutdown would rotate the file.
    let (mut second, _http2, _wallet2, _chain_id2) = launch_preconf_node!(cfg()).await;
    let landed = landed(&build_one_block!(second, Duration::from_millis(600)));

    assert!(landed.contains(&committed_hash), "the commitment must land; landed={landed:?}");
    assert!(landed.contains(&pooled_hash), "and the transaction it follows; landed={landed:?}");

    drop(first);
    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A build writes what the previous one executed before replaying any of it.
///
/// No commitment is involved, so nothing else would write the pool
/// transaction: it would sit in memory until one arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_build_writes_what_the_last_one_executed() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let pooled = signed_transfer(&wallet, chain_id, 0, OTHER).await;
    let pooled_hash = keccak256(&pooled);
    node.rpc.inject_tx(pooled).await.expect("inject pool transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Executes the transfer, then is superseded by the next job.
    hold_a_block_open!(node, Duration::from_millis(300));
    assert!(
        read_journal(&journal_file).is_empty(),
        "precondition: with no commitment and no slices, nothing is written yet",
    );

    hold_a_block_open!(node, Duration::from_millis(300));

    let written: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert_eq!(written, vec![pooled_hash], "the next build writes it before replaying it");

    std::fs::remove_dir_all(&journal_dir).ok();
}
