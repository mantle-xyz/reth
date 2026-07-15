//! Replacement / resubmission semantics for preconf-eligible txs.
//!
//! Full 2×2 coverage of {existing fifo state} × {new submission kind}:
//!
//! **Same-hash resubmit**:
//!
//! - `active_hash_resubmit_returns_already_in_progress` — a `Waiting`
//!   fifo entry **blocks** same-hash replacement at
//!   `attach_responder`; the RPC returns `AlreadyInProgress`
//!   synchronously without touching the pool.
//!
//! **Different-hash, same `(sender, nonce)`**:
//!
//! - `waiting_slot_blocks_different_hash_replacement` — a `Waiting`
//!   fifo entry **blocks** different-hash replacement at the pool
//!   validator (`PreconfAwareValidator::ReplaceActivePreconf`); wire
//!   surfaces as `Err(Call { "pool rejected: cannot replace active
//!   preconf commitment..." })`. Safety-critical: prevents a malicious
//!   client from evicting an in-flight preconf commitment by
//!   submitting a low-value same-`(sender, nonce)` tx.
//! - `timeout_slot_replaceable_by_different_hash` — `Timeout` entries
//!   are reclaimable, releasing the slot; a differently-signed tx for
//!   the same slot admits and lands on chain.
//! - `canceled_slot_replaceable_by_different_hash` — symmetric to
//!   Timeout: F1-Canceled entries also release the slot.
//!
//! **Failed replacement not covered here**: reliably triggering fifo
//! `Failed` from the integration layer would require a builder-level
//! rejection that survives pool validator screening (rare in-flight
//! state races), and no clean harness exists. The `Failed`-branch of
//! the state machine is instead pinned by the unit tests
//! `preconf_tx_set::tests::push_conflict_after_failed_evicts_and_inserts`
//! (different-hash replacement admits) and
//! `push_same_hash_after_failed_revives_to_waiting` (same-hash
//! resubmit revives) together with the validator-layer test
//! `pool_ext::validator::tests` that guard the `ReplaceActivePreconf`
//! release set.

use super::helpers::{send_preconf, PreconfCfgBuilder};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
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

/// First call parks a responder; a same-hash resubmission that arrives before
/// the deadline elapses must be rejected as `AlreadyInProgress`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_hash_resubmit_returns_already_in_progress() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Long timeout so the first call remains parked throughout the test.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(5_000)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;

    // First call: no payload job is running, so the responder is parked and
    // this future stays pending until either a build applies the tx or the
    // preconf timeout fires. Own it in a spawned task so the main test can
    // drive the second submission.
    let http_first = http.clone();
    let raw_first = raw_tx.clone();
    let first = tokio::spawn(async move { send_preconf(&http_first, raw_first).await });

    // Give the RPC handler time to complete step "attach_responder" before
    // the second call races it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("second submission of an active hash must be rejected");

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("already in progress") || msg.contains("in progress"),
                "unexpected error message: {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // The first call is still parked — abort it so the test finishes without
    // waiting the full 5s timeout.
    first.abort();
}

/// A Timeout entry in the fifo must NOT hold the `(sender, nonce)` slot
/// against replacement — after the first tx times out, a differently-
/// signed tx for the same slot (different `value`, hence different
/// hash) must be admitted and land on chain. Guards against a
/// regression where the Timeout-state entry blocks replacement forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_slot_replaceable_by_different_hash() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(150)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // First tx (nonce=0, value=1) — will time out with no build.
    let tx_a = signed_transfer(chain_id, &wallet, 0).await;
    let first = send_preconf(&http, tx_a).await.expect("first call Ok");
    assert!(
        matches!(first.status, PreconfStatus::Timeout),
        "first tx must time out; got {:?}",
        first.status,
    );

    // Second tx: same (sender, nonce) but different `value` ⇒ different
    // signed hash. Sign explicitly so the value differs from
    // `signed_transfer`'s hard-coded `1`.
    let tx_b: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            // `value=42` distinguishes tx_b's hash from tx_a's.
            value: Some(U256::from(42u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request)
            .await
            .encoded_2718()
            .into()
    };
    let expected_hash_b = alloy_primitives::keccak256(&tx_b);

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

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, tx_b).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("replacement must return Ok");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "replacement tx must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.tx_hash, expected_hash_b);

    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&expected_hash_b),
        "replacement tx must land in the next block; sealed = {sealed:?}",
    );
}

/// A `Waiting` fifo entry (client's first preconf submission actively
/// awaiting apply) must NOT be replaceable by a different-hash tx with
/// the same `(sender, nonce)`. The pool validator's
/// [`PreconfAwareValidator`] replacement guard rejects with
/// `ReplaceActivePreconf`, which the RPC handler wraps as
/// `PreconfError::PoolRejected(...)` — client sees
/// `Err(Call { "pool rejected: cannot replace active preconf
/// commitment..." })`.
///
/// Safety property: without this guard, a malicious client could evict
/// another submitter's in-flight preconf commitment by simply issuing
/// a same-`(sender, nonce)` tx with different content. The first
/// submitter's receipt would then be silently dropped, breaking the
/// preconf SLA. This test pins the wire signature so a regression in
/// the validator's release set (currently `Timeout | Canceled |
/// Failed`) is caught before it ships.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_slot_blocks_different_hash_replacement() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Long timeout so tx_A remains in `Waiting` throughout the test.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(5_000)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // tx_A parks a responder and sits in `Waiting` (no FCU → no apply).
    let tx_a = signed_transfer(chain_id, &wallet, 0).await;
    let http_first = http.clone();
    let raw_first = tx_a.clone();
    let first = tokio::spawn(async move { send_preconf(&http_first, raw_first).await });

    // Give the RPC handler time to complete Step 0-4 (decode, whitelist,
    // nonce, attach_responder, pool.add). By the time this sleep ends
    // the fifo has a `Waiting` entry for tx_A.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // tx_B: same sender + same nonce, but different `value` → different
    // signed hash → hits the validator's replacement guard.
    let tx_b: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(42u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request)
            .await
            .encoded_2718()
            .into()
    };

    let start = std::time::Instant::now();
    let err = send_preconf(&http, tx_b)
        .await
        .expect_err("different-hash submission against active Waiting slot must be rejected");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("pool rejected"),
                "expected `PreconfError::PoolRejected(...)` wrapping; got {}",
                e.message(),
            );
            assert!(
                msg.contains("cannot replace active preconf")
                    || msg.contains("replace active preconf"),
                "expected `ReplaceActivePreconf` Display substring; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // Pool-validator rejection is synchronous; no responder was parked
    // for tx_B, no fifo entry created.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "replacement rejection must fail fast (< 500ms); took {elapsed:?}",
    );

    // Clean up: abort tx_A's parked responder so the test doesn't wait
    // the full 5s preconf_timeout on teardown.
    first.abort();
}

/// Symmetric to `timeout_slot_replaceable_by_different_hash`: a
/// `Canceled` fifo entry (F1 pre-apply rejection, e.g. block gas budget)
/// also releases the `(sender, nonce)` slot. A differently-signed tx
/// for the same slot admits and lands on chain in the next slot.
///
/// The Canceled state is set up via the same F1 pattern as
/// `gas_budgets::canceled_tx_recoverable_in_next_slot`: three 21k-gas
/// txs against a 50k block cap; the third gets Canceled. Then a
/// different-hash tx replaces it after canon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_slot_replaceable_by_different_hash() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(30_000)
        .max_gas_per_block(50_000)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: tx0/tx1 land, tx2 F1-rejected (Canceled) ─────────────
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

    // Pre-signed 21k-gas transfers for nonces 0/1/2, all with value=1
    // (reuses top-level `signed_transfer`).
    let tx0 = signed_transfer(chain_id, &wallet, 0).await;
    let tx1 = signed_transfer(chain_id, &wallet, 1).await;
    let tx2_canceled = signed_transfer(chain_id, &wallet, 2).await;

    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t2 = tokio::spawn(async move { send_preconf(&http_c, tx2_canceled).await });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let payload_1 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 1")
        .expect("payload 1");

    let _ = t0.await.expect("t0 join").expect("tx0 must succeed");
    let _ = t1.await.expect("t1 join").expect("tx1 must succeed");
    let err_2 = t2
        .await
        .expect("t2 join")
        .expect_err("tx2 must be F1-rejected → fifo Canceled");
    assert!(
        matches!(err_2, ClientError::Call(ref e) if e.message().to_lowercase().contains("gas budget")),
        "expected BlockGasBudgetExceeded to produce fifo Canceled state; got {err_2:?}",
    );

    // Canonicalise slot 1 so on-chain nonce advances to 2 and the F1
    // budget resets. tx2's fifo entry stays in `Canceled` state
    // (nonce=2 is at the head of the sender's frontier; `forward` at
    // slot-2 prologue would clear entries with nonce < 2, leaving tx2
    // intact).
    let new_head = node.submit_payload(payload_1).await.expect("submit slot 1");
    node.update_forkchoice(new_head, new_head).await.expect("canon slot 1");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── Slot 2: submit tx2_replacement — same sender + same nonce=2,
    //    different `value` (⇒ different hash). Must replace the
    //    Canceled entry and land on chain.
    let attrs_2 = node.payload.next_attributes();
    let fcu_state_2 = node.current_forkchoice_state().expect("forkchoice state 2");
    let payload_id_2 = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state_2, Some(attrs_2))
        .await
        .expect("FCU 2")
        .payload_id
        .expect("payload_id 2");

    // Replacement tx: same sender + nonce=2 but different `value` (99
    // instead of 1) → different signed hash.
    let tx2_replacement: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(2),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(99u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request)
            .await
            .encoded_2718()
            .into()
    };
    let expected_replacement_hash = alloy_primitives::keccak256(&tx2_replacement);

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, tx2_replacement).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_2 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_2, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");

    let event = rpc_task.await.expect("rpc join").expect("replacement must return Ok");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "replacement of Canceled entry must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.tx_hash, expected_replacement_hash);

    let sealed: Vec<alloy_primitives::B256> = payload_2
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&expected_replacement_hash),
        "different-hash replacement of Canceled entry must land in block 2; sealed = {sealed:?}",
    );
}
