//! Submit different transactions for one sender and nonce through both RPC
//! methods at the same instant. The build may choose either, but it must never
//! return a successful preconfirmation for the transaction it did not build.

use super::helpers::{
    PreconfCfgBuilder, assert_preconf_answer_matches_the_block, send_normal, send_preconf,
};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use std::{sync::Arc, time::Duration};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, value: u64) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(value)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_different_hashes_same_nonce_never_return_false_success() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .preconf_timeout_ms(4_000)
        .build();
    let (mut node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let ordinary = signed_transfer(chain_id, &wallet, 1).await;
    let preconf = signed_transfer(chain_id, &wallet, 2).await;
    let ordinary_hash = keccak256(&ordinary);
    let preconf_hash = keccak256(&preconf);
    assert_ne!(ordinary_hash, preconf_hash);

    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let ordinary_task = {
        let http = http.clone();
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            send_normal(&http, ordinary).await
        })
    };
    let preconf_task = {
        let http = http.clone();
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            send_preconf(&http, preconf).await
        })
    };
    gate.wait().await;
    assert_eq!(
        ordinary_task.await.expect("ordinary RPC task").expect("pool accepts"),
        ordinary_hash
    );

    // Both requests have entered their RPC paths before the build starts.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let attrs = node.payload.next_attributes();
    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU succeeds")
        .payload_id
        .expect("payload id");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve kind")
        .expect("built payload");
    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();

    let included =
        [ordinary_hash, preconf_hash].into_iter().filter(|hash| sealed.contains(hash)).count();
    assert_eq!(included, 1, "exactly one transaction may consume nonce 0; sealed={sealed:?}");

    // Every status, not just `Success`: the previous `if let Ok(..) && Success`
    // let a `Timeout` — and any error at all — through without a word, which
    // is the one outcome worth hearing about. The helper holds each status to
    // what `PreconfStatus` says it means.
    assert_preconf_answer_matches_the_block(preconf_task.await, preconf_hash, &sealed);
}
