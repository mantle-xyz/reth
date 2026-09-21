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
//! - **Whitelist gate** (`PreconfClassifier::preview_eligibility`, `rpc.rs`) — rejects
//!   non-whitelisted `(sender, to)` before `attach_responder`.
//! - **Nonce-gap gate** (`rpc.rs`) — rejects `tx.nonce > pending_nonce` before `attach_responder`
//!   and before `pool.add_transaction`.
//! - **Preconf per-tx gas ceiling** (`rpc.rs` Step 3b) — rejects `tx.gas_limit >
//!   preconf_max_gas_per_tx` before the verdict is written and before the pool is asked, with a
//!   typed `PreconfError::PreconfGasLimitExceeded`. `PreconfAwareValidator` carries the same
//!   ceiling as defence in depth, but it gates on an already-frozen eligible verdict and so cannot
//!   be reached through this RPC — see `per_tx_gas_ceiling_rejected_at_rpc_not_by_the_pool`.
//!
//! ### Generic pool-validator rejections
//!
//! Reth's underlying `OpTransactionValidator` runs after `PreconfAwareValidator`
//! and produces its own typed errors (`intrinsic_gas_too_low`,
//! `insufficient_funds`, `block_gas_limit`, `nonce_too_low`, etc.). All of
//! these funnel through `rpc.rs::handle_inner`'s step-4 catch-all as
//! `PreconfError::PoolRejected(inner_kind)` and reach the client as
//! `Err(Call { message: "pool rejected: <inner>" })`. This file pins the
//! wire surface so an SDK relying on message-substring parsing survives
//! reth upgrades:
//!
//! - `intrinsic_gas_too_low_pool_rejects` — `gas_limit < intrinsic_gas`
//! - `insufficient_funds_pool_rejects` — `value + fees > balance`
//! - `block_gas_limit_exceeded_pool_rejects` — `gas_limit > block.gas_limit`
//!
//! Notes on adjacent coverage:
//!
//! - `nonce_too_low` (stale nonce, `tx.nonce < on_chain_nonce`) requires a two-slot setup (commit +
//!   canon first). Skipped in Phase 1 — the same `PoolRejected` wire wrapping applies; message
//!   contains "nonce too low".
//! - `replacement_underpriced` requires parking a first tx in pool then submitting a replacement
//!   with lower fees. Skipped in Phase 1 for the same reason.
//! - `base_fee` sub-pool routing is covered by
//!   `timeout::basefee_orphan_returns_timeout_and_clears_responder` (handles both the "pool sync
//!   reject" and "orphan → Timeout" branches).

use super::helpers::{PreconfCfgBuilder, send_preconf};
use crate::launch_preconf_node;
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

/// Non-whitelisted (sender, to) returns typed `NotPreconfEligible` error.
/// RPC never accepts the tx into the fifo; the pool sees no admission either.
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

    // Whitelist gate lives at rpc.rs:128, before `attach_responder` and
    // `pool.add_transaction`. Regression that moves the check past the
    // responder would still surface the error but only after
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
/// `pool.get_highest_consecutive_transaction_by_sender(...)`; if the
/// incoming tx's nonce exceeds `pending_nonce = max(on_chain_nonce,
/// pool_high + 1)`, the handler returns `PreconfError::NonceGap { .. }`
/// without attaching a responder or calling `pool.add_transaction`.
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
/// Step 3b checks the ceiling before it writes the verdict and before
/// `pool.add_transaction`, so the client gets
/// `PreconfError::PreconfGasLimitExceeded`.
///
/// Hence the negative assertion below: `PreconfAwareValidator` carries its own copy of the
/// ceiling as defence in depth (see `validator.rs`), and both Displays mention
/// `preconf_max_gas_per_tx` — so matching the substring the two arms share proves nothing,
/// and the test must also state which arm did *not* produce it.
///
/// The failure is synchronous (well under `preconf_timeout`): no fifo entry
/// is created, and the responder attached at Step 3 is cancelled on the way
/// out.
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
            let msg = e.message().to_lowercase();
            // `PreconfError::PreconfGasLimitExceeded`'s Display —
            // "preconf gas limit exceeded: tx gas limit {n} exceeds
            // preconf_max_gas_per_tx {max}". It names both numbers, so pin
            // them: the RPC knows the request's own gas limit, whereas the
            // validator's `PreconfGasLimitExceeded` carries neither.
            assert!(
                msg.contains("preconf gas limit exceeded"),
                "expected the RPC's own ceiling error; got {}",
                e.message(),
            );
            assert!(
                msg.contains("21000") && msg.contains("20000"),
                "the RPC error must name the offending limit and the cap; got {}",
                e.message(),
            );
            // The discriminating half. Without it, the validator's copy of the
            // ceiling firing instead would read as a pass.
            assert!(
                !msg.contains("pool rejected"),
                "the pool must never have been asked; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }

    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "per-tx cap rejection must fail fast; took {elapsed:?} — did the handler park a responder?",
    );
}

/// Build a signed transfer with an explicit `value`, on top of the
/// standard 21k-gas transfer shape. Used by `insufficient_funds_*`
/// which cannot express the huge value through `signed_transfer`.
async fn signed_transfer_with_value(
    chain_id: u64,
    wallet: &Wallet,
    nonce: u64,
    value: U256,
) -> alloy_primitives::Bytes {
    let request = TransactionRequest {
        chain_id: Some(chain_id),
        nonce: Some(nonce),
        to: Some(TxKind::Call(RECIPIENT.parse().unwrap())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        value: Some(value),
        input: TransactionInput::default(),
        ..Default::default()
    };
    TransactionTestContext::sign_tx(wallet.inner.clone(), request).await.encoded_2718().into()
}

/// Intrinsic-gas-too-low is rejected at pool admission and surfaced to
/// the client as `Err(Call { message: "pool rejected: <inner>" })`.
///
/// Setup: submit a preconf-eligible whitelisted tx with `gas_limit = 20_000`,
/// under the `21_000` intrinsic gas required for a plain transfer. The
/// preconf per-tx ceiling is set high enough (`preconf_max_gas_per_tx =
/// 1_000_000`) that it does NOT fire first — the underlying
/// `OpTransactionValidator` is the one that catches this.
///
/// Wire contract pinned:
///  - top-level wrapping: `pool rejected: ...` (from `PreconfError::PoolRejected`)
///  - inner substring: something like `intrinsic gas` / `gas limit too low` (reth's pool error
///    text; loose match to survive minor wording drift)
///  - fast-fail: no responder is parked, elapsed < 500ms
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intrinsic_gas_too_low_pool_rejects() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        // High enough so preconf per-tx cap doesn't fire before the
        // underlying validator's intrinsic-gas check.
        .max_gas_per_tx(1_000_000)
        .max_gas_per_block(1_000_000)
        .preconf_timeout_ms(1_500)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // 20_000 < 21_000 (intrinsic for empty-call transfer). Pool validator
    // rejects at admission with an `IntrinsicGasTooLow`-flavoured error.
    let raw_tx = signed_transfer(chain_id, &wallet, 0, 20_000).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("sub-intrinsic-gas tx must be rejected by the pool validator");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("pool rejected"),
                "expected `PreconfError::PoolRejected(...)` wrapping; got {}",
                e.message(),
            );
            assert!(
                msg.contains("intrinsic gas") ||
                    msg.contains("gas limit too low") ||
                    msg.contains("intrinsic") ||
                    msg.contains("gas too low"),
                "expected inner substring naming intrinsic-gas rejection; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "pool intrinsic-gas rejection must fail fast; took {elapsed:?}",
    );
}

/// Insufficient funds is rejected at pool admission and surfaced as
/// `Err(Call { message: "pool rejected: <inner>" })`.
///
/// Setup: submit a whitelisted preconf tx with `value = U256::MAX` — the
/// sender balance (Hardhat[0]'s ~1e24 wei) cannot cover it. Pool
/// validator rejects during balance/nonce state assembly.
///
/// Wire contract pinned as in `intrinsic_gas_too_low_pool_rejects`, with
/// the inner substring matching "insufficient" / "funds" / "balance".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insufficient_funds_pool_rejects() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        .preconf_timeout_ms(1_500)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // `U256::MAX` is well beyond Hardhat[0]'s preallocation (~1e24 wei).
    // Pool's account-state check fires before any preconf-specific gate.
    let raw_tx = signed_transfer_with_value(chain_id, &wallet, 0, U256::MAX).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("over-balance tx must be rejected by the pool validator");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("pool rejected"),
                "expected `PreconfError::PoolRejected(...)` wrapping; got {}",
                e.message(),
            );
            assert!(
                msg.contains("insufficient") || msg.contains("funds") || msg.contains("balance"),
                "expected inner substring naming insufficient-funds rejection; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "pool insufficient-funds rejection must fail fast; took {elapsed:?}",
    );
}

/// `tx.gas_limit > block.gas_limit` is rejected at pool admission and
/// surfaced as `Err(Call { message: "pool rejected: <inner>" })`.
///
/// Setup: `block_gas_limit` is `30_000_000` (0x1c9c380, see `assets/genesis.json`).
/// Submit a whitelisted preconf tx with `gas_limit = 40_000_000`. The
/// preconf per-tx ceiling is deliberately set to `100_000_000` so it does
/// NOT catch this first — the underlying pool validator's block-gas-limit
/// check is what fires.
///
/// Distinct from `per_tx_gas_ceiling_rejected_at_rpc_not_by_the_pool`, and the
/// contrast is the point: there the preconf-specific ceiling fires at the RPC
/// *before the pool is ever asked*, so the client gets
/// `PreconfError::PreconfGasLimitExceeded`. Here both preconf caps are lifted
/// out of the way on purpose, so the transaction reaches the pool and comes back
/// wrapped in `PoolRejected` — the reth-native `ExceedsGasLimit` flavour, which
/// SDKs cannot distinguish from any other pool rejection without parsing the
/// message. The two tests pin the two sides of that boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_gas_limit_exceeded_pool_rejects() {
    let recipient: Address = RECIPIENT.parse().unwrap();
    let wallet_addr = Wallet::default().with_chain_id(1).inner.address();

    let cfg = PreconfCfgBuilder::new()
        .whitelist_from(wallet_addr)
        .whitelist_to(recipient)
        // Both caps well above 30M so the preconf-specific per-tx and
        // per-block gates do NOT fire before the pool's block_gas_limit
        // check.
        .max_gas_per_tx(100_000_000)
        .max_gas_per_block(100_000_000)
        .preconf_timeout_ms(1_500)
        .build();

    let (_node, http, wallet, chain_id) = launch_preconf_node!(cfg).await;

    // 40M > 30M block_gas_limit → pool rejects.
    let raw_tx = signed_transfer(chain_id, &wallet, 0, 40_000_000).await;
    let start = std::time::Instant::now();
    let err = send_preconf(&http, raw_tx)
        .await
        .expect_err("tx gas_limit above block gas_limit must be rejected by the pool validator");
    let elapsed = start.elapsed();

    match err {
        ClientError::Call(ref e) => {
            let msg = e.message().to_lowercase();
            assert!(
                msg.contains("pool rejected"),
                "expected `PreconfError::PoolRejected(...)` wrapping; got {}",
                e.message(),
            );
            assert!(
                msg.contains("gas limit") || msg.contains("gas_limit") || msg.contains("exceeds"),
                "expected inner substring naming gas-limit-exceeded rejection; got {}",
                e.message(),
            );
        }
        other => panic!("expected Call error, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "pool block-gas-limit rejection must fail fast; took {elapsed:?}",
    );
}
