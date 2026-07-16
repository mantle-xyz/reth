//! Tx-pool *cumulative* balance reservation: the pool must reserve each pending tx's
//! L1 data fee + operator fee (`extra_balance_cost`) in its cumulative affordability
//! accounting, not just the intrinsic `cost()`. (PR #86; builds on the single-tx gate, #84.)
//!
//! A sender may cover the intrinsic cost of two pending txs yet not afford both once the
//! per-tx operator fee is reserved. Pre-#86 the pool gated only on intrinsic `cost()` and
//! admitted both; #86 reserves the overlay so the unaffordable second tx stays parked.
//!
//! Isolation: both txs are identical and both pass the single-tx admission gate; the *only*
//! variable across the two tests is the sender balance, moved by exactly one tx's operator-fee
//! reservation (`K`). So tx1's inclusion flips solely on the cumulative reservation.

use crate::helpers::{mantle_test_chain_spec, with_mantle_node};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, TxKind, U256, address, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::{core::client::ClientT, http_client::HttpClient};
use op_alloy_consensus::TxDeposit;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_node_api::TreeConfig;
use reth_optimism_node::payload::OpPayloadAttrs;
use std::{sync::Arc, time::Duration};

/// `L1Block` predeploy — recipient of the per-block L1-attributes deposit.
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

// --- Fee / balance model (all wei, all deterministic) ---
const GAS: u64 = 21_000;
const MAX_FEE_PER_GAS: u128 = 20_000_000_000; // 20 gwei
/// Intrinsic pool `cost()` per tx = `gas_limit` * `max_fee_per_gas` + value(0).
const INTRINSIC: u128 = GAS as u128 * MAX_FEE_PER_GAS; // 4.2e14
/// Flat operator fee reserved per tx. With `operator_fee_scalar = 0`, `operator_fee_charge`
/// reduces to the constant, so `extra_balance_cost == K` regardless of gas. Chosen > INTRINSIC
/// so the reservation clearly dominates the affordability decision.
const K: u128 = 1_000_000_000_000_000; // 1e15

/// Arsia L1-attributes calldata: L1 data fee zeroed (`base_fee_scalar` = `l1_base_fee` = 0) so the
/// only overlay is a flat operator fee `K` (`operator_fee_scalar = 0`, `operator_fee_constant =
/// K`).
fn arsia_l1_attributes_calldata() -> Bytes {
    let mut data = vec![0u8; 178];
    data[0..4].copy_from_slice(&hex!("49e72383")); // L1_BLOCK_ARSIA_SELECTOR
    let p = &mut data[4..]; // 174-byte arsia payload
    // p[0..4] base_fee_scalar = 0, p[32..64] l1_base_fee = 0  → L1 data fee == 0
    p[160..164].copy_from_slice(&0u32.to_be_bytes()); // operator_fee_scalar = 0 (flat)
    p[164..172].copy_from_slice(&(K as u64).to_be_bytes()); // operator_fee_constant = K
    data.into()
}

/// Encodes the per-block L1-attributes deposit as a 2718 envelope for the payload attributes.
fn l1_attributes_deposit_bytes() -> Bytes {
    TxDeposit {
        source_hash: B256::ZERO,
        from: Address::ZERO,
        to: TxKind::Call(L1_BLOCK),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: true,
        input: arsia_l1_attributes_calldata(),
        eth_value: 0,
        eth_tx_value: None,
    }
    .encoded_2718()
    .into()
}

/// Injects the operator-fee-carrying L1-attributes deposit as the first tx of every block, so the
/// pool's `l1_block_info` gains the operator-fee params and `extra_balance_cost` becomes non-zero.
fn attrs_with_operator_fee(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![l1_attributes_deposit_bytes()]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}

/// Base test genesis with `sender` funded to exactly `balance`.
fn chain_spec_with_funded_sender(
    sender: Address,
    balance: U256,
) -> Arc<reth_optimism_chainspec::OpChainSpec> {
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("assets/genesis.json")).expect("valid genesis JSON");
    genesis.alloc.insert(sender, GenesisAccount { balance, ..Default::default() });
    Arc::new(mantle_reth_chainspec::from_mantle_genesis(genesis))
}

/// A minimal value transfer with fixed gas/fee so `cost()` is exactly [`INTRINSIC`].
async fn signed_transfer(chain_id: u64, wallet: &Wallet, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(address!("0000000000000000000000000000000000000001"))),
        gas: Some(GAS),
        max_fee_per_gas: Some(MAX_FEE_PER_GAS),
        max_priority_fee_per_gas: Some(MAX_FEE_PER_GAS),
        value: Some(U256::ZERO),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Returns true once a receipt exists for `hash` (short retry to absorb head-settle lag).
async fn receipt_exists(client: &HttpClient, hash: B256) -> bool {
    for _ in 0..20 {
        let r: Option<serde_json::Value> = client
            .request("eth_getTransactionReceipt", vec![serde_json::json!(format!("{hash:#x}"))])
            .await
            .expect("receipt query");
        if r.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Boots a node whose blocks carry a flat `K` operator fee, funds the sender to `balance`, submits
/// two identical transfers (nonces 0 and 1), mines one block from the pool, and asserts tx0 is
/// always included while tx1's inclusion equals `expect_tx1_mined`.
async fn run(balance_wei: u128, expect_tx1_mined: bool) {
    reth_tracing::init_test_tracing();

    let base = mantle_test_chain_spec();
    let chain_id = base.chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let sender = wallet.inner.address();
    let chain_spec = chain_spec_with_funded_sender(sender, U256::from(balance_wei));

    with_mantle_node(
        chain_spec,
        attrs_with_operator_fee,
        TreeConfig::default(),
        move |node, client| async move {
            let mut node = node;

            // Warm-up: mine one deposit-only block so the pool's l1_block_info picks up the
            // operator-fee params before we validate the user txs (else extra_balance_cost == 0).
            let warm = node.advance_block().await.expect("mine warm-up block");
            node.sync_to(warm.block().hash()).await.expect("settle warm-up block");

            // Both txs pass the single-tx gate (each costs c_i + K <= balance).
            let raw0 = signed_transfer(chain_id, &wallet, 0).await;
            let raw1 = signed_transfer(chain_id, &wallet, 1).await;
            let h0: B256 = node.rpc.inject_tx(raw0).await.expect("tx0 admitted");
            let h1: B256 =
                node.rpc.inject_tx(raw1).await.expect("tx1 admitted (single-tx gate passes)");

            // Build a block from the pool: the best-tx iterator honours cumulative balance.
            let built = node.advance_block().await.expect("mine block from pool");
            node.sync_to(built.block().hash()).await.expect("settle built block");

            assert!(receipt_exists(&client, h0).await, "tx0 must be mined (scenario must be live)");
            let tx1_mined = receipt_exists(&client, h1).await;
            assert_eq!(
                tx1_mined, expect_tx1_mined,
                "tx1 mined = {tx1_mined}, expected {expect_tx1_mined}: with balance {balance_wei} \
                 the cumulative reservation of the per-tx operator fee (K = {K}) should gate tx1",
            );
        },
    )
    .await;
}

/// Balance covers both txs' intrinsic cost + one operator fee, but not the second — the second tx
/// must stay parked. Fails on pre-#86 code (which admits both).
#[tokio::test]
async fn cumulative_reservation_parks_unaffordable_second_tx() {
    run(2 * INTRINSIC + K, false).await;
}

/// Control: balance additionally covers the second operator fee → both txs mined. Proves tx1 is
/// otherwise includable, so its parking above is due solely to the operator-fee reservation.
#[tokio::test]
async fn cumulative_reservation_admits_both_when_operator_fee_covered() {
    run(2 * INTRINSIC + 2 * K, true).await;
}
