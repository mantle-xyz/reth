//! `ensure_only_one_payload`: at most one build task is live at a time.
//!
//! When a second `engine_forkchoiceUpdated` with attributes arrives on the same
//! parent, the payload service asks the generator for a new job; the generator
//! signal-cancels the previously spawned build. Reth's `PayloadBuilderService`
//! keeps every job in a `Vec` and only reaps one when its `Future` resolves —
//! which, for a preconf job, happens on cancel. So without this cancel a
//! superseded build would linger indefinitely — still subscribed to the shared
//! `PreconfTxSet` broadcast — and could apply preconf txs into a block that is
//! never committed, stealing them from the job that actually gets sealed.
//!
//! The clean, deterministic observable of the fix: the superseded job is
//! cancelled and reaped, so it is no longer resolvable (`resolve_kind` → `None`),
//! while the newest job stays live. Without the fix, the superseded job lingers
//! and still resolves to `Some(_)`.

use super::helpers::PreconfCfgBuilder;
use crate::launch_preconf_node;
use alloy_primitives::Address;
use reth_e2e_test_utils::wallet::Wallet;

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Creating a second build on the same parent cancels the first, so the payload
/// service reaps the superseded job and it is no longer resolvable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_build_is_cancelled_and_reaped() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();
    let (mut node, _http, _wallet, _chain_id) = launch_preconf_node!(cfg).await;

    let fcu_state = node.current_forkchoice_state().expect("forkchoice state");

    // Job A — the build that will be superseded ("leftover").
    let attrs_a = node.payload.next_attributes();
    let payload_id_a = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs_a))
        .await
        .expect("FCU A must succeed")
        .payload_id
        .expect("payload_id A present");

    // Job B — a second build on the SAME parent. `next_attributes` bumps the
    // timestamp, so this is a distinct payload id and thus a distinct job.
    // Creating it must cancel job A (`ensure_only_one_payload`).
    let attrs_b = node.payload.next_attributes();
    let payload_id_b = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .fork_choice_updated(fcu_state, Some(attrs_b))
        .await
        .expect("FCU B must succeed")
        .payload_id
        .expect("payload_id B present");
    assert_ne!(payload_id_a, payload_id_b, "two distinct jobs on the same parent");

    // Give the payload service a beat to poll the now-cancelled job A, observe
    // its `Ready`, and swap-remove it from its job set.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The newest job (B) is still live and resolves to a sealed payload.
    let resolved_b = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_b, reth_node_api::PayloadKind::Earliest)
        .await;
    assert!(
        matches!(resolved_b, Some(Ok(_))),
        "the newest build B must still be live and resolvable; got {resolved_b:?}"
    );

    // The superseded job A was cancelled by `ensure_only_one_payload` and reaped
    // by the service — it is no longer resolvable. Without the fix, A lingers and
    // this returns `Some(_)`.
    let resolved_a = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id_a, reth_node_api::PayloadKind::Earliest)
        .await;
    assert!(
        resolved_a.is_none(),
        "superseded build A must be cancelled and reaped (ensure_only_one_payload); \
         it is still resolvable ({resolved_a:?}), so the previous job was not cancelled"
    );
}
