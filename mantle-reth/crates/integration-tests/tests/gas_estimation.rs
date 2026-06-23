//! Smoke + behavioural tests for Mantle-specific gas estimation RPCs.
//!
//! These exercise the live `eth_estimateGas` / `eth_estimateTotalFee` paths through a
//! booted `MantleNode`, covering the Arsia funds-preflight wiring added in
//! `op-reth/crates/rpc/src/eth/call.rs` (port of op-geth `mantleArsiaCheckFunds` +
//! the `value > balance` pre-check). Numerical accuracy is cross-checked against geth
//! by `tests/rpc_compat`; the pure formula is unit-tested in `mantle-reth-eth-api`.

use crate::helpers::with_mantle_rpc_client;
use alloy_primitives::U256;
use jsonrpsee::core::client::ClientT;

/// Pre-funded Hardhat account from `assets/genesis.json` (holds ~1M ETH).
const FUNDED: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
/// Second pre-funded Hardhat account.
const FUNDED_2: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
/// An address with no genesis allocation — balance is zero.
const UNFUNDED: &str = "0x000000000000000000000000000000000000dEaD";

/// `eth_estimateGas` for a simple transfer returns >= 21000 via RPC.
///
/// No block mining needed — estimateGas works against genesis state.
#[tokio::test]
async fn estimate_gas_simple_transfer_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        let gas: U256 = client
            .request(
                "eth_estimateGas",
                vec![serde_json::json!({
                    "from": FUNDED,
                    "to": FUNDED_2,
                    "value": "0x1"
                })],
            )
            .await
            .expect("eth_estimateGas should succeed");

        assert!(gas >= U256::from(21_000u64), "expected >= 21000, got {gas}");
    })
    .await;
}

/// A value transfer the caller cannot afford is rejected up front with an
/// "insufficient funds for transfer" error — this is the `value >= balance` pre-check
/// in the Mantle `estimate_gas_at` override (op-geth `gasestimator.go` clause 6). Like
/// geth, the check only runs when a fee cap is set, so the request specifies
/// `maxFeePerGas`; it does not require a mined L1-info block.
#[tokio::test]
async fn estimate_gas_value_exceeds_balance_rejected_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        // UNFUNDED has zero balance; any non-zero value exceeds it. A fee cap is set so
        // the geth-gated value pre-check (feeCap != 0) actually runs.
        let res: Result<U256, _> = client
            .request(
                "eth_estimateGas",
                vec![serde_json::json!({
                    "from": UNFUNDED,
                    "to": FUNDED,
                    "value": "0xde0b6b3a7640000", // 1 ETH
                    "maxFeePerGas": "0x3b9aca00"   // 1 gwei → fee gate is open
                })],
            )
            .await;

        let err =
            res.expect_err("estimateGas must reject a value transfer from a zero-balance account");
        let msg = err.to_string();
        assert!(
            msg.contains("insufficient funds for transfer"),
            "expected 'insufficient funds for transfer', got: {msg}"
        );
    })
    .await;
}

/// With no value and no fee specified, an unfunded account still gets an estimate:
/// the `value > balance` pre-check is skipped (value == 0) and the Arsia funds
/// preflight is skipped (no fee → `fee_cap == 0`, mirroring op-geth's
/// `GasEstimationWithSkipCheckBalanceMode`). Documents the skip gates.
#[tokio::test]
async fn estimate_gas_zero_value_unfunded_skips_funds_check_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        let gas: U256 = client
            .request(
                "eth_estimateGas",
                vec![serde_json::json!({
                    "from": UNFUNDED,
                    "to": FUNDED
                })],
            )
            .await
            .expect("estimateGas with no value/fee should skip the funds check and succeed");

        assert!(gas >= U256::from(21_000u64), "expected >= 21000, got {gas}");
    })
    .await;
}

/// `eth_estimateTotalFee` is reachable on an Arsia chain and returns a structurally
/// valid total (L2 gas + L1 data + operator fee). Shares the estimateGas path with
/// `eth_estimateGas` (op-geth `DoEstimateGas`), so the funds-preflight wiring is
/// exercised transitively.
#[tokio::test]
async fn estimate_total_fee_simple_transfer_via_rpc() {
    with_mantle_rpc_client(|client| async move {
        let total: U256 = client
            .request(
                "eth_estimateTotalFee",
                // positional params: [request, block?] — block defaults to latest.
                vec![serde_json::json!({
                    "from": FUNDED,
                    "to": FUNDED_2,
                    "value": "0x1"
                })],
            )
            .await
            .expect("eth_estimateTotalFee should succeed on an Arsia chain");

        // L2 gas estimate is always >= 21000; the total fee is gas * price + L1 + operator,
        // so it must be representable (no panic / overflow). On the zero-base-fee genesis
        // state the numeric value may be small, so we only assert the call shape here —
        // numerical parity with geth lives in tests/rpc_compat.
        let _ = total;
    })
    .await;
}
