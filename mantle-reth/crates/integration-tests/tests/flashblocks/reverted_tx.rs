//! A transaction that reverts still belongs in the block.
//!
//! A deliberate difference from base, which leaves reverted transactions out.
//! Nothing here decides it: the arm calls `execute_transaction`, which commits
//! whatever it ran, and never asks how it went. Inclusion is a property of the
//! call that was chosen rather than of a rule written down anywhere.
//!
//! Which is why the neighbouring call is the thing to watch.
//! `execute_transaction_with_commit_condition` takes a predicate and keeps the
//! transaction only when it returns `CommitChanges::Yes` — base's shape, and
//! one `match` away from this one. A version bump that carries the upstream arm
//! over, or an optimisation that declines to pay DA for a transaction that
//! failed, arrives as a change that reads as tidying up. Without this test
//! everything stays green while the node starts disagreeing with every other
//! Mantle node about what a block contains — and disagreement about block
//! contents is a fork.
//!
//! Rejecting a transaction *before* running it is a different matter and stays
//! available: the arm already does it for transactions over the block's limits,
//! for blobs, for deposits. What must not happen is running one and then
//! dropping it for having reverted.

use std::time::Duration;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, address, bytes, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use op_alloy_consensus::OpReceipt;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

use crate::helpers::{PreconfCfgBuilder, mantle_chain_spec_with_account};

const CHAIN_ID: u64 = 5000;

/// Where the always-reverting contract is seeded.
const ALWAYS_REVERTS: Address = address!("000000000000000000000000000000000000dead");

/// `PUSH0 PUSH0 REVERT` — revert with an empty reason, whatever the call says.
///
/// Deliberately not a contract with a condition in it: the test needs the
/// revert to be the only thing that can happen, so a mistake in the call data
/// cannot quietly turn this into a success.
const REVERT_BYTECODE: Bytes = bytes!("5f5ffd");

async fn call_the_reverting_contract(wallet: &Wallet, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(CHAIN_ID),
        nonce: Some(nonce),
        to: Some(TxKind::Call(ALWAYS_REVERTS)),
        // Comfortably more than the revert consumes, so a failure here is the
        // contract reverting rather than the transaction running out.
        gas: Some(100_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Build one block and return the payload, receipts included.
macro_rules! build_one_block {
    ($node:expr, $pause:expr) => {{
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

        tokio::time::sleep($pause).await;

        $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build")
    }};
}

/// A reverted transaction is in the sealed block, and its receipt says it
/// failed.
///
/// Both halves are load-bearing. Without the second, a bytecode or calldata
/// mistake that let the call succeed would leave this asserting only that an
/// ordinary transaction lands — which is covered many times over elsewhere, and
/// would pass while testing nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reverted_transaction_is_still_part_of_the_block() {
    use reth_node_api::BuiltPayload;

    let spec = mantle_chain_spec_with_account(CHAIN_ID, ALWAYS_REVERTS, &REVERT_BYTECODE, &[]);
    // Deliberately NOT whitelisted: an eligible sender would be skipped by the
    // pool arm and applied by the preconf one, and this test is about the arm
    // that ordinary traffic goes through. Pinned by the sibling test below.
    let cfg = PreconfCfgBuilder::new().build();

    let (mut node, _http, wallet, _chain_id) = crate::launch_preconf_node!(cfg, spec).await;

    let raw = call_the_reverting_contract(&wallet, 0).await;
    let hash = keccak256(&raw);
    node.rpc.inject_tx(raw).await.expect("the pool takes a transaction that will revert");
    // Pool validation is asynchronous; let it settle before the build starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let payload = build_one_block!(node, Duration::from_millis(400));

    let in_block: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(Bytes::from(tx.encoded_2718())))
        .collect();
    let at = in_block.iter().position(|h| *h == hash);

    // Second half first: if this did not revert, the assertion below proves
    // nothing, and saying so here is clearer than a passing test that lied.
    let executed = payload.executed_block().expect("payload carries its execution output");
    let receipts = &executed.execution_output.receipts;
    // Matched rather than read through `TxReceipt`, whose trait this crate does
    // not depend on directly; the inner `status` is the same field either way.
    let reverted = at.and_then(|at| receipts.get(at)).is_some_and(|receipt| {
        let (OpReceipt::Legacy(r) |
        OpReceipt::Eip2930(r) |
        OpReceipt::Eip1559(r) |
        OpReceipt::Eip7702(r)) = receipt
        else {
            return false;
        };
        !r.status.coerce_status()
    });
    assert!(reverted, "the transaction was supposed to revert; receipts={receipts:?}");

    assert!(
        at.is_some(),
        "a reverted transaction must stay in the block — Mantle includes them where base does not; \
         block held {in_block:?}",
    );
}
