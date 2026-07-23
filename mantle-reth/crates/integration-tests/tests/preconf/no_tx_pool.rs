//! `no_tx_pool=true` payload attrs semantics — chain-safety guard.
//!
//! `no_tx_pool=true` signals a **deterministic derivation build**:
//! the block must exactly reproduce what other nodes derive from L1
//! data (deposits + sequencer-batched txs only). Injecting any preconf
//! tx during such a build would diverge the block hash from the network
//! consensus and cause a safe-head fork.
//!
//! This module locks the gate: even a `PreconfSource::Replay` fifo
//! entry (which normally bypasses deadline / gas-budget gates because
//! "receipt returned → tx must land") is skipped during a
//! `no_tx_pool=true` build. The Replay SLA is upheld by delayed
//! landing on the next `no_tx_pool=false` build, NOT by forcing the
//! tx into the derivation block.

use super::helpers::{PreconfCfgBuilder, mantle_payload_attributes};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::JournalEntry;
use reth_chainspec::EthChainSpec;
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

/// Replay-sourced fifo entry (journal-restored, promised commitment) is
/// the **strongest** carrier of the "must-land" SLA — deadline and F1
/// gas-budget gates both bypass it. Verifying it is NOT applied during
/// a `no_tx_pool=true` build proves the gate wraps the entire preconf
/// pipeline (not just the RPC-sourced arm).
///
/// Steps:
/// 1. Pre-load journal with a single promised tx (nonce=0).
/// 2. Launch node → journal restore pushes the tx into the fifo as `PreconfSource::Replay` (see
///    `service_builder::start`).
/// 3. Drive a build with `no_tx_pool=true` payload attrs.
/// 4. Assert the sealed block does **not** contain the promised tx.
/// 5. Then drive a normal `no_tx_pool=false` build and assert the tx lands there — proves the entry
///    was preserved, not consumed by the gated build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_tx_pool_gates_replay_source_entry() {
    let recipient: Address = RECIPIENT.parse().unwrap();

    let chain_id = super::helpers::mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let wallet_addr = wallet.inner.address();

    let raw_tx = signed_transfer(chain_id, &wallet, 0).await;
    let tx_hash = keccak256(&raw_tx);

    // Journal with a single promised commitment. Same construction as
    // `restart_replay::journal_replay_lands_promised_tx_in_next_block`.
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-journal-notxpool-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");

    let entry = JournalEntry {
        hash: tx_hash,
        tx_rlp: raw_tx.clone().into(),
        block_height: 1,
        committed_at_ms: 0,
    };
    let mut line = serde_json::to_vec(&entry).expect("encode JournalEntry");
    line.push(b'\n');
    std::fs::write(&journal_file, &line).expect("write journal file");

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .journal_path(journal_file.clone())
        .build();

    let (mut node, _http, _wallet_launched, _launched_chain_id) = launch_preconf_node!(cfg).await;

    // ── Build 1: no_tx_pool=true (derivation-style build) ────────────
    let mut attrs = node.payload.next_attributes();
    // Force derivation semantics: pool arm disabled + preconf pipeline
    // MUST be gated. `transactions: Some(vec![])` matches the empty-
    // batch case (no L1 deposits + no batched sequencer txs in this
    // slot); the resulting block should contain only the L2 system tx
    // (`L1Block.setL1BlockValuesEcotone` or equivalent) + no user txs.
    attrs.0.no_tx_pool = Some(true);
    attrs.0.transactions = Some(vec![]);

    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");
    let payload_id = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs))
        .await
        .expect("FCU must succeed for no_tx_pool build")
        .payload_id
        .expect("payload_id present");

    // Give the build task headroom to observe / skip the fifo entry
    // (Replay-source bypasses gates so if the guard is missing it would
    // apply almost immediately).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let payload_no_tx_pool = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind no_tx_pool")
        .expect("payload no_tx_pool");

    let sealed_no_tx_pool: Vec<alloy_primitives::B256> = payload_no_tx_pool
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        !sealed_no_tx_pool.contains(&tx_hash),
        "no_tx_pool build MUST NOT include the Replay-source fifo entry \
         (would diverge from L1-derivation → safe-head fork); sealed = {sealed_no_tx_pool:?}",
    );

    // ── Build 2: normal build (no_tx_pool=false via default) ─────────
    // Must land the previously-skipped Replay entry — proves the gated
    // build did not consume it, only deferred it.
    let attrs_2 = node.payload.next_attributes();
    assert!(
        attrs_2.0.no_tx_pool.unwrap_or(false) == false,
        "sanity: default mantle_payload_attributes must produce no_tx_pool=false/None",
    );
    let fcu_state_2 = node.current_forkchoice_state().expect("forkchoice state 2");
    let payload_id_2 = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state_2, Some(attrs_2))
        .await
        .expect("FCU 2")
        .payload_id
        .expect("payload_id 2");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload_normal = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_2, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind normal")
        .expect("payload normal");

    let sealed_normal: Vec<alloy_primitives::B256> = payload_normal
        .block()
        .body()
        .transactions()
        .map(|tx| keccak256(tx.encoded_2718()))
        .collect();
    assert!(
        sealed_normal.contains(&tx_hash),
        "deferred Replay entry must land in the following normal-slot build; sealed = {sealed_normal:?}",
    );

    // Silence unused import — `mantle_payload_attributes` isn't called
    // directly but its shape is what `next_attributes()` returns.
    let _ = mantle_payload_attributes;

    let _ = std::fs::remove_dir_all(&journal_dir);
}
