//! The **same** transaction submitted through both channels.
//!
//! Byte-identical payload, therefore one hash, sent once to
//! `eth_sendRawTransaction` and once to `eth_sendRawTransactionWithPreconf`.
//! Both orders are covered, because they reach the same end state — one hash
//! held by two independent queues — and must therefore give the same answer.
//!
//! What used to refuse the second submission depended on the order, and neither
//! reason survived the direct-admission change:
//!
//! | order | refused by | why it went away |
//! |---|---|---|
//! | ordinary first, then preconf | the frozen verdict: the ordinary admission latched `NotEligible`, and a verdict is immutable, so `claim_preconf` refused | nothing latches a verdict for ordinary transactions once the pool decoration is gone |
//! | preconf first, then ordinary | the pool's own hash dedup — the preconf transaction was *in* the pool, so resubmitting it was "already known" | preconf transactions no longer enter the pool, so the hash is new to it |
//!
//! Accepting both is a deliberate loosening, on the grounds that whatever lands
//! was put there by the client itself: "your preconfirmation failed" and "the
//! copy you submitted through the other channel landed" are both true
//! statements, and no promise the node made alone is broken.
//!
//! **What must not loosen** is what every test here asserts: the transaction
//! lands **exactly once** and consumes its nonce **exactly once**. Two separate
//! things hold that — the pool arm skips a transaction carrying a preconf
//! verdict, and an execution that slips past consumes the nonce so the second
//! attempt is dropped as nonce-too-low — so it is pinned directly rather than
//! argued from either.
//!
//! Which arm wins is asserted only where the submissions are ordered such that
//! one answer is right. In the first two tests both arrive after the build
//! opens and either outcome is acceptable; in the third both precede it, and
//! the preconf arm owning the transaction is the point.
//!
//! A different hash on the same `(sender, nonce)` is a different question —
//! that is replacement, and lives in `replacement.rs`.

use super::helpers::{PreconfCfgBuilder, send_preconf, wait_fifo_entry};
use crate::{canonicalize_payload, launch_preconf_node, launch_preconf_node_with_fifo};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::client::ClientT;
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

/// The invariant both tests exist for: `hash` occurs exactly once in `sealed`,
/// and the sender's canonical nonce advanced by exactly one.
async fn assert_landed_exactly_once(
    http: &jsonrpsee::http_client::HttpClient,
    sealed: &[B256],
    hash: B256,
    sender: Address,
) {
    let occurrences = sealed.iter().filter(|h| **h == hash).count();
    assert_eq!(
        occurrences, 1,
        "the transaction must be included exactly once, not {occurrences} times; \
         sealed = {sealed:?}"
    );

    let nonce: U256 = http
        .request("eth_getTransactionCount", vec![sender.to_string(), "latest".to_string()])
        .await
        .expect("eth_getTransactionCount");
    assert_eq!(
        nonce,
        U256::from(1u64),
        "the nonce must be consumed exactly once; a double-apply would show as 2"
    );
}

/// Ordinary submission first, preconf second.
///
/// The preconf call used to be refused outright, because the ordinary admission
/// had already frozen a non-preconf verdict for this hash. It is accepted now,
/// and races the pool arm for the same transaction — one of them lands it, and
/// the other is dropped at execution as nonce-too-low.
///
/// **Accepted** is asserted against the queue, not against the RPC result:
/// which arm wins is a race, so the call may legitimately come back with a
/// receipt *or* with a builder rejection. An entry in the queue is what says
/// admission let it through, and it says so either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_then_preconf_is_no_longer_refused() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    let raw = signed_transfer(chain_id, &wallet, 0).await;
    let hash: B256 = node.rpc.inject_tx(raw.clone()).await.expect("ordinary submission accepted");

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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw).await });

    // The assertion this test exists for: the preconf submission reached the
    // queue. Under the frozen-verdict rule it never would have.
    wait_fifo_entry(&fifo, sender, 0).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    // Drained so the spawned task does not outlive the node. Its value is not
    // asserted — see the doc comment.
    let _ = rpc_task.await;

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    canonicalize_payload!(node, payload).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_landed_exactly_once(&http, &sealed, hash, sender).await;
}

/// Preconf submission first, ordinary second.
///
/// The ordinary submission used to be "already known" — the preconf transaction
/// was itself in the pool, so the hash was a duplicate. Preconf transactions
/// never enter the pool now, so the hash is new to it and it is accepted; the
/// `expect` below is the assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_then_ordinary_is_no_longer_refused() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw = signed_transfer(chain_id, &wallet, 0).await;
    let hash = keccak256(raw.as_ref());

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
    let raw_for_preconf = raw.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_for_preconf).await });

    // Let the preconf submission take its place before the ordinary one
    // arrives, so the order under test is the one intended.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    node.rpc.inject_tx(raw).await.expect(
        "with preconf transactions out of the pool, an ordinary resubmission of the same \
         payload is a hash the pool has not seen",
    );

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let _ = rpc_task.await;

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    canonicalize_payload!(node, payload).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_landed_exactly_once(&http, &sealed, hash, sender).await;
}

/// Both submissions land **before** the build opens, which is the ordering the
/// other two do not reach.
///
/// The pool arm iterates a snapshot taken when the build starts. In the other
/// two tests the second submission arrives after that snapshot, so the pool arm
/// never sees a transaction carrying a preconf verdict and the filter that
/// would skip it never runs. Here it does: the transaction is in the pool, its
/// verdict is already `Eligible`, and the snapshot contains it.
///
/// What the filter buys is the client's receipt. Both arms could execute this
/// transaction and only one can — whichever goes first consumes the nonce. If
/// the pool arm wins, the transaction lands and the client is told `Timeout`,
/// having been promised a preconfirmation that quietly happened by another
/// route. Asserting `Success` alongside "landed exactly once" is what separates
/// the two outcomes; "landed exactly once" alone holds either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pooled_transaction_with_a_preconf_verdict_is_left_to_the_preconf_arm() {
    use mantle_reth_rpc_ext::PreconfStatus;

    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    let raw = signed_transfer(chain_id, &wallet, 0).await;
    let hash: B256 = node.rpc.inject_tx(raw.clone()).await.expect("ordinary submission accepted");

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw).await });

    // The premise: the verdict is frozen and the entry exists *before* the
    // build opens, so the snapshot the pool arm takes already contains a
    // transaction the preconf arm owns.
    wait_fifo_entry(&fifo, sender, 0).await;

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

    let event = rpc_task.await.expect("rpc join").expect("the preconf submission must be answered");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "the preconf arm must be the one that applied it; got {:?} (reason={:?})",
        event.status,
        event.reason,
    );

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    canonicalize_payload!(node, payload).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_landed_exactly_once(&http, &sealed, hash, sender).await;
}
