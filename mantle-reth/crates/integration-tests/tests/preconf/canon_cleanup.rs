//! Canonical-commit driven cleanup of preconf state.
//!
//! After a preconf-eligible tx is sealed and its block is committed to
//! canonical, the sender's fifo entry no longer holds the (sender, nonce)
//! slot: the next-nonce tx from the same sender is admitted, applied and
//! lands in the following block. Coverage:
//!
//! - `canon_commit_permits_next_nonce_from_same_sender` — base case: single nonce lands + canon →
//!   next nonce lands in next block.
//! - `canon_of_multi_nonce_batch_permits_higher_nonce_in_next_slot` — multi-nonce batch (0/1/2) in
//!   slot 1 canon'd → nonce=3 lands in slot 2. Guards `sync_fifo_forward_to_head`'s multi-nonce
//!   forward.
//! - `canon_does_not_leak_across_senders` — sender A's canon does not affect sender B's fresh
//!   submission. Guards per-sender scoping.
//! - `canon_across_sequential_slots_forwards_on_every_new_job` — nonce 0/1/2 across three separate
//!   slots; each canon runs `sync_fifo_forward_to_head` afresh. Guards against a caching bug that
//!   would skip forward after the first `PayloadJob`.
//!
//! Several tests here raise `preconf_timeout_ms` well above the 1.5s default: they assert
//! *where* a tx is routed after a canon commit, never how fast, so under parallel load the
//! default deadline is the only thing that fires — as a spurious `Timeout`.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::{canonicalize_payload, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
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

/// A preconf tx that lands in block N advances the sender's on-chain nonce;
/// after the block is committed to canonical, submitting the next-nonce tx
/// through the preconf RPC must succeed and land in block N+1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canon_commit_permits_next_nonce_from_same_sender() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(8_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: submit nonce=0, seal, commit to canonical ────────────
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;

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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let event = rpc_task.await.expect("rpc join").expect("nonce=0 must succeed");
    assert!(matches!(event.status, PreconfStatus::Success));
    let hash0 = event.tx_hash;

    // Commit to canonical: submit the payload, then push forkchoice with
    // head/safe/finalized all pointing at the new block. This is what
    // triggers the canon handler's forward + clean_reclaimable.
    let _new_head = canonicalize_payload!(node, payload).await;

    // Give the canon handler a beat to process the notification.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Slot 2: submit nonce=1, seal, verify it lands ────────────────
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;

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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let event = rpc_task.await.expect("rpc join").expect("nonce=1 must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "next-nonce preconf must succeed after canonical commit; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.block_height, 2, "next-nonce tx must be predicted for block 2");

    let block = payload.block();
    assert_eq!(block.number, 2);
    let sealed: Vec<B256> =
        block.body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&event.tx_hash), "block 2 must contain the nonce=1 tx");
    assert!(!sealed.contains(&hash0), "block 2 must not re-include the already-canon nonce=0 tx",);
}

/// Multi-nonce batch canon: same sender lands nonces 0/1/2 in slot 1,
/// then a fresh nonce=3 lands in slot 2. Verifies
/// `sync_fifo_forward_to_head` correctly forwards past a jump of >1
/// nonce in one canon step — not just the single-nonce case covered by
/// `canon_commit_permits_next_nonce_from_same_sender`.
///
/// Regression guard: an off-by-one in `fifo.forward(sender, max+1)`
/// (e.g. using `min` instead of `max`, or `<=` instead of `<`) would
/// either leave stale entries or drop the wrong ones, and would only
/// surface in the multi-nonce case.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canon_of_multi_nonce_batch_permits_higher_nonce_in_next_slot() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(8_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: land nonces 0, 1, 2 in one block, canonicalise ───────
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let tx2 = signed_transfer(chain_id, &wallet, 2).await;
    let hash0 = keccak256(&tx0);
    let hash1 = keccak256(&tx1);
    let hash2 = keccak256(&tx2);

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

    // Serial submit with 30ms gaps preserves nonce order into the pool.
    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t2 = tokio::spawn(async move { send_preconf(&http_c, tx2).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let payload_1 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 1")
        .expect("payload 1");

    let _ = t0.await.expect("t0 join").expect("tx0 must succeed");
    let _ = t1.await.expect("t1 join").expect("tx1 must succeed");
    let _ = t2.await.expect("t2 join").expect("tx2 must succeed");

    let sealed_1: Vec<B256> =
        payload_1.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    for (label, h) in [("nonce=0", hash0), ("nonce=1", hash1), ("nonce=2", hash2)] {
        assert!(sealed_1.contains(&h), "slot 1 must contain {label}; sealed={sealed_1:?}",);
    }

    let _new_head = canonicalize_payload!(node, payload_1).await;

    // ── Slot 2: submit nonce=3, must land in block 2 ─────────────────
    let tx3 = signed_transfer(chain_id, &wallet, 3).await;

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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, tx3).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_2 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");
    let event = rpc_task.await.expect("rpc join").expect("nonce=3 must succeed");

    assert!(
        matches!(event.status, PreconfStatus::Success),
        "nonce=3 must land after multi-nonce canon; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.block_height, 2, "nonce=3 must predict block 2");

    let sealed_2: Vec<B256> =
        payload_2.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed_2.contains(&event.tx_hash), "block 2 must contain nonce=3");
    // Slot 1's fully-sealed nonces must NOT reappear in block 2. Guards
    // against a `sync_fifo_forward_to_head` regression that fails to
    // drop entries at max_sealed_nonce.
    for (label, h) in [("nonce=0", hash0), ("nonce=1", hash1), ("nonce=2", hash2)] {
        assert!(
            !sealed_2.contains(&h),
            "block 2 must NOT re-include already-canon {label}; sealed={sealed_2:?}",
        );
    }
}

/// Canon of sender A's tx must NOT affect sender B's fifo/nonce state.
/// After A's slot-1 canon, B (who has never submitted before) submits
/// a fresh nonce=0 in slot 2, and it must land normally — proving that
/// `sync_fifo_forward_to_head` is per-sender scoped and canon-side
/// cleanup does not touch unrelated senders.
///
/// Regression guard: a global-instead-of-per-sender forward (e.g.
/// accidentally forwarding the fifo to A's nonce for all senders)
/// would either drop B's queued tx or cause a nonce mismatch at
/// dispatch. Both surface here as B's tx failing to land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canon_does_not_leak_across_senders() {
    use super::helpers::mantle_test_chain_spec;
    use reth_chainspec::EthChainSpec;

    let recipient: Address = RECIPIENT.parse().unwrap();

    // Addresses are chain-id-independent; placeholder here is fine for
    // whitelist config, then we re-derive signers with the launched
    // chain_id for actual signing (see happy_path::multi_sender_land_in_one_block).
    let chain_id_for_addrs = mantle_test_chain_spec().chain().id();
    let signers_for_addr = Wallet::new(3).with_chain_id(chain_id_for_addrs).wallet_gen();
    let sender_a_addr = signers_for_addr[0].address();
    // signers[1] collides with RECIPIENT; skip and use [2] as sender B.
    let sender_b_addr = signers_for_addr[2].address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a_addr)
        .whitelist_from(sender_b_addr)
        .whitelist_to(recipient)
        .build();

    let (mut node, http, wallet_a, chain_id) = launch_preconf_node!(cfg).await;
    assert_eq!(wallet_a.inner.address(), sender_a_addr);
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();

    // ── Slot 1: sender A lands nonce=0 and canonicalise ──────────────
    let tx_a = signed_transfer(chain_id, &wallet_a, 0).await;
    let hash_a = keccak256(&tx_a);

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
    let rpc_a = tokio::spawn(async move { send_preconf(&http_c, tx_a).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_1 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 1")
        .expect("payload 1");
    let _ = rpc_a.await.expect("rpc_a join").expect("A must succeed");

    let sealed_1: Vec<B256> =
        payload_1.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed_1.contains(&hash_a), "sender A's tx must land in slot 1");

    let _new_head = canonicalize_payload!(node, payload_1).await;

    // ── Slot 2: sender B submits nonce=0 (their first tx). A's canon
    //    must NOT affect B's fresh submission. ────────────────────────
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
    let rpc_b = tokio::spawn(async move { send_preconf(&http_c, tx_b).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_2 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");
    let event_b = rpc_b.await.expect("rpc_b join").expect("B must succeed");

    assert!(
        matches!(event_b.status, PreconfStatus::Success),
        "sender B's nonce=0 must land after A's canon; got {:?} reason={:?}",
        event_b.status,
        event_b.reason,
    );
    assert_eq!(event_b.block_height, 2);

    let sealed_2: Vec<B256> =
        payload_2.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed_2.contains(&hash_b), "block 2 must contain sender B's tx; sealed={sealed_2:?}",);
    assert!(
        !sealed_2.contains(&hash_a),
        "block 2 must NOT re-include sender A's canon'd tx; sealed={sealed_2:?}",
    );
}

/// Sequential canon across three slots, one tx per slot. Verifies
/// `sync_fifo_forward_to_head` runs on every new `PayloadJob` (not just
/// the first) and that per-slot state does not leak into later blocks.
///
/// Complements `canon_of_multi_nonce_batch_permits_higher_nonce_in_next_slot`,
/// which batches nonces into a single slot then canonicalises once —
/// this one canonicalises once per nonce, exercising the forward path
/// N times.
///
/// Regression guard: a caching bug that skips
/// `sync_fifo_forward_to_head` after the first `PayloadJob` would leak
/// slot-1 Success entries into slot 3's `replay_fifo_carryover`, and
/// `reset_success_to_waiting` would try to re-apply them against a
/// state where their nonces are already consumed → `BuilderRejected` /
/// `Failed` in later slots. Would only surface at slot 3+.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canon_across_sequential_slots_forwards_on_every_new_job() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(8_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Land nonces 0, 1, 2 across three separate slots, canonicalising
    // each in turn.
    let mut prior_hashes: Vec<B256> = Vec::new();
    for nonce in 0u64..3 {
        let tx = signed_transfer(chain_id, &wallet, nonce).await;
        let hash = keccak256(&tx);

        let attrs = node.payload.next_attributes();
        let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU")
            .payload_id
            .expect("payload_id");

        let http_c = http.clone();
        let rpc_task = tokio::spawn(async move { send_preconf(&http_c, tx).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let payload = node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload");
        let event = rpc_task
            .await
            .expect("rpc join")
            .unwrap_or_else(|_| panic!("nonce={nonce} must succeed"));

        assert!(
            matches!(event.status, PreconfStatus::Success),
            "nonce={nonce} status: {:?} reason={:?}",
            event.status,
            event.reason,
        );
        // Predicted block_height starts at 1 and advances by 1 per
        // canon step.
        assert_eq!(event.block_height, nonce + 1, "nonce={nonce} predicted block_height mismatch",);

        let sealed: Vec<B256> =
            payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
        assert!(
            sealed.contains(&hash),
            "block {} must contain nonce={nonce} tx; sealed={sealed:?}",
            nonce + 1,
        );
        // No prior-slot tx should reappear in this block — proves
        // `sync_fifo_forward_to_head` drops committed entries on every
        // new PayloadJob.
        for (prior_i, prior_hash) in prior_hashes.iter().enumerate() {
            assert!(
                !sealed.contains(prior_hash),
                "block {} must NOT re-include nonce={prior_i} tx (already canon'd \
                 in slot {}); sealed={sealed:?}",
                nonce + 1,
                prior_i + 1,
            );
        }

        prior_hashes.push(hash);

        // Canonicalise this slot before starting the next.
        let _new_head = canonicalize_payload!(node, payload).await;
    }
}
