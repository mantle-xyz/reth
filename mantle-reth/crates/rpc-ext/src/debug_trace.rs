//! `debug_traceBlock*` that reports a failed transaction trace on that transaction alone.
//!
//! Upstream's `DebugApi::trace_block` fails the whole call as soon as one transaction's tracer
//! fails — a JS `result` or hook that throws, a tracer timeout. op-geth instead returns
//! `{"error": …}` for that transaction and traces the rest of the block.
//!
//! These overrides return upstream's answer whenever it succeeds, so the success path stays
//! upstream's own code. Only when it fails is the block traced again here, one transaction at a
//! time, turning a tracer failure into a [`TraceResult::Error`] for that transaction.
//!
//! `debug_traceBadBlock` is not overridden: it reads upstream's private bad-block cache.

use alloy_consensus::{BlockHeader, transaction::TxHashRef};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::BlockError;
use alloy_rpc_types_trace::geth::{GethDebugTracingOptions, TraceResult};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, EvmEnvFor};
use reth_primitives_traits::{Block, BlockBody, RecoveredBlock};
use reth_rpc::DebugApi;
use reth_rpc_api::DebugApiServer;
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{
    FromEthApiError, RpcNodeCore,
    helpers::{EthTransactions, TraceExt},
};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::ProviderBlock;
use reth_tasks::pool::BlockingTaskGuard;
use revm::DatabaseCommit;
use revm_inspectors::tracing::{DebugInspector, TransactionContext};
use std::sync::Arc;

/// The `debug_traceBlock*` methods overridden by [`MantleDebugTrace`].
#[rpc(server, namespace = "debug")]
pub trait MantleDebugTraceApi {
    /// Traces each transaction of an RLP-encoded block.
    #[method(name = "traceBlock")]
    async fn debug_trace_block(
        &self,
        rlp_block: Bytes,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;

    /// Traces each transaction of the block with the given hash.
    #[method(name = "traceBlockByHash")]
    async fn debug_trace_block_by_hash(
        &self,
        block: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;

    /// Traces each transaction of the block with the given number.
    #[method(name = "traceBlockByNumber")]
    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;
}

/// Overrides the `debug_traceBlock*` methods; see the module docs.
pub struct MantleDebugTrace<Eth: RpcNodeCore> {
    upstream: DebugApi<Eth>,
    eth_api: Eth,
    /// Bounds concurrent re-traces. Upstream's tracing guard is private to `DebugApi`, so this is
    /// a separate one sized from the same `--rpc.max-tracing-requests`.
    tracing_guard: BlockingTaskGuard,
}

impl<Eth: RpcNodeCore> std::fmt::Debug for MantleDebugTrace<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MantleDebugTrace").finish_non_exhaustive()
    }
}

impl<Eth: RpcNodeCore> MantleDebugTrace<Eth> {
    /// Creates the overrides on top of upstream's `DebugApi` for the same `eth_api`.
    pub fn new(upstream: DebugApi<Eth>, eth_api: Eth, max_tracing_requests: usize) -> Self {
        Self { upstream, eth_api, tracing_guard: BlockingTaskGuard::new(max_tracing_requests) }
    }
}

impl<Eth> MantleDebugTrace<Eth>
where
    Eth: TraceExt + 'static,
{
    /// Re-traces the block with the given id, as upstream's `DebugApi::debug_trace_block` loads it.
    async fn retrace_block(
        &self,
        block_id: BlockId,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let evm_env = self.eth_api.evm_env_for_header(block.sealed_block().sealed_header())?;
        self.trace_each_transaction(block, evm_env, opts).await
    }

    /// Re-traces an RLP-encoded block, decoded as upstream's `DebugApi::debug_trace_raw_block`
    /// decodes it.
    async fn retrace_raw_block(
        &self,
        rlp_block: Bytes,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Eth::Error> {
        let block: ProviderBlock<Eth::Provider> = Decodable::decode(&mut rlp_block.as_ref())
            .map_err(BlockError::RlpDecodeRawBlock)
            .map_err(Eth::Error::from_eth_err)?;
        let evm_env = self
            .eth_api
            .evm_config()
            .evm_env(block.header())
            .map_err(RethError::other)
            .map_err(Eth::Error::from_eth_err)?;
        // Depending on EIP-2 the transactions are recovered differently.
        let senders = if self
            .eth_api
            .provider()
            .chain_spec()
            .is_homestead_active_at_block(block.header().number())
        {
            block.body().recover_signers()
        } else {
            block.body().recover_signers_unchecked()
        }
        .map_err(Eth::Error::from_eth_err)?;
        self.trace_each_transaction(
            Arc::new(block.into_recovered_with_signers(senders)),
            evm_env,
            opts,
        )
        .await
    }

    /// Upstream's `trace_block`, except that a transaction whose trace fails gets a
    /// [`TraceResult::Error`] and the block goes on.
    ///
    /// The failed transaction's state is taken from a second, untraced run: a tracer that fails
    /// can stop the EVM early, which would leave every later transaction traced against the wrong
    /// state. op-geth likewise advances the block state apart from the traced runs.
    async fn trace_each_transaction(
        &self,
        block: Arc<RecoveredBlock<ProviderBlock<Eth::Provider>>>,
        evm_env: EvmEnvFor<Eth::Evm>,
        opts: GethDebugTracingOptions,
    ) -> Result<Vec<TraceResult>, Eth::Error> {
        self.eth_api
            .spawn_with_state_at_block(block.parent_hash(), move |eth_api, mut db| {
                let mut results = Vec::with_capacity(block.body().transactions().len());

                eth_api.apply_pre_execution_changes(&block, &mut db)?;

                let mut transactions = block.transactions_recovered().enumerate().peekable();
                let mut inspector = DebugInspector::new(opts).map_err(Eth::Error::from_eth_err)?;
                while let Some((index, tx)) = transactions.next() {
                    let tx_hash = *tx.tx_hash();
                    let tx_env = eth_api.evm_config().tx_env(tx);

                    let res = eth_api.inspect(
                        &mut db,
                        evm_env.clone(),
                        tx_env.clone(),
                        &mut inspector,
                    )?;
                    let traced = inspector.get_result(
                        Some(TransactionContext {
                            block_hash: Some(block.hash()),
                            tx_hash: Some(tx_hash),
                            tx_index: Some(index),
                        }),
                        &tx_env,
                        &evm_env.block_env,
                        &res,
                        &mut db,
                    );
                    let state = match traced {
                        Ok(result) => {
                            results.push(TraceResult::Success { result, tx_hash: Some(tx_hash) });
                            res.state
                        }
                        Err(err) => {
                            results.push(TraceResult::Error {
                                error: EthApiError::from(err).to_string(),
                                tx_hash: Some(tx_hash),
                            });
                            eth_api.transact(&mut db, evm_env.clone(), tx_env)?.state
                        }
                    };

                    if transactions.peek().is_some() {
                        inspector.fuse().map_err(Eth::Error::from_eth_err)?;
                        // The next transaction executes on top of this one's state.
                        db.commit(state)
                    }
                }

                Ok(results)
            })
            .await
    }
}

#[async_trait]
impl<Eth> MantleDebugTraceApiServer for MantleDebugTrace<Eth>
where
    Eth: EthTransactions + TraceExt + 'static,
{
    async fn debug_trace_block(
        &self,
        rlp_block: Bytes,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        match <DebugApi<Eth> as DebugApiServer<RpcTxReq<Eth::NetworkTypes>>>::debug_trace_block(
            &self.upstream,
            rlp_block.clone(),
            opts.clone(),
        )
        .await
        {
            Ok(traces) => Ok(traces),
            Err(_) => {
                let _permit = self.tracing_guard.clone().acquire_owned().await;
                self.retrace_raw_block(rlp_block, opts.unwrap_or_default())
                    .await
                    .map_err(Into::into)
            }
        }
    }

    async fn debug_trace_block_by_hash(
        &self,
        block: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        match <DebugApi<Eth> as DebugApiServer<RpcTxReq<Eth::NetworkTypes>>>::debug_trace_block_by_hash(
            &self.upstream,
            block,
            opts.clone(),
        )
        .await
        {
            Ok(traces) => Ok(traces),
            Err(_) => {
                let _permit = self.tracing_guard.clone().acquire_owned().await;
                self.retrace_block(block.into(), opts.unwrap_or_default()).await.map_err(Into::into)
            }
        }
    }

    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        match <DebugApi<Eth> as DebugApiServer<RpcTxReq<Eth::NetworkTypes>>>::debug_trace_block_by_number(
            &self.upstream,
            block,
            opts.clone(),
        )
        .await
        {
            Ok(traces) => Ok(traces),
            Err(_) => {
                let _permit = self.tracing_guard.clone().acquire_owned().await;
                self.retrace_block(block.into(), opts.unwrap_or_default()).await.map_err(Into::into)
            }
        }
    }
}
