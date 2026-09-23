//! One sender, both submission channels, and who owns the nonce view.
//!
//! A whitelisted sender may use `eth_sendRawTransaction` and
//! `eth_sendRawTransactionWithPreconf` for different transactions. The two
//! channels keep separate queues, so "what is this sender's next usable nonce"
//! has two possible answers, and which one the preconf admission uses is a
//! client-visible decision.
//!
//! Today the preconf handler asks the transaction pool, so it sees the sender's
//! ordinary transactions whether or not they have executed. The direct-admission
//! design does not read the pool at all: its view starts from the **chain**
//! nonce and advances as transactions **execute**. The difference shows up in
//! exactly one window — an ordinary transaction sitting in the pool that has not
//! run yet:
//!
//! | ordinary tx state | today | after |
//! |---|---|---|
//! | queued in the pool, not executed | counted → next preconf nonce admitted | not counted → `NonceGap` |
//! | executed (this block or canonical) | counted | counted |
//!
//! The first row is a deliberate narrowing, not a regression to fix: the
//! preconf arm dispatches ahead of the pool arm, so a preconf transaction
//! admitted on top of an unexecuted pool transaction would usually fail at
//! execution anyway with nonce-too-high. Refusing at admission is the same
//! outcome delivered a couple of orders of magnitude sooner, and `NonceGap` is
//! already specified as "resend in nonce order".
//!
//! The second row is the one that must not move, and is what
//! `a_preconf_tx_after_a_canonical_normal_tx_is_admitted` guards.
//!
//! Not covered here: the *mid-block* case — an ordinary transaction executed by
//! the pool arm earlier in the same block being built. Observing it needs a
//! signal that the transaction has run before the block is sealed, which only
//! the flashblocks harness provides. It belongs with the account-view
//! implementation rather than with this file's RPC-level contracts.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::{canonicalize_payload, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
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

/// An ordinary transaction that has **landed** leaves the sender free to
/// continue on the preconf channel at the next nonce.
///
/// Green today and after — by different routes. Today the pool still holds the
/// executed transaction and reports the advanced pending nonce; afterwards the
/// preconf view reads the chain nonce, which the canonical block advanced. The
/// point of pinning it is that the *answer* must not change when the source
/// does.
///
/// Deliberately **not** `#[ignore]`d.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_after_a_canonical_normal_tx_is_admitted() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(8_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Block 1: an ordinary transaction, through the pool arm ───────────
    //
    // Being on the allowlist does not make it preconf — only the RPC method
    // does — so this lands the ordinary way. See `race_pool_arm.rs`.
    let normal = signed_transfer(chain_id, &wallet, 0).await;
    let normal_hash: B256 =
        node.rpc.inject_tx(normal).await.expect("plain sendRawTransaction accepted");

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

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&normal_hash),
        "setup: the ordinary tx must land in block 1 through the pool arm; sealed = {sealed:?}"
    );

    canonicalize_payload!(node, payload).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Block 2: the same sender continues on the preconf channel ────────
    let preconf = signed_transfer(chain_id, &wallet, 1).await;

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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, preconf).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect(
        "an executed ordinary tx must leave the sender free to continue on the preconf \
         channel — whichever component supplies the nonce view",
    );
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "expected Success, got {:?} (reason={:?})",
        event.status,
        event.reason
    );

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&event.tx_hash), "the preconf tx must land in block 2; {sealed:?}");
}

/// The transaction refused at admission after the change would have failed at
/// execution before it — so the narrowing costs nothing.
///
/// This is the load-bearing evidence for refusing rather than admitting. The
/// argument for the narrowing is "it was doomed anyway, and finding out sooner
/// is better"; without this test that is an assertion about the builder rather
/// than a fact about it. If it turned out the transaction *could* succeed
/// today, refusing it at admission would be a genuine loss of function, not a
/// tightening.
///
/// Why it is doomed: the preconf arm dispatches carryover **before** the pool
/// arm drains (`payload_builder.rs`), so the commitment at `nonce+1` is applied
/// while the ordinary transaction at `nonce` has not run. The EVM sees a nonce
/// above the account's and rejects it.
///
/// **Green today and after**, by different routes, which is exactly why the
/// assertions are deliberately loose — they name the outcome, not the error:
///
/// | | admission | outcome |
/// |---|---|---|
/// | today | passes (the pool's pending nonce covers the queued tx) | `BuilderRejected` at apply |
/// | after | `NonceGap` | refused outright |
///
/// Either way the commitment does not land and the client gets an error. The
/// ordinary transaction lands regardless, which is what makes this a narrowing
/// of *feedback latency* rather than of function.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_behind_an_unexecuted_normal_tx_never_lands_either_way() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Long enough that today's path reaches the builder and comes back
        // with its apply rejection, rather than timing out first — a timeout
        // would satisfy "did not land" for the wrong reason.
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Ordinary tx at nonce 0, queued and never executed.
    let normal = signed_transfer(chain_id, &wallet, 0).await;
    let normal_hash: B256 =
        node.rpc.inject_tx(normal).await.expect("plain sendRawTransaction accepted");

    // The commitment sits one nonce above it.
    let preconf = signed_transfer(chain_id, &wallet, 1).await;
    let preconf_hash = keccak256(&preconf);

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, preconf).await });

    // A fixed wait rather than polling pool state: after the change the
    // submission is refused outright and never reaches the pool, so anything
    // that polls for it would hang. This is ample for the pool to admit it
    // before the build starts.
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

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let outcome = rpc_task.await.expect("rpc join");
    assert!(
        outcome.is_err(),
        "a commitment sitting above an unexecuted ordinary tx cannot be honoured; \
         the client must be told so, whether at admission or after the apply. Got: {outcome:?}"
    );

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(!sealed.contains(&preconf_hash), "the commitment must not land; sealed = {sealed:?}");
    assert!(
        sealed.contains(&normal_hash),
        "the ordinary tx underneath it must still land — the narrowing is about when the \
         commitment's failure is reported, not about blocking the sender; sealed = {sealed:?}"
    );
}

/// An ordinary transaction still **queued** in the pool does not extend the
/// sender's preconf nonce view.
///
/// The narrowing described in the module docs. Today the pool's pending nonce
/// covers the queued transaction, so the follow-up preconf submission is
/// admitted and then simply waits — the client learns nothing until
/// `preconf_timeout`. Afterwards it is refused immediately with `NonceGap`,
/// which the client is already expected to handle by resending in order.
///
/// No block is built here on purpose: the ordinary transaction must stay
/// unexecuted for the window under test to exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_after_an_unexecuted_normal_tx_is_refused_as_a_nonce_gap() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Generous, so that a regression which admits the tx surfaces as a
        // slow failure rather than a fast pass — the assertions still catch it.
        .preconf_timeout_ms(1_500)
        .build();

    let (node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Queued in the pool, never executed — no FCU is issued.
    let normal = signed_transfer(chain_id, &wallet, 0).await;
    node.rpc.inject_tx(normal).await.expect("plain sendRawTransaction accepted");

    let preconf = signed_transfer(chain_id, &wallet, 1).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, preconf)
        .await
        .expect_err("nonce=1 must be refused while nonce=0 is merely queued in the pool");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(msg.contains("nonce gap"), "unexpected error message: {}", e.message());
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // The whole value of the narrowing is the speed of the answer. Without
    // this, an implementation that admits the tx and lets it time out would
    // still satisfy the match above once `Timeout` became an `Err`.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "the refusal must be synchronous (< 500ms); took {elapsed:?}",
    );
}
