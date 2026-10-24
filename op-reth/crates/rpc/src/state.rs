//! State provider factory for OP Proofs ExEx.

use alloy_eips::BlockId;
use derive_more::Constructor;
use jsonrpsee_types::error::ErrorObject;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStorageError, OpProofsStore, api::OpProofsProviderRO,
    provider::OpProofsStateProviderRef,
};
use reth_provider::{BlockIdReader, BlockNumReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use tracing::debug;

/// [MANTLE] How far behind the chain head a block missing from the proofs window may be and
/// still be answered from reth's own state.
///
/// The proofs engine writes to the sidecar in batches (every few blocks, or after an idle
/// interval), so the newest canonical blocks are briefly missing from the window. Reth's own
/// proof path is cheap this close to the head: the distance matches the changeset cache reth
/// keeps behind its persisted tip (`CHANGESET_CACHE_RETENTION_BLOCKS` in the engine tree).
pub const NEAR_HEAD_FALLBACK_BLOCKS: u64 = 64;

/// [MANTLE] Where the state for a requested block comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateSource {
    /// The block is inside the proofs window.
    Sidecar,
    /// The block is canonical and close to the head but not written to the sidecar yet.
    Reth,
    /// Neither: below the window, or too far above it.
    Missing,
}

/// [MANTLE] Picks the state source for `block`. The window's lower bound stays strict, so old
/// blocks never fall back to reth's slow historical proof path.
const fn state_source(block: u64, earliest: u64, latest: u64, head: u64) -> StateSource {
    if block < earliest {
        StateSource::Missing
    } else if block <= latest {
        StateSource::Sidecar
    } else if block <= head && head - block <= NEAR_HEAD_FALLBACK_BLOCKS {
        StateSource::Reth
    } else {
        StateSource::Missing
    }
}

/// Creates a factory for state providers using OP Proofs external proofs storage.
#[derive(Debug, Clone, Constructor)]
pub struct OpStateProviderFactory<Eth, P> {
    eth_api: Eth,
    preimage_store: OpProofsStorage<P>,
}

impl<'a, Eth, P> OpStateProviderFactory<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: OpProofsStore + Clone + 'a,
{
    /// Acquires a permit from the eth API's tracing-task semaphore (`--rpc.proof-permits`).
    ///
    /// `eth_getProof` on the sidecar is otherwise unbounded: the stock reth path and the Mantle
    /// `rpc-ext` override both take this permit before walking the trie, so hold it here too to
    /// cap how many historical-proof walks run at once.
    pub async fn acquire_tracing_permit(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
        self.eth_api.acquire_owned_tracing().await
    }

    /// [MANTLE] Returns the eth API, whose blocking-task pool runs proof walks.
    pub const fn eth_api(&self) -> &Eth {
        &self.eth_api
    }

    /// Creates a state provider for the given block id.
    ///
    /// [MANTLE] A canonical block above the proofs window and within
    /// [`NEAR_HEAD_FALLBACK_BLOCKS`] of the head is served from reth's own state, so the newest
    /// blocks stay queryable while the proofs engine has not written them yet.
    pub async fn state_provider(
        &'a self,
        block_id: Option<BlockId>,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let block_id = block_id.unwrap_or_default();
        // Check whether the distance to the block exceeds the maximum configured window.
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;

        let historical_provider =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;

        let provider_ro = self.preimage_store.provider_ro().map_err(ProviderError::from)?;

        // An empty proof window is semantically equivalent to an out-of-window lookup for
        // the proofs RPC: there's no proof for that block. Any other storage error is real
        // and propagates up.
        let window = match provider_ro.get_proof_window() {
            Ok(w) => w,
            Err(OpProofsStorageError::NoBlocksFound) => {
                return Err(ProviderError::StateForNumberNotFound(block_number));
            }
            Err(err) => return Err(ProviderError::from(err)),
        };

        let head = self.eth_api.provider().best_block_number()?;
        match state_source(block_number, window.earliest.number, window.latest.number, head) {
            StateSource::Sidecar => {
                let external_overlay_provider =
                    OpProofsStateProviderRef::new(historical_provider, provider_ro, block_number);
                Ok(Box::new(external_overlay_provider))
            }
            StateSource::Reth => {
                debug!(
                    target: "rpc::proofs",
                    block_number,
                    window_latest = window.latest.number,
                    head,
                    "block not in the proofs window yet, using reth state"
                );
                Ok(historical_provider)
            }
            StateSource::Missing => Err(ProviderError::StateForNumberNotFound(block_number)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [MANTLE] The sidecar answers inside its window, reth answers just above it near the head,
    /// and everything else stays missing.
    #[test]
    fn state_source_falls_back_only_near_the_head() {
        let (earliest, latest, head) = (100, 200, 203);
        for (block, expected) in [
            (99, StateSource::Missing),
            (100, StateSource::Sidecar),
            (200, StateSource::Sidecar),
            (201, StateSource::Reth),
            (203, StateSource::Reth),
            (204, StateSource::Missing),
        ] {
            assert_eq!(state_source(block, earliest, latest, head), expected, "block {block}");
        }

        // A sidecar far behind the head: only the last `NEAR_HEAD_FALLBACK_BLOCKS` fall back.
        let head = 1_000;
        let oldest_fallback = head - NEAR_HEAD_FALLBACK_BLOCKS;
        assert_eq!(state_source(oldest_fallback, 0, 500, head), StateSource::Reth);
        assert_eq!(state_source(oldest_fallback - 1, 0, 500, head), StateSource::Missing);

        // Blocks below the window never fall back, however close to the head they are.
        assert_eq!(state_source(9, 10, 10, 12), StateSource::Missing);
    }
}
