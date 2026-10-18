//! Pre-fifo synchronous rejection paths.
//!
//! When any pre-fifo check rejects, the RPC returns an `Err`
//! synchronously, no responder is parked, and no fifo entry is created —
//! so these cases have neither a `Timeout` SLA to guard nor an "on-chain
//! landing" contract to verify. They are grouped here rather than in
//! `timeout.rs` / `happy_path.rs` / `gas_budgets.rs` to keep those
//! files focused on their state-machine domains.
//!
//! Two families of checks live here:
//!
//! ### Preconf-specific rejections
//!
//! - **Whitelist gate** (`PreconfClassifier::preview_eligibility`) — rejects non-whitelisted
//!   `(sender, to)` before anything is recorded.
//! - **Nonce-gap gate** — rejects `tx.nonce > pending_nonce` before an entry is created.
//! - **Preconf per-tx gas ceiling** — rejects `tx.gas_limit > preconf_max_gas_per_tx` before the
//!   record is written, with a typed `PreconfError::PreconfGasLimitExceeded`. See
//!   `per_tx_gas_ceiling_rejected_at_rpc_not_by_the_pool`.
//!
//! ### Generic validator rejections live in `validator_error_mapping.rs`
//!
//! Refusals the shared validator chain makes — intrinsic gas too low,
//! insufficient funds, block gas limit exceeded, oversized init code, Mantle's
//! `MetaTx` — are asserted there, and only to the extent that the refusal
//! *names its reason*. The `pool rejected: <inner>` wrapper that
//! `rpc.rs::handle_inner`'s catch-all puts around them is not part of the
//! contract: the direct-admission design translates validator refusals itself,
//! so a test pinning the wrapper would fail by design and teach nothing.
//!
//! What stays here is the narrower family above: refusals the **preconf layer
//! itself** makes, before the validator is consulted at all.
//!
//! Adjacent coverage worth knowing about:
//!
//! - A stale nonce (`tx.nonce < on_chain_nonce`) is the validator's refusal, relayed by admission:
//!   `a_stale_nonce_is_refused_at_admission` below. A nonce the chain takes *after* the submission
//!   was queued is `NonceSuperseded`, covered in `pool_tx_on_committed_nonce.rs`.
//! - Base-fee routing: `timeout::basefee_orphan_returns_timeout_and_clears_responder` pins today's
//!   behaviour (parked, then `Ok(Timeout)`); `basefee_threshold.rs` holds the synchronous refusal
//!   that replaces it.
//! - The same-sender, both-channels nonce interaction is in `dual_channel_nonce.rs`.

use super::helpers::{PreconfCfgBuilder, send_preconf, wait_latest_nonce};
use crate::{canonize_built, launch_preconf_node, launch_preconf_node_with_fifo};
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use jsonrpsee::core::ClientError;
use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};

const RECIPIENT: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Build a signed transfer with an explicit nonce and gas limit.
async fn signed_transfer(
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

/// Non-whitelisted (sender, to) returns typed `NotPreconfEligible` error,
/// with nothing recorded: no queue entry, no commitment record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_whitelisted_returns_not_eligible() {
    // Placeholder whitelist satisfies `enabled=true` validation while
    // guaranteeing every `send_preconf` from `wallet_0` misses the gate.
    let placeholder = Address::from([0xFE; 20]);
    let cfg =
        PreconfCfgBuilder::new().whitelist_from(placeholder).whitelist_to(placeholder).build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0, 21_000).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("empty whitelist + all_preconfs=false must reject with NotPreconfEligible");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            assert!(
                e.message().to_lowercase().contains("not preconf eligible"),
                "unexpected error message: {}",
                e.message()
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    // The gate runs before any entry is created. A regression that moved it
    // past the responder would still surface the error, but only after
    // `preconf_timeout` elapses — guard the synchronous-reject contract.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "whitelist rejection must fail fast (< 500ms); took {elapsed:?} — did the handler park a responder?",
    );
}

/// Nonce gap is rejected synchronously with a typed error BEFORE any pool /
/// fifo interaction.
///
/// `rpc.rs::handle_inner` fetches `on_chain_nonce` and
/// the queue's own account view layered on the chain's nonce; if the incoming
/// tx's nonce exceeds what that says comes next, admission returns
/// `PreconfError::NonceGap { .. }` with nothing recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nonce_gap_rejected_synchronously() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Use a generous timeout so a *hypothetical* regression that
    // routes the tx into the fifo would surface as a slow-fail rather
    // than a fast-pass — the assertion below still catches it.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(1_500)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // nonce=5 with on-chain nonce=0 and no prior pool state.
    let raw_tx = signed_transfer(chain_id, &wallet, 5, 21_000).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("nonce gap must surface as an Err, not a Timeout / Success event");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            // Pin the exact `PreconfError::NonceGap` Display prefix — a
            // generic `contains("nonce")` fallback would silently accept
            // pool-layer `NonceTooLow` / `nonce mismatch` errors, hiding
            // a regression where rpc.rs's synchronous nonce-gap gate is
            // bypassed and the tx reaches the pool.
            assert!(msg.contains("nonce gap"), "unexpected error message: {}", e.message(),);
            assert!(
                e.message().contains('5'),
                "message must mention the offending tx nonce (5); got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "nonce gap must fail fast (< 500ms); took {elapsed:?} — did the handler park a responder?",
    );
}

/// Per-tx gas ceiling is enforced at the **RPC**, before the pool is asked.
///
/// Setup: `max_gas_per_tx = 20_000`. Submit a 21k-gas transfer. `rpc.rs`
/// Admission checks the ceiling before it writes the record, so the client
/// gets `PreconfError::PreconfGasLimitExceeded` naming both numbers.
///
/// The failure is synchronous (well under `preconf_timeout`): nothing is
/// recorded, so there is no responder for a client to be left waiting on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_tx_gas_ceiling_rejected_at_rpc_not_by_the_pool() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    // Cap under 21_000 → simple transfer exceeds. `max_gas_per_block`
    // must stay >= max_gas_per_tx (config invariant), so set it
    // explicitly to the same cap.
    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .max_gas_per_tx(20_000)
        .max_gas_per_block(20_000)
        // Long timeout so a hypothetical regression that DOES admit
        // the tx would surface as a slow success/timeout rather than
        // an ambiguous fast failure.
        .preconf_timeout_ms(1_500)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    let raw_tx = signed_transfer(chain_id, &wallet, 0, 21_000).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("21k-gas tx must be rejected against 20k per-tx cap");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            // Exact, because naming both numbers is the point: a refusal
            // that merely says "too big" leaves the client guessing which cap
            // it hit.
            assert_eq!(
                e.message(),
                "preconf gas limit exceeded: tx gas limit 21000 exceeds preconf_max_gas_per_tx 20000",
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "per-tx cap rejection must fail fast; took {elapsed:?} — did the handler park a responder?",
    );
}

/// A nonce the chain has already spent is refused at admission, synchronously,
/// with the validator's own reason and nothing queued.
///
/// Admission runs the shared validator over canonical state, so this is the
/// one nonce rule it does not decide for itself. The sender's nonce is
/// advanced by landing an ordinary transaction first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_nonce_is_refused_at_admission() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let sender = Wallet::default().with_chain_id(1).inner.address();
    let cfg = PreconfCfgBuilder::new().whitelist_from(sender).whitelist_to(recipient).build();
    let (mut node, http, wallet, chain_id, fifo) = launch_preconf_node_with_fifo!(cfg).await;

    // Spend nonce 0 through the pool and make the block canonical.
    let spent = signed_transfer(chain_id, &wallet, 0, 21_000).await;
    node.rpc.inject_tx(spent).await.expect("the pool takes the transfer");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let payload = node
        .inner
        .payload_builder_handle
        .resolve_kind(payload_id, reth_node_api::PayloadKind::Earliest)
        .await
        .expect("resolve_kind")
        .expect("payload build");
    canonize_built!(node, payload);
    wait_latest_nonce(&http, sender, 1).await;

    // A different transaction signed for the nonce that is now gone.
    let stale = signed_transfer(chain_id, &wallet, 0, 30_000).await;
    let stale_hash = alloy_primitives::keccak256(&stale);
    let start = std::time::Instant::now();
    let err = send_preconf(&http, stale).await.expect_err("a spent nonce cannot be preconfirmed");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => assert_eq!(
            e.message(),
            "pool rejected: transaction nonce is not consistent: next nonce 1, tx nonce 0",
        ),
        other => panic!("expected Call error, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "refused synchronously; took {elapsed:?}"
    );
    // The queue does hold `(sender, 0)`: the landed transfer's own record. The
    // refused submission must not have joined it.
    assert!(
        fifo.find_by_hash(&stale_hash).await.is_none(),
        "a refused submission must leave nothing in the queue",
    );
}
