//! A sender's chain can total more than its balance, and the EVM is what says
//! so.
//!
//! Scenario: a sender with on-chain balance `B` submits tx0 and tx1, each
//! carrying value `~0.6 B`. Either is affordable alone (`0.6 B < B`), so the
//! validator admits both; together (`~1.2 B`) they are not. The queue takes
//! both, the block executes tx0, and tx1 is refused against the balance tx0
//! left behind — `~0.4 B`.
//!
//! ## Why the queue does not pre-empt that
//!
//! It cannot do it correctly. Summing a sender's queued costs against its
//! canonical balance is wrong in both directions: the block being built may
//! already have spent that balance (so the sum reads low) or credited it (so
//! the sum reads high and refuses a transaction that was in order). Neither is
//! visible from the queue, and journal-replayed entries carry no cost figure at
//! all. There was such a check; it was written for an earlier shape of this
//! path, where a preconf transaction went through the transaction pool and the
//! pool would park it on `!ENOUGH_BALANCE` — a parked transaction never reaching
//! `Pending` got no queue entry, and the client waited out the whole timeout for
//! nothing. The preconf path no longer goes through the pool, so there is
//! nothing to park and nothing to predict.
//!
//! What the client is told does not change: `InsufficientFunds`, with the same
//! wording, from the builder instead of from admission — see
//! `apply::BuilderRejected`.
//!
//! ## Both transactions are preconf, and that is load-bearing
//!
//! The mixed case (an *ordinary* transaction eating the balance, then a preconf
//! one) is not covered here, and not by omission: an ordinary transaction the
//! preconf queue cannot see also makes the nonce after it read as a gap, so the
//! nonce rule refuses the submission before balance is ever reached. The one
//! window where the two come apart — the ordinary transaction already executed
//! in the block being built, so the nonce has moved but the canonical balance
//! has not — is now handled by the same EVM check this test exercises.

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node_with_fifo;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
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

/// tx0 lands, tx1 is refused against what tx0 left behind, and the block
/// carries exactly one of them.
///
/// Three assertions, and each would survive the other two failing:
///
/// - **both are queued.** `wait_fifo_entry` for nonce 1 before any build runs is what a reinstated
///   admission-time balance check breaks — it refuses tx1 there and the entry never appears.
/// - **the client is told why.** The wording is the one it was told before the judgement moved, so
///   a regression in `apply::classify` shows up here as a generic `builder rejected: ...`.
/// - **the block agrees.** tx0 in, tx1 out — a refusal that still committed the transaction would
///   satisfy the error assertion and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_over_the_balance_lands_as_far_as_the_balance_reaches() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Comfortably longer than the build driven below; both requests are
        // in flight across it, and a deadline firing first would turn a refusal
        // into a timeout.
        .preconf_timeout_ms(10_000)
        .build();

    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

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
    let hash0 = keccak256(&tx0);
    let hash1 = keccak256(&tx1);

    // Queued in nonce order, both before any build opens. Waiting on tx0's
    // entry first is what makes tx1 a successor rather than a nonce gap.
    let http_c = http.clone();
    let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
    super::helpers::wait_fifo_entry(&fifo, sender, 0).await;

    let http_c = http.clone();
    let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
    super::helpers::wait_fifo_entry(&fifo, sender, 1).await;

    // Only now is there a balance to judge against: the one tx0 leaves.
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
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    t0.await.expect("rpc join").expect("tx0 is affordable on its own and must land");

    let err = t1
        .await
        .expect("rpc join")
        .expect_err("tx1 cannot be paid for out of what tx0 left behind");
    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            // `cumulative` is part of the wire wording this error has always
            // carried — kept verbatim so a client parsing it does not have to
            // care which stage refused — and it is what separates this from
            // reth's per-tx "insufficient funds for gas ...", which tx1 does
            // not trip because it is affordable alone.
            assert!(
                msg.contains("insufficient funds") && msg.contains("cumulative"),
                "expected an insufficient-funds rejection, got: {}",
                e.message(),
            );
        }
        other => panic!("expected a Call error, got {other:?}"),
    }

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed.contains(&hash0), "tx0 must be in the block; sealed = {sealed:?}");
    assert!(!sealed.contains(&hash1), "tx1 must not be; sealed = {sealed:?}");
}
