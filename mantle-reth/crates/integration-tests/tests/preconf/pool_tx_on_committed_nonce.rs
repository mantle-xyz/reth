//! An ordinary pool transaction must not take a nonce an owed commitment holds.
//!
//! The two submission paths keep separate queues and the pool knows nothing
//! about preconf, so nothing on its admission path consults the commitment
//! slot index. The build loop's pool arm skips a candidate the fifo already
//! holds, but that check is by **hash** — a *different* transaction on the same
//! `(sender, nonce)` goes straight past it.
//!
//! In an ordinary build that is harmless: the carryover preamble dispatches
//! replayed commitments before the pool arm fires, so the commitment has taken
//! the nonce by the time the rival is offered and the rival fails on its own.
//! This test covers the build where that does not happen — the commitment
//! deferred for block capacity, which leaves its nonce unspent for the rest of
//! the block while the pool arm keeps admitting.
//!
//! Deferring is driven the same way `replay_transient_defer` drives it (a
//! per-block DA limit sized to fit one large-calldata transaction but not two),
//! and the commitment is brought back through the journal rather than a reorg:
//! restore's pre-pass calls `mark_promised`, so the slot is claimed exactly as
//! it would be after a reorg took the commitment's block back.

use super::helpers::{PreconfCfgBuilder, mantle_test_chain_spec};
use crate::{canonize_built, launch_preconf_node};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_preconf::JournalEntry;
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_optimism_payload_builder::config::OpDAConfig;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// High enough that no transaction here is a *permanent* per-tx reject.
const MAX_DA_TX_SIZE: u64 = 1_000_000;

/// Sized between one and two 8 KiB-calldata transactions (≈6.9 KB DA each), so
/// the second one defers. The remaining ≈3 KB is ample for the small rival.
const MAX_DA_BLOCK_SIZE: u64 = 10_000;

fn incompressible_calldata(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut seed = keccak256(b"mantle-preconf-pool-tx-on-committed-nonce");
    while out.len() < len {
        seed = keccak256(seed.as_slice());
        out.extend_from_slice(seed.as_slice());
    }
    out.truncate(len);
    out
}

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

fn write_journal(entries: &[JournalEntry]) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal_dir = std::env::temp_dir().join(format!(
        "mantle-preconf-pool-tx-on-committed-nonce-{}",
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

/// Build the pending payload for the current head, sleeping to let the preconf
/// carryover, the pool sweep tick and the select! loop all run.
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
        // Longer than the 200ms default sweep interval: the pool pacer starts
        // drained, so without at least one tick the pool arm never admits and
        // the rival would be absent for the wrong reason.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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

/// A commitment deferred for block capacity keeps its nonce: an ordinary pool
/// transaction on the same `(sender, nonce)` must be skipped for the rest of
/// that block, and the commitment must still be able to land afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pool_tx_cannot_take_the_nonce_of_a_deferred_commitment() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    // signers[1] collides with RECIPIENT; see `replay_da`.
    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_b = signer_b.address();

    // Filler — lands, and leaves the block's DA budget too small for the
    // commitment behind it.
    let tx_a =
        signed_call(wallet_a.inner.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000)
            .await;
    let hash_a = keccak256(&tx_a);

    // The commitment. A receipt for it went out in an earlier process, so
    // restore's pre-pass claims `(sender_b, 0)` before anything can be admitted
    // against it.
    let tx_b =
        signed_call(signer_b.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000).await;
    let hash_b = keccak256(&tx_b);

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

    // The rival: same sender, same nonce, a different transaction, and small
    // enough to fit the DA the commitment could not. Nothing on the pool's
    // admission path knows the nonce is spoken for, so this is accepted.
    let tx_c = signed_call(signer_b, chain_id, 0, Vec::new(), 100_000).await;
    let hash_c = keccak256(&tx_c);
    assert_ne!(hash_c, hash_b, "the rival has to be a different transaction");
    node.rpc.inject_tx(tx_c.clone()).await.expect("ordinary submission accepted");

    // ── Block 1: the filler lands, the commitment defers, the rival is offered.
    build_block!(node, sealed1, payload1);
    assert!(sealed1.contains(&hash_a), "the filler must land; sealed={sealed1:?}");
    assert!(
        !sealed1.contains(&hash_b),
        "precondition: the commitment defers on block DA; sealed={sealed1:?}",
    );
    assert!(
        !sealed1.contains(&hash_c),
        "a pool transaction must not take the nonce an owed commitment holds; sealed={sealed1:?}",
    );

    // Canonicalize so block 2 starts with a fresh DA budget.
    canonize_built!(node, payload1);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Block 2: the nonce was still free, so the commitment lands.
    build_block!(node, sealed2, _payload2);
    assert!(
        sealed2.contains(&hash_b),
        "the deferred commitment must still be able to land; sealed={sealed2:?}",
    );

    let _ = std::fs::remove_dir_all(&journal_dir);
}
