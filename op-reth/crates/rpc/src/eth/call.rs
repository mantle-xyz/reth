use crate::{OpEthApi, OpEthApiError, eth::RpcNodeCore};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_primitives::U256;
use alloy_rpc_types_eth::state::StateOverride;
use reth_chainspec::ChainSpecProvider;
use reth_optimism_evm::extract_l1_info;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::Block;
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert, RpcTxReq,
    helpers::{Call, EthCall, estimate::EstimateCall},
};
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};

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
            let estimate =
                EstimateCall::estimate_gas_at(self, request.clone(), at, state_override).await?;

            // [MANTLE] Post-estimation Arsia balance check (op-geth v1.5.5 mantleArsiaCheckFunds)
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
                    let Ok(state) =
                        self.provider().state_by_block_hash(block.header().parent_hash()) &&
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
