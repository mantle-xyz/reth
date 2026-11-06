//! A payload job abandoned while it waits for the build lock does nothing once
//! it gets the lock.
//!
//! Such a job may stand on a block a reorg has just taken back. Running on, it
//! would sweep the queue against that block's state and, if the block was this
//! node's, drop entries as landed in it.
//!
//! The test holds the build lock itself so the order is deterministic: the
//! abandoned job takes the lock first, then a probe of the test's own, then
//! the job that superseded it. The probe sees the queue exactly as the
//! abandoned job left it.

use std::time::Duration;

use alloy_consensus::TxEnvelope;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use super::helpers::PreconfCfgBuilder;
use crate::{canonize_built, launch_preconf_node_with_fifo};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64, value: u64) -> TxEnvelope {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(value)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await
}

/// Start a payload job on the current head and return its id.
macro_rules! start_job {
    ($node:expr) => {{
        let attrs = $node.payload.next_attributes();
        let fcu_state = $node.current_forkchoice_state().expect("forkchoice state");
        $node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(fcu_state, Some(attrs))
            .await
            .expect("FCU must succeed")
            .payload_id
            .expect("payload_id present")
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_abandoned_while_waiting_for_the_lock_leaves_the_queue_alone() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();
    let (mut node, _http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // Land the sender's nonce 0 through the pool arm. The arm stages what it
    // executes in the queue, and that entry is now stale: any build that runs
    // on this head drops it at its start.
    let landed_tx = signed_transfer(chain_id, &wallet, 0, 1).await;
    let stale: B256 = *landed_tx.tx_hash();
    node.rpc
        .inject_tx(landed_tx.encoded_2718().into())
        .await
        .expect("ordinary submission accepted");
    let id = start_job!(node);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let first = node
        .inner
        .payload_builder_handle
        .resolve_kind(id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    canonize_built!(node, first);
    assert!(fifo.contains(&stale).await, "precondition: the pool arm staged it in the queue");

    // The test holds the queue; the first job queues behind it.
    let held = fifo.lock_for_build().await;
    let _abandoned = start_job!(node);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A probe of the test's own, queued behind the first job.
    let probe = tokio::spawn({
        let fifo = fifo.clone();
        async move {
            let _guard = fifo.lock_for_build().await;
            fifo.contains(&stale).await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A second job supersedes the first, which is abandoned while it waits.
    let superseding = start_job!(node);
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(held);
    let still_queued = tokio::time::timeout(Duration::from_secs(10), probe)
        .await
        .expect("the probe gets the lock")
        .expect("probe task");
    assert!(
        still_queued,
        "the abandoned job must not have swept the queue before the probe took the lock",
    );

    // The superseding job builds normally, and its own sweep drops the entry.
    tokio::time::sleep(Duration::from_millis(300)).await;
    node.inner
        .payload_builder_handle
        .resolve_kind(superseding, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    assert!(!fifo.contains(&stale).await, "a build that runs does sweep the stale entry");
}
