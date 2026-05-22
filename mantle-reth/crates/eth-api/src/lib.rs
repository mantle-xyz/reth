//! Mantle-specific Eth API helpers.
//!
//! Provides [`mantle_arsia_check_funds`] which validates that the caller has sufficient balance
//! to cover L2 gas + L1 data fee + operator fee + value transfer, matching op-geth v1.5.5's
//! `mantleArsiaCheckFunds` in `eth/gasestimator/gasestimator.go`.

use alloy_primitives::U256;
use op_revm::L1BlockInfo;
use reth_optimism_forks::OpHardforks;

/// Error when caller's balance is insufficient for gas + L1 + operator + value.
#[derive(Debug, thiserror::Error)]
#[error(
    "insufficient funds for gas + L1 data fee + operator fee + value: have {balance}, need {total}"
)]
pub struct MantleInsufficientFunds {
    /// Total cost required.
    pub total: U256,
    /// Actual balance available.
    pub balance: U256,
}

/// Input for [`mantle_arsia_check_funds`].
#[derive(Debug)]
pub struct ArsiaFundsCheck<'a, C: ?Sized> {
    /// Estimated gas limit.
    pub gas_limit: u64,
    /// Effective fee cap (`max_fee_per_gas` or `gas_price`).
    pub fee_cap: U256,
    /// Value being transferred.
    pub value: U256,
    /// Caller's balance.
    pub from_balance: U256,
    /// L1 block info (for L1 cost + operator fee).
    pub l1_block_info: &'a L1BlockInfo,
    /// Transaction input data.
    pub tx_input: &'a [u8],
    /// Chain spec for hardfork queries.
    pub chain_spec: &'a C,
    /// Block timestamp.
    pub timestamp: u64,
}

/// Checks that the caller can afford `gas_limit * fee_cap + L1_fee + operator_fee + value`.
///
/// Port of op-geth v1.5.5 `mantleArsiaCheckFunds` (`eth/gasestimator/gasestimator.go`):
/// - Skips if `fee_cap == 0` (`GasEstimationWithSkipCheckBalanceMode`)
/// - Skips if Mantle Arsia not active
/// - L1 fee: `l1_block_info.calculate_tx_l1_cost_for_estimate(input, spec, 80)` (+80 bytes
///   overhead)
/// - Operator fee: `gas_limit * scalar * 100 + constant`
/// - Total: `gas_limit * fee_cap + l1_cost + operator_cost + value`
pub fn mantle_arsia_check_funds(
    check: &ArsiaFundsCheck<'_, impl OpHardforks>,
) -> Result<(), MantleInsufficientFunds> {
    // Skip if fee cap is zero (GasEstimationWithSkipCheckBalanceMode)
    if check.fee_cap.is_zero() {
        return Ok(());
    }
    // Skip if not Mantle Arsia
    if !check.chain_spec.is_mantle_arsia_active_at_timestamp(check.timestamp) {
        return Ok(());
    }

    let spec_id = resolve_mantle_spec_id(check.chain_spec, check.timestamp);

    // L1 data fee with +80 bytes geth signature overhead
    let l1_cost =
        check.l1_block_info.calculate_tx_l1_cost_for_estimate(check.tx_input, spec_id, 80);

    // Operator fee: gas_limit * scalar * 100 + constant
    let operator_cost = {
        let scalar = check.l1_block_info.operator_fee_scalar.unwrap_or(U256::ZERO);
        let constant = check.l1_block_info.operator_fee_constant.unwrap_or(U256::ZERO);
        U256::from(check.gas_limit)
            .saturating_mul(scalar)
            .saturating_mul(U256::from(100))
            .saturating_add(constant)
    };

    // L2 execution cost
    let l2_cost = U256::from(check.gas_limit).saturating_mul(check.fee_cap);

    // Total
    let total =
        l2_cost.saturating_add(l1_cost).saturating_add(operator_cost).saturating_add(check.value);

    if total > check.from_balance {
        return Err(MantleInsufficientFunds { total, balance: check.from_balance });
    }
    Ok(())
}

/// Resolve the Mantle-specific `OpSpecId` for a given timestamp.
pub fn resolve_mantle_spec_id(chain_spec: &impl OpHardforks, timestamp: u64) -> op_revm::OpSpecId {
    if chain_spec.is_mantle_arsia_active_at_timestamp(timestamp) {
        op_revm::OpSpecId::ARSIA
    } else if chain_spec.is_mantle_limb_active_at_timestamp(timestamp) {
        op_revm::OpSpecId::OSAKA
    } else if chain_spec.is_mantle_skadi_active_at_timestamp(timestamp) {
        op_revm::OpSpecId::ISTHMUS
    } else {
        alloy_op_evm::spec_by_timestamp_after_bedrock(chain_spec, timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantle_reth_chainspec::MANTLE_MAINNET;
    use reth_optimism_chainspec::OP_DEV;

    /// Arsia timestamp on Mantle mainnet — tests use this to enter the Arsia code path.
    fn arsia_ts() -> u64 {
        // Any timestamp after Arsia activation will do.
        // MANTLE_MAINNET has Arsia active at the known mainnet timestamp.
        // We use a far-future value so it's guaranteed active.
        u64::MAX
    }

    fn l1_info_with_operator_fee(scalar: u64, constant: u64) -> L1BlockInfo {
        L1BlockInfo {
            operator_fee_scalar: Some(U256::from(scalar)),
            operator_fee_constant: Some(U256::from(constant)),
            ..Default::default()
        }
    }

    // --- existing test ---

    #[test]
    fn check_funds_skips_when_fee_cap_zero() {
        let info = L1BlockInfo::default();
        assert!(
            mantle_arsia_check_funds(&ArsiaFundsCheck {
                gas_limit: 21_000,
                fee_cap: U256::ZERO,
                value: U256::ZERO,
                from_balance: U256::ZERO,
                l1_block_info: &info,
                tx_input: &[],
                chain_spec: OP_DEV.as_ref(),
                timestamp: 0,
            })
            .is_ok()
        );
    }

    // --- new tests ---

    #[test]
    fn check_funds_skips_pre_arsia() {
        // On OP_DEV (non-Mantle chain), the check always passes regardless of balance.
        let info = l1_info_with_operator_fee(1, 1_000_000);
        assert!(
            mantle_arsia_check_funds(&ArsiaFundsCheck {
                gas_limit: 21_000,
                fee_cap: U256::from(1_000_000_000u64), // 1 gwei
                value: U256::ZERO,
                from_balance: U256::ZERO, // zero balance
                l1_block_info: &info,
                tx_input: &[],
                chain_spec: OP_DEV.as_ref(),
                timestamp: arsia_ts(),
            })
            .is_ok(),
            "pre-Arsia (non-Mantle) should skip the balance check entirely"
        );
    }

    #[test]
    fn check_funds_sufficient_balance() {
        // L2 cost = 21000 * 10 gwei = 210_000 gwei
        // Operator cost = 21000 * 0 * 100 + 0 = 0 (default zeros)
        // L1 cost ≈ 0 (empty input)
        // Total ≈ 210_000 gwei
        let info = L1BlockInfo::default();
        let fee_cap = U256::from(10_000_000_000u64); // 10 gwei
        let l2_cost = U256::from(21_000u64) * fee_cap;

        // Give exactly enough
        assert!(
            mantle_arsia_check_funds(&ArsiaFundsCheck {
                gas_limit: 21_000,
                fee_cap,
                value: U256::ZERO,
                from_balance: l2_cost, // exactly enough
                l1_block_info: &info,
                tx_input: &[],
                chain_spec: MANTLE_MAINNET.as_ref(),
                timestamp: arsia_ts(),
            })
            .is_ok()
        );
    }

    #[test]
    fn check_funds_insufficient_balance() {
        let info = L1BlockInfo::default();
        let fee_cap = U256::from(10_000_000_000u64); // 10 gwei
        let l2_cost = U256::from(21_000u64) * fee_cap;

        // Give 1 wei less than needed
        let result = mantle_arsia_check_funds(&ArsiaFundsCheck {
            gas_limit: 21_000,
            fee_cap,
            value: U256::ZERO,
            from_balance: l2_cost - U256::from(1),
            l1_block_info: &info,
            tx_input: &[],
            chain_spec: MANTLE_MAINNET.as_ref(),
            timestamp: arsia_ts(),
        });

        assert!(result.is_err(), "should fail with 1 wei less than needed");
        let err = result.unwrap_err();
        assert!(err.total > err.balance);
    }

    #[test]
    fn check_funds_with_value_transfer() {
        let info = L1BlockInfo::default();
        let fee_cap = U256::from(10_000_000_000u64);
        let value = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let l2_cost = U256::from(21_000u64) * fee_cap;
        let total_needed = l2_cost + value;

        // Balance covers gas but not value
        let result = mantle_arsia_check_funds(&ArsiaFundsCheck {
            gas_limit: 21_000,
            fee_cap,
            value,
            from_balance: l2_cost, // enough for gas, not for value
            l1_block_info: &info,
            tx_input: &[],
            chain_spec: MANTLE_MAINNET.as_ref(),
            timestamp: arsia_ts(),
        });
        assert!(result.is_err(), "should fail when balance covers gas but not value");

        // Balance covers everything
        assert!(
            mantle_arsia_check_funds(&ArsiaFundsCheck {
                gas_limit: 21_000,
                fee_cap,
                value,
                from_balance: total_needed,
                l1_block_info: &info,
                tx_input: &[],
                chain_spec: MANTLE_MAINNET.as_ref(),
                timestamp: arsia_ts(),
            })
            .is_ok()
        );
    }

    #[test]
    fn check_funds_with_operator_fee() {
        // operator_cost = gas_limit * scalar * 100 + constant
        // = 21000 * 2 * 100 + 50000 = 4_250_000
        let info = l1_info_with_operator_fee(2, 50_000);
        let fee_cap = U256::from(10_000_000_000u64);
        let l2_cost = U256::from(21_000u64) * fee_cap;
        let operator_cost =
            U256::from(21_000u64) * U256::from(2u64) * U256::from(100u64) + U256::from(50_000u64);
        let total = l2_cost + operator_cost;

        // Enough for L2 gas but not operator fee
        let result = mantle_arsia_check_funds(&ArsiaFundsCheck {
            gas_limit: 21_000,
            fee_cap,
            value: U256::ZERO,
            from_balance: l2_cost, // not enough — missing operator cost
            l1_block_info: &info,
            tx_input: &[],
            chain_spec: MANTLE_MAINNET.as_ref(),
            timestamp: arsia_ts(),
        });
        assert!(result.is_err());

        // Enough for everything
        assert!(
            mantle_arsia_check_funds(&ArsiaFundsCheck {
                gas_limit: 21_000,
                fee_cap,
                value: U256::ZERO,
                from_balance: total,
                l1_block_info: &info,
                tx_input: &[],
                chain_spec: MANTLE_MAINNET.as_ref(),
                timestamp: arsia_ts(),
            })
            .is_ok()
        );
    }

    #[test]
    fn check_funds_with_calldata_l1_fee() {
        // Non-empty input increases L1 data fee.
        // With empty operator fee, the only extra cost beyond L2 gas is L1 data fee.
        let info = L1BlockInfo::default();
        let fee_cap = U256::from(10_000_000_000u64);
        let l2_cost = U256::from(21_000u64) * fee_cap;
        let calldata = vec![0xffu8; 256]; // 256 bytes of non-zero calldata

        // Exact L2 cost as balance — L1 data fee makes it insufficient
        let result = mantle_arsia_check_funds(&ArsiaFundsCheck {
            gas_limit: 21_000,
            fee_cap,
            value: U256::ZERO,
            from_balance: l2_cost, // only covers L2, not L1 data fee
            l1_block_info: &info,
            tx_input: &calldata,
            chain_spec: MANTLE_MAINNET.as_ref(),
            timestamp: arsia_ts(),
        });

        // Whether this fails depends on L1BlockInfo defaults (base fee = 0 → L1 cost = 0).
        // With default L1BlockInfo, L1 cost is 0, so this should pass.
        // This test documents the behavior: with zero L1 base fee, calldata doesn't add cost.
        assert!(result.is_ok(), "with default (zero) L1 base fee, calldata adds no L1 cost");

        // Now set a non-zero L1 base fee and token_ratio to make L1 data fee meaningful.
        // Mantle's L1 cost formula requires token_ratio > 0 to produce non-zero L1 fees.
        let info_with_l1_fee = L1BlockInfo {
            l1_base_fee: U256::from(30_000_000_000u64), // 30 gwei L1 base fee
            l1_base_fee_scalar: U256::from(5000u64),    // typical scalar
            token_ratio: U256::from(1u64),              // 1:1 ratio (simplest non-zero)
            ..Default::default()
        };

        let result = mantle_arsia_check_funds(&ArsiaFundsCheck {
            gas_limit: 21_000,
            fee_cap,
            value: U256::ZERO,
            from_balance: l2_cost, // only L2 cost — L1 data fee will push it over
            l1_block_info: &info_with_l1_fee,
            tx_input: &calldata,
            chain_spec: MANTLE_MAINNET.as_ref(),
            timestamp: arsia_ts(),
        });
        // L1 data fee with non-zero L1 base fee and 256 bytes should be significant
        // This should now fail (insufficient funds)
        assert!(result.is_err(), "with non-zero L1 base fee, calldata should add L1 cost");
    }
}
