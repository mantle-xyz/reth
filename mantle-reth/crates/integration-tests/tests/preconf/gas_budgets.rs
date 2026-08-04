//! Block-level preconf gas budget enforcement + Canceled-state
//! recovery.
//!
//! `apply_one_preconf`'s F1 gate (`cfg.preconf_max_gas_per_block`)
//! rejects txs whose cumulative preconf gas would exceed the budget,
//! with a typed `BlockGasBudgetExceeded { max, used, limit }`, after
//! some earlier txs have already applied in the same slot. The
//! rejected entry lands in fifo `Canceled` state — reclaimable, and
//! revivable in the next slot via `push_if_absent`'s reclaimable-
//! revive branch (mirrors Timeout recovery in `timeout.rs`).
//!
//! The per-tx ceiling (`cfg.preconf_max_gas_per_tx`, enforced by
//! `PreconfAwareValidator` before the fifo) lives in
//! `validation_reject.rs` — it's a pre-fifo rejection path, not part
//! of the dispatch-time budget accounting.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Build a signed transfer with an explicit nonce **and** gas limit.
/// `gas_limit` is exposed so per-tx-cap tests can push above the
/// configured cap without touching the transfer semantics.
async fn signed_transfer_with_gas(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    gas_limit: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(gas_limit),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Block-level preconf gas budget triggers `BlockGasBudgetExceeded`.
///
/// Setup: `max_gas_per_block = 50_000`, `max_gas_per_tx = 30_000`.
/// Submit three 21k-gas transfers (nonces 0/1/2) in the same slot:
/// - tx0: `preconf_gas_used = 0`, needs 21k → passes (used = 21k)
/// - tx1: `preconf_gas_used = 21k`, needs 21k → 42k ≤ 50k → passes (used = 42k)
/// - tx2: `preconf_gas_used = 42k`, needs 21k → 63k > 50k → **rejected**
///
/// The third RPC surfaces the typed `BlockGasBudgetExceeded` error;
/// the first two return `Ok(Success)` and land on chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_gas_budget_exceeded_rejects_third_tx() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        // Per-tx cap high enough to not shadow the block budget gate.
        .max_gas_per_tx(30_000)
        // Two transfers fit; the third goes over.
        .max_gas_per_block(50_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Start the build so the RPCs have a running payload job to dispatch into.
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

    // Pre-sign all three so submission latency does not race the
    // per-block budget accounting (which is what the assertion below
    // actually cares about).
    let tx0 = signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await;
    let tx1 = signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await;
    let tx2 = signed_transfer_with_gas(chain_id, &wallet, 2, 21_000).await;

    // Send serially so nonces enter the pool in order — `pool.add`
    // rejects gap-out nonces at admission, so parallel spawns would
    // race and could surface as `PoolRejected(nonce gap)` instead of
    // the F1 budget gate we're trying to test.
    let http_clone = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_clone, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_clone = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_clone, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_clone = http.clone();
    let t2 = tokio::spawn(async move { send_preconf(&http_clone, tx2).await });

    // Pre-compute the three hashes for post-seal SLA verification.
    let tx0_hash =
        alloy_primitives::keccak256(&signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await);
    let tx1_hash =
        alloy_primitives::keccak256(&signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await);
    let tx2_hash =
        alloy_primitives::keccak256(&signed_transfer_with_gas(chain_id, &wallet, 2, 21_000).await);

    // Give dispatch a beat to work through all three, then finalize.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind not cancelled")
        .expect("payload build must produce a sealed payload");

    let ev0 = t0.await.expect("t0 join").expect("tx0 RPC must not be an error");
    let ev1 = t1.await.expect("t1 join").expect("tx1 RPC must not be an error");
    let err2 =
        t2.await.expect("t2 join").expect_err("tx2 must be rejected by the block gas budget gate");

    assert!(
        matches!(ev0.status, PreconfStatus::Success),
        "tx0 status: {:?} reason={:?}",
        ev0.status,
        ev0.reason
    );
    assert!(
        matches!(ev1.status, PreconfStatus::Success),
        "tx1 status: {:?} reason={:?}",
        ev1.status,
        ev1.reason
    );

    match err2 {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("block gas budget") || msg.contains("gas budget"),
                "unexpected error message: {}",
                e.message(),
            );
            // The `Display` impl carries all three numeric fields for
            // SDK diagnostics — pin the block-limit + tx-limit values.
            assert!(
                e.message().contains("50000") && e.message().contains("21000"),
                "expected max=50000 and limit=21000 in error message; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // SLA guard: tx0/tx1 must land, tx2 must NOT — the per-block gas
    // budget gate marks tx2 `Canceled` (`dispatch.rs`
    // `apply_one_preconf`), which fires the fifo-layer pool-eviction
    // callback. A regression in either step would let the pool arm pack
    // the rejected tx into the block despite the client seeing an Err.
    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(sealed.contains(&tx0_hash), "tx0 must be sealed; sealed={sealed:?}");
    assert!(sealed.contains(&tx1_hash), "tx1 must be sealed; sealed={sealed:?}");
    assert!(
        !sealed.contains(&tx2_hash),
        "SLA violation: F1-gate-rejected tx2 {tx2_hash:?} must NOT land in the block; sealed={sealed:?}",
    );

    // Layer-1 SLA guard: tx2 must be gone from the pool. The
    // pool-eviction callback fires from `mark_canceled` inside
    // `apply_one_preconf`'s block-gas-budget gate; if it ever regresses,
    // this asserts before the pool arm has a chance to reseat tx2 into
    // a later block.
    assert!(
        reth_transaction_pool::TransactionPool::get(&node.inner.pool, &tx2_hash).is_none(),
        "block-gas-budget-canceled tx2 {tx2_hash:?} must be evicted from the pool",
    );
}

/// F1-gate `Canceled` state is **reclaimable** — same-hash resubmit in
/// a later slot (with a fresh gas budget) must revive the fifo entry
/// (`push_if_absent`'s Timeout | Canceled → Waiting branch) and land
/// the tx on chain. Symmetric to Timeout recovery, but the trigger is
/// the server-side F1 gate (`mark_canceled`), not the client-side
/// deadline. Guards R7/SLA-1's promise that Canceled is not terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_tx_recoverable_in_next_slot() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Same shape as `block_gas_budget_exceeded_rejects_third_tx`, but
    // with a longer client timeout so the second-slot retry has budget
    // to complete.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(30_000)
        .max_gas_per_block(50_000)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // ── Slot 1: tx0/tx1 land, tx2 rejected (Canceled) ────────────────
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

    let tx0 = signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await;
    let tx1 = signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await;
    let tx2 = signed_transfer_with_gas(chain_id, &wallet, 2, 21_000).await;
    let tx2_hash = alloy_primitives::keccak256(&tx2);

    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let tx2_first_send = tx2.clone();
    let t2_first = tokio::spawn(async move { send_preconf(&http_c, tx2_first_send).await });

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
    let err2 = t2_first.await.expect("t2 join").expect_err("tx2 must be F1-rejected in slot 1");
    match err2 {
        ClientError::Call(ref e) => {
            assert!(
                e.message().to_lowercase().contains("gas budget"),
                "expected BlockGasBudgetExceeded; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // Canonicalise slot 1 so on-chain nonce advances to 2 and the F1
    // budget resets for slot 2.
    let new_head = node.submit_payload(payload_1).await.expect("submit slot 1");
    node.update_forkchoice(new_head, new_head).await.expect("canon slot 1");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── Slot 2: same-hash tx2 must succeed ───────────────────────────
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

    let http_c = http.clone();
    let tx2_second_send = tx2.clone();
    let t2_second = tokio::spawn(async move { send_preconf(&http_c, tx2_second_send).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_2 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_2, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind 2")
        .expect("payload 2");

    let event2 = t2_second.await.expect("t2 second join").expect("tx2 retry must return Ok");
    assert!(
        matches!(event2.status, PreconfStatus::Success),
        "Canceled tx must be revivable in the next slot; got {:?} reason={:?}",
        event2.status,
        event2.reason,
    );
    assert_eq!(event2.tx_hash, tx2_hash);

    let sealed_2: Vec<alloy_primitives::B256> = payload_2
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed_2.contains(&tx2_hash),
        "revived Canceled tx must land in block 2; sealed = {sealed_2:?}",
    );
}

/// F1 block-gas-budget accounting is **per-slot cumulative**, not per-sender.
///
/// Three distinct whitelisted senders (Hardhat[0], [2], [3]; index 1 is
/// `RECIPIENT`) each submit one 21k-gas transfer against a
/// `max_gas_per_block = 50_000` cap. Sender A / B land (cumulative 21k → 42k);
/// sender C would push cumulative to 63k and is F1-rejected with
/// `BlockGasBudgetExceeded`.
///
/// Companion to `block_gas_budget_exceeded_rejects_third_tx` (same sender,
/// EVM nonce order forces apply order): this variant guards against a
/// regression that would scope `preconf_gas_used` per-sender rather than
/// per-slot — under such a regression, all three txs would land and blow
/// the SLA.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_gas_budget_rejects_third_sender_in_same_slot() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    // Addresses are chain_id-independent; placeholder here is fine.
    let addr_signers = Wallet::new(4).with_chain_id(1).wallet_gen();
    let sender_a_addr = addr_signers[0].address();
    let sender_b_addr = addr_signers[2].address();
    let sender_c_addr = addr_signers[3].address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a_addr)
        .whitelist_from(sender_b_addr)
        .whitelist_from(sender_c_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(30_000)
        .max_gas_per_block(50_000)
        .build();

    let (mut node, http, wallet_a, chain_id) = launch_preconf_node!(cfg).await;
    assert_eq!(wallet_a.inner.address(), sender_a_addr);

    // Chain-id-bound signers for hand-signing B and C (Wallet has private
    // fields so the returned handles must be reused via `wallet_gen`).
    let signers = Wallet::new(4).with_chain_id(chain_id).wallet_gen();
    let signer_b = signers[2].clone();
    let signer_c = signers[3].clone();

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

    let tx_a = signed_transfer_with_gas(chain_id, &wallet_a, 0, 21_000).await;
    // Inline sign for B and C — the `Wallet` handle has private fields
    // and can't be constructed for arbitrary sender indices, so we drop
    // to the raw signer as `multi_sender_land_in_one_block` does.
    let make_request = || TransactionRequest {
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
    let tx_b: alloy_primitives::Bytes =
        TransactionTestContext::sign_tx(signer_b, make_request()).await.encoded_2718().into();
    let tx_c: alloy_primitives::Bytes =
        TransactionTestContext::sign_tx(signer_c, make_request()).await.encoded_2718().into();

    let tx_a_hash = alloy_primitives::keccak256(&tx_a);
    let tx_b_hash = alloy_primitives::keccak256(&tx_b);
    let tx_c_hash = alloy_primitives::keccak256(&tx_c);

    // Serial submit with 30ms gaps preserves fifo order across senders so
    // that C is guaranteed to be the third entry the dispatch loop sees.
    let http_c = http.clone();
    let ta = tokio::spawn(async move { send_preconf(&http_c, tx_a).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let tb = tokio::spawn(async move { send_preconf(&http_c, tx_b).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let tc = tokio::spawn(async move { send_preconf(&http_c, tx_c).await });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let ev_a = ta.await.expect("ta join").expect("sender A RPC must not err");
    let ev_b = tb.await.expect("tb join").expect("sender B RPC must not err");
    let err_c =
        tc.await.expect("tc join").expect_err("sender C tx must trip the block gas budget gate");

    assert!(matches!(ev_a.status, PreconfStatus::Success), "sender A: {:?}", ev_a.reason);
    assert!(matches!(ev_b.status, PreconfStatus::Success), "sender B: {:?}", ev_b.reason);
    match err_c {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("gas budget"),
                "sender C unexpected error message: {}",
                e.message(),
            );
            assert!(
                e.message().contains("50000") && e.message().contains("21000"),
                "expected max=50000 and limit=21000 in error message; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(sealed.contains(&tx_a_hash), "sender A tx must be sealed; sealed={sealed:?}");
    assert!(sealed.contains(&tx_b_hash), "sender B tx must be sealed; sealed={sealed:?}");
    assert!(
        !sealed.contains(&tx_c_hash),
        "sender C tx must NOT be sealed after F1 rejection; sealed={sealed:?}",
    );
    assert!(
        reth_transaction_pool::TransactionPool::get(&node.inner.pool, &tx_c_hash).is_none(),
        "F1-canceled sender C tx must be evicted from pool",
    );
}

/// Same-slot resubmit of an F1-rejected tx: `dispatch.rs::apply_one_preconf`
/// dedups on `loop_state.excluded_reason(&hash)` before re-running any
/// gate, but **forwards the stored rejection reason** to any responder
/// attached by the second submission. The revived fifo entry (Canceled
/// → Waiting via `push_if_absent`) never reaches the F1 gate a second
/// time, yet the client sees a consistent error rather than a slow
/// `Ok(Timeout)`.
///
/// Wire contract pinned by this test:
///
///   1st call → `Err(BlockGasBudgetExceeded)` (dispatch F1 gate, fast)
///   2nd call same slot → **same** `Err(BlockGasBudgetExceeded)` (dedup
///     forwards the stored reason, also fast — well under `preconf_timeout`)
///
/// Cross-slot recovery (canonicalise then resubmit, budget resets) is
/// covered by `canceled_tx_recoverable_in_next_slot`.
///
/// Regression risks this guards:
///  - `LoopState::excluded` reverting from `HashMap<TxHash, PreconfError>` back to a plain
///    `HashSet<TxHash>` — the dedup branch would then have no reason to forward and the client
///    would fall back to waiting the full `preconf_timeout`.
///  - The dedup branch dropping its `cancel_responder(..., reason)` call and just `return`ing —
///    same slow-Timeout regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceled_tx_same_slot_resubmit_forwards_same_error() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Long-ish timeout so any regression that falls back to the RPC-layer
    // deadline path is unambiguously slower than the fast-forward path.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(30_000)
        .max_gas_per_block(50_000)
        .preconf_timeout_ms(1_500)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

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

    let tx0 = signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await;
    let tx1 = signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await;
    let tx2 = signed_transfer_with_gas(chain_id, &wallet, 2, 21_000).await;
    let tx2_hash = alloy_primitives::keccak256(&tx2);

    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let http_c = http.clone();
    let tx2_first = tx2.clone();
    let t2_first = tokio::spawn(async move { send_preconf(&http_c, tx2_first).await });

    // First submit: dispatch F1 fires → Err(BlockGasBudgetExceeded).
    let _ = t0.await.expect("t0 join").expect("tx0 must succeed");
    let _ = t1.await.expect("t1 join").expect("tx1 must succeed");
    let err_first =
        t2_first.await.expect("t2 first join").expect_err("first tx2 submit must be F1-rejected");
    let first_message = match err_first {
        ClientError::Call(ref e) => {
            assert!(
                e.message().to_lowercase().contains("gas budget"),
                "expected BlockGasBudgetExceeded on first submit; got {}",
                e.message(),
            );
            e.message().to_string()
        }
        other => panic!("expected Call error on first submit, got {other:?}"),
    };

    // Same-slot resubmit — dedup forwards the stored F1 reason via
    // `cancel_responder`, so the client sees the SAME error, fast.
    let start = std::time::Instant::now();
    let err_second = send_preconf(&http, tx2)
        .await
        .expect_err("same-slot resubmit must surface the stored BlockGasBudgetExceeded");
    let elapsed = start.elapsed();

    match err_second {
        ClientError::Call(ref e) => {
            assert!(
                e.message().to_lowercase().contains("gas budget"),
                "same-slot resubmit must forward BlockGasBudgetExceeded; got {}",
                e.message(),
            );
            // The stored reason carries the same numeric fields as the
            // first-submit error — clients relying on the `max`/`used`/
            // `limit` values must see identical values across submits.
            assert_eq!(
                e.message(),
                first_message,
                "second submit error text must match the first submit exactly",
            );
        }
        other => panic!("expected Call error on second submit, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "second submit must fast-forward the stored reason, not wait preconf_timeout; \
         elapsed={elapsed:?}",
    );

    // Close the build and verify tx2 never lands in this slot.
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        !sealed.contains(&tx2_hash),
        "F1-rejected tx must never land in its own slot, even after resubmit; sealed={sealed:?}",
    );
}

/// Journal-replay-sourced fifo entries **bypass** the F1 block-gas-budget
/// gate — required by the SLA "once a receipt was returned to the client,
/// the tx must land on chain, even across a restart".
///
/// The bypass is a single guard in `apply_one_preconf`
/// (`dispatch.rs:246`, `if is_rpc && used + limit > max`). If someone
/// accidentally deletes the `is_rpc &&` half, replay txs would silently
/// be rejected under budget pressure and the SLA would break. The other
/// available regression test (`journal_replay_lands_promised_tx_in_next_block`)
/// only exercises a single replay tx that is well under budget, so it
/// would still pass under such a regression.
///
/// Construction: hand-write a JSON-Lines journal with **two** consecutive
/// replay commitments from the same sender (nonces 0 and 1, 21k gas
/// each). Configure `max_gas_per_block = 30_000` — after the first
/// replay applies, cumulative preconf gas is 21k; the second replay
/// would trip `21k + 21k > 30k` if F1 applied to it. Bypass semantics
/// require both txs to land in the first block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_source_bypasses_block_gas_budget() {
    use super::helpers::mantle_test_chain_spec;
    use mantle_reth_preconf::JournalEntry;
    use reth_chainspec::EthChainSpec;

    let recipient: Address = RECIPIENT.parse().unwrap();

    // Chain-id-bound signing must match what the launched node accepts;
    // derive it from the same helper the launch macro uses.
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    let tx0 = signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await;
    let tx1 = signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await;
    let tx0_hash = alloy_primitives::keccak256(&tx0);
    let tx1_hash = alloy_primitives::keccak256(&tx1);

    // JSON-Lines journal, one `JournalEntry` per line. Mirrors the
    // construction in `restart_replay.rs`.
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");

    let entries = [
        JournalEntry {
            hash: tx0_hash,
            tx_rlp: tx0.clone(),
            block_height: 1,
            committed_at_ms: 0,
        },
        JournalEntry {
            hash: tx1_hash,
            tx_rlp: tx1.clone(),
            block_height: 1,
            committed_at_ms: 0,
        },
    ];
    let mut buf = Vec::new();
    for entry in &entries {
        let mut line = serde_json::to_vec(entry).expect("encode JournalEntry");
        line.push(b'\n');
        buf.extend_from_slice(&line);
    }
    std::fs::write(&journal_file, &buf).expect("write journal file");

    // Per-tx cap = 21k (each replay tx equals but does not exceed).
    // Per-block cap = 30k — cumulative 42k would exceed F1 if the gate
    // applied to Replay-sourced entries. Bypass required for both to land.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(21_000)
        .max_gas_per_block(30_000)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _wallet_launched, _launched_chain_id) = launch_preconf_node!(cfg).await;

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

    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();

    assert!(sealed.contains(&tx0_hash), "replay tx0 must land; sealed={sealed:?}",);
    assert!(
        sealed.contains(&tx1_hash),
        "replay tx1 must land despite cumulative preconf gas (42k) exceeding \
         max_gas_per_block (30k) — Replay source is required to bypass F1; \
         sealed={sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
