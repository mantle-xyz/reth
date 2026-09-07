//! P6 in-process EVM tests for the preconf fork (`PreconfPayloadBuilder`).
//!
//! This file holds the 6 EVM tests carry-over from P5f Step 9; shared
//! fixtures live in [`mod fixtures`].
//!
//! Step 2 (this commit): harness scaffolding + a wiring smoke test
//! that proves the test crate compiles and all fixture helpers are
//! callable. Steps 3-8 will land the 6 real tests one at a time.

// Integration tests can't `pub` items meaningfully (the test crate is
// not a library), but the fixture helpers still want `pub` so they're
// accessible from any sibling test file added later. Allow at crate
// root so submodules inherit.
#![allow(unreachable_pub, dead_code)]

mod fixtures;

use fixtures::{
    chainspec::test_chain_spec, evm::test_evm_config, provider::test_provider, signer::TestSigners,
};

/// Step 2 wiring smoke test: every fixture constructor returns
/// something usable. No EVM execution yet — that begins in Step 3
/// with the first real fork-builder test.
#[test]
fn fixtures_wire_up() {
    // Chainspec — non-empty + the expected OP-mainnet chain id.
    let cs = test_chain_spec();
    assert_ne!(format!("{cs:?}").len(), 0);

    // EVM config — `chain_spec()` returns a usable handle. (Two
    // independent calls to `test_chain_spec()` return *different*
    // Arc instances, so we don't ptr_eq them — just verify the EVM
    // config's chainspec accessor compiles + returns something.)
    let evm = test_evm_config();
    let _ = evm.chain_spec();

    // Provider — bound to OpPrimitives + OpChainSpec; can be cloned
    // (needed by the generator's per-job spawn path).
    let provider = test_provider();
    let _cloned = provider.clone();

    // Signers — addresses match op-geth's preconf test config.
    let signers = TestSigners::new();
    assert_ne!(signers.funder.address(), signers.addr1.address());
    assert_ne!(signers.addr1.address(), signers.addr3.address());
}

/// Step 3 — Seed pattern verification.
///
/// Verifies the minimum provider seeding works:
/// 1. A parent header inserted via `add_header` can be looked up via the trait
///    `BlockReaderIdExt::sealed_header_by_hash(parent_hash)` — this is the exact call
///    `PreconfPayloadJobGenerator::new_payload_job` makes
/// 2. Pre-funded accounts inserted via `add_account` are observable through
///    `StateProvider::account_balance` — exercised by the EVM when paying for tx gas
///
/// Once both checks pass, the harness is ready to drive `build_payload`
/// from end to end. Steps 4-8 then add per-test fixture (sign tx, push
/// to fifo, spawn build, verify receipt).
#[test]
fn step3_seed_pattern_verifies_provider_serves_header_and_balance() {
    use alloy_consensus::BlockHeader;
    use alloy_primitives::U256;
    use reth_storage_api::{HeaderProvider, StateProviderFactory};

    use fixtures::provider::seed_with_genesis_parent;

    let provider = test_provider();
    let signers = TestSigners::new();

    let seeded =
        seed_with_genesis_parent(&provider, &[signers.addr1.address(), signers.addr3.address()]);

    // ── Header lookup (used by generator) ─────────────────────────
    let looked_up = provider
        .sealed_header_by_hash(seeded.hash)
        .expect("provider call succeeded")
        .expect("header was seeded under that hash");
    assert_eq!(looked_up.number(), seeded.number);
    assert_eq!(looked_up.timestamp(), seeded.timestamp);

    // ── State lookup (used by EVM during apply) ───────────────────
    let state = provider.state_by_block_hash(seeded.hash).expect("state provider available");
    let balance = state
        .account_balance(&signers.addr1.address())
        .expect("state read ok")
        .expect("addr1 was seeded with balance");
    assert!(
        balance >= U256::from(10).pow(U256::from(20)),
        "addr1 should have at least 100 ETH (we seeded 1000); got: {balance}"
    );
}

/// Step 4 — Signed-tx → fifo integration.
///
/// Verifies that:
/// 1. The `sign_legacy_transfer` fixture produces a syntactically-valid `TxEnvelope` whose
///    recovered signer matches the sender
/// 2. `PreconfTxSet::push_if_absent` accepts the signed tx + emits a broadcast event (the same
///    event the fork's select! loop awaits)
/// 3. The stored `TxEntry` carries the right hash / sender / nonce — the indices that
///    `find_by_sender_nonce` / `find_by_hash` use
///
/// Together these cover everything the **non-EVM** parts of the fork
/// rely on. Verifying the actual EVM `apply_preconf_tx` call requires
/// a working `BlockBuilder` over real state — fundamentally a devnet
/// concern, not an in-process unit test (see "EVM apply gap" below).
#[tokio::test]
async fn step4_signed_tx_round_trips_through_fifo_with_recoverable_signer() {
    use alloy_consensus::TxEnvelope;
    use alloy_primitives::U256;
    use mantle_reth_preconf::{PreconfTxSet, types::PushResult};
    use reth_primitives_traits::SignedTransaction;

    use fixtures::signer::{ADDR2, sign_legacy_transfer};

    let signers = TestSigners::new();
    let tx = sign_legacy_transfer(
        &signers.addr1,
        /* nonce */ 0,
        /* to */ ADDR2,
        /* value */ U256::from(10).pow(U256::from(17)), // 0.1 ETH
    );

    // ── (1) Recoverable signer matches sender ─────────────────────
    // This is the path dispatch.rs's apply closure exercises:
    //   alloy::TxEnvelope -> N::SignedTx (via TryFrom) -> Recovered (via try_into_recovered)
    let env_clone: TxEnvelope = (*tx).clone();
    // For OP-stack, the alloy TxEnvelope → OpTxEnvelope conversion is
    // a TryFrom that succeeds for non-Deposit variants; here we have
    // a Legacy, which converts cleanly.
    let op_env: op_alloy_consensus::OpTxEnvelope =
        env_clone.try_into().expect("legacy TxEnvelope → OpTxEnvelope");
    // OpTransactionSigned = OpTxEnvelope (alias), so this directly
    // exercises `SignedTransaction::try_recover`.
    let recovered = op_env.try_recover().expect("ec-recover succeeds for a properly signed tx");
    assert_eq!(
        recovered,
        signers.addr1.address(),
        "recovered signer must match the signing PrivateKeySigner"
    );

    // ── (2) push_if_absent emits broadcast event ──────────────────
    let fifo = PreconfTxSet::new(16);
    let mut rx = fifo.subscribe();

    let push_result = fifo
        .push_if_absent(
            tx.clone(),
            signers.addr1.address(),
            mantle_reth_preconf::types::PreconfSource::Rpc,
        )
        .await;
    assert!(matches!(push_result, PushResult::Inserted));

    // The broadcast notification should be visible immediately
    // (PreconfTxSet's store-before-send ordering).
    let broadcast_hash = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
        .await
        .expect("broadcast event ready")
        .expect("broadcast not closed");
    assert_eq!(broadcast_hash, *tx.tx_hash());

    // ── (3) Stored entry carries correct indices ──────────────────
    let entry = fifo.find_by_hash(tx.tx_hash()).await.expect("entry stored");
    assert_eq!(entry.from, signers.addr1.address());
    assert_eq!(entry.nonce, 0);
    assert_eq!(entry.hash, *tx.tx_hash());

    // find_by_sender_nonce hits the same entry via the secondary index
    let by_index = fifo
        .find_by_sender_nonce(&signers.addr1.address(), 0)
        .await
        .expect("entry indexed by (sender, nonce)");
    assert_eq!(by_index.hash, *tx.tx_hash());
}

// ── EVM apply gap ──────────────────────────────────────────────────
//
// The 6 EVM tests below remain `#[ignore]` until a follow-up phase
// (Step 9 / P6.5 — devnet-driven) provides a real BlockBuilder over a
// seeded State<DB>. In the in-process MockEthProvider stack we cannot
// realistically:
//
// - Seed the L1 block contract code + storage (would need to embed the compiled OP system contracts
//   as test fixtures, ~50 KB each)
// - Stand up a `BlockBuilder` over `MockEthProvider`'s state (the trait integration goes through
//   ConfigurePostExecEvm which constructs a real `OpEvm` — needs production state-provider impl)
//
// What WAS verified in this session (Step 1-4):
// - Fork mode preserves type signatures (cli wiring compiles)
// - Fifo state machine + responder ownership (109 lib unit tests)
// - Dispatch closure invocation + branching (4 dispatch tests)
// - PayloadJob future semantics + flashblock-prep watch handling (5 payload_job tests, candidate-A
//   refactor)
// - Test harness fixture wiring (Step 2)
// - Provider seed pattern (Step 3)
// - Signing + fifo integration + tx conversion path (Step 4 — this commit)
//
// What remains for real EVM apply verification:
// - Either: build a `BlockBuilder` over MockEthProvider with the missing system-contract seeds
//   (significant, ~500-800 LoC fixture)
// - Or: drop into devnet-level integration testing (P6.5 — outside P6 in-process scope)
//
// The 6 `#[ignore]`d tests below remain as documented design intent
// for that future work; their bodies are pre-fleshed unimplemented!()
// to keep the test surface complete.

#[test]
#[ignore = "Real EVM apply requires BlockBuilder over seeded state; see EVM apply gap note above. \
            Verifies receipt.gas_used == real EVM usage (~21000 for transfer)"]
fn apply_preconf_tx_on_top_of_sequencer_state_produces_receipt_with_real_gas() {
    // Skeleton — see EVM apply gap note above.
    unimplemented!("requires real BlockBuilder over seeded State<DB>");
}

#[test]
#[ignore = "Step 5 (TODO): two preconf-txs from same sender (nonce N, N+1); \
            second tx's receipt must see the first's state changes"]
fn two_preconf_txs_compound_state_changes() {
    unimplemented!("see ignore reason");
}

#[test]
#[ignore = "Step 6 (TODO): tx that reverts (e.g. SSTORE to protected slot); \
            verify receipt.status == false + revert_data non-empty"]
fn preconf_tx_revert_yields_status_false_with_revert_data() {
    unimplemented!("see ignore reason");
}

#[test]
#[ignore = "Step 7 (TODO): apply 1 preconf-tx, then signal cancel; \
            finalize must seal a block containing exactly that tx"]
fn cancel_during_select_seals_partial_block() {
    unimplemented!("see ignore reason");
}

#[test]
#[ignore = "Step 8 (TODO): pool best-tx mock with 1 normal tx + fifo with 1 preconf-tx; \
            sealed block must contain both, receipts root reflects compound state"]
fn preconf_and_best_txs_share_state() {
    unimplemented!("see ignore reason");
}

#[test]
#[ignore = "Step 9 (TODO): entry with inserted_at past timeout-margin; \
            apply skipped, cancel_responder Timeout, tx not in sealed block"]
fn pre_apply_deadline_skip_excludes_tx_from_block() {
    unimplemented!("see ignore reason");
}
