//! Pool-arm regression + co-existence coverage.
//!
//! `apply_one_best_tx` in the fork's payload builder skips preconf-
//! eligible txs (`cfg.is_preconf_tx(sender, to)`) before executing
//! them, so a preconf-eligible tx that was admitted to the pool via a
//! **plain** `eth_sendRawTransaction` can never leak out via the pool
//! iterator ahead of its fifo entry. It must land on chain via the
//! preconf pipeline (pool listener → fifo broadcast → `apply_one_preconf`).
//!
//! Coverage:
//!
//! - `preconf_eligible_regular_sendtx_still_lands` — matches predicate, pool arm skips, preconf arm
//!   applies.
//! - `non_preconf_eligible_regular_sendtx_lands_via_pool_arm` — does NOT match predicate, gate is a
//!   no-op, tx lands through the vanilla pool best-tx iterator. Guards against a regression where
//!   the gate accidentally rejects all pool-path txs on a preconf- enabled node.
//! - `preconf_and_pool_txs_coexist_in_one_block` — a preconf-RPC tx and a regular-sendTx tx (from
//!   independent senders) target the same slot; both land in that block, and the `select!`-biased
//!   preconf arm ensures the preconf tx precedes the pool-arm tx.
//! - `pool_tx_arriving_after_build_start_waits_for_the_next_block` — the pool iterator is a
//!   snapshot, so a tx that reaches the pool after the build began is not in this block; it is not
//!   lost either, and lands in the next one.

use super::helpers::PreconfCfgBuilder;
use crate::{canonize_built, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
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

/// Preconf-eligible tx submitted through the ordinary `eth_sendRawTransaction`
/// still lands on chain (via listener → fifo → dispatch), even though the pool
/// arm now short-circuits it.
///
/// Regression guard: before the fix, this tx would leak through the
/// pool best-tx iterator ahead of its fifo entry, potentially
/// double-applying or bypassing preconf ordering. After the fix, the
/// pool arm hits the `is_preconf_tx` gate + `mark_invalid`, and the
/// tx can only reach the block through `apply_one_preconf`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_eligible_regular_sendtx_still_lands() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(recipient).build();

    let (mut node, _http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Submit via ordinary sendRawTransaction — no responder attached,
    // no preconf event returned.
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

    let block = payload.block();
    assert_eq!(block.number, 1);
    let sealed: Vec<B256> =
        block.body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&tx_hash),
        "preconf-eligible tx submitted via regular sendRawTransaction must still land \
         on chain (via preconf listener → fifo → apply_one_preconf); \
         hash {tx_hash:?} not in sealed block: {sealed:?}"
    );
}

/// Non-preconf-eligible tx submitted via ordinary `eth_sendRawTransaction`
/// reaches chain through the vanilla pool best-tx iterator
/// (`apply_one_best_tx`). The `is_preconf_tx` gate evaluates to false and the
/// arm executes normally.
///
/// Regression guard: catches a hypothetical accidental broadening of
/// the gate (e.g. `!is_preconf_tx` typo) that would starve the pool
/// path on any preconf-enabled node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_preconf_eligible_regular_sendtx_lands_via_pool_arm() {
    // Whitelist an unrelated placeholder pair so the config validates
    // (`enabled=true` needs non-empty from/to). Our test wallet is not
    // on the from-list, and RECIPIENT is not on the to-list ⇒
    // `is_preconf_tx` returns false for the tx below ⇒ pool arm handles it.
    let placeholder = Address::from([0xFE; 20]);
    let cfg =
        PreconfCfgBuilder::new().whitelist_from(placeholder).whitelist_to(placeholder).build();

    let (mut node, _http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash: B256 =
        node.rpc.inject_tx(raw_tx.clone()).await.expect("plain sendRawTransaction accepted");

    // The pool arm relies on the adaptive-N quota schedule to release
    // gas over successive sweep ticks; a single 21k tx fits comfortably
    // in the first tick's batch (block_gas_limit / ticks_remaining >> 21k).
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

    // Wait long enough for at least one sweep tick to bump `pool_quota`
    // above 21k — default `sweep_interval = 200ms`, so 400ms guarantees
    // 1-2 ticks even after subtracting scheduler jitter.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let block = payload.block();
    let sealed: Vec<B256> =
        block.body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&tx_hash),
        "non-preconf-eligible tx must land through the pool arm; \
         hash {tx_hash:?} not in sealed block: {sealed:?}"
    );
}

/// Preconf-RPC tx (fifo arm) and plain-sendTx tx (pool arm) target the
/// same slot; both must land, and the biased `select!` in
/// `build_payload` must place the preconf tx at a lower body index than
/// the pool-arm tx.
///
/// The two senders are chosen from different derivation indices so the
/// pool-arm tx does NOT match the whitelist (`is_preconf_tx=false`) —
/// otherwise it would also be routed into the fifo and both txs would
/// land through the preconf path, which is not what we're testing.
///
/// **The pool tx is injected before the FCU, and has to be:** `build_payload`
/// freezes its pool iterator at the start of Stage 3, so a tx admitted after
/// that is a candidate for the *next* block (see
/// `pool_tx_arriving_after_build_start_waits_for_the_next_block`). This test is
/// about arm ordering within one block, so both txs must be candidates for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_and_pool_txs_coexist_in_one_block() {
    use super::helpers::{mantle_test_chain_spec, send_preconf};
    use reth_chainspec::EthChainSpec;

    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id_for_addrs = mantle_test_chain_spec().chain().id();
    let signers = Wallet::new(3).with_chain_id(chain_id_for_addrs).wallet_gen();
    let preconf_sender_addr = signers[0].address();
    // signers[1] address collides with RECIPIENT — skip it. signers[2] is
    // deliberately absent from the whitelist so its tx routes to the pool arm.
    let pool_sender_signer = signers[2].clone();

    // Whitelist ONLY the preconf sender; the pool sender is not on the
    // list and its tx therefore misses `is_preconf_tx`.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(preconf_sender_addr)
        .whitelist_to(recipient)
        .build();

    let (mut node, http, preconf_wallet, chain_id) = launch_preconf_node!(cfg).await;
    assert_eq!(preconf_wallet.inner.address(), preconf_sender_addr);

    // Pool-arm tx: submit via `inject_tx` (plain `eth_sendRawTransaction`),
    // **before** the FCU so it is in the pool when the build snapshots it.
    // `pool_sender_signer.address()` misses the whitelist ⇒ preconf listener
    // filter drops it ⇒ never enters fifo ⇒ pool arm is its only path.
    let pool_tx: alloy_primitives::Bytes = {
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
        TransactionTestContext::sign_tx(pool_sender_signer, request).await.encoded_2718().into()
    };
    let pool_hash: B256 = node.rpc.inject_tx(pool_tx).await.expect("pool sendTx accepted");

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

    // Preconf-RPC tx: submitted through the preconf handler; parks a
    // responder, waits for dispatch to apply.
    let preconf_tx = signed_transfer(chain_id, &preconf_wallet, 0).await;
    let preconf_hash = keccak256(&preconf_tx);
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, preconf_tx).await });

    // Give the preconf arm a small head start so it wins the first
    // select! tick; also long enough for at least one sweep_ticker fire
    // (default 200ms interval) to release pool_quota above 21k.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf tx must succeed");
    assert_eq!(event.tx_hash, preconf_hash);

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    let idx_preconf = sealed
        .iter()
        .position(|h| *h == preconf_hash)
        .unwrap_or_else(|| panic!("preconf tx must land; sealed={sealed:?}"));
    let idx_pool = sealed
        .iter()
        .position(|h| *h == pool_hash)
        .unwrap_or_else(|| panic!("pool tx must land; sealed={sealed:?}"));
    assert!(
        idx_preconf < idx_pool,
        "biased select! must place preconf arm ahead of pool arm; idx_preconf={idx_preconf}, idx_pool={idx_pool}",
    );
}

/// **The pool iterator is a snapshot taken at build start.** A tx that reaches
/// the pool after that is not a candidate for the block being built; it stays
/// pooled and lands in the next one.
///
/// Regression guard for `without_updates()` in `build_payload` — without it the
/// iterator keeps a live subscription to the pending pool and pulls the tx into
/// block 1, failing the first assertion.
///
/// Both halves matter: block 1 proves the snapshot holds, block 2 proves the tx
/// was **deferred, not dropped**.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_tx_arriving_after_build_start_waits_for_the_next_block() {
    // Whitelist an unrelated placeholder so the config validates while the test
    // wallet misses the list — `is_preconf_tx` is false, so the pool arm is this
    // tx's only route and the preconf pipeline is not in the picture.
    let placeholder = Address::from([0xFE; 20]);
    let cfg =
        PreconfCfgBuilder::new().whitelist_from(placeholder).whitelist_to(placeholder).build();

    let (mut node, _http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Block 1: build starts against an empty pool ──────────────────
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

    // The FCU returns once the job is registered, not once it reaches Stage 3 —
    // this is what makes the tx provably later than the snapshot.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash: B256 =
        node.rpc.inject_tx(raw_tx).await.expect("plain sendRawTransaction accepted");

    // Two `sweep_interval`s (200ms default): the pool arm has had its quota and
    // run, so a miss below is the snapshot, not a ceiling that never opened.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    assert_eq!(payload.block().number, 1);
    let sealed_1: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        !sealed_1.contains(&tx_hash),
        "a tx admitted to the pool after the build snapshotted it must not be in that block; \
         hash {tx_hash:?} leaked into sealed block 1: {sealed_1:?}"
    );

    // ── Block 2: still pooled, and now inside the snapshot ───────────
    canonize_built!(node, payload);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

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

    // One sweep tick is enough for a single 21k tx (gas_per_batch >> 21k).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let sealed_2: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed_2.contains(&tx_hash),
        "the tx was deferred, not dropped — it must land in the next block; \
         hash {tx_hash:?} not in sealed block 2: {sealed_2:?}"
    );
}
