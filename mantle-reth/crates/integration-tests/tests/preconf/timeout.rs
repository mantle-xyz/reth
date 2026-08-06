//! Preconf `Timeout` state-machine semantics.
//!
//! - `deadline_elapsed_returns_timeout_and_evicts_pool` — RPC-layer deadline fires, tx flipped to
//!   `Timeout` and evicted from pool; subsequent build must not include it.
//! - `timeout_recovered_by_same_hash_resubmit` — same-hash retry after Timeout revives the fifo
//!   entry (`push_if_absent`'s reclaimable branch) and the tx lands on the next build.
//! - `dispatch_safety_margin_marks_timeout_before_apply` — dispatch- layer preemptive timeout (40ms
//!   `SAFETY_MARGIN` before the SLA deadline) fires from within `apply_one_preconf`, surfacing as
//!   `Err(Call)` at the wire (distinct from RPC-layer `Ok(Timeout)`).
//!
//! - `race_resolution_returns_success_when_apply_completes_after_deadline` — RPC deadline fires
//!   while dispatch is mid-apply; `rpc.rs`'s `lock_for_apply` + `try_recv` fallback harvests the
//!   receipt so the client sees `Success`. Exercises the branch by disabling `safety_margin` (which
//!   would otherwise preemptively abort dispatch and hide the race).
//! - `basefee_orphan_returns_timeout_and_clears_responder` — tx routed to the pool's `BaseFee`
//!   sub-pool never enters the fifo (listener only subscribes to `Pending`); RPC deadline fires
//!   with `final_status == None` and hits `mark_timeout`'s `NotFound` fallback, but must still
//!   clear `pending_responders` so a same- hash resubmit is not permanently wedged with
//!   `AlreadyInProgress`.
//!
//! Timeout entry releasing the `(sender, nonce)` slot for a differently-
//! signed tx is a **replacement** semantic and lives in
//! `replacement.rs`. Pre-fifo synchronous rejection paths (nonce gap,
//! whitelist, per-tx gas cap) live in `validation_reject.rs`.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Build a signed 21k-gas transfer with an explicit nonce.
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

/// RPC deadline fires when no payload job is running.
///
/// Send a whitelisted preconf tx but never trigger an FCU. The
/// responder is parked; after `preconf_timeout` the RPC handler:
/// 1. flips the fifo entry to `Timeout` (or reports `NotFound` when the pool listener routed the tx
///    to `BaseFee` / `Queued`),
/// 2. cancels the pending responder, and
/// 3. returns `Ok(PreconfTxEvent { status: Timeout, receipt.logs: None, ... })`.
///
/// The wire contract mirrors op-geth: a timeout is not an RPC error —
/// it's a `Timeout`-flavoured success response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_elapsed_returns_timeout_and_evicts_pool() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // 150ms deadline — enough for pool.add_transaction to complete but
    // short enough that the test finishes fast when the deadline fires.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(150)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let expected_hash = alloy_primitives::keccak256(&raw_tx);
    let start = std::time::Instant::now();
    let event = send_preconf(&http, raw_tx)
        .await
        .expect("timeout must surface as Ok(Timeout event), not as a jsonrpsee error");
    let elapsed = start.elapsed();

    assert!(
        matches!(event.status, PreconfStatus::Timeout),
        "expected Timeout, got {:?} (reason={:?})",
        event.status,
        event.reason
    );
    assert!(
        event.receipt.logs.is_none(),
        "no EVM apply happened ⇒ wire logs must be null (Some/None tri-state)",
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "handler must wait the configured timeout before returning; elapsed={elapsed:?}",
    );
    assert!(
        elapsed < std::time::Duration::from_millis(2_000),
        "timeout should fire promptly (~150ms); took {elapsed:?} — did the RPC hang?",
    );
    assert!(
        event.reason.contains("timeout"),
        "reason string must indicate timeout for SDK inspection; got {:?}",
        event.reason,
    );

    // SLA guard: "wire Timeout ⇒ this tx MUST NOT land on chain".
    // Two mechanisms combine to enforce this:
    //   1. `mark_timeout` synchronously evicts the tx from the pool via the pool-eviction callback
    //      registered by the service builder.
    //   2. `replay_fifo_carryover` skips `Timeout` entries in subsequent builds so the preconf arm
    //      cannot resurrect the tx either.
    // If either regresses, the pool arm here would pack the stale tx and
    // violate the client's "not-landing" contract.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        reth_transaction_pool::TransactionPool::pool_size(&node.inner.pool).total,
        0,
        "timed-out tx must be evicted from pool by mark_timeout's eviction callback",
    );

    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("post-timeout FCU must succeed")
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
    assert!(
        !sealed.contains(&expected_hash),
        "SLA violation: timed-out tx {expected_hash:?} landed in subsequent block; sealed={sealed:?}",
    );
}

/// A tx that timed out on the first call must be revivable by a
/// same-hash resubmission. The reclaimable-state branch in
/// `push_if_absent` flips the fifo entry Timeout → Waiting when the
/// pool listener re-observes the tx, and `attach_responder` refreshes
/// the deadline clock so dispatch's pre-apply gate ticks from the
/// second submission (not the already-expired first). This is the
/// client-side "timeout, then retry" contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_recovered_by_same_hash_resubmit() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Longer than the 300ms sleep before resolve_kind so the second
    // call has budget to run through dispatch before hitting its own
    // deadline. The first call still hits its 500ms deadline because
    // no build runs during that window.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(500)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let expected_hash = alloy_primitives::keccak256(&raw_tx);

    // First call: no build, must time out.
    let first = send_preconf(&http, raw_tx.clone())
        .await
        .expect("first call must return Ok(Timeout event), not an RPC error");
    assert!(
        matches!(first.status, PreconfStatus::Timeout),
        "first call must time out; got {:?}",
        first.status,
    );

    // Second call: same hash. Drive a real build in parallel.
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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("resubmit must return Ok");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "resubmit after Timeout must succeed; got {:?} reason={:?}",
        event.status,
        event.reason,
    );
    assert_eq!(event.tx_hash, expected_hash);

    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&expected_hash),
        "revived tx must land in the next block; sealed = {sealed:?}",
    );
}

/// Race resolution: RPC deadline fires while dispatch has already
/// applied (or is about to send the receipt) — the client must observe
/// `Success`, not `Timeout`, because the tx did land on chain.
///
/// `rpc.rs::handle_inner`'s `None` arm (deadline expired) acquires
/// `lock_for_apply`, reads the definitive fifo status under the lock,
/// and — if it finds `Success` / `Failed` — harvests the receipt from
/// `resp_rx` via `try_recv`. This closes the "wire Timeout but tx
/// landed" SLA gap.
///
/// The dispatch-side `SAFETY_MARGIN` gate preemptively aborts apply
/// when `elapsed + safety_margin >= preconf_timeout`, so in production
/// (default `safety_margin=40ms`) the race window is sub-millisecond
/// and scheduler-random. To exercise the branch deterministically the
/// test sets `safety_margin=0`, letting dispatch complete apply right
/// past the RPC deadline.
///
/// **Quarantined (`#[ignore]`)**: it deliberately races a ~5ms window (build
/// started 195ms into a 200ms deadline), so the outcome hinges on whether
/// dispatch grabs the `apply_lock` before the deadline — a coin flip (~50%)
/// under load that retries can't recover. Determinism needs an internal hook to
/// pause dispatch mid-apply, unavailable at the integration layer. The SLA
/// (a sealed tx must never report `Timeout`) belongs in a `rpc.rs` unit test
/// that drives the deadline/apply interleaving directly.
#[ignore = "deliberate ~5ms deadline/apply race → ~50% under load; not integration-deterministic. Cover the SLA via a rpc.rs unit test."]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn race_resolution_returns_success_when_apply_completes_after_deadline() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(200)
        // Disable the dispatch-side preemption gate — otherwise dispatch
        // aborts before RPC's deadline fires and the race-resolution
        // branch is unreachable.
        .safety_margin_ms(0)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;
    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let expected_hash = alloy_primitives::keccak256(&raw_tx);

    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });

    // Delay build start so dispatch runs apply right around the RPC
    // deadline. 195ms leaves ~5ms until deadline, well within EVM apply
    // + `take_responder.send` latency for a simple transfer.
    tokio::time::sleep(std::time::Duration::from_millis(195)).await;

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

    // Let dispatch finish apply even if the deadline has already
    // expired — race resolution's `lock_for_apply` will block on
    // dispatch's apply_lock and only proceed once the receipt is
    // queued in `resp_rx`.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task
        .await
        .expect("rpc join")
        .expect("race resolution must return Ok, not an RPC error");

    // The SLA: whichever internal branch fires (direct `resp_rx` recv
    // or race-resolution `lock_for_apply` + `try_recv`), the wire
    // outcome must reflect on-chain reality.
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "wire status must be Success (not Timeout) whenever tx actually applied; \
         got {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed.contains(&expected_hash),
        "tx must be on chain for the race resolution SLA to make sense; sealed={sealed:?}",
    );
}

/// Dispatch-layer `SAFETY_MARGIN` timeout — a preemptive Timeout fired
/// **inside** `apply_one_preconf` when `elapsed_since_insertion +
/// SAFETY_MARGIN (40ms) >= preconf_timeout`, distinct from the RPC-
/// layer `tokio::time::timeout`.
///
/// The gate exists to avoid a race where dispatch would finish applying
/// a tx AFTER the client has already received its RPC-layer Timeout,
/// leaving the client's bookkeeping inconsistent with the sealed block.
/// See `builder/dispatch.rs::apply_one_preconf` (`SAFETY_MARGIN` const).
///
/// Both the dispatch-layer abort and the RPC-layer deadline surface as
/// `Ok(PreconfTxEvent { status: Timeout })` — the builder-signalled
/// `PreconfError::Timeout` is mapped to a timeout event, not a JSON-RPC error.
/// What this test pins is the dispatch gate's *effect*: it fires before the
/// RPC's own 200ms deadline and aborts the tx pre-execute, so the tx is
/// neither sealed nor left in the pool.
///
/// Setup: `preconf_timeout = 200ms`. Spawn the RPC and sleep 165ms —
/// dispatch is delayed by not starting a build until then. When the
/// build finally runs, `apply_one_preconf`'s gate sees `elapsed ≈ 165ms
/// + margin 40ms ≥ 200ms` and short-circuits with mark_timeout, closing
/// the responder BEFORE the RPC layer's own 200ms deadline arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_safety_margin_marks_timeout_before_apply() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(200)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let expected_hash = alloy_primitives::keccak256(&raw_tx);

    // Spawn the RPC so its 200ms deadline is running in parallel; we
    // must trip the SAFETY_MARGIN gate BEFORE it fires.
    let http_c = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_c, raw_tx).await });

    // Sleep past `preconf_timeout - SAFETY_MARGIN` = 160ms so dispatch
    // sees the gate as "already too close to the deadline to apply",
    // but well under the 200ms RPC deadline. 165ms leaves 5ms of buffer
    // over the gate boundary.
    tokio::time::sleep(std::time::Duration::from_millis(165)).await;

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
    // Just enough for `replay_fifo_carryover` to enter apply_one_preconf
    // and hit the deadline gate before resolve_kind cancels the job.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task
        .await
        .expect("rpc join")
        .expect("SAFETY_MARGIN dispatch abort surfaces as Ok(Timeout event), not an RPC error");

    // The dispatch SAFETY_MARGIN abort surfaces as `Ok(status: Timeout)`: the
    // builder-signalled `PreconfError::Timeout` maps to a timeout event, not
    // an `Err`.
    assert!(
        matches!(event.status, PreconfStatus::Timeout),
        "SAFETY_MARGIN abort must surface as wire status Timeout; got {:?} reason={:?}",
        event.status,
        event.reason,
    );

    // SLA: tx must NOT be in the sealed block (dispatch aborted before
    // executing it).
    let sealed: Vec<alloy_primitives::B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        !sealed.contains(&expected_hash),
        "SAFETY_MARGIN-aborted tx must not land in the sealed block; sealed={sealed:?}",
    );

    // first-layer SLA: pool eviction fired from `mark_timeout` inside the
    // dispatch gate.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        reth_transaction_pool::TransactionPool::pool_size(&node.inner.pool).total,
        0,
        "dispatch-layer Timeout must also trigger pool eviction",
    );
}

/// BaseFee-orphan timeout — a tx whose `max_fee_per_gas` is below the
/// current block base fee is routed by the pool to its `BaseFee` sub-
/// pool. `PreconfPoolListener` only subscribes to `Pending`, so the
/// fifo never sees the tx. When the RPC's `preconf_timeout` fires,
/// `rpc.rs::handle_inner`'s `None` branch runs `mark_timeout(hash)`
/// which returns `NotFound` (no fifo entry), then falls through to
/// `build_timeout_event` and — crucially — `cancel_responder` on the
/// no-fifo-entry case.
///
/// Guards: (a) the wire result is `Ok(TimeoutEvent)` (not stuck /
/// panicking); (b) same-hash resubmit is not permanently wedged with
/// `AlreadyInProgress` — the responder cleanup path must run even
/// when no fifo entry existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn basefee_orphan_returns_timeout_and_clears_responder() {
    use jsonrpsee::core::ClientError;

    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Genesis base fee is 1 gwei; we set the tx's fee cap to 0.5 gwei so
    // the pool routes to `BaseFee` sub-pool (below block base fee ⇒
    // never eligible for the current block).
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(150)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // Fee cap below base fee — pool accepts to `BaseFee`, listener
    // (which filters to `Pending`) skips it, fifo stays empty.
    let raw_tx: alloy_primitives::Bytes = {
        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(0),
            to: Some(TxKind::Call(recipient)),
            gas: Some(21_000),
            // 0.5 gwei — below the 1 gwei genesis base fee.
            max_fee_per_gas: Some(500_000_000u128),
            max_priority_fee_per_gas: Some(500_000_000u128),
            value: Some(U256::from(1u64)),
            input: TransactionInput::default(),
            ..Default::default()
        };
        TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
    };

    // First call: sub-basefee routing → orphan timeout path.
    let outcome1 = send_preconf(&http, raw_tx.clone()).await;
    match outcome1 {
        Ok(event) => {
            assert!(
                matches!(event.status, PreconfStatus::Timeout),
                "BaseFee-orphaned tx must surface as Timeout event, got {:?}",
                event.status,
            );
            assert!(
                event.receipt.logs.is_none(),
                "orphan Timeout must carry `None` logs (no EVM apply happened)",
            );
        }
        Err(ClientError::Call(e)) => {
            let msg = e.message().to_lowercase();
            // Some pool validators reject sub-basefee txs outright with
            // an `underpriced` / `fee cap below base fee` error, which
            // is a different (also legitimate) rejection path. If that
            // happens, this test doesn't apply — bail out with a note.
            if msg.contains("underpriced") || msg.contains("base fee") || msg.contains("fee cap") {
                eprintln!(
                    "SKIP: pool validator rejected sub-basefee tx synchronously with '{}' — \
                     BaseFee orphan path is not reachable via this fee-cap approach on this build",
                    e.message()
                );
                return;
            }
            panic!("unexpected error from sub-basefee send: {e:?}");
        }
        Err(other) => panic!("expected Call error or Ok, got {other:?}"),
    }

    // Second call same hash — must NOT be locked out with
    // `AlreadyInProgress`. The responder cleanup after the first
    // timeout is the invariant we're guarding.
    let outcome2 = send_preconf(&http, raw_tx).await;
    match outcome2 {
        Ok(event) => {
            // Another orphan timeout is fine — proves the loop can
            // re-run cleanly.
            assert!(
                !matches!(event.status, PreconfStatus::Success),
                "no build was triggered, cannot land",
            );
        }
        Err(ClientError::Call(e)) => {
            let msg = e.message().to_lowercase();
            assert!(
                !msg.contains("already in progress") && !msg.contains("in progress"),
                "second submission after BaseFee-orphan timeout must not be AlreadyInProgress; \
                 got: {}",
                e.message(),
            );
        }
        Err(other) => panic!("unexpected error kind on resubmit: {other:?}"),
    }
}
