//! DA-footprint gate on the preconf replay (reorg-reinject) path.
//!
//! A preconf `success` is a must-land commitment, yet the DA-footprint gate
//! is a consensus constraint that also applies to replayed entries (unlike
//! the preconf gas budget, which Replay bypasses). So a committed tx that a
//! reorg reverts and re-injects can still fail to land if the block it
//! replays into is short on DA headroom — the commitment loses to the DA
//! constraint. These tests pin that tradeoff down.
//!
//! A real reorg can't be driven here (reth's `debug_setHead` is a no-op
//! stub), but its observable end equals the restart path: the tx re-enters
//! the fifo from the journal as a `Replay` source. So, like
//! `restart_replay.rs`, we hand-write journal entries and, like
//! `da_footprint.rs`, set a tight `OpDAConfig`; the gate is source-agnostic
//! (see `apply_preconf_with_da`).
//!
//! Like `da_footprint.rs`, these assert observable behavior (tx not on
//! chain, node healthy) rather than the process-global
//! `preconf.fifo.da_rejected_total` counter, which can't be isolated in this
//! shared-process test binary.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec};
use crate::launch_preconf_node;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_preconf::JournalEntry;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_payload_builder::config::OpDAConfig;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Per-tx DA limit (bytes): above the 100-byte floor (a plain transfer
/// passes) but below a few-KB calldata tx's estimate. Same as `da_footprint.rs`.
const MAX_DA_TX_SIZE: u64 = 1_000;

/// Incompressible (keccak-derived) calldata so its fastlz DA estimate can't
/// shrink back toward the 100-byte floor.
fn incompressible_calldata(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut seed = keccak256(b"mantle-preconf-reorg-replay-da-test");
    while out.len() < len {
        seed = keccak256(seed.as_slice());
        out.extend_from_slice(seed.as_slice());
    }
    out.truncate(len);
    out
}

/// Sign a call to `RECIPIENT` with `calldata`. The caller sizes `gas` to
/// clear the preconf per-tx gas gate, leaving the DA gate as the only one
/// exercised.
async fn signed_call(
    signer: PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    calldata: Vec<u8>,
    gas: u64,
) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse::<Address>().unwrap())),
        gas: Some(gas),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(0u64)),
        input: TransactionInput::new(calldata.into()),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(signer, request).await.encoded_2718().into()
}

/// Write the entries as a JSON-Lines journal under a unique tempdir;
/// returns `(journal_file, journal_dir)`.
fn write_journal(entries: &[JournalEntry]) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-reorg-da-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&journal_dir).expect("mkdir journal_dir");
    let journal_file = journal_dir.join("preconf.journal");
    let mut buf = Vec::new();
    for entry in entries {
        let mut line = serde_json::to_vec(entry).expect("encode JournalEntry");
        line.push(b'\n');
        buf.extend_from_slice(&line);
    }
    std::fs::write(&journal_file, &buf).expect("write journal file");
    (journal_file, journal_dir)
}

/// A large-calldata Replay entry under a tight DA limit is rejected by the
/// gate: it never lands, and the node still builds the block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_over_da_limit_rejected_and_not_on_chain() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let wallet = Wallet::default().with_chain_id(chain_id);
    let sender = wallet.inner.address();

    // ~4 KB of incompressible calldata → DA estimate far over MAX_DA_TX_SIZE.
    let raw_tx =
        signed_call(wallet.inner.clone(), chain_id, 0, incompressible_calldata(4_096), 300_000)
            .await;
    let tx_hash = keccak256(&raw_tx);

    let entry =
        JournalEntry { hash: tx_hash, tx_rlp: raw_tx.clone(), block_height: 1, committed_at_ms: 0 };
    let (journal_file, journal_dir) = write_journal(&[entry]);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender)
        .whitelist_to(recipient)
        // Gas caps clear the calldata-heavy tx so it reaches the DA gate.
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(10_000_000)
        .journal_path(journal_file.clone())
        .build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, 30_000_000);
    let (mut node, _http, _wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

    // Restore pushed the entry as `Replay`; `replay_fifo_carryover` runs it
    // through `apply_preconf_with_da`, whose DA gate rejects it.
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

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A successful `resolve_kind` is itself the node-health assertion.
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        !sealed.contains(&tx_hash),
        "over-DA replay tx must NOT land despite the earlier commitment; sealed={sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// A DA rejection is per-entry, not a build abort: a rejected large-calldata
/// Replay entry ordered before a small within-limit one from another sender
/// does not stop the small one from landing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_over_da_limit_does_not_block_other_replay() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // signers[1] collides with RECIPIENT, so B uses signers[2] (see
    // `restart_replay::journal_replay_across_multiple_senders`).
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_b = signer_b.address();

    // A: over the DA limit; B: at the 100-byte floor, under the limit.
    let tx_a =
        signed_call(wallet_a.inner.clone(), chain_id, 0, incompressible_calldata(4_096), 300_000)
            .await;
    let hash_a = keccak256(&tx_a);
    let tx_b = signed_call(signer_b, chain_id, 0, Vec::new(), 21_000).await;
    let hash_b = keccak256(&tx_b);

    // A first, so B landing proves the loop continued past A's rejection.
    let entries = [
        JournalEntry { hash: hash_a, tx_rlp: tx_a.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash_b, tx_rlp: tx_b.clone(), block_height: 1, committed_at_ms: 0 },
    ];
    let (journal_file, journal_dir) = write_journal(&entries);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a)
        .whitelist_from(sender_b)
        .whitelist_to(recipient)
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(10_000_000)
        .journal_path(journal_file.clone())
        .build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, 30_000_000);
    let (mut node, _http, _wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

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

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let sealed: Vec<B256> =
        payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(
        !sealed.contains(&hash_a),
        "over-DA replay entry (sender A) must NOT land; sealed={sealed:?}",
    );
    assert!(
        sealed.contains(&hash_b),
        "within-DA replay entry (sender B) must still land after A's DA rejection; \
         sealed={sealed:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
