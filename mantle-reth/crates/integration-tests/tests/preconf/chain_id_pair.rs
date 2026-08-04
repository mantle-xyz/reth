//! Chain-id pair coverage for the preconf pipeline.
//!
//! The Mantle chain-config spec requires that any fork / fee / system-
//! parameter change is exercised against every L2 the sequencer runs on
//! — Mantle Mainnet (`5000`), Mantle Sepolia (`5003`) and Mantle Hoodi
//! (`50002`). The happy-path check below is instantiated once per L2 so
//! the same preconf logic is verified end-to-end under every id.
//!
//! Only the `chainId` differs between runs; the fork schedule, EIP-1559
//! parameters and pre-funded allocations come from the shared
//! `assets/genesis.json`, mirroring op-e2e's practice of pinning fork
//! timing to `time 0` for the whole matrix.

use super::helpers::{PreconfCfgBuilder, mantle_chain_spec_for, send_preconf};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use mantle_reth_rpc_ext::PreconfStatus;
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

/// Body of the pair-test: launch a node bound to the supplied chain id,
/// submit a whitelisted preconf tx, and assert it lands on chain in the
/// first sealed block.
macro_rules! run_pair_case {
    ($chain_id:expr) => {{
        let recipient: Address = RECIPIENT.parse().unwrap();
        let wallet_addr = Wallet::default().with_chain_id($chain_id).inner.address();

        let cfg =
            PreconfCfgBuilder::new().whitelist_from(wallet_addr).whitelist_to(recipient).build();

        let (mut node, http, wallet, chain_id) =
            launch_preconf_node!(cfg, mantle_chain_spec_for($chain_id)).await;

        assert_eq!(chain_id, $chain_id, "launched node must use the requested chain id");

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
            "chainId={} preconf status: {:?} reason={:?}",
            $chain_id,
            event.status,
            event.reason,
        );

        let sealed: Vec<B256> =
            payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
        assert!(
            sealed.contains(&event.tx_hash),
            "chainId={} preconf tx must land in block 1; sealed = {sealed:?}",
            $chain_id,
        );
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_happy_path_on_mantle_mainnet() {
    run_pair_case!(5000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_happy_path_on_mantle_sepolia() {
    run_pair_case!(5003);
}

/// Mantle Hoodi (`chain_id=50002`) preconf happy path.
///
/// Pins mantle-reth's chain-id independence for the preconf pipeline:
/// with `mantle_chain_spec_for(50002)` patched into the genesis, the
/// standard preconf flow (whitelist → attach → dispatch → seal) must
/// work identically to mainnet / sepolia — a regression that adds a
/// hardcoded chain-id check would surface here.
///
/// **Scope note**: passing this test only proves this repo's preconf
/// logic is chain-id agnostic. Full-stack Hoodi readiness additionally
/// requires `MantleHoodiUpgradeConfig` in op-geth's `params/mantle.go`
/// (tracked separately in cross-repo Gap table) and op-node config
/// wiring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_happy_path_on_mantle_hoodi() {
    run_pair_case!(50002);
}

/// Sign a 21k-gas transfer with an explicit `gas` limit. Used by the
/// gas-budget pair scenario to build txs that push over the per-block
/// preconf gas cap.
async fn signed_transfer_with_gas(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    gas_limit: u64,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(gas_limit),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Body of the gas-budget pair-test: submit three 21k-gas preconf txs
/// under a `max_gas_per_block=50_000` cap and assert the first two
/// succeed while the third is rejected with the typed block-gas-budget
/// error. Runs the same fee-model-adjacent path under whichever L2
/// chain id the caller supplies.
macro_rules! run_gas_budget_case {
    ($chain_id:expr) => {{
        let recipient: Address = RECIPIENT.parse().unwrap();
        let wallet_addr = Wallet::default().with_chain_id($chain_id).inner.address();

        let cfg = PreconfCfgBuilder::new()
            .whitelist_from(wallet_addr)
            .whitelist_to(recipient)
            .max_gas_per_tx(30_000)
            .max_gas_per_block(50_000)
            .build();

        let (mut node, http, wallet, chain_id) =
            launch_preconf_node!(cfg, mantle_chain_spec_for($chain_id)).await;
        assert_eq!(chain_id, $chain_id);

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

        let tx0 = signed_transfer_with_gas(chain_id, &wallet, 0, 21_000).await;
        let tx1 = signed_transfer_with_gas(chain_id, &wallet, 1, 21_000).await;
        let tx2 = signed_transfer_with_gas(chain_id, &wallet, 2, 21_000).await;

        let http_c = http.clone();
        let t0 = tokio::spawn(async move { send_preconf(&http_c, tx0).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let http_c = http.clone();
        let t1 = tokio::spawn(async move { send_preconf(&http_c, tx1).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let http_c = http.clone();
        let t2 = tokio::spawn(async move { send_preconf(&http_c, tx2).await });

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let _payload = node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build");

        let ev0 = t0.await.expect("t0 join").expect("tx0 must succeed");
        let ev1 = t1.await.expect("t1 join").expect("tx1 must succeed");
        let err2 = t2
            .await
            .expect("t2 join")
            .expect_err("tx2 must be rejected by the block gas budget gate");

        assert!(
            matches!(ev0.status, PreconfStatus::Success),
            "chainId={} tx0 status {:?}",
            $chain_id,
            ev0.status,
        );
        assert!(
            matches!(ev1.status, PreconfStatus::Success),
            "chainId={} tx1 status {:?}",
            $chain_id,
            ev1.status,
        );

        match err2 {
            ClientError::Call(ref e) => {
                let msg = e.message().to_lowercase();
                assert!(
                    msg.contains("block gas budget") || msg.contains("gas budget"),
                    "chainId={} unexpected error message: {}",
                    $chain_id,
                    e.message(),
                );
            }
            other => panic!("chainId={} expected Call error, got {other:?}", $chain_id),
        }
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_block_gas_budget_enforced_on_mantle_mainnet() {
    run_gas_budget_case!(5000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_block_gas_budget_enforced_on_mantle_sepolia() {
    run_gas_budget_case!(5003);
}

/// See `preconf_happy_path_on_mantle_hoodi` for scope: this pins the
/// F1 block-gas-budget path is chain-id agnostic for 50002. Full-stack
/// Hoodi readiness is tracked separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preconf_block_gas_budget_enforced_on_mantle_hoodi() {
    run_gas_budget_case!(50002);
}
