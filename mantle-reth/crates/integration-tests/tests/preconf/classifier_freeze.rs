//! Regression coverage for the two ways a runtime allowlist change used to
//! split a transaction's classification.
//!
//! Eligibility is decided once, at pool admission, and frozen
//! (`PreconfClassifier`). The reason that matters is that the pool arm and the
//! preconf arm are a **partition**: the pool arm skips a tx precisely because
//! the preconf arm is expected to apply it. Both arms therefore have to read
//! the same record. When they each derived eligibility live from the
//! allowlists, an update landing mid-flight broke the partition in both
//! directions:
//!
//! * **Case A** (eligible → not eligible): the client already holds a preconfirmation receipt and a
//!   fifo entry exists, but the pool arm stops skipping. The tx lands via the normal path, nobody
//!   applies the fifo entry, the responder never fires — the client sees `Timeout` for a tx that is
//!   on chain, and the commitment we handed out is broken.
//! * **Case B** (not eligible → eligible): no fifo entry was ever created, yet the pool arm starts
//!   skipping. Neither arm applies the tx, so it is silently excluded from block building until the
//!   allowlist flips back. A liveness failure with no error anywhere, hitting a tx that was never
//!   promised anything.
//!
//! The test mutates the allowlist **after** both txs have been admitted, which
//! is only possible because `launch_preconf_node_with_classifier!` hands back
//! the node's own `Arc<PreconfClassifier>`.
//!
//! Asymmetry worth knowing before reading further: only the Case B half actually
//! discriminates — reverting the pool arm to a live allowlist lookup makes it
//! fail (`sealed = []`) while the Case A half stays green. The test's own doc
//! comment explains why, and what covers Case A instead.
//!
//! The test does not depend on canonicalisation — it asserts on the resolved
//! payload — so it stays clear of the harness's known parallel-load flakiness
//! (see `docs/preconf-integration-test-harness-issues.md` problem 1).

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec, send_preconf};
use crate::launch_preconf_node_with_classifier;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use reth_chainspec::EthChainSpec;
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

/// Same transfer, signed by an arbitrary signer rather than the harness wallet.
async fn signed_transfer_from(
    signer: alloy_signer_local::PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
) -> alloy_primitives::Bytes {
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
    TransactionTestContext::sign_tx(signer, request).await.encoded_2718().into()
}

/// One node, one allowlist flip, both directions at once.
///
/// The flip replaces `(wallet, RECIPIENT)` with `(other, RECIPIENT)`, so in a
/// single `update_whitelist` it **removes** the sender of a parked commitment
/// (Case A direction) and **adds** the sender of a tx that was already frozen as
/// ineligible (Case B direction). Both assertions then read off one block.
///
/// Merged into a single test on purpose: every node-spawning test adds parallel
/// load, and this suite's canonicalisation is load-sensitive (see
/// `docs/preconf-integration-test-harness-issues.md` problem 1) — two launches
/// measurably increased unrelated flakiness, one does not.
///
/// Sequence:
/// 1. `other` (not allowlisted) submits a plain tx → validator freezes `NotEligible`.
/// 2. `wallet` (allowlisted) submits through the preconf RPC with **no payload job open** → the
///    fifo entry parks in `Waiting`, verdict frozen `Eligible`.
/// 3. Flip the allowlists as above.
/// 4. FCU to start the job, build, assert both txs landed and the commitment was honoured.
///
/// ## What each half proves — measured, not assumed
///
/// **Case B half is the discriminating one.** Reverting `apply_one_best_tx` to a
/// live allowlist lookup makes the `other` assertion fail with `sealed = []`:
/// the pool arm re-derives "eligible" from the widened list and skips the tx,
/// while the preconf arm has no fifo entry for it, so no arm builds it. That is
/// the silent liveness failure the freeze exists to prevent.
///
/// **Case A half does not discriminate**, and that was verified the same way:
/// under the live lookup the commitment still came back `Success`. The reason is
/// structural — `build_payload`'s prologue replays fifo carryover *before* the
/// `select!` loop pulls its first pool tx, and the loop is biased towards the
/// preconf arm, so a parked commitment is always applied by the preconf arm
/// whatever the pool arm's predicate says. The window where Case A can really
/// bite is the sub-millisecond gap between `add_transaction` returning and the
/// listener creating the fifo entry (the gap the skip guard exists for), which a
/// test cannot enter without instrumentation hooks. Case A's regression
/// coverage is therefore the classifier unit test
/// `verdict_is_frozen_when_allowlist_shrinks`, plus the type-level fact that no
/// consumer can reach the allowlists any more.
///
/// The half is still worth asserting: it is the only end-to-end evidence that a
/// mid-run `update_whitelist` — what the on-chain watcher does in production —
/// leaves an outstanding commitment intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frozen_verdicts_survive_an_allowlist_flip_in_both_directions() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // A second funded signer, absent from the initial allowlists. Index 2
    // because signers[1] collides with RECIPIENT. The signer's chain id must be
    // the spec's, or signing rejects the request (`TransactionChainIdMismatch`).
    let other_signer =
        Wallet::new(3).with_chain_id(mantle_test_chain_spec().chain().id()).wallet_gen()[2].clone();
    let other_addr = other_signer.address();

    // Generous client deadline: the commitment must survive the flip, the FCU
    // round-trip and a sweep tick before dispatch runs, and dispatch
    // pre-emptively skips entries whose `elapsed + safety_margin` has already
    // reached the deadline.
    // Stated as an explicit pair, because this test *is* about the allowlist:
    // step 3 swaps this one rule for `(other_addr, recipient)`, and the two
    // should be legible side by side as one-for-one.
    let setup = PreconfCfgBuilder::new()
        .whitelist_pair(wallet_addr, recipient)
        .preconf_timeout_ms(5_000)
        .build();
    let (mut node, http, wallet, chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, mantle_test_chain_spec()).await;

    // ── 1. Case B subject: plain `eth_sendRawTransaction` from a sender that is
    // not allowlisted. The validator classifies synchronously during admission,
    // so the verdict is frozen by the time this returns.
    let other_tx = signed_transfer_from(other_signer, chain_id, 0).await;
    let other_hash: B256 =
        node.rpc.inject_tx(other_tx).await.expect("plain sendRawTransaction accepted");

    // ── 2. Case A subject: preconf submission with no payload job open, so the
    // commitment parks instead of being applied immediately.
    let preconf_tx = signed_transfer(chain_id, &wallet, 0).await;
    let preconf_hash = keccak256(&preconf_tx);
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, preconf_tx).await });

    // Long enough for the RPC handler to attach its responder and the pool
    // listener to create the fifo entry.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── 3. One flip, both directions.
    classifier.update_whitelist(
        [(other_addr, recipient)].into_iter().collect(),
        Default::default(),
        Default::default(),
    );
    assert!(
        !classifier.preview_eligibility(&wallet_addr, Some(&recipient)),
        "the parked commitment's sender must now look ineligible — else Case A proves nothing",
    );
    assert!(
        classifier.preview_eligibility(&other_addr, Some(&recipient)),
        "the plain tx's sender must now look eligible — else Case B proves nothing",
    );

    // ── 4. Build.
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

    // One or two sweep ticks (200ms default) so the pool arm has genuinely had
    // its chance at both txs.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf submission must not error");
    assert!(
        matches!(event.status, mantle_reth_rpc_ext::PreconfStatus::Success),
        "a commitment made before the allowlist flip must still be honoured; \
         got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.tx_hash, preconf_hash);

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&preconf_hash),
        "the preconfirmed tx must be in the block; sealed = {sealed:?}",
    );
    assert!(
        sealed.contains(&other_hash),
        "a tx frozen as not-preconf must keep being built by the pool arm; widening the \
         allowlist afterwards must not strand it. sealed = {sealed:?}",
    );
}
