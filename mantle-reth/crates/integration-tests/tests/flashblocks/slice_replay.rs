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

/// A second payee, deliberately left out of every allowlist these tests set up.
/// A transfer to it from an allowlisted sender is an ordinary pool transaction,
/// which is what lets one sender hold both kinds at once — the allowlist is
/// keyed on the `(from, to)` pair, not on the sender alone.
const OTHER: Address = address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

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
    signed_transfer_to(wallet, chain_id, nonce, RECIPIENT).await
}

async fn signed_transfer_to(wallet: &Wallet, chain_id: u64, nonce: u64, to: Address) -> Bytes {
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

/// A payload the consensus layer took and then did not use hands back too.
///
/// `getPayload` is not proof the block lands. The consensus layer can answer
/// `SYNCING` or `INVALID`, fail over to another sequencer, or reorg its unsafe
/// head — and in every one of those the transactions this build pruned are in
/// no block and no pool, with the senders' pool nonces left a step ahead of the
/// chain. The pool *discards* what sits below the nonce it holds rather than
/// parking it, so leaving it there costs those senders everything they send
/// until a real block corrects it.
///
/// What says the payload went nowhere is the next job's parent: if the
/// consensus layer had built on what it was given, that parent would be this
/// block. Here it is not, which is exactly what this harness reproduces by
/// never making the first block canonical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payload_the_consensus_layer_did_not_use_hands_back() {
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
        "precondition: the transaction is in the payload the consensus layer asked for, \
         so the build pruned it from the pool",
    );

    // Deliberately not canonized: the next job starts from the same parent, so
    // the block above went nowhere.
    let second = build_one_block!(node, Duration::from_millis(500));
    assert!(
        ordinary_transactions(&second).contains(&hash),
        "a payload that went nowhere must give its transactions back; \
         otherwise they are in no block and no pool",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A build the consensus layer did build on hands nothing back.
///
/// The healthy path: the block is canonical, the senders really have moved on,
/// and the canonical update is what maintains the pool from here.
///
/// What this can and cannot show: once the block is canonical a hand-back would
/// be *wasteful* rather than wrong — `add_external_transactions` re-validates,
/// and a transaction already mined is refused on its nonce — so this cannot
/// distinguish "did not hand back" from "handed back and was refused". It pins
/// the outcome that matters, that the healthy path still lands each transaction
/// exactly once. The decision itself is pinned where it is observable, by
/// `payload_job_generator::went_nowhere_tests`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_the_consensus_layer_used_hands_nothing_back() {
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

    // The consensus layer builds on it, so the next job's parent is this block
    // and nothing is owed back.
    crate::canonize_built!(node, first.clone());

    let second = build_one_block!(node, Duration::from_millis(500));
    assert!(
        !ordinary_transactions(&second).contains(&hash),
        "a transaction that landed must not be mined twice",
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

/// A superseded build's announcement is settled exactly once.
///
/// Two builds at one height: the first announces and prunes a transaction, then
/// is superseded; the second seals the block that lands and goes canonical.
///
/// **What this test actually exercises today is the landed branch**: the second
/// build has room and takes the transaction, so what is pinned is that the next
/// build does not hand it out again. Without that, a transaction that reached
/// the canonical block would be replayed into the one after it and mined twice.
///
/// **It does not cover the whole-group-clear hole.** The bug that shape guards
/// against — `drop_landed_groups` clearing the entire index instead of only the
/// group whose sealed hash matches `parent_hash` — is only observable when the
/// second build cannot carry the transaction, and nothing in this fixture fills
/// the block, so that branch never runs. The `landed_in_second == false` arm is
/// written out because the invariant is stated for both, not because this
/// harness reaches it. **Anyone looking for coverage of the grouping hole has
/// not found it here**: its guard is the unit test
/// `a_group_that_did_not_seal_the_parent_survives_and_needs_checking` in
/// `mantle-reth/crates/preconf/src/unlanded.rs`.
///
/// Making the branch reachable means packing the first build's block to near
/// capacity so the second has no room. That is a deliberate follow-up, deferred
/// as an expensive and timing-sensitive fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_builds_announcement_outlives_the_block_that_replaced_it() {
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

    // First build announces and prunes it, then is superseded.
    hold_a_block_open!(node, Duration::from_millis(500));

    let announced: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert!(
        announced.contains(&hash),
        "precondition: the superseded build must have announced it, else it was never \
         pruned and there is nothing to recover; journaled={announced:?}",
    );

    // The build that replaces it seals a block and that block becomes canonical.
    let second = build_one_block!(node, Duration::from_millis(500));
    crate::canonize_built!(node, second.clone());

    // Whether the second build happened to carry it does not matter; what must
    // not happen is the transaction being neither in a block nor recoverable —
    // nor, on the other side, being handed out again after it has landed.
    let landed_in_second = ordinary_transactions(&second).contains(&hash);
    let third = build_one_block!(node, Duration::from_millis(500));
    let in_third = ordinary_transactions(&third).contains(&hash);
    if landed_in_second {
        assert!(
            !in_third,
            "the canonical block already carries it; replaying it again would mine it twice",
        );
    } else {
        assert!(
            in_third,
            "a superseded build's announcement must still be recoverable after \
             the replacing block goes canonical; landed_in_second=false",
        );
    }

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// A derivation build replays nothing.
///
/// `no_tx_pool` means "execute exactly what the attributes name". A block
/// derived with an extra transaction folded in would not match the one the
/// sequencer built, which is the whole point of deriving it — the safe head
/// would fork.
///
/// The build that follows is what keeps this from passing for the wrong reason:
/// an empty derivation block proves nothing on its own if the index were empty,
/// or if replay had been switched off everywhere. Asserting that the very next
/// ordinary build does carry the transaction says the index was loaded and
/// willing, and that the derivation build declined on purpose.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_derivation_build_replays_no_unlanded_transaction() {
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

    // Leave an announced, unlanded transaction behind.
    hold_a_block_open!(node, Duration::from_millis(500));

    let announced: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert!(
        announced.contains(&hash),
        "precondition: there must be something in the index for the derivation build to \
         refuse; journaled={announced:?}",
    );

    // A derivation slot: attributes with `no_tx_pool` set.
    let mut attrs = node.payload.next_attributes();
    attrs.0.no_tx_pool = Some(true);
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    let derived = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    assert!(
        !ordinary_transactions(&derived).contains(&hash),
        "a derivation build must carry only what its attributes name",
    );

    // …and the refusal is a decision about this build, not the end of the debt.
    let ordinary = build_one_block!(node, Duration::from_millis(500));
    assert!(
        ordinary_transactions(&ordinary).contains(&hash),
        "the next ordinary build must still replay it, or the assertion above only \
         proved the index was empty",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}

/// One sender's nonces are ordered across both sources of the preamble.
///
/// The allowlist is keyed on a `(from, to)` pair, so a single sender can hold a
/// preconf commitment and an ordinary pool transaction at consecutive nonces —
/// the commitment reaches the next build through the fifo's carryover, the pool
/// transaction through the unlanded index. Dispatching one source fully before
/// the other offers the commitment's higher nonce first, the EVM refuses it for
/// the gap, and for a carried-over entry that refusal is terminal: the
/// commitment is broken with no second attempt, and no later arm picks it up
/// because a preconf-eligible transaction is barred from the pool arm.
///
/// Nothing else catches that. `order_preamble`'s own unit tests call it
/// directly, so they stay green when the call site stops using it; this is the
/// only place the two sources are populated by the node itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_senders_nonces_are_ordered_across_both_preamble_sources() {
    let sender = Wallet::default().inner.address();
    let (journal_file, journal_dir) = shared_journal_path();
    // Only `sender → RECIPIENT` is preconf-eligible. `sender → OTHER` is not,
    // which is what puts the two transactions on two different paths.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_pair(sender, RECIPIENT)
        .journal_path(journal_file.clone())
        .build();
    let (mut node, http, wallet, chain_id, _fb) =
        crate::launch_flashblocks_node!(cfg, flashblocks_cfg()).await;

    // Nonce 0 to the unallowlisted payee: an ordinary pool transaction.
    let pooled = signed_transfer_to(&wallet, chain_id, 0, OTHER).await;
    let pooled_hash = keccak256(&pooled);
    node.rpc.inject_tx(pooled).await.expect("inject");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Held open long enough for the ticker to execute nonce 0, announce it and
    // prune it — which is what puts it in the unlanded index. This build is
    // never sealed, so the index still owes it.
    hold_a_block_open!(node, Duration::from_millis(400));

    // Nonce 1 to the allowlisted payee, committed against the in-flight build.
    // It is applied there and so ends up `Success` in the fifo; the build then
    // goes nowhere, the chain nonce never moves, and the entry survives
    // `sync_fifo_forward_to_head` into the next build's carryover.
    let committed = signed_transfer_to(&wallet, chain_id, 1, RECIPIENT).await;
    let committed_hash = keccak256(&committed);
    let event = send_preconf(&http, committed).await.expect("the commitment is accepted");
    assert_eq!(event.tx_hash, committed_hash);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let announced: Vec<B256> = read_journal(&journal_file).iter().map(|e| e.hash).collect();
    assert!(
        announced.contains(&pooled_hash),
        "precondition: nonce 0 must have been announced and pruned, else it is still in \
         the pool and the two sources never meet; journaled={announced:?}",
    );

    // The build that has to reconcile the two.
    let payload = build_one_block!(node, Duration::from_millis(600));
    let landed = ordinary_transactions(&payload);

    assert!(
        landed.contains(&pooled_hash),
        "the unlanded pool transaction must land; landed={landed:?}",
    );
    assert!(
        landed.contains(&committed_hash),
        "and so must the carried-over commitment — offered before its predecessor it is \
         refused for the nonce gap, terminally; landed={landed:?}",
    );
    assert!(
        landed.iter().position(|h| *h == pooled_hash) <
            landed.iter().position(|h| *h == committed_hash),
        "nonce 0 before nonce 1, whichever source each came from; landed={landed:?}",
    );

    std::fs::remove_dir_all(&journal_dir).ok();
}
