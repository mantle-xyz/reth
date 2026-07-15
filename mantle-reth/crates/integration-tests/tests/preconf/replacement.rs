//! Replacement / resubmission semantics for preconf-eligible txs.
//!
//! Two sides of the same coin:
//!
//! - `active_hash_resubmit_returns_already_in_progress` — a `Waiting`
//!   fifo entry **blocks** same-hash replacement; the RPC returns
//!   `AlreadyInProgress` synchronously without touching the pool.
//! - `timeout_slot_replaceable_by_different_hash` — a `Timeout` fifo
//!   entry **releases** the `(sender, nonce)` slot; a differently-signed
//!   tx for the same slot admits and lands on chain via the standard
//!   preconf pipeline.

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
