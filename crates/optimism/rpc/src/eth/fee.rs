use super::{OpEthApi, OpNodeCore};
use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U256};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_primitives_traits::{Block, BlockBody};
use reth_rpc_eth_api::{
    helpers::{EthFees, LoadBlock, LoadFee},
    FromEthApiError,
};
use reth_rpc_eth_types::{EthApiError, FeeHistoryCache, GasPriceOracle};
use reth_rpc_server_types::constants::gas_oracle::DEFAULT_MIN_SUGGESTED_PRIORITY_FEE;
use reth_storage_api::{BlockReader, BlockReaderIdExt, ReceiptProvider, StateProviderFactory};

impl<N> LoadFee for OpEthApi<N>
where
    Self: LoadBlock<Provider = N::Provider>,
    N: OpNodeCore<
        Provider: BlockReaderIdExt
                      + ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
                      + StateProviderFactory,
    >,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache {
        self.inner.eth_api.fee_history_cache()
    }

    /// Optimism-specific priority fee suggestion
    async fn suggested_priority_fee(&self) -> Result<U256, Self::Error>
    where
        Self: 'static,
    {
        // Delegate to the Optimism-specific implementation that mirrors op-geth's SuggestOptimismPriorityFee
        self.suggest_optimism_priority_fee().await
    }
}

impl<N> EthFees for OpEthApi<N>
where
    Self: LoadFee,
    N: OpNodeCore,
{
}

impl<N> OpEthApi<N>
where
    N: OpNodeCore<
        Provider: BlockReaderIdExt
                      + ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
                      + StateProviderFactory,
    >,
{
    /// Optimism-specific gas price suggestion algorithm
    ///
    /// This implements the same algorithm as op-geth's `SuggestOptimismPriorityFee`:
    /// 1. Start with minimum suggested priority fee from config (default 0.0001 gwei)
    /// 2. Check if the last block is at capacity
    /// 3. If at capacity, return median + 10% of previous block's effective tips
    /// 4. Otherwise, return the minimum suggestion
    async fn suggest_optimism_priority_fee(
        &self,
    ) -> Result<U256, <Self as reth_rpc_eth_api::EthApiTypes>::Error> {
        // Retrieve minimum suggested priority fee from configuration, fallback to default if not configured
        let min_suggested_priority_fee = self.get_min_suggested_priority_fee();
        let mut suggestion = min_suggested_priority_fee;

        // Fetch the latest block header
        let header = self
            .inner
            .eth_api
            .provider()
            .sealed_header_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(<Self as reth_rpc_eth_api::EthApiTypes>::Error::from_eth_err)?
            .ok_or(EthApiError::HeaderNotFound(B256::ZERO.into()))?;

        // Fetch receipts for the latest block
        let receipts = self
            .inner
            .eth_api
            .provider()
            .receipts_by_block(alloy_eips::HashOrNumber::Hash(header.hash()))
            .map_err(<Self as reth_rpc_eth_api::EthApiTypes>::Error::from_eth_err)?;

        if let Some(receipts) = receipts {
            // Calculate maximum gas usage per transaction
            // Compute individual transaction gas consumption (non-cumulative)
            let mut max_tx_gas_used = 0;
            for (i, receipt) in receipts.iter().enumerate() {
                let gas_used = if i == 0 {
                    receipt.cumulative_gas_used()
                } else {
                    receipt.cumulative_gas_used() - receipts[i - 1].cumulative_gas_used()
                };
                max_tx_gas_used = max_tx_gas_used.max(gas_used);
            }

            // Check if block is at capacity using op-geth's logic: gas_used + max_tx_gas_used > gas_limit
            if header.gas_used() + max_tx_gas_used > header.gas_limit() {
                tracing::info!("Block is at capacity, calculating median + 10%");

                // Fetch block transactions for tip calculation
                let block = self
                    .inner
                    .eth_api
                    .provider()
                    .block_by_hash(header.hash())
                    .map_err(<Self as reth_rpc_eth_api::EthApiTypes>::Error::from_eth_err)?
                    .ok_or(EthApiError::HeaderNotFound(header.hash().into()))?;

                let base_fee = block.header().base_fee_per_gas().unwrap_or_default();
                let mut tips = Vec::new();

                // Collect effective tips from all transactions
                for tx in block.body().transactions_iter() {
                    if let Some(tip) = tx.effective_tip_per_gas(base_fee) {
                        tips.push(U256::from(tip));
                    }
                }
                if tips.is_empty() {
                    tracing::error!("block was at capacity but doesn't have transactions");
                    return Ok(suggestion);
                }

                // Sort tips and calculate median
                tips.sort_unstable();
                let median = tips[tips.len() / 2];

                // Apply 10% increase: median + median / 10
                let new_suggestion = median + median / U256::from(10);

                // Only use new suggestion if it exceeds the minimum threshold
                if new_suggestion > suggestion {
                    suggestion = new_suggestion;
                }

                tracing::debug!(
                    "Calculated suggestion: median={}, new_suggestion={}, final={}",
                    median,
                    new_suggestion,
                    suggestion
                );
            }
        }

        // Apply maximum price cap constraint
        if let Some(max_price) = self.inner.eth_api.gas_oracle().config().max_price {
            if suggestion > max_price {
                suggestion = max_price;
                tracing::info!("Capped suggestion to max_price: {}", max_price);
            }
        }

        tracing::info!("Final optimism priority fee suggestion: {}", suggestion);
        Ok(suggestion)
    }

    /// Retrieves the minimum suggested priority fee, following op-geth's configuration logic
    ///
    /// Implementation mirrors op-geth's behavior:
    /// 1. If `MinSuggestedPriorityFee` is configured and > 0, use the configured value
    /// 2. Otherwise, use the default value [`DEFAULT_MIN_SUGGESTED_PRIORITY_FEE`] (0.0001 gwei = 100,000 wei)
    fn get_min_suggested_priority_fee(&self) -> U256 {
        // Retrieve MinSuggestedPriorityFee from gas oracle configuration
        if let Some(config_value) =
            self.inner.eth_api.gas_oracle().config().min_suggested_priority_fee
        {
            if config_value > U256::ZERO {
                tracing::info!("Using configured min_suggested_priority_fee: {}", config_value);
                return config_value;
            }
        }

        // Fallback to default value if not configured or invalid
        tracing::info!(
            "Using default min_suggested_priority_fee: {}",
            DEFAULT_MIN_SUGGESTED_PRIORITY_FEE
        );
        U256::from(DEFAULT_MIN_SUGGESTED_PRIORITY_FEE)
    }
}
