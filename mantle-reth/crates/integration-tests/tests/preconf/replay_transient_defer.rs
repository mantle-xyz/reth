//! Transient block-capacity **defer** on the preconf replay path.
//!
//! Distinct from `replay_da.rs`, which covers the **permanent** case (a
//! tx whose DA footprint exceeds the *per-tx* limit — it can never fit any
//! block, so the commitment is broken). Here the tx fits an *empty* block but
//! the *current* block is already too full (transient). For a `Replay`-sourced
//! commitment the builder must **defer** it — leave it `Waiting` and land it
//! in a later block — rather than mark it `Failed`. This mirrors op-geth's
//! "keep in the FIFO and retry next block" behavior (eventually included,
//! never lost).
//!
//! Two invariants are pinned:
//!   - a single transient-over Replay entry lands in the *next* block (proving defer, not reject —
//!     a rejected entry would never land);
//!   - two same-sender Replay entries where the lower nonce is deferred: the higher nonce must
//!     **cascade-defer** (not be admitted out of order and nonce-too-high fail), so both land in
//!     order in the next block.
//!
//! Like `replay_da.rs`, a real reorg can't be driven (reth's
//! `debug_setHead` is a no-op stub); the observable end equals the restart
//! path — the tx re-enters the fifo from the journal as `Replay`.

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

/// Per-tx DA limit — set high so NO tx is a *permanent* (per-tx) reject; the
/// binding constraint in these tests is the per-block limit below.
const MAX_DA_TX_SIZE: u64 = 1_000_000;

/// Per-block DA limit. Sized to sit between one and two large-calldata txs:
/// an 8 KiB incompressible-calldata tx estimates to ≈6.9 KB DA (fastlz-fjord),
/// so one fits (≤10 000) but two together (≈13.8 KB) do not — forcing the
/// second into the *transient* branch.
const MAX_DA_BLOCK_SIZE: u64 = 10_000;

/// Incompressible (keccak-derived) calldata so its fastlz DA estimate cannot
/// shrink back toward the 100-byte floor.
fn incompressible_calldata(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut seed = keccak256(b"mantle-preconf-transient-defer-test");
    while out.len() < len {
        seed = keccak256(seed.as_slice());
        out.extend_from_slice(seed.as_slice());
    }
    out.truncate(len);
    out
}

/// Sign a call to `RECIPIENT` with `calldata`, sizing `gas` to clear the
/// preconf per-tx gas gate so the DA gate is the only one exercised.
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

/// Write the entries as a JSON-Lines journal under a unique tempdir.
fn write_journal(entries: &[JournalEntry]) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-transient-defer-{}",
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

/// Build the pending payload for the current head, sleeping to let the
/// preconf carryover + select! loop run. Binds `$sealed` to the sealed tx
/// hashes and `$payload` to the built payload (for canonicalization).
macro_rules! build_block {
    ($node:expr, $sealed:ident, $payload:ident) => {
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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let $payload = $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build");
        let $sealed: Vec<B256> =
            $payload.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    };
}

/// A single Replay entry that fits an empty block but overflows the current
/// block (transient) must **defer** — not land in block 1, but land in the
/// next block after the block filler is canonicalized away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_da_replay_defers_then_lands_next_block() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // signers[1] collides with RECIPIENT; A = signers[0] (default), B uses
    // signers[2] (see `replay_da::replay_over_da_limit_does_not_block_other_replay`).
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_b = signer_b.address();

    // Both large (≈6.9 KB DA each): A alone fits the 10 KB block, A+B do not.
    let tx_a =
        signed_call(wallet_a.inner.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000)
            .await;
    let hash_a = keccak256(&tx_a);
    let tx_b = signed_call(signer_b, chain_id, 0, incompressible_calldata(8_192), 600_000).await;
    let hash_b = keccak256(&tx_b);

    // A first, so B is the one that overflows the current block.
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
        .max_gas_per_block(20_000_000)
        .journal_path(journal_file.clone())
        .build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, MAX_DA_BLOCK_SIZE);
    let (mut node, _http, _wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

    // ── Block 1: A lands, B defers (transient — current block full). ──
    build_block!(node, sealed1, payload1);
    assert!(sealed1.contains(&hash_a), "A (fits) must land in block 1; sealed={sealed1:?}");
    assert!(
        !sealed1.contains(&hash_b),
        "B (transient over-block-DA) must NOT land in block 1; sealed={sealed1:?}",
    );

    // Canonicalize block 1 so its DA budget is released for block 2.
    let new_head = node.submit_payload(payload1).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Block 2: B was kept Waiting (deferred, not Failed) → it lands. ──
    build_block!(node, sealed2, _payload2);
    assert!(
        sealed2.contains(&hash_b),
        "deferred B must land in block 2 (proving defer, not reject); sealed={sealed2:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}

/// Two same-sender Replay entries (nonce 0, 1) where the nonce-0 tx is
/// deferred (transient): the nonce-1 tx must **cascade-defer**, not be
/// admitted out of order and nonce-too-high fail. Both land, in order, in the
/// next block. Without the cascade, nonce-1 would be marked `Failed` in
/// block 1 and never land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_sender_replay_cascade_defers_together() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // S = signers[0] (default), the same-sender pair; filler X = signers[2].
    let wallet_s = Wallet::default().with_chain_id(chain_id);
    let sender_s = wallet_s.inner.address();
    let signer_x = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_x = signer_x.address();

    // Filler X (large) consumes most of the block DA so S's nonce-0 (also
    // large) overflows → deferred. S's nonce-1 is small but must inherit the
    // defer via the cascade.
    let tx_x = signed_call(signer_x, chain_id, 0, incompressible_calldata(8_192), 600_000).await;
    let hash_x = keccak256(&tx_x);
    let tx_s0 =
        signed_call(wallet_s.inner.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000)
            .await;
    let hash_s0 = keccak256(&tx_s0);
    let tx_s1 = signed_call(wallet_s.inner.clone(), chain_id, 1, Vec::new(), 21_000).await;
    let hash_s1 = keccak256(&tx_s1);

    // Order: filler first (consumes DA), then S's nonce 0, then nonce 1.
    let entries = [
        JournalEntry { hash: hash_x, tx_rlp: tx_x.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash_s0, tx_rlp: tx_s0.clone(), block_height: 1, committed_at_ms: 0 },
        JournalEntry { hash: hash_s1, tx_rlp: tx_s1.clone(), block_height: 1, committed_at_ms: 0 },
    ];
    let (journal_file, journal_dir) = write_journal(&entries);

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_s)
        .whitelist_from(sender_x)
        .whitelist_to(recipient)
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(20_000_000)
        .journal_path(journal_file.clone())
        .build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, MAX_DA_BLOCK_SIZE);
    let (mut node, _http, _wallet, _chain_id) =
        launch_preconf_node!(cfg, mantle_test_chain_spec(), da_config = da_config).await;

    // ── Block 1: filler lands; S's nonce 0 defers, nonce 1 cascade-defers. ──
    build_block!(node, sealed1, payload1);
    assert!(sealed1.contains(&hash_x), "filler must land in block 1; sealed={sealed1:?}");
    assert!(!sealed1.contains(&hash_s0), "S nonce 0 must defer in block 1; sealed={sealed1:?}");
    assert!(
        !sealed1.contains(&hash_s1),
        "S nonce 1 must CASCADE-defer (not nonce-too-high fail) in block 1; sealed={sealed1:?}",
    );

    let new_head = node.submit_payload(payload1).await.expect("submit_payload");
    node.update_forkchoice(new_head, new_head).await.expect("finalize block 1");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Block 2: both S entries kept Waiting → both land, in nonce order. ──
    build_block!(node, sealed2, _payload2);
    assert!(sealed2.contains(&hash_s0), "S nonce 0 must land in block 2; sealed={sealed2:?}");
    assert!(
        sealed2.contains(&hash_s1),
        "S nonce 1 must land in block 2 (cascade kept it Waiting, not Failed); sealed={sealed2:?}",
    );
    // Nonce order preserved: s0 before s1 in the sealed tx list.
    let pos0 = sealed2.iter().position(|h| h == &hash_s0);
    let pos1 = sealed2.iter().position(|h| h == &hash_s1);
    assert!(pos0 < pos1, "nonce 0 must be sealed before nonce 1; sealed={sealed2:?}");

    let _ = std::fs::remove_dir_all(&journal_dir);
}
