//! On-chain preconf allowlist governance, against a real node.
//!
//! The unit tests in `mantle-reth-preconf`'s `whitelist` module already cover the
//! slot arithmetic, the has-code check, and the watcher's trigger decision — but
//! they run on `MockEthProvider`, whose storage is a hand-populated map and whose
//! canonical-state subscription can never emit (it drops the sender). This suite
//! exists for the three things only a live node can prove:
//!
//! 1. The slot arithmetic matches **real** reth storage as produced by a genesis alloc, not just
//!    the mock's map.
//! 2. A loaded allowlist actually reaches the tx-admission decision (the classifier, at the RPC
//!    entry), rather than merely landing in the classifier's own state.
//! 3. The watcher refreshes off **real** `CanonStateNotification`s produced by real block
//!    production.
//!
//! ## What is deliberately not tested here
//!
//! * **The contract itself** (two auth gates, idempotence, `MAX_BATCH`) — covered by the 19 forge
//!   tests in `mantle-v2/packages/contracts-bedrock/test/PreconfWhitelist.t.sol`. The seam between
//!   the two repos is the storage layout and the event topic0, and both are asserted from both
//!   sides.
//! * **The full L1→L2 governance path** (governance Safe → `L1CrossDomainMessenger.sendMessage` →
//!   deposit → `relayMessage` → the auth gates) — that needs an L1 and op-node, so it is a testnet
//!   exercise; it has been run end-to-end on public Mantle Sepolia.
//! * **The production wiring itself.** Cold start and the watcher now run inside `build_pool`,
//!   which this harness does execute, so the T1 tests observe the real thing rather than calling
//!   the entry points themselves. The T2 tests below still spawn their own watcher: the node's is
//!   racing block production from the moment `build_pool` returns, and a test needs a handle it can
//!   abort.

use super::helpers::{
    PreconfCfgBuilder, PreconfSetup, address_array_storage, layout_version_storage,
    mantle_chain_spec_with_account, pair_array_storage, send_preconf, storage_writer_bytecode,
};
use crate::launch_preconf_node_with_classifier;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, bytes};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use mantle_reth_preconf::{
    FROM_WILDCARDS_SLOT, PAIRS_SLOT, TO_WILDCARDS_SLOT, WHITELIST_UPDATED_TOPIC0,
    bootstrap_whitelist, run_whitelist_watcher,
};
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
use reth_provider::BlockNumReader;
use std::{sync::Arc, time::Duration};

/// Where the tests place the whitelist contract. Arbitrary — production passes the
/// real deployment address via `--preconf.whitelist-contract`.
const WL: Address = Address::new([0xc0; 20]);

/// The recipient half of the seeded exact rule.
const ALLOWED_TO: Address = Address::new([0xbb; 20]);

/// A recipient no rule covers, on either side.
const OTHER_TO: Address = Address::new([0xcc; 20]);

const CHAIN_ID: u64 = 5000;

/// Genesis storage for a whitelist authorizing exactly `from x to` as explicit
/// pairs, with both wildcard sets empty.
///
/// The cross product keeps the meaning these tests were written against — they
/// predate the move from two independent lists to explicit pairs, and none of
/// them is about the shape of the allowlist. [`wildcard_storage`] covers the
/// arms this one never touches.
fn whitelist_storage(from: &[Address], to: &[Address]) -> Vec<(B256, B256)> {
    let pairs: Vec<_> = from.iter().flat_map(|f| to.iter().map(move |t| (*f, *t))).collect();
    let mut storage = pair_array_storage(PAIRS_SLOT, &pairs);
    storage.extend(address_array_storage(FROM_WILDCARDS_SLOT, &[]));
    storage.extend(address_array_storage(TO_WILDCARDS_SLOT, &[]));
    storage.push(layout_version_storage());
    storage
}

/// The default sender used by the test harness wallet.
fn wallet_address() -> Address {
    Wallet::default().with_chain_id(CHAIN_ID).inner.address()
}

/// Config in on-chain whitelist mode, pointing at [`WL`].
///
/// The in-memory lists start empty on purpose — every test loads them from state.
fn onchain_cfg() -> PreconfSetup {
    PreconfCfgBuilder::new().whitelist_contract(WL).build()
}

/// A signed transfer from the harness wallet to `to`.
async fn signed_transfer(wallet: &Wallet, nonce: u64, to: Address) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(CHAIN_ID),
        nonce: Some(nonce),
        to: Some(TxKind::Call(to)),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(U256::from(1u64)),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// A signed call to the whitelist stub, which rewrites the allowlist arrays and
/// emits `WhitelistUpdated`.
async fn signed_stub_call(wallet: &Wallet, nonce: u64) -> Bytes {
    let request = TransactionRequest {
        chain_id: Some(CHAIN_ID),
        nonce: Some(nonce),
        to: Some(TxKind::Call(WL)),
        // Four SSTOREs into cold slots (~22k each) plus a LOG1.
        gas: Some(300_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// How many event-carrying blocks a watcher test will produce before giving up.
///
/// More than one is required, and the reason is a race in the *test*, not in the
/// watcher: `tokio::spawn(run_whitelist_watcher(..))` returns before the task has
/// run far enough to call `canonical_state_stream()`, so a block canonicalised
/// immediately afterwards can be committed before the subscription exists — and a
/// notification nobody is subscribed to is simply gone. Production never hits this
/// because `build_pool` spawns the watcher long before the first block.
///
/// Re-emitting is sound because the stub's write is idempotent: every call stores
/// the same values, so any single notification that does get observed converges the
/// cache to the same place.
const CONVERGENCE_ATTEMPTS: usize = 5;

/// Polling budget for any "wait until it converges" loop, as 50 ms ticks.
///
/// Generous on purpose: the whole suite runs seven node-spawning tests in
/// parallel, and under that load block commit, state-provider refresh and the
/// watcher's reload all take noticeably longer than when a test runs alone. A
/// large budget costs nothing on the happy path (the loops exit as soon as the
/// condition holds) and is what keeps these tests off the flaky list.
const POLL_TICKS: usize = 200;

// ===== T1: reading and admission on a real provider =====

/// Cold start must read a genesis-allocated whitelist out of real reth storage,
/// and must have done so **by the time the node finishes launching**.
///
/// Two things only a live node can prove:
///
/// * the slot arithmetic matches real reth storage as written by the genesis loader — the unit
///   tests hand-populate a mock map using the same code that computes the slots, so they prove
///   nothing about the layout;
/// * the load happens early enough. Cold start runs inside `build_pool`, ahead of the pool
///   listener, the RPC server and the payload builder, so there is no window in which a tx could be
///   admitted — and have its verdict frozen — against empty allowlists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_start_loads_genesis_whitelist_before_the_node_is_up() {
    let sender = wallet_address();
    let spec = mantle_chain_spec_with_account(
        CHAIN_ID,
        WL,
        &bytes!("00"),
        &whitelist_storage(&[sender], &[ALLOWED_TO]),
    );

    let setup = onchain_cfg();
    let cfg = setup.cfg.clone();
    let (node, _http, _wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;

    // Nothing was loaded by this test — the node did it inside `build_pool`.
    assert_eq!(
        classifier.whitelist_counts(),
        (1, 0, 0),
        "cold start must already have run by the time launch returns",
    );
    assert!(classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)));
    assert!(!classifier.preview_eligibility(&sender, Some(&OTHER_TO)));

    // Idempotent: the watcher re-runs this same read on every relevant
    // notification, so a repeat must converge on the same state.
    bootstrap_whitelist(&node.inner.provider, &cfg, &classifier).expect("re-read must succeed");
    assert_eq!(classifier.whitelist_counts(), (1, 0, 0));
    assert!(classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)));
}

/// The loaded allowlist must reach the RPC admission check, not just sit in the
/// config — i.e. a tx whose eligibility exists only because of on-chain state gets
/// preconfirmed and sealed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whitelisted_tx_from_chain_state_takes_preconf_path() {
    let sender = wallet_address();
    let spec = mantle_chain_spec_with_account(
        CHAIN_ID,
        WL,
        &bytes!("00"),
        &whitelist_storage(&[sender], &[ALLOWED_TO]),
    );

    let setup = onchain_cfg();
    let cfg = setup.cfg.clone();
    let (mut node, http, wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;
    bootstrap_whitelist(&node.inner.provider, &cfg, &classifier).expect("bootstrap");

    let raw_tx = signed_transfer(&wallet, 0, ALLOWED_TO).await;

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
    tokio::time::sleep(Duration::from_millis(300)).await;

    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");

    let event = rpc_task.await.expect("rpc join").expect("preconf must succeed");
    assert!(
        matches!(event.status, mantle_reth_rpc_ext::PreconfStatus::Success),
        "eligibility came from chain state, so this must be preconfirmed: {:?} reason={:?}",
        event.status,
        event.reason,
    );

    let sealed: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .map(|tx| alloy_primitives::keccak256(tx.encoded_2718()))
        .collect();
    assert!(sealed.contains(&event.tx_hash), "preconf tx must be sealed; sealed = {sealed:?}");
}

/// A recipient that no on-chain rule covers must be refused, proving the admission
/// check consults the loaded allowlist rather than waving everything through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tx_outside_chain_whitelist_is_not_preconf_eligible() {
    let sender = wallet_address();
    let spec = mantle_chain_spec_with_account(
        CHAIN_ID,
        WL,
        &bytes!("00"),
        &whitelist_storage(&[sender], &[ALLOWED_TO]),
    );

    let setup = onchain_cfg();
    let cfg = setup.cfg.clone();
    let (_node_guard, http, wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;
    bootstrap_whitelist(&_node_guard.inner.provider, &cfg, &classifier).expect("bootstrap");

    // The seeded allowlist is the single pair `(sender, ALLOWED_TO)` and no
    // wildcards, so `(sender, OTHER_TO)` matches none of the three arms.
    let raw_tx = signed_transfer(&wallet, 0, OTHER_TO).await;
    let result = send_preconf(&http, raw_tx).await;

    match result {
        Err(err) => {
            // Typed rejection from the RPC layer is the expected outcome.
            let msg = err.to_string();
            assert!(
                msg.to_lowercase().contains("preconf") || msg.to_lowercase().contains("eligible"),
                "expected a not-eligible rejection, got: {msg}",
            );
        }
        Ok(event) => assert!(
            !matches!(event.status, mantle_reth_rpc_ext::PreconfStatus::Success),
            "a tx outside the on-chain allowlist must not be preconfirmed, got {:?}",
            event.status,
        ),
    }
}

/// The has-code check, through a real provider: an address with nothing deployed
/// at it is this node's own misconfiguration and must be fatal.
///
/// Now that cold start lives in `build_pool`, "fatal" means the **node does not
/// launch** — `build_pool` returns `eyre::Result`, so the failure aborts the
/// launch instead of leaving a node running on empty allowlists. Asserted as a
/// panic because the harness macro `expect`s the launch; the payload carries the
/// `ContractHasNoCode` message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[should_panic(expected = "has no code")]
async fn cold_start_refuses_to_launch_on_a_code_less_whitelist_address() {
    // Allocate the whitelist somewhere else, leaving `WL` empty.
    let elsewhere = Address::new([0xd0; 20]);
    let spec = mantle_chain_spec_with_account(
        CHAIN_ID,
        elsewhere,
        &bytes!("00"),
        &whitelist_storage(&[wallet_address()], &[ALLOWED_TO]),
    );

    let _ = launch_preconf_node_with_classifier!(onchain_cfg(), spec).await;
}

/// A deployed whitelist that currently allows nobody is governance's decision, not
/// an error: the node must load the empty allowlist and keep running. Regression guard
/// — making this fatal would let a governance mistake stop the sequencer from
/// restarting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_accepts_empty_onchain_whitelist() {
    let spec =
        mantle_chain_spec_with_account(CHAIN_ID, WL, &bytes!("00"), &whitelist_storage(&[], &[]));

    let setup = onchain_cfg();
    let cfg = setup.cfg.clone();
    let (node, _http, _wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;

    bootstrap_whitelist(&node.inner.provider, &cfg, &classifier)
        .expect("an empty on-chain allowlist must not fail startup");
    assert_eq!(classifier.whitelist_counts(), (0, 0, 0));
    assert!(!classifier.preview_eligibility(&wallet_address(), Some(&ALLOWED_TO)));
}

// ===== T2: watcher end-to-end on real canonical notifications =====
//
// Both tests below are `#[ignore]`d, and it is worth being precise about why,
// because they do pass — 5/5 when run serially:
//
//     cargo test -p mantle-reth-integration-tests --test preconf \
//         whitelist_onchain::watcher -- --ignored --test-threads=1
//
// Run as part of the parallel suite they fail roughly one time in five, always
// with the same signature: the stub call is present in the resolved payload and
// `submit_payload` + `update_forkchoice` both succeed, yet `best_block_number()`
// stays at 0 and the storage write never becomes visible — the canonicalisation
// simply does not take effect. That is a property of this harness under load, not
// of the watcher: the pre-existing `happy_path::weth_deposit_carries_log_through_
// to_receipt` fails with the identical symptom (it reads state after a fixed
// 100 ms sleep following the same submit/FCU pair) and is flaky for the same
// reason.
//
// Quarantining the individual tests follows the repo's own guidance in CLAUDE.md
// ("quarantine that single test (#[ignore]) rather than moving the whole tier back
// to nightly"). Un-ignore once block production in this harness is deterministic —
// note that `NodeTestContext::advance_block()`, which does the proper engine
// handshake, is not usable here: it waits on the payload-attributes / built-payload
// event pair, which the preconf payload-builder fork does not drive, so it times
// out "waiting for a non-empty payload". Every other preconf suite hand-rolls the
// sequence for that same reason.

/// Produces and canonicalises one block that tries to include a stub call at
/// `nonce`, which is what makes reth emit a real `CanonStateNotification`.
///
/// Evaluates to `true` if the stub call actually made it into the block. Inclusion
/// is **not** guaranteed on any single attempt — `inject_tx` returns once the RPC
/// accepted the tx, but pool validation is asynchronous, so the builder may run
/// before the tx is available. Callers retry.
///
/// Note it checks for its own tx hash rather than a non-empty block: every OP block
/// already carries the L1-attributes deposit as `tx[0]`, so a length check would
/// pass even when the stub call was dropped.
///
/// Declared as a macro rather than a function to avoid spelling out the harness's
/// deeply generic node type at a call site.
macro_rules! produce_stub_block {
    ($node:expr, $wallet:expr, $nonce:expr) => {{
        let raw_tx = signed_stub_call(&$wallet, $nonce).await;
        let want_hash = alloy_primitives::keccak256(&raw_tx);
        $node.rpc.inject_tx(raw_tx).await.expect("inject stub call");
        // Give asynchronous pool validation a chance before the builder starts.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Hand-rolled rather than `advance_block()`: that helper waits on the
        // payload-attributes / built-payload event pair, which the preconf payload
        // builder fork does not drive the way it expects (it times out "waiting for
        // a non-empty payload"). Every other preconf suite hand-rolls for the same
        // reason.
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
        // Let the builder's sweep tick admit the pooled tx before resolving.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let payload = $node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
            .await
            .expect("resolve_kind")
            .expect("payload build");

        let included = payload
            .block()
            .body()
            .transactions()
            .any(|tx| alloy_primitives::keccak256(tx.encoded_2718()) == want_hash);

        let new_head = $node.submit_payload(payload).await.expect("submit_payload");
        $node.update_forkchoice(new_head, new_head).await.expect("canonicalise stub block");

        included
    }};
}

/// Reads one of the whitelist's storage slots at the current canonical head.
fn onchain_slot<P: reth_provider::StateProviderFactory>(provider: &P, slot: B256) -> U256 {
    provider
        .latest()
        .expect("state provider")
        .storage(WL, slot)
        .expect("storage read")
        .unwrap_or_default()
}

/// Waits until `slot` reads back as `want`, returning whether it got there.
///
/// Polls instead of reading once: `update_forkchoice` returning does not mean
/// `latest()` already resolves to the new head, so a single read can legitimately
/// observe pre-block state. (The pre-existing `happy_path` WETH test papers over
/// the same race with a fixed 100 ms sleep, which is why it is flaky under load.)
async fn await_onchain_slot<P: reth_provider::StateProviderFactory>(
    provider: &P,
    slot: B256,
    want: U256,
) -> bool {
    for _ in 0..POLL_TICKS {
        if onchain_slot(provider, slot) == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The watcher must pick up an on-chain change. A stub at `WL` rewrites both list
/// slots and emits `WhitelistUpdated`; producing that block yields a real
/// `CanonStateNotification`, which is the one thing the unit tests structurally
/// cannot exercise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_refreshes_on_whitelist_updated_event() {
    let sender = wallet_address();

    // The stub rewrites the allowlist to the single pair `(sender, ALLOWED_TO)`.
    let post_update = whitelist_storage(&[sender], &[ALLOWED_TO]);
    let code = storage_writer_bytecode(&post_update, WHITELIST_UPDATED_TOPIC0);

    // Genesis starts with a *different* single entry on each side, so the refresh
    // is observable as a change rather than a no-op.
    let initial = whitelist_storage(&[Address::new([0x11; 20])], &[Address::new([0x22; 20])]);
    let spec = mantle_chain_spec_with_account(CHAIN_ID, WL, &code, &initial);

    let setup = onchain_cfg();
    let cfg = Arc::new(setup.cfg.clone());
    let (mut node, _http, wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;

    bootstrap_whitelist(&node.inner.provider, &cfg, &classifier).expect("bootstrap");
    assert_eq!(classifier.whitelist_counts(), (1, 0, 0));
    assert!(
        !classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)),
        "sender not yet allowlisted on chain"
    );

    // A second watcher, owned by the test so it can be aborted. The node's own
    // watcher (spawned in `build_pool`) is already running; both reads are
    // idempotent, so racing them changes nothing.
    let watcher = tokio::spawn(run_whitelist_watcher(
        node.inner.provider.clone(),
        cfg.clone(),
        classifier.clone(),
    ));

    let mut chain_changed = false;
    let mut converged = false;
    for nonce in 0..CONVERGENCE_ATTEMPTS as u64 {
        let included = produce_stub_block!(node, wallet, nonce);
        if !included {
            // Pool validation lost the race with the builder; the tx stays queued
            // and the next attempt uses the next nonce.
            continue;
        }

        // Separates "the chain never changed" from "the watcher did not react".
        // Asserts on the element slot, not the length: genesis already had length
        // 1, so a length check could not tell the two states apart.
        // Index 1 of the pair layout is `pairs[0].from`; index 2 would be its
        // `.to`. Either proves the write landed.
        let element_slot = pair_array_storage(PAIRS_SLOT, &[(sender, ALLOWED_TO)])[1].0;
        chain_changed = await_onchain_slot(
            &node.inner.provider,
            element_slot,
            U256::from_be_bytes(sender.into_word().0),
        )
        .await;
        assert!(
            chain_changed,
            "an included stub call must have rewritten the pair list; \
             slot0={:?} element={:?} want={:?} block={:?}",
            onchain_slot(&node.inner.provider, B256::from(U256::from(PAIRS_SLOT))),
            onchain_slot(&node.inner.provider, element_slot),
            U256::from_be_bytes(sender.into_word().0),
            node.inner.provider.best_block_number(),
        );

        for _ in 0..POLL_TICKS {
            if classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)) {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if converged {
            break;
        }
    }

    assert!(chain_changed, "the stub call was never included in {CONVERGENCE_ATTEMPTS} blocks");
    assert!(
        converged,
        "watcher never applied the on-chain update after {CONVERGENCE_ATTEMPTS} blocks; \
         counts = {:?}",
        classifier.whitelist_counts(),
    );

    watcher.abort();
}

/// Governance draining the allowlist at runtime must be applied faithfully — the
/// sequencer stops fast-pathing rather than keeping the stale entries alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_applies_governance_draining_the_whitelist() {
    let sender = wallet_address();

    // The stub sets all three list lengths to zero and emits the event. Leaving
    // the element slots populated is deliberate: a real `_rm` would also leave
    // stale words behind once the length shrinks, and the reader must trust the
    // length.
    let drained = vec![
        (B256::from(U256::from(PAIRS_SLOT)), B256::ZERO),
        (B256::from(U256::from(FROM_WILDCARDS_SLOT)), B256::ZERO),
        (B256::from(U256::from(TO_WILDCARDS_SLOT)), B256::ZERO),
    ];
    let code = storage_writer_bytecode(&drained, WHITELIST_UPDATED_TOPIC0);

    let spec = mantle_chain_spec_with_account(
        CHAIN_ID,
        WL,
        &code,
        &whitelist_storage(&[sender], &[ALLOWED_TO]),
    );

    let setup = onchain_cfg();
    let cfg = Arc::new(setup.cfg.clone());
    let (mut node, _http, wallet, _chain_id, classifier) =
        launch_preconf_node_with_classifier!(setup, spec).await;

    bootstrap_whitelist(&node.inner.provider, &cfg, &classifier).expect("bootstrap");
    assert!(classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)), "allowlisted at genesis");

    let watcher = tokio::spawn(run_whitelist_watcher(
        node.inner.provider.clone(),
        cfg.clone(),
        classifier.clone(),
    ));

    let mut chain_changed = false;
    let mut converged = false;
    for nonce in 0..CONVERGENCE_ATTEMPTS as u64 {
        let included = produce_stub_block!(node, wallet, nonce);
        if !included {
            continue;
        }

        chain_changed = await_onchain_slot(
            &node.inner.provider,
            B256::from(U256::from(PAIRS_SLOT)),
            U256::ZERO,
        )
        .await;
        assert!(chain_changed, "an included stub call must have cleared the pair length slot");

        for _ in 0..POLL_TICKS {
            if classifier.whitelist_counts() == (0, 0, 0) {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if converged {
            break;
        }
    }

    assert!(chain_changed, "the stub call was never included in {CONVERGENCE_ATTEMPTS} blocks");
    assert!(
        converged,
        "watcher never applied the drain after {CONVERGENCE_ATTEMPTS} blocks; counts = {:?}",
        classifier.whitelist_counts(),
    );
    assert!(
        !classifier.preview_eligibility(&sender, Some(&ALLOWED_TO)),
        "a drained on-chain allowlist must stop the fast path",
    );

    watcher.abort();
}
