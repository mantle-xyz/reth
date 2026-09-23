//! The base-fee floor the preconf path admits against.
//!
//! A transaction whose `max_fee_per_gas` is under the base fee of the block
//! being built cannot execute in it. The transaction pool handles this by
//! *parking* — the transaction waits in the `BaseFee` sub-pool and is promoted
//! if the base fee later falls in its favour.
//!
//! Parking is wrong for a preconfirmation. A parked transaction is not a
//! candidate for the block being built, so nobody applies it and the client
//! discovers this only when `preconf_timeout` expires. The client waited the full budget to be told
//! nothing happened — and the transaction is then evicted anyway.
//!
//! So the preconf path refuses instead of parking, and does so synchronously.
//! Two things have to hold for that to be an improvement rather than a
//! reshuffle, and there is a test for each:
//!
//! - it must be **fast** — the whole point is not waiting out the timeout;
//! - the floor must **track the block being built**, not a value captured once at startup. A floor
//!   that went stale would refuse transactions that later became perfectly valid, which is the
//!   parking problem again with worse ergonomics.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::{canonicalize_payload, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::{
    core::{ClientError, client::ClientT},
    http_client::HttpClient,
};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer_with_fee(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    max_fee: u128,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(max_fee),
        max_priority_fee_per_gas: Some(max_fee),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Base fee of the chain's current head, read off the header rather than
/// assumed — the EIP-1559 parameters live in genesis and the decay per empty
/// block is not this file's business to re-derive.
async fn head_base_fee(http: &HttpClient) -> u128 {
    let block: serde_json::Value = http
        .request("eth_getBlockByNumber", (String::from("latest"), false))
        .await
        .expect("eth_getBlockByNumber");
    let raw = block
        .get("baseFeePerGas")
        .and_then(|v| v.as_str())
        .expect("post-1559 headers carry baseFeePerGas");
    u128::from_str_radix(raw.trim_start_matches("0x"), 16).expect("hex base fee")
}

/// Build and canonicalise one empty block, so the base fee decays.
macro_rules! advance_empty_block {
    ($node:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        let payload_id = $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let payload = $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build");
        canonicalize_payload!($node, payload).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }};
}

/// A transaction under the base fee is refused immediately, not parked.
///
/// This replaces the outcome pinned by
/// `timeout::basefee_orphan_returns_timeout_and_clears_responder`, which
/// asserts today's behaviour: no queue entry, client waits out
/// `preconf_timeout`, `Ok(Timeout)`.
///
/// The elapsed-time assertion is the substance of the test. Without it an
/// implementation that still parks the transaction and let it time out would
/// satisfy the error match as soon as `Timeout` were reported as an `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sub_basefee_tx_is_refused_synchronously() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Deliberately wide: a regression that parks the transaction shows up
        // as a slow failure the elapsed assertion below catches, rather than
        // as a fast pass.
        .preconf_timeout_ms(3_000)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // 0.5 gwei against a 1 gwei genesis base fee.
    let raw_tx = signed_transfer_with_fee(chain_id, &wallet, 0, 500_000_000).await;

    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("a transaction under the base fee cannot execute and must be refused");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("base fee"),
                "the refusal must name the base fee so the client knows what to raise; got: {}",
                e.message()
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "refusing instead of parking is only an improvement if it is immediate; \
         took {elapsed:?} against a 3s timeout — is the transaction still being parked?",
    );
}

/// The floor follows the chain: a transaction refused against one block's base
/// fee is admitted once the base fee falls below it.
///
/// Directly exercises "the threshold is refreshed as blocks are produced". A
/// floor captured once — at startup, or at the first build — would leave this
/// transaction refused forever.
///
/// The fee cap is derived from the observed base fee rather than hard-coded, so
/// the test does not depend on the genesis EIP-1559 parameters: one empty block
/// decays the base fee by a fixed fraction, which is always enough to cross a
/// threshold set one wei below it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_basefee_floor_follows_the_chain() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // One wei under the current floor: refused now, and comfortably above
    // what the floor becomes after a single empty block.
    let base_before = head_base_fee(&http).await;
    let fee_cap = base_before - 1;

    let raw_tx = signed_transfer_with_fee(chain_id, &wallet, 0, fee_cap).await;
    let err = send_preconf(&http, raw_tx.clone())
        .await
        .expect_err("one wei under the floor must be refused");
    assert!(
        matches!(&err, ClientError::Call(e) if e.message().to_lowercase().contains("base fee")),
        "expected a base-fee refusal, got {err:?}"
    );

    // An empty block decays the base fee.
    advance_empty_block!(node);

    let base_after = head_base_fee(&http).await;
    assert!(
        base_after <= fee_cap,
        "test setup: one empty block must bring the base fee ({base_after}) to or below \
         the fee cap ({fee_cap}); the chain's EIP-1559 parameters may have changed"
    );

    // Same transaction, unchanged bytes — only the floor moved.
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
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    rpc_task.await.expect("rpc join").expect(
        "once the base fee has fallen below the fee cap the same transaction must be \
         admitted — the floor is refreshed per build, not captured once",
    );
}
