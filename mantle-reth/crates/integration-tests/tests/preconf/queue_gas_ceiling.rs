//! The queue refuses a backlog it could not drain.
//!
//! Three of the four ceilings admission enforces bound what the queue costs to
//! hold — entries, bytes, per-sender slots. None of them says anything about
//! whether it can empty. A block absorbs at most `preconf_max_gas_per_block` of
//! preconf gas, so a queue holding several blocks' worth is a set of promises
//! whose tail cannot be kept: every client past the first block's worth waits
//! out `preconf_timeout` and is told nothing happened.
//!
//! `preconf_queue_gas_blocks` is how many blocks' worth we will hold. Both
//! tests set it explicitly and tightly: the default is deliberately generous,
//! because overflowing a *single* block is ordinary and has its own handler
//! (dispatch cancels, a resubmit revives next slot). What this ceiling is for
//! is the queue that can never drain, and a small number here reproduces that
//! without needing to queue tens of megagas.

use super::helpers::{PreconfCfgBuilder, send_preconf, wait_fifo_entry};
use crate::launch_preconf_node_with_fifo;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
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

/// Two 21k transfers against a 30k ceiling: the first fits, the pair does not.
///
/// No forkchoice update in between, so nothing drains the queue and the second
/// submission meets the first still sitting there. Both are well under the
/// per-tx cap, which is what makes this the queue's rule rather than the
/// transaction's — a per-tx ceiling would have let both through.
///
/// One block's worth is set explicitly rather than taken from the default:
/// the default is eight, and at eight this pair is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_transaction_is_refused_once_the_queue_holds_a_block_of_gas() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .max_gas_per_tx(25_000)
        .max_gas_per_block(30_000)
        .queue_gas_blocks(1)
        .preconf_timeout_ms(3_000)
        .build();

    let (_node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // Parked: no build is running, so this waits and holds its place in the
    // queue for the duration of the test.
    let first = signed_transfer(chain_id, &wallet, 0).await;
    let http_first = http.clone();
    let parked = tokio::spawn(async move { send_preconf(&http_first, first).await });
    wait_fifo_entry(&fifo, sender, 0).await;

    let second = signed_transfer(chain_id, &wallet, 1).await;
    let err = send_preconf(&http, second)
        .await
        .expect_err("21k more gas does not fit under a 30k ceiling");

    // Word for word what the pool answers when it has no room, because the
    // client should not have to learn two vocabularies for one condition.
    match err {
        ClientError::Call(ref e) => assert_eq!(e.message(), "txpool is full"),
        other => panic!("expected Call error, got {other:?}"),
    }

    parked.abort();
}

/// Raising the multiplier raises the ceiling with it — the same pair that was
/// refused above is admitted at two blocks' worth.
///
/// Without this, a ceiling that simply refused everything would pass the test
/// above for the wrong reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_larger_backlog_allowance_admits_the_pair_that_was_refused() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .max_gas_per_tx(25_000)
        .max_gas_per_block(30_000)
        .queue_gas_blocks(2)
        .preconf_timeout_ms(3_000)
        .build();

    let (_node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    let first = signed_transfer(chain_id, &wallet, 0).await;
    let http_first = http.clone();
    let parked_a = tokio::spawn(async move { send_preconf(&http_first, first).await });
    wait_fifo_entry(&fifo, sender, 0).await;

    let second = signed_transfer(chain_id, &wallet, 1).await;
    let http_second = http.clone();
    let parked_b = tokio::spawn(async move { send_preconf(&http_second, second).await });

    // Reaching the queue is the assertion: both are parked waiting for a build
    // that never comes, so neither call returns.
    wait_fifo_entry(&fifo, sender, 1).await;

    parked_a.abort();
    parked_b.abort();
}
