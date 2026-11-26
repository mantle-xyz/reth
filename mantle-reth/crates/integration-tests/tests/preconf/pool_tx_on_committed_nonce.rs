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
//!
//! The second test is the boundary of that rule rather than another case of
//! it: a submission that is still queued has had no event go out, so the hold
//! has not started and an ordinary transaction may take the nonce. What it
//! pins is that the client is told so.

use super::helpers::{
    PreconfCfgBuilder, mantle_test_chain_spec, send_preconf, wait_fifo_entry, wait_latest_nonce,
};
use crate::{canonize_built, launch_preconf_node, launch_preconf_node_with_fifo};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use mantle_reth_preconf::{ClaimKind, JournalEntry};
use mantle_reth_rpc_ext::PreconfStatus;
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
        JournalEntry {
            hash: hash_a,
            tx_rlp: tx_a.clone(),
            block_height: 1,
            committed_at_ms: 0,
            kind: ClaimKind::Promised,
        },
        JournalEntry {
            hash: hash_b,
            tx_rlp: tx_b.clone(),
            block_height: 1,
            committed_at_ms: 0,
            kind: ClaimKind::Promised,
        },
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

/// Where the nonce hold starts, from the other side.
///
/// `tx_hash` binds from the moment the event goes out, and the apply that
/// produces that event is also what claims the slot. A submission that is only
/// *queued* has no event behind it, so an ordinary transaction on the same
/// `(sender, nonce)` is free to take it — and the sender's own wallet is the
/// one likely to: preconf transactions never enter the pool, so
/// `eth_getTransactionCount(sender, "pending")` does not count them and a
/// retry reuses the nonce.
///
/// The rule that has to hold is the other one: nothing binding may be handed
/// out for a transaction that then does not land. The client here is answered
/// with an error, not with an event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_commitment_that_loses_its_nonce_is_answered_with_an_error() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();

    let wallet_a = Wallet::default().with_chain_id(chain_id);
    let sender_a = wallet_a.inner.address();
    let signer_b = Wallet::new(3).with_chain_id(chain_id).wallet_gen()[2].clone();
    let sender_b = signer_b.address();

    // No journal entries, so no restore pre-pass: the commitment below reaches
    // the queue with nothing recorded against its slot, the state every fresh
    // submission passes through.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(sender_a)
        .whitelist_from(sender_b)
        .whitelist_to(recipient)
        .max_gas_per_tx(5_000_000)
        .max_gas_per_block(20_000_000)
        .preconf_timeout_ms(3_000)
        .build();

    let da_config = OpDAConfig::new(MAX_DA_TX_SIZE, MAX_DA_BLOCK_SIZE);
    let (mut node, http, _wallet, _chain_id, _classifier, fifo, _fb) = launch_preconf_node!(
        @build cfg, mantle_test_chain_spec(),
        |svc| mantle_reth_cli::node::MantleNode::default()
            .with_preconf(svc)
            .with_da_config(da_config)
    )
    .await;

    // Filler — lands, and leaves the block's DA budget too small for the
    // submission queued behind it, which is what holds that submission in the
    // queue long enough for the rival to be offered.
    let tx_a =
        signed_call(wallet_a.inner.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000)
            .await;
    let hash_a = keccak256(&tx_a);
    let http_a = http.clone();
    let filler_task = tokio::spawn(async move { send_preconf(&http_a, tx_a).await });
    wait_fifo_entry(&fifo, sender_a, 0).await;

    let tx_b =
        signed_call(signer_b.clone(), chain_id, 0, incompressible_calldata(8_192), 600_000).await;
    let hash_b = keccak256(&tx_b);
    let http_b = http.clone();
    let queued_task = tokio::spawn(async move { send_preconf(&http_b, tx_b).await });
    wait_fifo_entry(&fifo, sender_b, 0).await;

    // The sender's own ordinary transaction on the nonce it just queued.
    let tx_c = signed_call(signer_b, chain_id, 0, Vec::new(), 100_000).await;
    let hash_c = keccak256(&tx_c);
    assert_ne!(hash_c, hash_b, "the rival has to be a different transaction");
    node.rpc.inject_tx(tx_c).await.expect("ordinary submission accepted");

    build_block!(node, sealed1, _payload1);

    assert!(sealed1.contains(&hash_a), "the filler must land; sealed={sealed1:?}");
    assert!(
        !sealed1.contains(&hash_b),
        "precondition: the queued submission defers on block DA; sealed={sealed1:?}",
    );
    assert!(
        sealed1.contains(&hash_c),
        "precondition: the rival takes the nonce while the submission waits; sealed={sealed1:?}",
    );

    // The rule. Whatever the queue decides, the client must not be holding a
    // binding `tx_hash` for a transaction whose nonce is gone.
    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), queued_task)
        .await
        .expect("the preconf call has to answer within the deadline")
        .expect("the rpc task must not panic");
    match answer {
        Err(_) => {}
        Ok(event) => assert!(
            matches!(event.status, PreconfStatus::Timeout),
            "no binding event may go out for a transaction that cannot land; got {event:?}",
        ),
    }

    filler_task.abort();
}

/// A submission still queued when the chain spends its nonce is answered with
/// `NonceSuperseded`, by name, so the client can tell "re-sign with the next
/// nonce" apart from "try again".
///
/// The nonce is taken by a transaction forced into a derivation block. A
/// derivation build dispatches nothing from the queue, so the submission is
/// still waiting when that block becomes canonical; the next ordinary build's
/// prologue syncs the queue to the new head and finds the entry behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_submission_overtaken_by_the_chain_is_told_its_nonce_was_superseded() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let chain_id = mantle_test_chain_spec().chain().id();
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(Wallet::default().with_chain_id(chain_id).inner.address())
        .whitelist_to(recipient)
        .preconf_timeout_ms(10_000)
        .build();
    let (mut node, http, wallet, _chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;
    let sender = wallet.inner.address();

    // Queued while no build is running, so it parks as `Waiting`.
    let queued = signed_call(wallet.inner.clone(), chain_id, 0, Vec::new(), 90_000).await;
    let queued_hash = keccak256(&queued);
    let http_q = http.clone();
    let queued_task = tokio::spawn(async move { send_preconf(&http_q, queued).await });
    wait_fifo_entry(&fifo, sender, 0).await;

    // The chain spends nonce 0 on a different transaction, delivered the way
    // derived blocks are: forced in, with the pool and the queue both off.
    let rival = signed_call(wallet.inner.clone(), chain_id, 0, Vec::new(), 100_000).await;
    let rival_hash = keccak256(&rival);
    let mut attrs = node.payload.next_attributes();
    attrs.0.no_tx_pool = Some(true);
    attrs.0.transactions = Some(vec![rival]);
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
    let payload1 = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    let sealed1: Vec<B256> =
        payload1.block().body().transactions().map(|tx| keccak256(tx.encoded_2718())).collect();
    assert!(sealed1.contains(&rival_hash), "premise: the rival lands; sealed={sealed1:?}");
    assert!(!sealed1.contains(&queued_hash), "premise: a derivation build dispatches nothing");
    canonize_built!(node, payload1);
    wait_latest_nonce(&http, sender, 1).await;

    // The next ordinary build looks at the queue against the new head.
    build_block!(node, sealed2, _payload2);
    assert!(!sealed2.contains(&queued_hash), "a superseded submission cannot land");

    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), queued_task)
        .await
        .expect("the preconf call has to answer once the build has synced the queue")
        .expect("the rpc task must not panic");
    match answer {
        Err(jsonrpsee::core::ClientError::Call(e)) => assert_eq!(
            e.message(),
            "sender nonce has advanced past 0; this transaction can no longer be applied",
        ),
        other => panic!("expected the NonceSuperseded refusal, got {other:?}"),
    }
}
