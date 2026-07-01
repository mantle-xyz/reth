use crate::{OpEthApi, OpEthApiError, eth::RpcNodeCore};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_network::TransactionBuilder;
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::state::StateOverride;
use reth_chainspec::{ChainSpecProvider, MIN_TRANSACTION_GAS};
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, TransactionEnvMut, overrides::apply_state_overrides};
use reth_optimism_evm::extract_l1_info;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::Block;
use reth_revm::{
    database::{EvmStateProvider, StateProviderDatabase},
    db::State,
};
use reth_rpc_eth_api::{
    AsEthApiError, FromEthApiError, FromEvmError, IntoEthApiError, RpcConvert, RpcTxReq,
    helpers::{
        Call, EthCall,
        estimate::{EstimateCall, update_estimated_gas_range},
    },
};
use reth_rpc_eth_types::{
    RpcInvalidTransactionError,
    error::api::{FromEvmHalt, FromRevert},
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use revm::{
    Database,
    context::Block as _,
    context_interface::{Cfg as _, Transaction, result::ExecutionResult},
    primitives::KECCAK_EMPTY,
};
use tracing::trace;

impl<N, Rpc> EthCall for OpEthApi<N, Rpc>
where
    N: RpcNodeCore<
        Provider: BlockReaderIdExt
                      + ChainSpecProvider<ChainSpec: OpHardforks>
                      + StateProviderFactory,
    >,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
{
    #[allow(clippy::manual_async_fn)]
    fn estimate_gas_at(
        &self,
        request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        at: BlockId,
        state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<U256, Self::Error>> + Send {
        async move {
            // [MANTLE] Pre-check: value transfer (op-geth `gasestimator.go` clause 6,
            // lines 98-105). geth only runs this when a fee cap is set
            // (`feeCap.BitLen() != 0`, i.e. not `GasEstimationWithSkipCheckBalanceMode`)
            // and rejects when `value >= balance`. We mirror both the fee gate and the
            // `>=` comparison, evaluated against the target block state (matching geth's
            // `StateAndHeaderByNumberOrHash`). Without a fee, upstream `estimate_gas_at`
            // skips its own `caller_gas_allowance` check too, so this stays gated to avoid
            // diverging from geth (which returns an estimate rather than erroring).
            if let Some(from) = request.as_ref().from {
                let value = request.as_ref().value.unwrap_or(U256::ZERO);
                let fee_cap = U256::from(
                    request
                        .as_ref()
                        .max_fee_per_gas
                        .unwrap_or(request.as_ref().gas_price.unwrap_or(0)),
                );
                if !value.is_zero() &&
                    !fee_cap.is_zero() &&
                    let Ok(Some(block)) = self.provider().block_by_id(at) &&
                    let Ok(state) = self.provider().state_by_block_id(at)
                {
                    let balance = state.account_balance(&from).ok().flatten().unwrap_or(U256::ZERO);
                    if value >= balance {
                        let hi = request.as_ref().gas.unwrap_or(block.header().gas_limit());
                        return Err(reth_rpc_eth_types::EthApiError::InvalidParams(format!(
                            "failed with {hi} gas: insufficient funds for transfer: address {from}"
                        ))
                        .into());
                    }
                }
            }

            let estimate =
                EstimateCall::estimate_gas_at(self, request.clone(), at, state_override).await?;

            // [MANTLE] Post-estimation Arsia balance check (op-geth v1.5.5 mantleArsiaCheckFunds)
            // geth uses target block state (opts.State from StateAndHeaderByNumberOrHash).
            let chain_spec = self.provider().chain_spec();
            if chain_spec.is_mantle() &&
                let Ok(Some(block)) = self.provider().block_by_id(at) &&
                chain_spec.is_mantle_arsia_active_at_timestamp(block.header().timestamp())
            {
                let fee_cap = U256::from(
                    request
                        .as_ref()
                        .max_fee_per_gas
                        .unwrap_or(request.as_ref().gas_price.unwrap_or(0)),
                );
                if !fee_cap.is_zero() &&
                    let Ok(mut l1_block_info) = extract_l1_info(block.body()) &&
                    let Ok(state) = self.provider().state_by_block_id(at) &&
                    let Some(from) = request.as_ref().from
                {
                    if let Ok(Some(ratio)) = state.storage(
                        op_revm::constants::GAS_ORACLE_CONTRACT,
                        op_revm::constants::TOKEN_RATIO_SLOT.into(),
                    ) {
                        l1_block_info.token_ratio = ratio;
                    }
                    let balance = state.account_balance(&from).ok().flatten().unwrap_or(U256::ZERO);
                    let input = request.as_ref().input.input().cloned().unwrap_or_default();

                    if let Err(e) = mantle_reth_eth_api::mantle_arsia_check_funds(
                        &mantle_reth_eth_api::ArsiaFundsCheck {
                            gas_limit: estimate.try_into().unwrap_or(u64::MAX),
                            fee_cap,
                            value: request.as_ref().value.unwrap_or(U256::ZERO),
                            from_balance: balance,
                            l1_block_info: &l1_block_info,
                            tx_input: &input,
                            chain_spec: chain_spec.as_ref(),
                            timestamp: block.header().timestamp(),
                        },
                    ) {
                        return Err(
                            reth_rpc_eth_types::EthApiError::InvalidParams(e.to_string()).into()
                        );
                    }
                }
            }

            Ok(estimate)
        }
    }
}

impl<N, Rpc> EstimateCall for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
{
    /// [MANTLE] Overrides upstream `estimate_gas_with` to gate the caller balance
    /// allowance by `maxFeePerGas` (matching op-geth `gasestimator.go`) instead of the
    /// effective gas price used by upstream reth, and to only take the basic-transfer
    /// short-circuit when that allowance permits it.
    ///
    /// Everything else is a verbatim copy of
    /// `reth_rpc_eth_api::helpers::estimate::EstimateCall::estimate_gas_with` (reth rev
    /// 88505c7). The two `[MANTLE]` blocks below are the only deviations. See
    /// `docs/op-reth-estimategas-fix-and-upstream.md` for the rationale.
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        state: S,
        state_override: Option<StateOverride>,
    ) -> Result<U256, Self::Error>
    where
        S: EvmStateProvider,
    {
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;
        evm_env.cfg_env.disable_fee_charge = true;

        // set nonce to None so that the correct nonce is chosen by the EVM
        request.as_mut().take_nonce();

        // [MANTLE] Capture the fee cap (maxFeePerGas, falling back to gasPrice) before
        // `create_txn_env` consumes `request`. op-geth gates the balance allowance by this
        // value (`gasestimator.go` L109, `feeCap = call.GasFeeCap`), whereas upstream reth
        // divides by `tx_env.gas_price()`, which the RPC path collapses to the effective
        // gas price `min(maxFee, base + tip)` via `CallFees::ensure_fees`.
        let fee_cap: u128 =
            request.as_ref().max_fee_per_gas.unwrap_or(request.as_ref().gas_price.unwrap_or(0));

        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.as_ref().gas_limit();
        let tx_request_gas_price = request.as_ref().gas_price();
        // the gas limit of the corresponding block
        let max_gas_limit = evm_env
            .cfg_env
            .tx_gas_limit_cap
            // If EIP-8037 is enabled, the transaction gas limit cap is not applicable
            .filter(|_| !evm_env.cfg_env.is_amsterdam_eip8037_enabled())
            .map_or_else(
                || evm_env.block_env.gas_limit(),
                |cap| cap.min(evm_env.block_env.gas_limit()),
            );

        // Determine the highest possible gas limit, considering both the request's specified
        // limit and the block's limit.
        let mut highest_gas_limit = tx_request_gas_limit
            .map(|mut tx_gas_limit| {
                if max_gas_limit < tx_gas_limit {
                    tx_gas_limit = max_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(max_gas_limit);

        // Configure the evm env
        let mut db = State::builder().with_database(StateProviderDatabase::new(state)).build();

        // Apply any state overrides if specified.
        if let Some(state_override) = state_override {
            apply_state_overrides(state_override, &mut db).map_err(Self::Error::from_eth_err)?;
        }

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        // Check if this is a basic transfer (no input data to account with no code)
        let is_basic_transfer = if tx_env.input().is_empty() &&
            let TxKind::Call(to) = tx_env.kind()
        {
            match db.database.basic_account(&to) {
                Ok(Some(account)) => {
                    account.bytecode_hash.is_none() || account.bytecode_hash == Some(KECCAK_EMPTY)
                }
                _ => true,
            }
        } else {
            false
        };

        // [MANTLE] Compute the caller's balance allowance using the *maxFeePerGas*
        // (`fee_cap`), matching op-geth `gasestimator.go:109`
        // (`allowance = (balance - value) / feeCap`). Upstream reth calls
        // `caller_gas_allowance`, which divides by `tx_env.gas_price()` — the effective
        // gas price `min(maxFee, base + tip)` on the RPC path — over-estimating the
        // allowance when `maxFee >> effective`. The execution `tx_env` keeps its effective
        // `gas_price`, so the GASPRICE opcode value is unchanged.
        //
        // `balance_allowance` is tracked separately from `highest_gas_limit` (which may also
        // be lowered by a user-supplied `gas`) so the basic-transfer short-circuit below can
        // gate purely on affordability, mirroring geth's `execute(21000)` buyGas check. When
        // no fee is set (`fee_cap == 0`), geth estimates in skip-balance mode
        // (`GasEstimationWithSkipCheckBalanceMode`), so the allowance is unbounded.
        let balance_allowance: u64 = if fee_cap > 0 {
            // Read the balance through the `State` overlay (`db.basic`, as
            // `caller_gas_allowance` does), NOT `db.database` — the latter is the raw
            // provider and bypasses `stateOverride` balances applied above.
            let balance = db
                .basic(tx_env.caller())
                .map_err(Self::Error::from_eth_err)?
                .map(|acc| acc.balance)
                .unwrap_or_default();
            balance
                .saturating_sub(tx_env.value())
                .checked_div(U256::from(fee_cap))
                .unwrap_or_default()
                .saturating_to()
        } else {
            u64::MAX
        };
        if fee_cap > 0 {
            // [MANTLE] Explicit fee: gate by maxFeePerGas (`balance_allowance`).
            highest_gas_limit = highest_gas_limit.min(balance_allowance);
        } else if tx_env.gas_price() > 0 {
            // No explicit fee: preserve upstream effective-based gating verbatim, so the
            // no-fee path (message and value) is unchanged from stock reth.
            highest_gas_limit =
                highest_gas_limit.min(self.caller_gas_allowance(&mut db, &evm_env, &tx_env)?);
        }

        // If the provided gas limit is less than computed cap, use that
        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        // Create EVM instance once and reuse it throughout the entire estimation process
        let mut evm = self.evm_config().evm_with_env(&mut db, evm_env);

        // [MANTLE] Only take the 21000 basic-transfer short-circuit when the caller can
        // actually afford 21000 gas at `maxFeePerGas` (`balance_allowance`). Upstream reth
        // returns `MIN_TRANSACTION_GAS` here unconditionally: estimation disables fee
        // charging (`disable_fee_charge`), so the short-circuit execution never checks the
        // maxFee balance and would hand back an unaffordable estimate. geth's equivalent
        // `execute(21000)` fails `buyGas` when the balance can't cover `21000 * maxFee` and
        // falls through to `gas required exceeds allowance`. We gate on `balance_allowance`
        // (not `highest_gas_limit`, which a low user-supplied `gas` can also reduce — geth
        // ignores a `gas` below the intrinsic cost, so gating on it would wrongly reject
        // those requests).
        if is_basic_transfer && balance_allowance >= MIN_TRANSACTION_GAS {
            // If the tx is a simple transfer (call to an account with no code) we can
            // shortcircuit. But simply returning `MIN_TRANSACTION_GAS` is dangerous because
            // there might be additional field combos that bump the price up, so we try
            // executing the function with the minimum gas limit to make sure.
            let mut min_tx_env = tx_env.clone();
            min_tx_env.set_gas_limit(MIN_TRANSACTION_GAS);

            // Reuse the same EVM instance
            if let Ok(res) = evm.transact(min_tx_env).map_err(Self::Error::from_evm_err) &&
                res.result.is_success()
            {
                return Ok(U256::from(MIN_TRANSACTION_GAS));
            }
        }

        trace!(target: "rpc::eth::estimate", ?tx_env, gas_limit = tx_env.gas_limit(), is_basic_transfer, "Starting gas estimation");

        // Execute the transaction with the highest possible gas limit.
        let mut res = match evm.transact(tx_env.clone()).map_err(Self::Error::from_evm_err) {
            Err(err)
                if err.is_gas_too_high() &&
                    (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some()) =>
            {
                return Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit);
            }
            Err(err) if err.is_gas_too_low() => {
                // This failed because the configured gas cost of the tx was lower than what
                // actually consumed by the tx. This can happen if the request provided fee
                // values manually and the resulting gas cost exceeds the sender's allowance,
                // so we return the appropriate error here
                return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                    gas_limit: tx_env.gas_limit(),
                }
                .into_eth_err());
            }
            // Propagate other results (successful or other errors).
            ethres => ethres?,
        };

        let gas_refund = match res.result {
            ExecutionResult::Success { gas, .. } => gas.final_refunded(),
            ExecutionResult::Halt { reason, .. } => {
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()));
            }
            ExecutionResult::Revert { output, .. } => {
                return if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() {
                    Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit)
                } else {
                    Err(Self::Error::from_revert(output))
                };
            }
        };

        // At this point we know the call succeeded but want to find the _best_ (lowest) gas
        // the transaction succeeds with. We find this by doing a binary search over the
        // possible range.

        // we know the tx succeeded with the configured gas limit, so we can use that as the
        // highest, in case we applied a gas cap due to caller allowance above
        highest_gas_limit = tx_env.gas_limit();

        // NOTE: this is the gas the transaction used, which is less than the transaction
        // requires to succeed.
        let mut gas_used = res.result.tx_gas_used();
        // the lowest value is capped by the gas used by the unconstrained transaction
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        // As stated in Geth, there is a good chance that the transaction will pass if we set
        // the gas limit to the execution gas used plus the gas refund, so we check this first
        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            let mut optimistic_tx_env = tx_env.clone();
            optimistic_tx_env.set_gas_limit(optimistic_gas_limit);
            res = evm.transact(optimistic_tx_env).map_err(Self::Error::from_evm_err)?;
            gas_used = res.result.tx_gas_used();
            update_estimated_gas_range(
                res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        // Pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        trace!(target: "rpc::eth::estimate", ?highest_gas_limit, ?lowest_gas_limit, ?mid_gas_limit, "Starting binary search for gas");

        // Binary search narrows the range to find the minimum gas limit needed for the
        // transaction to succeed.
        while lowest_gas_limit + 1 < highest_gas_limit {
            // An estimation error is allowed once the current gas limit range used in the
            // binary search is small enough (less than 1.5% of the highest gas limit)
            let ratio = (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64);
            if ratio < ESTIMATE_GAS_ERROR_RATIO {
                break;
            };

            let mut mid_tx_env = tx_env.clone();
            mid_tx_env.set_gas_limit(mid_gas_limit);

            match evm.transact(mid_tx_env).map_err(Self::Error::from_evm_err) {
                Err(err) if err.is_gas_too_high() => {
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    lowest_gas_limit = mid_gas_limit;
                }
                ethres => {
                    res = ethres?;
                    update_estimated_gas_range(
                        res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }
}

impl<N, Rpc> Call for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }
}
