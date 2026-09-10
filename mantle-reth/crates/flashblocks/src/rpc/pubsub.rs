//! `eth_` `PubSub` extension serving flashblocks alongside standard subscriptions.

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_eth::{Filter, Log, pubsub::Params};
use futures::stream;
use jsonrpsee::{
    PendingSubscriptionSink, SubscriptionSink,
    core::{SubscriptionResult, async_trait},
    proc_macros::rpc,
    server::SubscriptionMessage,
};
use op_alloy_network::Optimism;
use reth_rpc::eth::EthPubSub as RethEthPubSub;
use reth_rpc_eth_api::{
    EthApiTypes, RpcBlock, RpcNodeCore, RpcTransaction,
    pubsub::EthPubSubApiServer as RethEthPubSubApiServer,
};
use reth_tasks::Runtime;
use serde::Serialize;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::error;

use crate::{
    FlashblocksAPI, TransactionWithLogs,
    rpc::types::{ExtendedSubscriptionKind, FlashblocksSubscriptionKind},
};

/// Eth pub-sub RPC extension for flashblocks and standard subscriptions.
#[rpc(server, namespace = "eth")]
pub trait EthPubSubApi {
    /// Create an Eth subscription for the given kind.
    ///
    /// Standard kinds are delegated to reth; `newFlashblocks`, `pendingLogs` and
    /// `newFlashblockTransactions` are served from flashblock state.
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = serde_json::Value
    )]
    async fn subscribe(
        &self,
        kind: ExtendedSubscriptionKind,
        params: Option<Params>,
    ) -> SubscriptionResult;
}

/// `Eth` pubsub implementation that extends reth's with flashblocks support.
#[derive(Clone, Debug)]
pub struct EthPubSub<Eth, FB> {
    inner: RethEthPubSub<Eth>,
    flashblocks_state: Arc<FB>,
}

impl<Eth, FB> EthPubSub<Eth, FB> {
    /// Creates a new instance with the given eth API and flashblocks state.
    pub fn new(
        eth_api: Eth,
        subscription_task_spawner: Runtime,
        flashblocks_state: Arc<FB>,
    ) -> Self {
        Self { inner: RethEthPubSub::new(eth_api, subscription_task_spawner), flashblocks_state }
    }

    /// Yields each new flashblock as a full RPC block.
    fn new_flashblocks_stream(flashblocks_state: Arc<FB>) -> impl Stream<Item = RpcBlock<Optimism>>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()).filter_map(|result| {
            let pending_blocks = match result {
                Ok(blocks) => blocks,
                Err(err) => {
                    error!(message = "error in flashblocks stream", error = %err);
                    return None;
                }
            };
            Some(pending_blocks.get_latest_block(true))
        })
    }

    /// Yields matching logs from the latest flashblock only, one per message.
    fn pending_logs_stream(flashblocks_state: Arc<FB>, filter: Filter) -> impl Stream<Item = Log>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                move |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(message = "error in flashblocks stream for pending logs", error = %err);
                            return None;
                        }
                    };
                    let logs = pending_blocks.get_latest_flashblock_logs(&filter);
                    if logs.is_empty() { None } else { Some(logs) }
                },
            ),
            stream::iter,
        )
    }

    /// Yields transactions with their logs from the latest flashblock only.
    fn new_flashblock_transactions_full_stream(
        flashblocks_state: Arc<FB>,
    ) -> impl Stream<Item = TransactionWithLogs>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(message = "error in flashblocks stream for transactions", error = %err);
                            return None;
                        }
                    };
                    let txs = pending_blocks.get_latest_flashblock_transactions_with_logs();
                    if txs.is_empty() { None } else { Some(txs) }
                },
            ),
            stream::iter,
        )
    }

    /// Yields latest-flashblock transactions with at least one log matching the filter.
    fn new_flashblock_transactions_filtered_stream(
        flashblocks_state: Arc<FB>,
        filter: Filter,
    ) -> impl Stream<Item = TransactionWithLogs>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                move |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(message = "error in flashblocks stream for filtered transactions", error = %err);
                            return None;
                        }
                    };
                    let txs = pending_blocks
                        .get_latest_flashblock_transactions_with_logs_filtered(&filter);
                    if txs.is_empty() { None } else { Some(txs) }
                },
            ),
            stream::iter,
        )
    }

    /// Yields transaction hashes from the latest flashblock only.
    fn new_flashblock_transactions_hash_stream(
        flashblocks_state: Arc<FB>,
    ) -> impl Stream<Item = B256>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                BroadcastStream::new(flashblocks_state.subscribe_to_flashblocks()),
                |result| {
                    let pending_blocks = match result {
                        Ok(blocks) => blocks,
                        Err(err) => {
                            error!(message = "error in flashblocks stream for transaction hashes", error = %err);
                            return None;
                        }
                    };
                    let hashes = pending_blocks.get_latest_flashblock_transaction_hashes();
                    if hashes.is_empty() { None } else { Some(hashes) }
                },
            ),
            stream::iter,
        )
    }
}

#[async_trait]
impl<Eth, FB> EthPubSubApiServer for EthPubSub<Eth, FB>
where
    Eth: RpcNodeCore + EthApiTypes + Clone + Send + Sync + 'static,
    RethEthPubSub<Eth>: RethEthPubSubApiServer<RpcTransaction<Eth::NetworkTypes>>,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        kind: ExtendedSubscriptionKind,
        params: Option<Params>,
    ) -> SubscriptionResult {
        if let Some(standard_kind) = kind.as_standard() {
            return RethEthPubSubApiServer::subscribe(&self.inner, pending, standard_kind, params)
                .await;
        }

        let ExtendedSubscriptionKind::Flashblocks(flashblocks_kind) = kind else {
            unreachable!("standard subscription types are delegated to inner");
        };

        let sink = pending.accept().await?;

        match flashblocks_kind {
            FlashblocksSubscriptionKind::NewFlashblocks => {
                let stream = Self::new_flashblocks_stream(Arc::clone(&self.flashblocks_state));

                tokio::spawn(async move {
                    pipe_from_stream(sink, stream).await;
                });
            }
            FlashblocksSubscriptionKind::PendingLogs => {
                let filter = match params {
                    Some(Params::Logs(filter)) => *filter,
                    _ => Filter::default(),
                };

                let stream = Self::pending_logs_stream(Arc::clone(&self.flashblocks_state), filter);

                tokio::spawn(async move {
                    pipe_from_stream(sink, stream).await;
                });
            }
            FlashblocksSubscriptionKind::NewFlashblockTransactions => match params {
                Some(Params::Logs(filter)) => {
                    let stream = Self::new_flashblock_transactions_filtered_stream(
                        Arc::clone(&self.flashblocks_state),
                        *filter,
                    );
                    tokio::spawn(async move {
                        pipe_from_stream(sink, stream).await;
                    });
                }
                Some(Params::Bool(true)) => {
                    let stream = Self::new_flashblock_transactions_full_stream(Arc::clone(
                        &self.flashblocks_state,
                    ));
                    tokio::spawn(async move {
                        pipe_from_stream(sink, stream).await;
                    });
                }
                _ => {
                    let stream = Self::new_flashblock_transactions_hash_stream(Arc::clone(
                        &self.flashblocks_state,
                    ));
                    tokio::spawn(async move {
                        pipe_from_stream(sink, stream).await;
                    });
                }
            },
        }

        Ok(())
    }
}

/// Pipes all stream items to the subscription sink until the stream ends, the
/// client disconnects, or serialization fails.
async fn pipe_from_stream<T, St>(sink: SubscriptionSink, mut stream: St)
where
    St: Stream<Item = T> + Unpin,
    T: Serialize,
{
    loop {
        tokio::select! {
            _ = sink.closed() => return,

            maybe_item = stream.next() => {
                let Some(item) = maybe_item else {
                    return;
                };

                let msg = match SubscriptionMessage::new(
                    sink.method_name(),
                    sink.subscription_id(),
                    &item
                ) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!(
                            target: "flashblocks::pubsub",
                            %err,
                            "failed to serialize subscription message"
                        );
                        return;
                    }
                };

                if sink.send(msg).await.is_err() {
                    return;
                }
            }
        }
    }
}
