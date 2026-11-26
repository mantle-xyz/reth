//! Historical proofs RPC server implementation.

use crate::{metrics::EthApiExtMetrics, state::OpStateProviderFactory};
use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::error::ErrorObject;
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_provider::StateProofProvider;
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_api::FromEthApiError;
use reth_rpc_eth_types::EthApiError;
use std::time::Instant;

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    /// Returns the account and storage values of the specified account including the Merkle-proof.
    /// This call can be used to verify that the data you are pulling from is not tampered with.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

#[derive(Debug)]
/// Overrides applied to the `eth_` namespace of the RPC API for historical proofs ExEx.
pub struct EthApiExt<Eth, P> {
    eth_api: Eth,
    preimage_store: OpProofsStorage<P>,
    metrics: EthApiExtMetrics,
}

impl<Eth, P> EthApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: OpProofsStore + Clone + 'static,
{
    /// Creates a new instance of the `EthApiExt`.
    pub fn new(eth_api: Eth, preimage_store: OpProofsStorage<P>) -> Self {
        Self { eth_api, preimage_store, metrics: EthApiExtMetrics::default() }
    }

    /// Computes the proof on the blocking IO pool so the trie walk does not run on an async
    /// worker thread.
    async fn get_proof_inner(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> Result<EIP1186AccountProofResponse, Eth::Error> {
        // Bound concurrent historical-proof walks with the eth API's tracing-task permit, as the
        // stock reth `eth_getProof` and the Mantle `rpc-ext` override both do. The key count
        // itself is not capped here — that gap is shared with upstream reth and tracked
        // separately.
        let permit = self
            .eth_api
            .acquire_owned_tracing()
            .await
            .map_err(|_| Eth::Error::from_eth_err(EthApiError::InternalEthError))?;
        let preimage_store = self.preimage_store.clone();

        self.eth_api
            .spawn_blocking_io_fut(move |eth_api| async move {
                // Hold the permit for the full lifetime of the blocking task, including after
                // the requesting future is cancelled.
                let _permit = permit;
                let state_provider_factory = OpStateProviderFactory::new(eth_api, preimage_store);
                let state = state_provider_factory
                    .state_provider(block_number)
                    .await
                    .map_err(Eth::Error::from_eth_err)?;
                let storage_keys = keys.iter().map(|key| key.as_b256()).collect::<Vec<_>>();
                let proof = state
                    .proof(Default::default(), address, &storage_keys)
                    .map_err(Eth::Error::from_eth_err)?;

                // [MANTLE] geth returns B256::ZERO for codeHash and storageHash on an account
                // that does not exist, where reth returns KECCAK_EMPTY / EMPTY_ROOT_HASH. Mantle
                // follows geth, so the exclusion-proof shape stays the one op-geth clients
                // already parse.
                Ok(proof.into_eip1186_response_with(keys, true))
            })
            .await
    }
}

#[async_trait]
impl<Eth, P> EthApiOverrideServer for EthApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: OpProofsStore + Clone + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        let start = Instant::now();
        self.metrics.get_proof_requests.increment(1);

        let result = self.get_proof_inner(address, keys, block_number).await.map_err(Into::into);

        match &result {
            Ok(_) => {
                self.metrics.get_proof_latency.record(start.elapsed().as_secs_f64());
                self.metrics.get_proof_successful_responses.increment(1);
            }
            Err(_) => self.metrics.get_proof_failures.increment(1),
        }

        result
    }
}
