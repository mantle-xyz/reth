//! A preconf tx whose cost — cumulative across the sender's already-pending
//! txs — exceeds the on-chain balance is rejected **synchronously** with
//! `InsufficientFunds`, rather than admitted, parked by the pool on
//! `!ENOUGH_BALANCE`, and left to block the client for the full
//! `preconf_timeout` with no commitment.
//!
//! Scenario: a sender with on-chain balance `B` submits tx0 carrying value
//! `~0.6 B` (affordable alone → pending) then tx1 carrying value `~0.6 B`.
//! tx1 passes per-tx validation (`0.6 B < B`), but its cumulative cost
//! (`~1.2 B`) exceeds `B`, so the RPC's step-2 balance pre-check
//! (`get_pending_nonce_and_cumulative_cost`) rejects it before pool
//! admission — the same "surface it synchronously" contract as the nonce-gap
//! pre-check.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::{ClientError, client::ClientT};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Sign a 21k-gas transfer to `RECIPIENT` with an explicit nonce and value.
/// Value is the lever: it dominates the tx cost (`gas * max_fee + value`), so
/// the cumulative-balance arithmetic is controlled by `value` alone.
async fn signed_transfer_value(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    value: U256,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(value),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// tx1 is rejected with `InsufficientFunds` because tx0 + tx1 together exceed
/// the sender's balance, even though each is individually affordable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cumulative_balance_shortfall_rejects_second_tx() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Long enough that tx0's in-flight request stays `Waiting` (pending in
        // the pool) throughout — if tx0 timed out and were evicted, tx1 would
        // read as a nonce gap instead of an insufficient-funds shortfall.
        .preconf_timeout_ms(10_000)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // On-chain balance `B`. Each tx carries `value ≈ 0.6 B`: affordable alone
    // (`0.6 B < B`), but two together (`1.2 B`) exceed `B`.
    let balance: U256 = http
        .request("eth_getBalance", vec![sender.to_string(), "latest".to_string()])
        .await
        .expect("eth_getBalance");
    assert!(balance > U256::ZERO, "sender must be pre-funded by genesis");
    let value_each = balance / U256::from(5) * U256::from(3); // 0.6 * B

    let tx0 = signed_transfer_value(chain_id, &wallet, 0, value_each).await;
    let tx1 = signed_transfer_value(chain_id, &wallet, 1, value_each).await;

    // tx0 in-flight: admitted to the pool's pending sub-pool (affordable, next
    // nonce) and blocks awaiting a commitment we never build. Poll until the
    // pending nonce reflects it, so tx1's step-2 pre-check sees tx0's cost as
    // already-committed rather than treating tx1 as a nonce gap.
    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });

    let mut tx0_pending = false;
    for _ in 0..40 {
        let pending_nonce: U256 = http
            .request("eth_getTransactionCount", vec![sender.to_string(), "pending".to_string()])
            .await
            .expect("eth_getTransactionCount");
        if pending_nonce == U256::from(1) {
            tx0_pending = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(tx0_pending, "tx0 must be pending before tx1 is submitted");

    // tx1: cumulative cost (tx0 + tx1 ≈ 1.2 B) exceeds `B`. Step-2 rejects
    // synchronously with `InsufficientFunds`, before pool admission — the
    // client does not wait out `preconf_timeout`.
    let err = send_preconf(&http, tx1)
        .await
        .expect_err("tx1 must be rejected for insufficient cumulative funds");
    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            // `cumulative` pins this to the preconf `InsufficientFunds` variant
            // — distinct from reth's per-tx "insufficient funds for gas ..."
            // (which tx1 would not trip, since it is affordable alone).
            assert!(
                msg.contains("insufficient funds") && msg.contains("cumulative"),
                "expected a cumulative insufficient-funds rejection, got: {}",
                e.message(),
            );
        }
        other => panic!("expected a Call error, got {other:?}"),
    }

    // tx0 was only ever pending (never committed); drop its in-flight RPC.
    t0.abort();
}
