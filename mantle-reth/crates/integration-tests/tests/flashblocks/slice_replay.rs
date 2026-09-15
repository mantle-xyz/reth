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

/// A build nobody used gives back what it took from the pool.
///
/// Slicing prunes each slice's transactions the moment it goes out, a good half
/// slot before the block could be canonical. If that build is then thrown away
/// — superseded by a later job for the same height, timed out, dropped — the
/// transactions are in no block and no pool, and nothing else brings them back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_nobody_used_gives_back_what_it_pruned() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Held open long enough for the ticker to execute it and prune it away.
    hold_a_block_open!(node, Duration::from_millis(500));

    // A second job for the same height supersedes the first, which is one of
    // the ways a build ends up used by nobody.
    let payload = build_one_block!(node, Duration::from_millis(500));

    assert!(
        ordinary_transactions(&payload).contains(&hash),
        "a transaction the abandoned build pruned must be back in the pool for the next one",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// And the sender's nonce comes back with them.
///
/// The slice boundary also tells the pool the sender has moved on, which is a
/// lie once the build is thrown away. The pool discards transactions below the
/// nonce it holds — discards, not parks — so leaving it raised would cost the
/// sender everything they send until a real block corrected it.
///
/// Nothing sets it back explicitly: re-admitting the transactions overwrites
/// the sender's record with the nonce the validator reads from the chain. This
/// asserts the outcome rather than the mechanism, so it holds whichever way the
/// nonce ends up being restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_nobody_used_gives_back_the_senders_nonce() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let first = signed_transfer(&wallet, chain_id, 0).await;
    node.rpc.inject_tx(first).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    hold_a_block_open!(node, Duration::from_millis(500));

    // Nonce 1 is what the sender would send next if nonce 0 had landed. It did
    // not — the build that executed it was thrown away — so the pool must still
    // be expecting nonce 0 and must not discard this one for being behind.
    let second = signed_transfer(&wallet, chain_id, 1).await;
    let second_hash = keccak256(&second);
    node.rpc.inject_tx(second).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(500));
    let landed = ordinary_transactions(&payload);

    assert!(
        landed.contains(&second_hash),
        "the sender's next transaction must survive the abandoned build; landed={landed:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A build that was used hands nothing back.
///
/// Its transactions are in a block on its way to the consensus layer, and the
/// senders really have moved on. Handing them back would put a block's worth of
/// transactions into the pool for the next canonical update to take out again,
/// every block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_that_was_used_hands_nothing_back() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let first = build_one_block!(node, Duration::from_millis(500));
    assert!(
        ordinary_transactions(&first).contains(&hash),
        "precondition: the transaction is in the payload the consensus layer asked for",
    );

    // The next build starts from the same parent — this harness never makes the
    // first block canonical — so a transaction handed back would reappear here.
    let second = build_one_block!(node, Duration::from_millis(500));
    assert!(
        !ordinary_transactions(&second).contains(&hash),
        "a used build must not put its transactions back for the next one to take again",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// With slicing off there is nothing to hand back, and the loop must not go
/// looking. Nothing prunes mid-build, so the pool never lost anything, and a
/// hand-back here would be re-admitting transactions the pool still holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_slicing_an_abandoned_build_hands_nothing_back() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, _http, wallet, chain_id) = crate::launch_preconf_node!(cfg).await;

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    hold_a_block_open!(node, Duration::from_millis(300));
    let payload = build_one_block!(node, Duration::from_millis(300));

    // It was never pruned, so it is still in the pool and lands exactly once.
    let landed = ordinary_transactions(&payload);
    assert_eq!(
        landed.iter().filter(|h| **h == hash).count(),
        1,
        "with nothing pruned there is nothing to give back; landed={landed:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A commitment is not handed back to the pool.
///
/// The fifo holds it and replays it on the next build, which is a path of its
/// own; putting it back in the pool as well would leave the two arms each
/// believing they owe the same transaction, and the pool arm skips it anyway
/// for as long as its verdict stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commitment_is_not_handed_back_to_the_pool() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    hold_a_block_open!(node, Duration::from_millis(200));
    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let hash = keccak256(&raw);
    send_preconf(&http, raw).await.expect("the commitment is accepted");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The build above is abandoned by this one, which rebuilds the same height.
    let payload = build_one_block!(node, Duration::from_millis(500));
    let landed = ordinary_transactions(&payload);

    assert_eq!(
        landed.iter().filter(|h| **h == hash).count(),
        1,
        "the commitment lands once, by the fifo's replay and not by the pool's; landed={landed:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// The same hand-back, reached the way the technical design names it: a reorg.
///
/// A reorg arrives as a forkchoice update whose head points elsewhere, and the
/// generator supersedes the job in flight — which is the same `abandon` any
/// other superseding update causes, so this exercises the path
/// `a_build_nobody_used_gives_back_what_it_pruned` already covers. It is kept
/// because the trigger is the production-shaped one: the rebuilt block differs
/// from the abandoned one by its L1 origin, so the reorg is observable rather
/// than assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_a_reorg_threw_away_gives_back_what_it_pruned() {
    use crate::helpers::l1_info_deposit;

    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (node, _http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    // The block the reorg will build on.
    let genesis = node.current_forkchoice_state().expect("forkchoice state").head_block_hash;
    let (base, _) = crate::op_node_slot_l1!(node, on = genesis, n = 0, l1 = 1);

    let raw = signed_transfer(&wallet, chain_id, 0).await;
    let hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A build at height 1 against L1 origin 100, held open long enough for the
    // ticker to execute the transaction and prune it away. Never resolved.
    let _pid = crate::fcu_v3_start!(node, base, crate::helpers::l1_attrs(1, 100));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The reorg: same height, different L1 origin. The build above is superseded.
    let (_head, sealed) = crate::reorg_to!(node, base, n = 1, l1 = 200);

    assert_eq!(
        sealed[0],
        keccak256(l1_info_deposit(200)),
        "precondition: the rebuild must reference the new L1 origin, or no reorg happened",
    );
    assert!(
        sealed.contains(&hash),
        "a transaction the reorged-away build pruned must be back for the rebuild; \
         sealed={sealed:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}
