//! `slice_replay`: what a second process makes of the journal the first left.
//!
//! The two halves of the mechanism are tested apart elsewhere — that a sliced
//! transaction reaches the file (`slice_journal`), and that a hand-written file
//! is replayed correctly (`preconf::restart_replay`). This joins them: one node
//! slices a block and writes what it announced, another starts against the same
//! file and has to land all of it.
//!
//! The first node is left running rather than shut down. A journal only has to
//! survive a process that did not get to clean up, and shutting down cleanly
//! runs a final rotation that drops records the classifier never tracked —
//! which every pool transaction is. Leaving it alive is the closest this
//! harness gets to a process that died.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::{JournalEntry, flashblocks::FlashblockProducerConfig};
use op_alloy_consensus::DEPOSIT_TX_TYPE_ID;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use crate::helpers::{PreconfCfgBuilder, send_preconf};

const RECIPIENT: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Slice fast enough that a held-open block produces several, and let the OS
/// pick the endpoint port so concurrent tests never collide.
fn flashblocks_cfg() -> FlashblockProducerConfig {
    FlashblockProducerConfig {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        block_time: Duration::from_millis(100),
        ..Default::default()
    }
}

/// A journal path under a fresh directory, for two nodes to share.
fn shared_journal_path() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "mantle-flashblocks-slice-replay-{}",
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

/// Open a block and leave it open for `pause`, long enough for the ticker to
/// publish — and journal — several slices. Deliberately never resolved: the
/// block the first node was building is the one the second has to rebuild.
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

/// Build one block and return it.
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

fn ordinary_transactions(payload: &reth_optimism_node::OpBuiltPayload) -> Vec<B256> {
    payload
        .block()
        .body()
        .transactions()
        .map(|tx| Bytes::from(tx.encoded_2718()))
        .filter(|raw| raw.first().is_some_and(|tag| *tag != DEPOSIT_TX_TYPE_ID))
        .map(|raw| keccak256(&raw))
        .collect()
}

/// Everything a slice announced comes back, in the order it executed.
///
/// The first node slices a block it never seals. The second starts against the
/// journal that block left behind and builds the same height — and every
/// transaction subscribers were shown has to be in it, or someone watched a
/// transaction that then vanished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_process_lands_everything_the_slices_announced() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = || {
        PreconfCfgBuilder::new()
            .whitelist_pair(sender, RECIPIENT)
            .journal_path(journal_file.clone())
            .build()
    };

    // ── The process that gets interrupted ──
    let (mut first, _http, wallet, chain_id, _fb_addr) =
        crate::launch_flashblocks_node!(cfg(), flashblocks_cfg()).await;

    let mut sent = Vec::new();
    for nonce in 0..3u64 {
        let raw = signed_transfer(&wallet, chain_id, nonce).await;
        sent.push(keccak256(&raw));
        first.rpc.inject_tx(raw).await.expect("inject transfer");
    }
    // Pool validation is asynchronous; let it settle before the build starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    hold_a_block_open!(first, Duration::from_millis(600));

    let announced: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert!(
        sent.iter().all(|hash| announced.contains(hash)),
        "precondition: the first node must have journaled what it sliced; \
         sent={sent:?} journaled={announced:?}",
    );

    // ── The process that has to make good on it ──
    // `first` is deliberately still alive: a clean shutdown would rotate the
    // file and take these records with it.
    let (mut second, _http2, _wallet2, _chain_id2, _fb2) =
        crate::launch_flashblocks_node!(cfg(), flashblocks_cfg()).await;

    let payload = build_one_block!(second, Duration::from_millis(600));
    let landed = ordinary_transactions(&payload);

    for (nonce, hash) in sent.iter().enumerate() {
        assert!(
            landed.contains(hash),
            "nonce {nonce} was announced by a slice and must land after the restart; \
             landed={landed:?}",
        );
    }
    let positions: Vec<usize> =
        sent.iter().map(|hash| landed.iter().position(|h| h == hash).expect("present")).collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "and in the order they executed; got positions {positions:?}",
    );
    // Says only that the block is in nonce order, which the EVM would enforce
    // whatever order they were dispatched in. Dispatching them out of order
    // costs the later ones instead — the case the assertion above catches, and
    // the reason it is stated as "all of them land" rather than as an ordering.

    drop(first);
    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A preconf commitment and ordinary transactions from one sender, together.
///
/// Both are journaled, by different writers at different moments, and both have
/// to come back. The commitment additionally keeps what it always had: it is
/// replayed as a commitment, not re-judged as a fresh request.
///
/// The file order the two writers leave behind is not forced here — that needs
/// a tick boundary to fall between two transactions of one sender, which is a
/// race to arrange and a flake to keep. `preconf::restart_replay`'s
/// `journal_replay_out_of_nonce_order_still_lands_every_commitment` writes the
/// bad order by hand instead, and covers it without depending on timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commitment_and_ordinary_transactions_both_survive_the_restart() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = || {
        PreconfCfgBuilder::new()
            .whitelist_pair(sender, RECIPIENT)
            .journal_path(journal_file.clone())
            .build()
    };

    let (mut first, http, wallet, chain_id, _fb_addr) =
        crate::launch_flashblocks_node!(cfg(), flashblocks_cfg()).await;

    // Nonce 0 through the pool, so the commitment at nonce 1 clears the
    // gap check the preconf RPC applies.
    let pooled = signed_transfer(&wallet, chain_id, 0).await;
    let pooled_hash = keccak256(&pooled);
    first.rpc.inject_tx(pooled).await.expect("inject transfer");
    tokio::time::sleep(Duration::from_millis(200)).await;

    hold_a_block_open!(first, Duration::from_millis(300));

    let committed = signed_transfer(&wallet, chain_id, 1).await;
    let committed_hash = keccak256(&committed);
    let event = send_preconf(&http, committed).await.expect("the commitment is accepted");
    assert_eq!(event.tx_hash, committed_hash);

    // Let the tick after the commitment carry the pooled transaction to disk.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let journaled: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    for (label, hash) in
        [("the pooled transaction", pooled_hash), ("the commitment", committed_hash)]
    {
        assert!(
            journaled.contains(&hash),
            "precondition: {label} must be journaled before the restart; journaled={journaled:?}",
        );
    }

    let (mut second, _http2, _wallet2, _chain_id2, _fb2) =
        crate::launch_flashblocks_node!(cfg(), flashblocks_cfg()).await;

    let payload = build_one_block!(second, Duration::from_millis(600));
    let landed = ordinary_transactions(&payload);

    assert!(landed.contains(&pooled_hash), "the pooled transaction must land; landed={landed:?}");
    assert!(landed.contains(&committed_hash), "the commitment must land; landed={landed:?}");
    assert!(
        landed.iter().position(|h| *h == pooled_hash) <
            landed.iter().position(|h| *h == committed_hash),
        "and nonce 0 before nonce 1",
    );

    drop(first);
    std::fs::remove_dir_all(&journal_dir).ok();
}
