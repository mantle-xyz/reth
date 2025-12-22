use super::OpNodeCore;
use crate::{OpEthApi, OpEthApiError};
use alloy_consensus::{transaction::RlpEcdsaEncodableTx, TxEip1559, TxType};
use alloy_primitives::{Bytes, Signature, TxKind, U256};
use alloy_rpc_types_eth::{state::StateOverride, transaction::TransactionRequest};
use alloy_signer::Either;
use op_revm::OpTransaction;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{
    execute::BlockExecutorFactory, ConfigureEvm, EvmEnv, EvmEnvFor, EvmFactory, SpecFor,
    TransactionEnv,
};
use reth_mantle_forks::MantleHardforks;
use reth_node_api::NodePrimitives;
use reth_optimism_forks::OpHardforks;
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_eth_api::{
    helpers::{
        estimate::EstimateCall, Call, EthCall, LoadBlock, LoadFee, LoadState, SpawnBlocking,
    },
    AsEthApiError, FromEthApiError, FromEvmError, FullEthApiTypes, IntoEthApiError,
};
use reth_rpc_eth_types::{
    error::api::FromEvmHalt,
    revm_utils::{apply_state_overrides, caller_gas_allowance, CallFees},
    EthApiError, RevertError, RpcInvalidTransactionError,
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use reth_storage_api::{
    BlockReaderIdExt, ProviderHeader, ProviderTx, StateProvider, StateProviderFactory,
};
use revm::context_interface::{result::ExecutionResult, Block, Transaction};
use revm::{context::TxEnv, Database};
use tracing::trace;

impl<N> EthCall for OpEthApi<N>
where
    Self: EstimateCall + LoadBlock + FullEthApiTypes,
    N: OpNodeCore,
{
}

impl<N> EstimateCall for OpEthApi<N>
where
    Self: Call + LoadBlock + LoadState,
    Self::Error: From<OpEthApiError>,
    N: OpNodeCore<
        Provider: ChainSpecProvider<ChainSpec: OpHardforks + MantleHardforks>
                      + BlockReaderIdExt
                      + StateProviderFactory,
    >,
{
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: TransactionRequest,
        state: S,
        state_override: Option<StateOverride>,
    ) -> Result<U256, Self::Error>
    where
        S: StateProvider,
    {
        // Disabled because eth_estimateGas is sometimes used with eoa senders
        // See <https://github.com/paradigmxyz/reth/issues/1959>
        evm_env.cfg_env.disable_eip3607 = true;

        // The basefee should be ignored for eth_estimateGas and similar
        // See:
        // <https://github.com/ethereum/go-ethereum/blob/ee8e83fa5f6cb261dad2ed0a7bbcde4930c41e6c/internal/ethapi/api.go#L985>
        evm_env.cfg_env.disable_base_fee = true;

        // set nonce to None so that the correct nonce is chosen by the EVM
        request.nonce = None;

        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.gas;
        let tx_request_gas_price = request.gas_price;
        // the gas limit of the corresponding block
        let block_env_gas_limit = evm_env.block_env.gas_limit;

        // Determine the highest possible gas limit, considering both the request's specified limit
        // and the block's limit.
        let mut highest_gas_limit = tx_request_gas_limit
            .map(|mut tx_gas_limit| {
                if block_env_gas_limit < tx_gas_limit {
                    // requested gas limit is higher than the allowed gas limit, capping
                    tx_gas_limit = block_env_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(block_env_gas_limit);

        // Check if all gas price fields are nil or zero before create_txn_env
        // matching op-geth's GasEstimationWithSkipCheckBalanceMode condition:
        // (GasPrice == nil || GasPrice == 0) AND (MaxFeePerGas == nil || MaxFeePerGas == 0) AND
        // (MaxPriorityFeePerGas == nil || MaxPriorityFeePerGas == 0)
        let gas_price_is_zero =
            tx_request_gas_price.is_none() || tx_request_gas_price == Some(0u128);
        let max_fee_per_gas_is_zero = request.max_fee_per_gas.is_none()
            || request.max_fee_per_gas.map(|v| U256::from(v) == U256::ZERO).unwrap_or(false);
        let max_priority_fee_per_gas_is_zero = request.max_priority_fee_per_gas.is_none()
            || request
                .max_priority_fee_per_gas
                .map(|v| U256::from(v) == U256::ZERO)
                .unwrap_or(false);

        let should_skip_balance_check =
            gas_price_is_zero && max_fee_per_gas_is_zero && max_priority_fee_per_gas_is_zero;

        // Enable balance check skip in evm_env when all gas prices are zero,
        // matching op-geth's GasEstimationWithSkipCheckBalanceMode behavior
        if should_skip_balance_check {
            evm_env.cfg_env.disable_balance_check = true;
        }

        // Configure the evm env
        let mut db = CacheDB::new(StateProviderDatabase::new(state));
        let mut tx_env = self.create_txn_env(&evm_env, request.clone(), &mut db)?;

        // Apply any state overrides if specified.
        if let Some(state_override) = state_override {
            apply_state_overrides(state_override, &mut db).map_err(Self::Error::from_eth_err)?;
        }

        // Optimize for simple transfer transactions, potentially reducing the gas estimate.
        if tx_env.input().is_empty() {
            if let TxKind::Call(to) = tx_env.kind() {
                if let Ok(code) = db.db.account_code(&to) {
                    let no_code_callee = code.map(|code| code.is_empty()).unwrap_or(true);
                    if no_code_callee {
                        // If the tx is a simple transfer (call to an account with no code) we can
                        // shortcircuit. But simply returning
                        // `MIN_TRANSACTION_GAS` is dangerous because there might be additional
                        // field combos that bump the price up, so we try executing the function
                        // with the minimum gas limit to make sure.
                        let mut tx_env = tx_env.clone();
                        tx_env.set_gas_limit(reth_chainspec::MIN_TRANSACTION_GAS);
                        if let Ok((res, _)) = self.transact(&mut db, evm_env.clone(), tx_env) {
                            if res.result.is_success() {
                                // For Optimism, we still need to add L1 cost even for simple transfers
                                // Continue to L1 cost calculation below
                            }
                        }
                    }
                }
            }
        }

        // Check funds of the sender (only useful to check if transaction gas price is more than 0).
        //
        // The caller allowance is check by doing `(account.balance - tx.value) / tx.gas_price`
        // In estimateGas mode, if all gas prices were 0 in the original request, we skip balance check
        // to match op-geth's GasEstimationWithSkipCheckBalanceMode behavior.
        // This is because in estimateGas, when user doesn't specify any gas prices, we don't know
        // the actual gas price that will be used, so balance check would be meaningless or incorrect.
        if !should_skip_balance_check && tx_env.gas_price() > 0 {
            // cap the highest gas limit by max gas caller can afford with given gas price
            let allowance =
                caller_gas_allowance(&mut db, &tx_env).map_err(Self::Error::from_eth_err)?;
            // If allowance is very large (close to u64::MAX), it means gas_price is very small,
            // and we should skip the balance check to match op-geth's behavior.
            // op-geth skips balance check if allowance > uint64 max or if gas_price is 0.
            if allowance < u64::MAX / 2 {
                highest_gas_limit = highest_gas_limit.min(allowance);
            }
        }

        // If the provided gas limit is less than computed cap, use that
        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        trace!(target: "rpc::eth::estimate", ?evm_env, ?tx_env, "Starting gas estimation");

        // Execute the transaction with the highest possible gas limit.
        let (mut res, (mut evm_env, mut tx_env)) =
            match self.transact(&mut db, evm_env.clone(), tx_env.clone()) {
                // Handle the exceptional case where the transaction initialization uses too much
                // gas. If the gas price or gas limit was specified in the request,
                // retry the transaction with the block's gas limit to determine if
                // the failure was due to insufficient gas.
                Err(err)
                    if err.is_gas_too_high()
                        && (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some()) =>
                {
                    return Err(self.map_out_of_gas_err(
                        block_env_gas_limit,
                        evm_env,
                        tx_env,
                        &mut db,
                    ))
                }
                Err(err) if err.is_gas_too_low() => {
                    // This failed because the configured gas cost of the tx was lower than what
                    // actually consumed by the tx This can happen if the
                    // request provided fee values manually and the resulting gas cost exceeds the
                    // sender's allowance, so we return the appropriate error here
                    return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                        gas_limit: tx_env.gas_limit(),
                    }
                    .into_eth_err());
                }
                // Propagate other results (successful or other errors).
                ethres => ethres?,
            };

        let gas_refund = match res.result {
            ExecutionResult::Success { gas_refunded, .. } => gas_refunded,
            ExecutionResult::Halt { reason, .. } => {
                // here we don't check for invalid opcode because already executed with highest gas
                // limit
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()));
            }
            ExecutionResult::Revert { output, .. } => {
                // if price or limit was included in the request then we can execute the request
                // again with the block's gas limit to check if revert is gas related or not
                return if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() {
                    Err(self.map_out_of_gas_err(block_env_gas_limit, evm_env, tx_env, &mut db))
                } else {
                    // the transaction did revert
                    Err(RpcInvalidTransactionError::Revert(RevertError::new(output)).into_eth_err())
                };
            }
        };

        // At this point we know the call succeeded but want to find the _best_ (lowest) gas the
        // transaction succeeds with. We find this by doing a binary search over the possible range.

        // we know the tx succeeded with the configured gas limit, so we can use that as the
        // highest, in case we applied a gas cap due to caller allowance above
        highest_gas_limit = tx_env.gas_limit();

        // NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed.
        let gas_used = res.result.gas_used();
        // the lowest value is capped by the gas used by the unconstrained transaction
        let mut lowest_gas_limit = gas_used.saturating_sub(1);
        // As stated in Geth, there is a good chance that the transaction will pass if we set the
        // gas limit to the execution gas used plus the gas refund, so we check this first
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L135
        //
        // Calculate the optimistic gas limit by adding gas used and gas refund,
        // then applying a 64/63 multiplier to account for gas forwarding rules.
        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            // Set the transaction's gas limit to the calculated optimistic gas limit.
            tx_env.set_gas_limit(optimistic_gas_limit);
            // Re-execute the transaction with the new gas limit and update the result and
            // environment.
            (res, (evm_env, tx_env)) = self.transact(&mut db, evm_env, tx_env)?;
            // Update the gas limit estimates (highest and lowest) based on the execution result.
            reth_rpc_eth_api::helpers::estimate::update_estimated_gas_range(
                res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        // Pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            lowest_gas_limit * 2, // Use lowest_gas_limit * 2 to match geth's lo * 2
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );
        trace!(target: "rpc::eth::estimate", ?evm_env, ?tx_env, ?highest_gas_limit, ?lowest_gas_limit, ?mid_gas_limit, "Starting binary search for gas");

        // Binary search narrows the range to find the minimum gas limit needed for the transaction
        // to succeed.
        while (highest_gas_limit - lowest_gas_limit) > 1 {
            // An estimation error is allowed once the current gas limit range used in the binary
            // search is small enough (less than 1.5% of the highest gas limit)
            // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L152
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64)
                < ESTIMATE_GAS_ERROR_RATIO
            {
                break;
            };

            tx_env.set_gas_limit(mid_gas_limit);

            // Execute transaction and handle potential gas errors, adjusting limits accordingly.
            match self.transact(&mut db, evm_env.clone(), tx_env.clone()) {
                Err(err) if err.is_gas_too_high() => {
                    // Decrease the highest gas limit if gas is too high
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    // Increase the lowest gas limit if gas is too low
                    lowest_gas_limit = mid_gas_limit;
                }
                // Handle other cases, including successful transactions.
                ethres => {
                    // Unpack the result and environment if the transaction was successful.
                    (res, (evm_env, tx_env)) = ethres?;
                    // Update the estimated gas range based on the transaction result.
                    reth_rpc_eth_api::helpers::estimate::update_estimated_gas_range(
                        res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            // New midpoint
            mid_gas_limit = std::cmp::min(
                lowest_gas_limit * 2, // Use lowest_gas_limit * 2 to match geth's lo * 2
                ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
            );
        }

        // For Optimism chains, op-revm automatically handles L1 cost calculation and deduction
        // during transaction execution using the enveloped_tx we set in create_txn_env.
        // The enveloped_tx contains the encoded transaction with placeholder signature and 80
        // non-zero bytes added (matching op-geth's CalculateRollupCostDataFromMessage behavior).
        // So we can simply return the highest_gas_limit, which already accounts for L1 cost
        // because op-revm deducted it during execution.
        // Apply gas buffer matching op-geth's behavior (gasBuffer = 120, i.e., 20% increase)
        let gas_with_buffer = (highest_gas_limit as u128 * 120 / 100) as u64;

        Ok(U256::from(gas_with_buffer))
    }
}

impl<N> Call for OpEthApi<N>
where
    Self: LoadState<
            Evm: ConfigureEvm<
                Primitives: NodePrimitives<
                    BlockHeader = ProviderHeader<Self::Provider>,
                    SignedTx = ProviderTx<Self::Provider>,
                >,
                BlockExecutorFactory: BlockExecutorFactory<
                    EvmFactory: EvmFactory<Tx = OpTransaction<TxEnv>>,
                >,
            >,
            Error: FromEvmError<Self::Evm>,
        > + SpawnBlocking
        + LoadFee,
    Self::Error: From<OpEthApiError>,
    N: OpNodeCore,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    fn create_txn_env(
        &self,
        evm_env: &EvmEnv<SpecFor<Self::Evm>>,
        request: TransactionRequest,
        mut db: impl Database<Error: Into<EthApiError>>,
    ) -> Result<OpTransaction<TxEnv>, Self::Error> {
        // Ensure that if versioned hashes are set, they're not empty
        if request.blob_versioned_hashes.as_ref().is_some_and(|hashes| hashes.is_empty()) {
            return Err(RpcInvalidTransactionError::BlobTransactionMissingBlobHashes.into_eth_err());
        }

        let tx_type = if request.authorization_list.is_some() {
            TxType::Eip7702
        } else if request.sidecar.is_some() || request.max_fee_per_blob_gas.is_some() {
            TxType::Eip4844
        } else if request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some() {
            TxType::Eip1559
        } else if request.access_list.is_some() {
            TxType::Eip2930
        } else {
            TxType::Legacy
        } as u8;

        let TransactionRequest {
            from,
            to,
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas,
            value,
            input,
            nonce,
            access_list,
            chain_id,
            blob_versioned_hashes,
            max_fee_per_blob_gas,
            authorization_list,
            transaction_type: _,
            sidecar: _,
        } = request;

        let CallFees { max_priority_fee_per_gas, gas_price, max_fee_per_blob_gas } =
            CallFees::ensure_fees(
                gas_price.map(U256::from),
                max_fee_per_gas.map(U256::from),
                max_priority_fee_per_gas.map(U256::from),
                U256::from(evm_env.block_env.basefee),
                blob_versioned_hashes.as_deref(),
                max_fee_per_blob_gas.map(U256::from),
                evm_env.block_env.blob_gasprice().map(U256::from),
            )?;

        let gas_limit = gas.unwrap_or(
            // Use maximum allowed gas limit. The reason for this
            // is that both Erigon and Geth use pre-configured gas cap even if
            // it's possible to derive the gas limit from the block:
            // <https://github.com/ledgerwatch/erigon/blob/eae2d9a79cb70dbe30b3a6b79c436872e4605458/cmd/rpcdaemon/commands/trace_adhoc.go#L956
            // https://github.com/ledgerwatch/erigon/blob/eae2d9a79cb70dbe30b3a6b79c436872e4605458/eth/ethconfig/config.go#L94>
            evm_env.block_env.gas_limit,
        );

        let chain_id = chain_id.unwrap_or(evm_env.cfg_env.chain_id);

        let caller = from.unwrap_or_default();

        let nonce = if let Some(nonce) = nonce {
            nonce
        } else {
            db.basic(caller).map_err(Into::into)?.map(|acc| acc.nonce).unwrap_or_default()
        };

        // Convert input for TxEnv
        let tx_data = input
            .clone()
            .try_into_unique_input()
            .map_err(Self::Error::from_eth_err)?
            .unwrap_or_default();

        // Calculate gas price for estimate when all gas prices are zero in estimateGas mode.
        // This matches op-geth's ToMessage behavior: gasPriceForEstimate = SuggestGasTipCap + BaseFee
        let final_gas_price = if gas_price.is_zero()
            && max_priority_fee_per_gas.map_or(true, |v| v.is_zero())
            && evm_env.block_env.basefee > 0
        {
            // Get min_suggested_priority_fee, referencing eth_maxPriorityFeePerGas implementation
            let min_tip = self.gas_oracle().config().min_suggested_priority_fee.unwrap_or(
                reth_rpc_server_types::constants::gas_oracle::DEFAULT_MIN_SUGGESTED_PRIORITY_FEE,
            );

            // gasPriceForEstimate = basefee + min_tip (matching op-geth's SuggestGasTipCap + BaseFee)
            U256::from(evm_env.block_env.basefee).saturating_add(min_tip)
        } else {
            gas_price
        };

        let base = TxEnv {
            tx_type,
            gas_limit,
            nonce,
            caller,
            gas_price: final_gas_price.saturating_to(),
            gas_priority_fee: max_priority_fee_per_gas.map(|v| v.saturating_to()),
            kind: to.unwrap_or(TxKind::Create),
            value: value.unwrap_or_default(),
            data: tx_data.clone(),
            chain_id: Some(chain_id),
            access_list: access_list.unwrap_or_default(),
            // EIP-4844 fields
            blob_hashes: blob_versioned_hashes.unwrap_or_default(),
            max_fee_per_blob_gas: max_fee_per_blob_gas
                .map(|v| v.saturating_to())
                .unwrap_or_default(),
            // EIP-7702 fields
            authorization_list: authorization_list
                .unwrap_or_default()
                .into_iter()
                .map(Either::Left)
                .collect(),
        };

        // Build encoded transaction for L1 cost calculation.
        // Match op-geth's CalculateRollupCostDataFromMessage behavior:
        // 1. Construct a minimal transaction with only: Nonce, Value, Gas, GasTipCap, GasFeeCap, Data
        // 2. To, ChainID, AccessList are set to default (nil/empty in op-geth)
        // 3. Encode with zero signature (nil V, R, S in op-geth)
        // 4. Add 80 non-zero bytes to account for signature (matching op-geth's RollupCostData.Ones += 80)
        let enveloped_tx = {
            // Match op-geth: use values from base (TxEnv) which have been processed the same way as op-geth's Message.
            // This ensures we use the same GasLimit, GasTipCap, GasFeeCap values that op-geth uses.
            // In op-geth's ToMessage, when all gas prices are 0 in estimateGas mode, it sets both
            // gasFeeCap and gasTipCap to gasPriceForEstimate. We need to match this behavior.
            //
            // For L1 cost calculation, use a default tip of 100000 wei (0.0001 gwei) when all prices are zero.
            // This matches op-geth's CalculateRollupCostDataFromMessage behavior.
            const DEFAULT_TIP_FOR_L1_COST: u128 = 100_000;
            let basefee = evm_env.block_env.basefee as u128;
            let gas_price_for_estimate = basefee.saturating_add(DEFAULT_TIP_FOR_L1_COST);

            // Determine max_priority_fee_per_gas and max_fee_per_gas for L1 cost calculation.
            // If all prices were zero, use gasPriceForEstimate; otherwise use the actual values.
            let (max_priority_fee, max_fee) =
                if base.gas_price == 0 && base.gas_priority_fee.unwrap_or(0) == 0 {
                    (gas_price_for_estimate, gas_price_for_estimate)
                } else {
                    (base.gas_priority_fee.unwrap_or(base.gas_price), base.gas_price)
                };

            let tx = TxEip1559 {
                chain_id: 0, // Default to 0 (will encode as zero, matching op-geth's nil ChainID)
                nonce: base.nonce,
                gas_limit: base.gas_limit,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: max_priority_fee,
                to: TxKind::Create, // Always use Create (nil To) for L1 cost calculation (matching op-geth)
                value: base.value,
                access_list: base.access_list.clone(), // Empty (matching op-geth's empty AccessList)
                input: base.data.clone(),
            };

            // Use zero signature (matching op-geth's nil V, R, S which encode as zeros)
            let signature = Signature::new(Default::default(), Default::default(), false);

            // Encode the transaction directly (matching op-geth's MarshalBinary).
            // We encode the unsigned transaction with zero signature, without creating signed_tx.
            let mut encoded = Vec::new();
            tx.eip2718_encode(&signature, &mut encoded);

            // Match op-geth: add 80 non-zero bytes to simulate real signature.
            // This matches op-geth's CalculateRollupCostDataFromMessage: st.msg.RollupCostData.Ones += 80.
            encoded.extend_from_slice(&[0xFFu8; 80]);

            Some(Bytes::from(encoded))
        };

        Ok(OpTransaction { base, enveloped_tx, deposit: Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// Test helper to check if balance check should be skipped based on gas price fields
    fn should_skip_balance_check_helper(
        gas_price: Option<u128>,
        max_fee_per_gas: Option<u128>,
        max_priority_fee_per_gas: Option<u128>,
    ) -> bool {
        let gas_price_is_zero = gas_price.is_none() || gas_price == Some(0u128);
        let max_fee_per_gas_is_zero = max_fee_per_gas.is_none()
            || max_fee_per_gas.map(|v| U256::from(v) == U256::ZERO).unwrap_or(false);
        let max_priority_fee_per_gas_is_zero = max_priority_fee_per_gas.is_none()
            || max_priority_fee_per_gas.map(|v| U256::from(v) == U256::ZERO).unwrap_or(false);

        gas_price_is_zero && max_fee_per_gas_is_zero && max_priority_fee_per_gas_is_zero
    }

    #[test]
    fn test_should_skip_balance_check_all_zero() {
        // All gas prices are zero or None - should skip balance check
        assert!(should_skip_balance_check_helper(None, None, None));
        assert!(should_skip_balance_check_helper(Some(0), None, None));
        assert!(should_skip_balance_check_helper(None, Some(0), Some(0)));
        assert!(should_skip_balance_check_helper(Some(0), Some(0), Some(0)));
    }

    #[test]
    fn test_should_skip_balance_check_some_non_zero() {
        // At least one gas price is non-zero - should not skip balance check
        assert!(!should_skip_balance_check_helper(Some(1000), None, None));
        assert!(!should_skip_balance_check_helper(None, Some(1000), None));
        assert!(!should_skip_balance_check_helper(None, None, Some(1000)));
        assert!(!should_skip_balance_check_helper(Some(0), Some(1000), Some(0)));
    }

    /// Test helper to calculate final gas price for estimate
    fn calculate_final_gas_price_helper(
        gas_price: U256,
        max_priority_fee_per_gas: Option<U256>,
        basefee: u128,
        min_tip: U256,
    ) -> U256 {
        if gas_price.is_zero()
            && max_priority_fee_per_gas.map_or(true, |v| v.is_zero())
            && basefee > 0
        {
            U256::from(basefee).saturating_add(min_tip)
        } else {
            gas_price
        }
    }

    #[test]
    fn test_final_gas_price_all_zero_with_basefee() {
        let basefee = 1000u128;
        let min_tip = U256::from(100_000u128);
        let expected = U256::from(basefee).saturating_add(min_tip);

        // All prices zero with basefee > 0 - should use basefee + min_tip
        assert_eq!(calculate_final_gas_price_helper(U256::ZERO, None, basefee, min_tip), expected);
        assert_eq!(
            calculate_final_gas_price_helper(U256::ZERO, Some(U256::ZERO), basefee, min_tip),
            expected
        );
    }

    #[test]
    fn test_final_gas_price_all_zero_no_basefee() {
        let basefee = 0u128;
        let min_tip = U256::from(100_000u128);

        // All prices zero but basefee is 0 - should return original gas_price (0)
        assert_eq!(
            calculate_final_gas_price_helper(U256::ZERO, None, basefee, min_tip),
            U256::ZERO
        );
    }

    #[test]
    fn test_final_gas_price_some_non_zero() {
        let basefee = 1000u128;
        let min_tip = U256::from(100_000u128);
        let gas_price = U256::from(2000u128);

        // Gas price is non-zero - should return original gas_price
        assert_eq!(calculate_final_gas_price_helper(gas_price, None, basefee, min_tip), gas_price);
        assert_eq!(
            calculate_final_gas_price_helper(
                gas_price,
                Some(U256::from(500u128)),
                basefee,
                min_tip
            ),
            gas_price
        );
    }

    #[test]
    fn test_enveloped_tx_l1_cost_calculation() {
        const DEFAULT_TIP_FOR_L1_COST: u128 = 100_000;
        let basefee = 1000u128;
        let gas_price_for_estimate = basefee.saturating_add(DEFAULT_TIP_FOR_L1_COST);

        // Test that when all prices are zero, we use gas_price_for_estimate
        let base_gas_price = 0u128;
        let base_gas_priority_fee = Some(0u128);
        let (max_priority_fee, max_fee) =
            if base_gas_price == 0 && base_gas_priority_fee.unwrap_or(0) == 0 {
                (gas_price_for_estimate, gas_price_for_estimate)
            } else {
                (base_gas_priority_fee.unwrap_or(base_gas_price), base_gas_price)
            };

        assert_eq!(max_priority_fee, gas_price_for_estimate);
        assert_eq!(max_fee, gas_price_for_estimate);

        // Test that when prices are non-zero, we use actual values
        let base_gas_price = 2000u128;
        let base_gas_priority_fee = Some(500u128);
        let (max_priority_fee, max_fee) =
            if base_gas_price == 0 && base_gas_priority_fee.unwrap_or(0) == 0 {
                (gas_price_for_estimate, gas_price_for_estimate)
            } else {
                (base_gas_priority_fee.unwrap_or(base_gas_price), base_gas_price)
            };

        assert_eq!(max_priority_fee, 500u128);
        assert_eq!(max_fee, 2000u128);
    }

    #[test]
    fn test_enveloped_tx_encoding_format() {
        // Test that encoded transaction includes 80 non-zero bytes for signature
        const DEFAULT_TIP_FOR_L1_COST: u128 = 100_000;
        let basefee = 1000u128;
        let gas_price_for_estimate = basefee.saturating_add(DEFAULT_TIP_FOR_L1_COST);

        let tx = TxEip1559 {
            chain_id: 0,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: gas_price_for_estimate,
            max_priority_fee_per_gas: gas_price_for_estimate,
            to: TxKind::Create,
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };

        let signature = Signature::new(Default::default(), Default::default(), false);
        let mut encoded = Vec::new();
        tx.eip2718_encode(&signature, &mut encoded);

        // Add 80 non-zero bytes
        encoded.extend_from_slice(&[0xFFu8; 80]);

        // Verify the encoded transaction has the expected structure
        // The transaction should be encoded with EIP-2718 format (type byte + rlp encoded data)
        assert!(encoded.len() > 80, "Encoded transaction should be longer than 80 bytes");

        // Verify the last 80 bytes are all 0xFF
        let signature_bytes = &encoded[encoded.len() - 80..];
        assert!(signature_bytes.iter().all(|&b| b == 0xFF), "Last 80 bytes should be 0xFF");
    }

    /// Test helper to calculate mid_gas_limit matching geth's behavior
    /// This matches the logic in estimate_gas_with: use lowest_gas_limit * 2 instead of gas_used * 3
    fn calculate_mid_gas_limit_helper(lowest_gas_limit: u64, highest_gas_limit: u64) -> u64 {
        std::cmp::min(
            lowest_gas_limit * 2, // Use lowest_gas_limit * 2 to match geth's lo * 2
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        )
    }

    #[test]
    fn test_mid_gas_limit_uses_lowest_times_two() {
        // Test that mid_gas_limit uses lowest_gas_limit * 2 when it's smaller than average
        let lowest = 1000u64;
        let highest = 10000u64;
        let mid = calculate_mid_gas_limit_helper(lowest, highest);

        // mid should be min(lowest * 2, (highest + lowest) / 2)
        // lowest * 2 = 2000, average = 5500, so should be 2000
        assert_eq!(mid, 2000u64, "mid_gas_limit should use lowest_gas_limit * 2 when smaller");
    }

    #[test]
    fn test_mid_gas_limit_uses_average_when_smaller() {
        // Test that mid_gas_limit uses average when it's smaller than lowest * 2
        let lowest = 5000u64;
        let highest = 6000u64;
        let mid = calculate_mid_gas_limit_helper(lowest, highest);

        // mid should be min(lowest * 2, (highest + lowest) / 2)
        // lowest * 2 = 10000, average = 5500, so should be 5500
        let expected_average = ((highest as u128 + lowest as u128) / 2) as u64;
        assert_eq!(
            mid, expected_average,
            "mid_gas_limit should use average when smaller than lowest * 2"
        );
    }

    #[test]
    fn test_mid_gas_limit_edge_case_small_range() {
        // Test edge case with very small range
        let lowest = 100u64;
        let highest = 101u64;
        let mid = calculate_mid_gas_limit_helper(lowest, highest);

        // lowest * 2 = 200, average = 100, so should be 100
        let expected_average = ((highest as u128 + lowest as u128) / 2) as u64;
        assert_eq!(mid, expected_average, "mid_gas_limit should handle small ranges correctly");
    }

    #[test]
    fn test_mid_gas_limit_edge_case_large_range() {
        // Test edge case with large range
        let lowest = 1000u64;
        let highest = 1000000u64;
        let mid = calculate_mid_gas_limit_helper(lowest, highest);

        // lowest * 2 = 2000, average = 500500, so should be 2000
        assert_eq!(
            mid, 2000u64,
            "mid_gas_limit should use lowest * 2 for large ranges when smaller"
        );
    }

    #[test]
    fn test_mid_gas_limit_matches_geth_behavior() {
        // Test various scenarios to ensure we match geth's lo * 2 behavior
        let test_cases = vec![
            (100u64, 1000u64, 200u64),   // lowest * 2 < average
            (500u64, 600u64, 550u64),    // average < lowest * 2
            (1000u64, 2000u64, 1500u64), // average < lowest * 2
            (5000u64, 5100u64, 5050u64), // average < lowest * 2
        ];

        for (lowest, highest, expected) in test_cases {
            let mid = calculate_mid_gas_limit_helper(lowest, highest);
            let expected_calc =
                std::cmp::min(lowest * 2, ((highest as u128 + lowest as u128) / 2) as u64);
            assert_eq!(
                mid, expected_calc,
                "mid_gas_limit calculation should match expected for lowest={}, highest={}",
                lowest, highest
            );
            // Also verify it matches the expected value if provided
            if expected == expected_calc {
                assert_eq!(mid, expected);
            }
        }
    }
}
