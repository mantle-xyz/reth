//! Sanity coverage for the predeploy-populated genesis fixture.
//!
//! `mantle_chain_spec_with_predeploys_for` re-uses op-chain-ops'
//! `BuildL2DeveloperGenesis` output (via the checked-in `.devnet/`
//! dump), giving tests a chain state that mirrors production — with
//! `GasPriceOracle`, `L1Block`, `SequencerFeeVault`, `L2StandardBridge`
//! and 20+ other L2 predeploys already deployed at their canonical
//! `0x4200…` addresses.
//!
//! This file verifies two things:
//! 1. A node launched against that spec is still functional end-to-end: the preconf happy-path
//!    lands a whitelisted tx in the first block.
//! 2. The `L1Block` predeploy actually has bytecode after boot — i.e. the fixture was carried through
//!    into the live in-memory state, not silently dropped by the spec-loader.

use super::helpers::{PreconfCfgBuilder, mantle_chain_spec_with_predeploys_for, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_rpc_ext::PreconfStatus;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_provider::StateProviderFactory;

/// Canonical `L1Block` predeploy address on every OP-stack chain.
const L1_BLOCK_ADDR: Address = Address::new([
    0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x15,
]);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn predeploy_genesis_boots_and_l1block_has_code() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(5000).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(recipient).build();

    let (mut node, http, wallet, chain_id) =
        launch_preconf_node!(cfg, mantle_chain_spec_with_predeploys_for(5000)).await;
    assert_eq!(chain_id, 5000);

    // Post-boot state must expose the L1Block predeploy bytecode from
    // the devnet dump — otherwise the fixture was dropped by the
    // spec-loader.
    let state = node.inner.provider.latest().expect("state provider");
    let code = state
        .account_code(&L1_BLOCK_ADDR)
        .expect("account_code lookup")
        .expect("L1Block predeploy must have code");
    assert!(
        !code.original_bytes().is_empty(),
        "L1Block bytecode must be non-empty; got {} bytes",
        code.original_bytes().len(),
    );

    // Preconf happy-path must still work — proves the extra alloc did
    // not break the payload pipeline.
    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;

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

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf must succeed");
    assert!(
        matches!(event.status, PreconfStatus::Success),
        "preconf status: {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        sealed.contains(&event.tx_hash),
        "preconf tx must land under predeploy-populated genesis; sealed = {sealed:?}",
    );
}
