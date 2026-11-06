//! `--rollup.max-uncompressed-block-size` reaches the preconf builder and caps
//! preconf transactions, as it caps the pool arm.
//!
//! Each test launches the node with the cap set through `RollupArgs`, the way
//! the CLI flag arrives, so they also cover the config reaching the builder.
//!
//! - `a_preconf_tx_over_the_uncompressed_cap_is_rejected` — a transaction larger than the cap on
//!   its own is refused, and is not in the block.
//! - `a_preconf_tx_under_the_uncompressed_cap_lands` — a plain transfer under the same cap lands,
//!   so the cap is not refusing everything.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_node::args::RollupArgs;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Uncompressed block size cap (bytes): room for the block's L1-attributes
/// deposit and a plain transfer, not for a few KB of calldata.
const CAP: u64 = 2_000;

async fn signed_call(chain_id: u64, wallet: &Wallet, calldata: Vec<u8>, gas: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(0),
        to: Some(TxKind::Call(RECIPIENT.parse::<Address>().unwrap())),
        gas: Some(gas),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::new(calldata.into()),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Launch a preconf node with `CAP` set, build one block while `raw` is
/// submitted, and return the RPC outcome and the sealed transaction hashes.
async fn submit_and_build(
    raw: impl FnOnce(u64, Wallet) -> std::pin::Pin<Box<dyn std::future::Future<Output = Bytes>>>,
) -> (Result<mantle_reth_rpc_ext::PreconfTxEvent, jsonrpsee::core::ClientError>, B256, Vec<B256>) {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let sender = Wallet::default().with_chain_id(chain_id).inner.address();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(10_000_000)
        .build();
    let (mut node, http, wallet, chain_id, _classifier, _fifo, _fb) = launch_preconf_node!(
        @build cfg, mantle_test_chain_spec(),
        |svc| mantle_reth_cli::node::MantleNode::new(RollupArgs {
            max_uncompressed_block_size: Some(CAP),
            ..Default::default()
        })
        .with_preconf(svc)
    )
    .await;

    let raw_tx = raw(chain_id, wallet).await;
    let tx_hash = keccak256(&raw_tx);

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

    let rpc_task = tokio::spawn(async move { send_preconf(&http, raw_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let sealed =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();

    (rpc_task.await.expect("rpc join"), tx_hash, sealed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_over_the_uncompressed_cap_is_rejected() {
    let (outcome, tx_hash, sealed) = submit_and_build(|chain_id, wallet| {
        Box::pin(async move { signed_call(chain_id, &wallet, vec![0xab; 4_096], 300_000).await })
    })
    .await;

    match outcome {
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("uncompressed size"), "expected the size cap's error, got: {msg}");
        }
        Ok(event) => {
            panic!("expected a rejection, got Ok({:?}) reason={:?}", event.status, event.reason)
        }
    }
    assert!(!sealed.contains(&tx_hash), "it must not land; sealed={sealed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preconf_tx_under_the_uncompressed_cap_lands() {
    let (outcome, tx_hash, sealed) = submit_and_build(|chain_id, wallet| {
        Box::pin(async move { signed_call(chain_id, &wallet, Vec::new(), 21_000).await })
    })
    .await;

    let event = outcome.expect("a plain transfer fits under the cap");
    assert!(matches!(event.status, PreconfStatus::Success), "{:?}", event.reason);
    assert!(sealed.contains(&tx_hash), "it must land; sealed={sealed:?}");
}
