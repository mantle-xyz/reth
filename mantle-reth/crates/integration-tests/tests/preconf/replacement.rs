//! Replacement / resubmission semantics for preconf-eligible txs.
//!
//! Four axes, because the first alone left real gaps:
//!
//! 1. {existing fifo state} × {new submission kind} — the 2×2 below.
//! 2. **{incumbent has a fifo entry} × {incumbent has only a frozen verdict}** —
//!    `a_landed_commitment_still_owns_its_nonce_with_no_fifo_entry`. The guard keys on the
//!    classifier's slot index, not on fifo membership, because the fifo entry is created
//!    asynchronously.
//! 3. **{sender's eligibility unchanged} × {sender de-allowlisted mid-flight}** —
//!    `replacement_is_refused_after_sender_leaves_the_allowlist`. The newcomer's *own* verdict must
//!    not gate the guard, or the two arms end up holding one tx each for the same nonce.
//! 4. **{replacement passes its own checks} × {replacement is itself rejected}** —
//!    `rejected_replacement_leaves_the_reclaimable_holder_intact`. Deciding a holder is reclaimable
//!    must not tear it down before the newcomer has cleared its own gates.
//!
//! ---
//!
//! Axis 1, {existing fifo state} × {new submission kind}:
//!
//! **Same-hash resubmit**:
//!
//! - `active_hash_resubmit_returns_already_in_progress` — a `Waiting` fifo entry **blocks**
//!   same-hash replacement at `attach_responder`; the RPC returns `AlreadyInProgress` synchronously
//!   without touching the pool.
//!
//! **Different-hash, same `(sender, nonce)`**:
//!
//! - `waiting_slot_blocks_different_hash_replacement` — a `Waiting` fifo entry **blocks**
//!   different-hash replacement at the pool validator
//!   (`PreconfAwareValidator::ReplaceActivePreconf`); wire surfaces as `Err(Call { "pool rejected:
//!   cannot replace active preconf commitment..." })`. Safety-critical: prevents a malicious client
//!   from evicting an in-flight preconf commitment by submitting a low-value same-`(sender, nonce)`
//!   tx.
//! - `timeout_slot_replaceable_by_different_hash` — `Timeout` entries are reclaimable, releasing
//!   the slot; a differently-signed tx for the same slot admits and lands on chain.
//! - `canceled_slot_replaceable_by_different_hash` — symmetric to Timeout: budget-Canceled entries
//!   also release the slot.
//!
//! - `failed_slot_replaceable_by_different_hash` — symmetric to the Timeout / Canceled cases.
//!   Triggering fifo `Failed` in the integration layer needs a builder-level rejection that
//!   survives pool-validator screening; the setup here engineers a nonce race where a
//!   non-preconf-eligible tx fills sender's nonce=0 in the pool (letting the RPC-layer nonce-gap
//!   gate admit the preconf tx at nonce=1) but biased select! runs fifo dispatch BEFORE the pool
//!   arm applies nonce=0, so reth's builder sees `tx.nonce=1 > expected 0` and rejects → fifo
//!   `Failed`.

use super::helpers::{PreconfCfgBuilder, send_preconf, wait_pending_nonce};
use crate::{canonicalize_payload, launch_preconf_node, launch_preconf_node_with_classifier};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::{ClientError, client::ClientT};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> alloy_primitives::Bytes {
    signed_transfer_with(chain_id, wallet, nonce, U256::from(1u64)).await
}

/// `signed_transfer` with an explicit value, so two txs can share
/// `(sender, nonce)` while hashing differently.
async fn signed_transfer_with(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    value: U256,
) -> alloy_primitives::Bytes {
    signed_transfer_bumped(chain_id, wallet, nonce, value, 20e9 as u128).await
}

/// `signed_transfer_with` plus an explicit gas price, so a replacement can
/// clear reth's own price-bump rule and actually reach the preconf guard.
async fn signed_transfer_bumped(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    value: U256,
    fee: u128,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(fee),
        max_priority_fee_per_gas: Some(fee),
        value: Some(value),
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
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
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
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
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
                msg.contains("cannot replace active preconf") ||
                    msg.contains("replace active preconf"),
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
/// `Canceled` fifo entry (block-gas-budget pre-apply rejection, e.g. block gas budget)
/// also releases the `(sender, nonce)` slot. A differently-signed tx
/// for the same slot admits and lands on chain in the next slot.
///
/// The Canceled state is set up via the same block-gas-budget gate pattern as
/// `gas_budgets::canceled_tx_recoverable_in_next_slot`: three 21k-gas
/// txs against a 50k block cap; the third gets Canceled. Then a
/// different-hash tx replaces it after canon.
///
/// Two determinism gates keep this stable under load: `wait_pending_nonce`
/// (slot-1 nonce-gap) and `canonize!` (slot-1 canon-progress — otherwise the
/// slot-2 job re-applies the still-`Success` tx0/tx1 and exhausts the budget
/// before the replacement lands).
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

    // ── Slot 1: tx0/tx1 land, tx2 budget-rejected (Canceled) ─────────────
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

    // Submit in nonce order, waiting for each to become pending before the
    // next — otherwise the follow-up tx's nonce-gap pre-check races the pool's
    // async admission and is rejected as a gap under load.
    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    wait_pending_nonce(&http, wallet_addr, 1).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    wait_pending_nonce(&http, wallet_addr, 2).await;
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
    let err_2 =
        t2.await.expect("t2 join").expect_err("tx2 must be budget-rejected → fifo Canceled");
    assert!(
        matches!(err_2, ClientError::Call(ref e) if e.message().to_lowercase().contains("gas budget")),
        "expected BlockGasBudgetExceeded to produce fifo Canceled state; got {err_2:?}",
    );

    // Canonicalise slot 1 so on-chain nonce advances to 2 and the F1
    // budget resets. tx2's fifo entry stays in `Canceled` state
    // (nonce=2 is at the head of the sender's frontier; `forward` at
    // slot-2 prologue would clear entries with nonce < 2, leaving tx2
    // intact).
    let _new_head = canonicalize_payload!(node, payload_1).await;

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
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
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

/// Symmetric to `timeout_slot_replaceable_by_different_hash` and
/// `canceled_slot_replaceable_by_different_hash`: a `Failed` fifo entry
/// (reth-builder pre-execute rejection) also releases the
/// `(sender, nonce)` slot; a differently-signed tx for the same slot
/// admits and lands on chain.
///
/// Engineering fifo `Failed` in integration:
///
/// 1. Whitelist only `(wallet, RECIPIENT_A)`. A "shadow" non-preconf- eligible tx to `RECIPIENT_B`
///    is injected at nonce=0 via `inject_tx` (plain `eth_sendRawTransaction`). Pool admits it to
///    `Pending`, but the preconf listener filters it out — a plain submission freezes no eligible
///    verdict — so no fifo entry is created for nonce=0.
///
/// 2. A preconf tx to `RECIPIENT_A` at nonce=1 is submitted via `send_preconf`. RPC's Step-2
///    nonce-gap gate reads `get_highest_consecutive_transaction_by_sender` → returns the pending
///    nonce=0 → `pending_nonce = 1`, so tx.nonce=1 is NOT a gap and passes. Listener sees this
///    preconf-eligible tx and pushes it into the fifo (Waiting).
///
/// 3. Build loop's `biased` select! prioritises `fifo_rx.recv` over the pool arm, whose gate
///    `pool_gas_used < pool_quota` is blocked at build start because `pool_quota = 0` until the
///    first `sweep_ticker.tick()` fires (~`sweep_interval` = 200ms). So the preconf tx nonce=1 is
///    dispatched BEFORE the pool arm applies the shadow tx nonce=0. Reth's builder sees the
///    in-flight state's sender nonce is still 0, but tx.nonce=1 — nonce mismatch → the builder
///    returns Err(nonce_...) → `apply_preconf_tx` wraps as `PreconfError::BuilderRejected(...)` →
///    `apply_one_preconf` marks the fifo entry `Failed` and sends the Err to the responder.
///    Client's `send_preconf` awaits and receives `Err(Call { "builder rejected: ..." })`.
///
/// 4. After the `sweep_ticker` fires and pool arm applies the shadow tx nonce=0, the block is
///    sealed containing just that tx. Canon slot 1 → sender's on-chain nonce = 1; the `Failed` fifo
///    entry at nonce=1 survives `sync_fifo_forward_to_head` (nonce=1 is NOT < the new on-chain
///    nonce of 1).
///
/// 5. Slot 2: the client submits a **replacement** preconf tx: same sender, nonce=1, but different
///    `value` (⇒ different hash). The pool validator's
///    `PreconfAwareValidator::ReplaceActivePreconf` release-set `Timeout | Canceled | Failed`
///    admits it (drops old Failed entry, admits new). Fresh sender nonce=1 now matches; the preconf
///    tx lands on chain in block 2.
///
/// Guards the "Failed is in the replacement release set" invariant
/// end-to-end — the unit test
/// `preconf_tx_set::tests::push_conflict_after_failed_evicts_and_inserts`
/// covers the fifo-layer state transition; this test covers the wire
/// contract through validator + pool + fifo + dispatch.
///
/// **Marked `#[ignore]` because the setup is time-sensitive under
/// parallel test load**: the fifo `Failed` trigger relies on the biased
/// select! running preconf dispatch BEFORE the pool arm's first
/// `sweep_ticker` tick applies the shadow tx nonce=0. Under contention,
/// reth's canon-state propagation between slot 1 and slot 2 can also
/// lag past the 500ms sleep, leaving the state provider observing the
/// pre-block-1 snapshot when slot 2's `PayloadJob` queries it — the
/// replacement tx then sees `expected nonce = 0` at the builder and
/// fails again with `BuilderRejected("nonce 1 too high")`. In
/// isolation (`cargo test ... failed_slot_replaceable_by_different_hash
/// --nocapture`) the test passes reliably. Same flake class as
/// `weth_deposit_*` / `canon_cleanup` — root cause is reth-side canon
/// notification cadence, not a preconf-layer bug. Enabled by removing
/// the `#[ignore]` for manual regression checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "time-sensitive under parallel load; passes reliably in isolation. See doc comment."]
async fn failed_slot_replaceable_by_different_hash() {
    // Any address distinct from RECIPIENT works. `0xAA…` is unlikely
    // to collide with any Hardhat-mnemonic address in the test genesis.
    let recipient_a: Address = RECIPIENT.parse().unwrap();
    let recipient_b: Address = Address::from([0xAA; 20]);
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient_a) // recipient_b intentionally NOT whitelisted
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: engineer fifo Failed via nonce-race ───────────────────
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

    // Shadow non-preconf-eligible tx: nonce=0 to RECIPIENT_B (whitelist miss).
    let shadow_tx: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient_b)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            value: Some(U256::from(1u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };
    let shadow_hash = alloy_primitives::keccak256(&shadow_tx);
    let _admitted: alloy_primitives::B256 = node
        .rpc
        .inject_tx(shadow_tx)
        .await
        .expect("shadow tx admitted via plain sendRawTransaction");

    // Let the pool digest and the (non-)preconf listener finish its
    // filter step before submitting the preconf tx.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // Preconf tx: nonce=1 to RECIPIENT_A (whitelist hit). Different
    // value from the eventual replacement so signed hashes differ.
    let preconf_tx = signed_transfer(chain_id, &wallet, 1).await;
    let preconf_hash = alloy_primitives::keccak256(&preconf_tx);
    let http_c = http.clone();
    let preconf_task = tokio::spawn(async move { send_preconf(&http_c, preconf_tx).await });

    // Wait long enough for: preconf listener to push fifo entry (~10ms),
    // dispatch to run apply_one_preconf and fail at builder (~few ms),
    // sweep_ticker to fire (default 200ms), pool arm to apply shadow tx.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload_1 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 1")
        .expect("payload 1");

    // Preconf tx must have received Err(BuilderRejected) → fifo Failed.
    let preconf_err = preconf_task
        .await
        .expect("preconf task join")
        .expect_err("preconf tx must fail at builder (nonce race)");
    match preconf_err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("builder rejected") || msg.contains("nonce"),
                "expected BuilderRejected-flavour error (fifo Failed marker); got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // Sealed block 1: shadow tx via pool arm; preconf tx NOT present.
    let sealed_1: Vec<alloy_primitives::B256> = payload_1
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed_1.contains(&shadow_hash),
        "shadow tx must be sealed via pool arm; sealed={sealed_1:?}",
    );
    assert!(
        !sealed_1.contains(&preconf_hash),
        "failed preconf tx must NOT land in block 1; sealed={sealed_1:?}",
    );

    // Canonicalise slot 1 → sender's on-chain nonce advances to 1.
    // Sleep 500ms (longer than the 300ms baseline used elsewhere) to
    // give reth's canon-state propagation extra headroom under parallel
    // test load — otherwise `state_provider.state_by_block_hash(parent)`
    // at slot 2's PayloadJob prologue may still return the pre-block-1
    // snapshot, and the replacement tx's `nonce=1` would then be seen
    // as too high (expected=0).
    let _new_head = canonicalize_payload!(node, payload_1).await;

    // ── Slot 2: submit replacement — same (sender, nonce=1),
    //    different value ⇒ different hash. Must replace the Failed
    //    fifo entry and land on chain.
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

    let preconf_replacement: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(1),
            to: Some(TxKind::Call(recipient_a)),
            gas: Some(21_000),
            max_fee_per_gas: Some(20e9 as u128),
            max_priority_fee_per_gas: Some(20e9 as u128),
            // `value=42` distinguishes hash from the original preconf_tx
            // (`value=1` from `signed_transfer`).
            value: Some(U256::from(42u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };
    let replacement_hash = alloy_primitives::keccak256(&preconf_replacement);
    assert_ne!(
        replacement_hash, preconf_hash,
        "sanity: replacement hash must differ from the failed preconf hash"
    );

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, preconf_replacement).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_2 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_2, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");

    let event = rpc_task.await.expect("rpc join").expect("replacement must succeed");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "different-hash replacement of Failed entry must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.tx_hash, replacement_hash);

    let sealed_2: Vec<alloy_primitives::B256> = payload_2
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed_2.contains(&replacement_hash),
        "replacement of Failed entry must land in block 2; sealed={sealed_2:?}",
    );
}

/// **The state the fifo cannot see**: a transaction that owns its
/// `(sender, nonce)` while having no fifo entry at all.
///
/// Reaching it is the whole reason the guard's occupancy check is the
/// classifier's slot index rather than the fifo. The window the index exists for
/// — between `validate_transaction` claiming the slot and the pool listener
/// pushing the entry — is sub-millisecond and not reachable from a test. The
/// **retention window** is the same state held open deterministically: once a
/// commitment lands, `forward()` removes its fifo entry the moment its nonce
/// advances, but the slot is kept for `SEAL_DEPTH` persisted blocks so a reorg
/// cannot hand the nonce away.
///
/// So: get a commitment on chain, then try to take its nonce. The guard runs
/// **before** the inner validator, so a refusal here says `ReplaceActivePreconf`
/// rather than nonce-too-low — which is what makes the assertion discriminating
/// rather than merely true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_landed_commitment_still_owns_its_nonce_with_no_fifo_entry() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(5_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: a preconf commitment at nonce 0, applied and sealed.
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
        .expect("payload_id must be present when attributes are supplied");

    let tx = signed_transfer(chain_id, &wallet, 0).await;
    let http_c = http.clone();
    let committed = tokio::spawn(async move { send_preconf(&http_c, tx).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload");
    let _ = committed.await.expect("join").expect("the commitment must succeed");

    // Canonicalise it. The on-chain nonce advances to 1, so the next payload
    // job's `sync_fifo_forward_to_head` drops the entry for nonce 0 — while the
    // classifier keeps the slot for the retention window.
    let _new_head = canonicalize_payload!(node, payload).await;

    // Open the next job, which is what runs the forward.
    let attrs_2 = node.payload.next_attributes();
    let fcu_state_2 = node.current_forkchoice_state().expect("forkchoice state 2");
    let _ = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state_2, Some(attrs_2))
        .await
        .expect("FCU 2 must succeed");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── Now take the nonce. Different hash, and a fee bump so reth's own
    //    price rule cannot be the thing that refuses it.
    let replacement =
        signed_transfer_bumped(chain_id, &wallet, 0, U256::from(2u64), 40e9 as u128).await;
    let err = ClientT::request::<alloy_primitives::B256, _>(
        &http,
        "eth_sendRawTransaction",
        vec![replacement.to_string()],
    )
    .await
    .expect_err("a nonce held through the retention window must not be handed away");

    let msg = err.to_string();
    assert!(
        msg.contains("cannot replace active preconf commitment"),
        "expected ReplaceActivePreconf — nonce-too-low would mean the guard never ran; got: {msg}",
    );
}

/// **Eligibility can change mid-flight, and the guard must not care.**
///
/// The sequence a user can actually produce:
///
/// 1. `A` submits `a` through the preconf RPC while allowlisted — `a` is frozen `Eligible`, owns
///    `(A, nonce)`, and gets a fifo entry.
/// 2. Governance revokes the rule that covered `A`.
/// 3. `A` submits `b`: same nonce, higher fee. It is judged against the *new* allowlist, so its own
///    verdict is `NotEligible`.
///
/// If the guard only ran for preconf-eligible newcomers, `b` would be admitted
/// as an ordinary transaction: `a` would sit on the preconf arm (the fifo holds
/// its envelope, so it executes even though the pool evicted it) while `b` sat
/// on the pool arm, for the same nonce. Whichever arm ran first would silently
/// kill the other — `a`'s client seeing `Failed`, or `b` never landing with no
/// error at all. Which one loses depends on the builder's internal arm ordering,
/// which is not a guarantee.
///
/// op-geth cannot reach this state: its guard reads only the incumbent's preconf
/// status (`legacypool.go`, `ErrPreconfInProcess`), never the newcomer's
/// eligibility. This test pins reth to the same rule.
///
/// Discriminating power was verified by restoring the `verdict.is_preconf()`
/// gate around the slot check: `b` is then accepted and this test fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_is_refused_after_sender_leaves_the_allowlist() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(5_000)
        .build();

    let (_node, http, wallet, chain_id, classifier) =
        launch_preconf_node_with_classifier!(cfg, crate::helpers::mantle_test_chain_spec()).await;

    // 1. `a` parks a responder and takes the slot.
    let tx_a = signed_transfer(chain_id, &wallet, 0).await;
    let http_a = http.clone();
    let a = tokio::spawn(async move { send_preconf(&http_a, tx_a).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // 2. Governance revokes the rule covering `A`; `a`'s verdict stays frozen `Eligible`. A rule
    //    for a *different* sender is left in place on purpose: refusing `b` must follow from the
    //    allowlist no longer covering this sender, not from the allowlist being empty.
    classifier.update_whitelist(
        [(Address::from([0xA1; 20]), recipient)].into_iter().collect(),
        Default::default(),
        Default::default(),
    );

    // 3. `b`: same nonce, higher fee, and now judged NotEligible.
    let tx_b = signed_transfer_bumped(chain_id, &wallet, 0, U256::from(2u64), 40e9 as u128).await;
    let err = ClientT::request::<alloy_primitives::B256, _>(
        &http,
        "eth_sendRawTransaction",
        vec![tx_b.to_string()],
    )
    .await
    .expect_err("a de-whitelisted replacement must not be able to evict an in-flight commitment");

    let msg = err.to_string();
    assert!(
        msg.contains("cannot replace active preconf commitment"),
        "expected ReplaceActivePreconf, got: {msg}",
    );

    a.abort();
}

/// **A replacement that is itself rejected must not destroy the holder it was
/// allowed to reclaim.**
///
/// The guard decides "this holder is reclaimable" before the per-tx gas ceiling
/// and the inner validator have had their say. If the teardown ran at that
/// point, a replacement that then fails its own checks would leave the sender
/// with neither transaction: the holder's fifo entry gone (so its same-hash
/// retry can no longer revive it) and the replacement not admitted either.
///
/// Setup: `a` times out (a reclaimable state, so the guard would let `b`
/// through), then `b` arrives for the same `(sender, nonce)` with a gas limit
/// above `--preconf.max-gas-per-tx`, so it is rejected *after* that decision.
///
/// Asserted through the classifier rather than the wire, because that is where
/// the difference is visible: with an eager teardown `a`'s verdict is forgotten
/// and its slot handed to `b` (then released again when `b` is rejected);
/// deferred, `a` keeps both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_replacement_leaves_the_reclaimable_holder_intact() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(150)
        // `b` below asks for 500k, so it trips this ceiling.
        .max_gas_per_tx(100_000)
        .max_gas_per_block(6_000_000)
        .build();

    let (_node, http, wallet, chain_id, classifier) =
        launch_preconf_node_with_classifier!(cfg, crate::helpers::mantle_test_chain_spec()).await;

    // `a` parks and times out — reclaimable, and still owns the slot.
    let tx_a = signed_transfer(chain_id, &wallet, 0).await;
    let a_hash = alloy_primitives::keccak256(&tx_a);
    let first = send_preconf(&http, tx_a).await.expect("first call Ok");
    assert!(
        matches!(first.status, PreconfStatus::Timeout),
        "setup: `a` must reach a reclaimable state; got {:?}",
        first.status,
    );
    assert_eq!(
        classifier.slot_owner(&wallet_addr, 0),
        Some(a_hash),
        "setup: `a` must still own the slot",
    );

    // `b`: same (sender, nonce), reclaim would be allowed — but its gas limit
    // trips the per-tx ceiling, so it is rejected after that point.
    let tx_b: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(500_000),
            max_fee_per_gas: Some(40e9 as u128),
            max_priority_fee_per_gas: Some(40e9 as u128),
            value: Some(U256::from(7u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };
    let err = send_preconf(&http, tx_b).await.expect_err("`b` must trip the per-tx gas ceiling");
    let msg = err.to_string();
    assert!(
        msg.contains("preconf_max_gas_per_tx"),
        "expected the gas ceiling to be what rejects `b`, got: {msg}",
    );

    // The holder must be untouched.
    assert_eq!(
        classifier.verdict(&a_hash),
        Some(mantle_reth_preconf::Verdict::Eligible),
        "`a`'s frozen verdict must survive a failed replacement attempt",
    );
    assert_eq!(
        classifier.slot_owner(&wallet_addr, 0),
        Some(a_hash),
        "`a` must still own its slot after a failed replacement attempt",
    );
}
