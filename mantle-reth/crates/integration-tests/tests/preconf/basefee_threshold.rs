//! Which base fee a preconf transaction is held to, and who holds it there.
//!
//! A transaction whose `max_fee_per_gas` is under the base fee of the block it
//! executes in cannot execute in it. The question is only ever *which* block,
//! and the queue cannot answer it: between payload jobs the most recent base
//! fee it has seen belongs to the block already sealed, not to the one the
//! transaction will go into. Mantle's EIP-1559 denominator is 8, so those two
//! differ by up to 12.5% — enough to refuse a transaction that would have
//! executed perfectly well.
//!
//! So the queue holds no opinion and the EVM decides, against the block it is
//! actually building. Two things have to hold for that to be workable, and
//! there is a test for each:
//!
//! - the refusal must still **name the base fee**, so a client knows to raise its cap rather than
//!   to retry unchanged. It reaches them as `BaseFeeTooLow` either way — see
//!   `apply::BuilderRejected`.
//! - the judgement must **track the chain**: the same bytes refused against one block must be
//!   accepted once the base fee falls below their cap. A threshold captured once would refuse them
//!   forever.
//!
//! Both tests therefore drive a build. That is the cost of the change: with no
//! build open there is nobody to refuse the transaction yet, and it waits in
//! the queue until one opens.

use super::helpers::{PreconfCfgBuilder, send_preconf, wait_fifo_entry};
use crate::launch_preconf_node_with_fifo;
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

/// A transaction under the base fee is queued, then refused by the builder —
/// and the refusal names the base fee.
///
/// Both halves are the substance. The `wait_fifo_entry` before any build is
/// driven pins that the queue really did take it: a reinstated admission-time
/// threshold would refuse it there and the entry would never appear. The error
/// assertion pins that moving the judgement to the EVM did not cost the client
/// the reason — `GasPriceLessThanBasefee` carries neither figure, so a
/// regression in `apply::classify` surfaces here as a generic
/// `builder rejected: ...`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sub_basefee_tx_is_refused_by_the_builder() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Wide enough that the build below comfortably beats the client's
        // deadline; a narrow one would turn a correct refusal into a timeout.
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // 0.5 gwei against a 1 gwei genesis base fee.
    let raw_tx = signed_transfer_with_fee(chain_id, &wallet, 0, 500_000_000).await;

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });

    // Before any build: the queue took it. Nothing has judged the fee cap yet,
    // because nothing yet knows which block it would go into.
    wait_fifo_entry(&fifo, sender, 0).await;

    // Now give it a block to be judged against.
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
    let _payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let err = rpc_task
        .await
        .expect("rpc join")
        .expect_err("a transaction under the base fee cannot execute and must be refused");

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
}

/// A cap under the **head's** base fee but over the **next block's** is
/// applied, because the next block is the one that decides.
///
/// This is the case a queue-held threshold gets wrong, and the reason the
/// threshold moved. The queue can only know a base fee a build published, so
/// between jobs its newest value belongs to the block already sealed. The
/// block being built has its own, and on a chain below its gas target that one
/// is lower — by up to 12.5% at Mantle's EIP-1559 denominator of 8. Anything
/// the client bids into that gap executes perfectly well and used to be
/// refused.
///
/// Set up with no arithmetic about the decay: the head is genesis, which is
/// empty, so the block built below is guaranteed to carry a lower base fee
/// than the cap derived from it. The assertion is that the transaction lands —
/// a reinstated threshold refuses it, and the test says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_the_head_would_refuse_but_the_next_block_accepts_is_applied() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(3_000)
        .build();

    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // One wei under the head's base fee — the exact shape a threshold taken
    // from the head refuses.
    let head_base = head_base_fee(&http).await;
    let fee_cap = head_base - 1;
    let raw_tx = signed_transfer_with_fee(chain_id, &wallet, 0, fee_cap).await;

    let http_clone = http.clone();
    let rpc_task = tokio::spawn(async move { send_preconf(&http_clone, raw_tx).await });
    wait_fifo_entry(&fifo, sender, 0).await;

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

    let event = rpc_task.await.expect("rpc join").expect(
        "a cap over the base fee of the block being built must be applied, however the \
         head's own base fee compares to it",
    );

    // The premise, stated rather than assumed: the built block really does
    // carry a lower base fee than the head it extends. Without it the test
    // would pass for the wrong reason on a chain that was not decaying.
    let built_base =
        payload.block().base_fee_per_gas.expect("post-London block carries a base fee");
    assert!(
        u128::from(built_base) <= fee_cap,
        "test premise: the block being built ({built_base}) must sit at or below the cap \
         ({fee_cap}) taken from the head ({head_base}); genesis is empty, so this holds \
         unless the chain's EIP-1559 parameters changed",
    );
    assert_eq!(event.status, mantle_reth_rpc_ext::PreconfStatus::Success);
}
