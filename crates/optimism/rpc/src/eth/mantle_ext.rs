//! Mantle Eth API extension implementation.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::Block;
use jsonrpsee::types::ErrorObject;
use jsonrpsee_core::RpcResult;
use reth_rpc_eth_api::{MantleEthApiServer, PreconfTxEvent};
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};

/// Mantle-specific `Eth` API extensions implementation.
///
/// This provides Mantle-specific RPC methods such as `getBlockRange` and
/// `sendRawTransactionWithPreconf`.
#[derive(Clone, Debug)]
pub struct MantleEthExtApi<Provider> {
    /// The provider type used to interact with the node.
    #[allow(dead_code)]
    // Will be used when implementing get_block_range and send_raw_transaction_with_preconf
    provider: Provider,
}

impl<Provider> MantleEthExtApi<Provider>
where
    Provider: BlockReaderIdExt + StateProviderFactory + Clone + 'static,
{
    /// Creates a new [`MantleEthExtApi`].
    #[allow(clippy::missing_const_for_fn)] // Provider type is generic and cannot be const
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }

    #[inline]
    #[allow(dead_code, clippy::missing_const_for_fn)] // Will be used when implementing methods
    fn provider(&self) -> &Provider {
        &self.provider
    }
}

#[async_trait::async_trait]
impl<Provider> MantleEthApiServer for MantleEthExtApi<Provider>
where
    Provider: BlockReaderIdExt + StateProviderFactory + Clone + 'static,
{
    async fn get_block_range(
        &self,
        _start_number: BlockNumberOrTag,
        _end_number: BlockNumberOrTag,
        _full_transactions: bool,
    ) -> RpcResult<Vec<Block>> {
        // TODO: Implement getBlockRange for Mantle
        Err(ErrorObject::owned(-32000, "getBlockRange is not yet implemented", None::<()>))
    }

    async fn send_raw_transaction_with_preconf(&self, _bytes: Bytes) -> RpcResult<PreconfTxEvent> {
        // TODO: Implement sendRawTransactionWithPreconf for Mantle
        Err(ErrorObject::owned(
            -32000,
            "sendRawTransactionWithPreconf is not yet implemented",
            None::<()>,
        ))
    }
}
