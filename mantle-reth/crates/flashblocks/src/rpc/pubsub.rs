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
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use op_alloy_network::Optimism;
use reth_rpc::eth::EthPubSub as RethEthPubSub;
use reth_rpc_eth_api::{
    EthApiTypes, RpcBlock, RpcNodeCore, RpcTransaction,
    pubsub::EthPubSubApiServer as RethEthPubSubApiServer,
};
use reth_tasks::Runtime;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::{
    Stream, StreamExt,
    wrappers::{BroadcastStream, errors::BroadcastStreamRecvError},
};
use tracing::error;

use crate::{
    FlashblocksAPI, Metrics, TransactionWithLogs,
    rpc::types::{ExtendedSubscriptionKind, FlashblocksSubscriptionKind},
};

/// Eth pub-sub RPC extension for flashblocks and standard subscriptions.
#[rpc(server, namespace = "eth")]
pub trait EthPubSubApi {
    /// Create an Eth subscription for the given kind.
    ///
    /// Standard kinds are delegated to reth; `newFlashblocks`, `pendingLogs` and
    /// `newFlashblockTransactions` are served from flashblock state.
    ///
    /// `newFlashblocks` publishes the latest full preview block as a snapshot,
    /// not a slice delta. Its zero block hash is provisional; notifications do
    /// not provide reliable delivery or slice-gap detection. After reconnecting,
    /// query a fresh preview and reconcile finalized history with the canonical
    /// chain. Consumers needing slice indices and predecessor links must use
    /// the producer's raw flashblock stream.
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
        flashblock_updates(flashblocks_state.subscribe_to_flashblocks(), "newFlashblocks")
            .map(|pending_blocks| pending_blocks.get_latest_block(true))
    }

    /// Yields matching logs from the latest flashblock only, one per message.
    fn pending_logs_stream(flashblocks_state: Arc<FB>, filter: Filter) -> impl Stream<Item = Log>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                flashblock_updates(flashblocks_state.subscribe_to_flashblocks(), "pendingLogs"),
                move |pending_blocks| {
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
                flashblock_updates(
                    flashblocks_state.subscribe_to_flashblocks(),
                    "newFlashblockTransactions(full)",
                ),
                |pending_blocks| {
                    let txs = pending_blocks.get_latest_flashblock_transactions_with_logs();
                    if txs.is_empty() { None } else { Some(txs) }
                },
            ),
            stream::iter,
        )
    }

    /// Yields latest-flashblock transactions the filter selects, as
    /// [`FlashblocksSubscriptionKind::NewFlashblockTransactions`] defines it.
    fn new_flashblock_transactions_filtered_stream(
        flashblocks_state: Arc<FB>,
        filter: Filter,
    ) -> impl Stream<Item = TransactionWithLogs>
    where
        FB: FlashblocksAPI + Send + Sync + 'static,
    {
        futures::StreamExt::flat_map(
            StreamExt::filter_map(
                flashblock_updates(
                    flashblocks_state.subscribe_to_flashblocks(),
                    "newFlashblockTransactions(filtered)",
                ),
                move |pending_blocks| {
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
                flashblock_updates(
                    flashblocks_state.subscribe_to_flashblocks(),
                    "newFlashblockTransactions(hashes)",
                ),
                |pending_blocks| {
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

        if flashblocks_kind == FlashblocksSubscriptionKind::PendingLogs &&
            !matches!(params.as_ref(), None | Some(Params::None | Params::Logs(_)))
        {
            pending
                .reject(ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    "Invalid params for pendingLogs: expected a log filter object or null",
                    None::<()>,
                ))
                .await;
            return Ok(());
        }

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

/// The overlay updates one flashblocks subscription reads.
///
/// A subscriber that falls behind the broadcast loses the updates it dropped
/// and carries on from the oldest one still held; the subscriber is not told,
/// so `subscription_lagged_total` is the only record. The stream also counts
/// itself in `subscriptions_active` until it is dropped.
fn flashblock_updates<T>(
    updates: broadcast::Receiver<T>,
    subscription: &'static str,
) -> impl Stream<Item = T>
where
    T: Clone + Send + 'static,
{
    let open = OpenSubscription::new();
    StreamExt::filter_map(BroadcastStream::new(updates), move |update| {
        let _open = &open;
        match update {
            Ok(update) => Some(update),
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                Metrics::subscription_lagged().increment(skipped);
                error!(
                    subscription,
                    skipped, "flashblocks subscriber fell behind; updates skipped"
                );
                None
            }
        }
    })
}

/// Holds one count in `subscriptions_active` for as long as it lives.
#[derive(Debug)]
struct OpenSubscription;

impl OpenSubscription {
    fn new() -> Self {
        Metrics::subscriptions_active().increment(1.0);
        Self
    }
}

impl Drop for OpenSubscription {
    fn drop(&mut self) {
        Metrics::subscriptions_active().decrement(1.0);
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

#[cfg(test)]
mod tests {
    use futures::{StreamExt as _, executor::block_on};
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tokio::sync::broadcast;

    use super::flashblock_updates;

    /// A subscriber that falls behind loses what the channel dropped, and the
    /// subscription carries on rather than ending. The count of skipped updates
    /// is the only trace that anything was lost.
    #[test]
    fn a_lagging_subscription_counts_the_updates_it_skipped() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            let (tx, rx) = broadcast::channel(2);
            let updates = flashblock_updates(rx, "test");
            for update in 0..5u32 {
                tx.send(update).expect("the subscription is listening");
            }
            drop(tx);

            let received: Vec<u32> = block_on(updates.collect());
            assert_eq!(received, [3, 4], "the subscription keeps going after the gap");
        });

        let rendered = handle.render();
        assert!(
            rendered.lines().any(|line| line == "flashblocks_consumer_subscription_lagged_total 3"),
            "the three dropped updates must be counted:\n{rendered}",
        );
    }

    /// Counted while the stream exists, which is as long as the subscription
    /// task holds it.
    #[test]
    fn an_open_subscription_is_counted_until_it_is_dropped() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let active = |value: u32| format!("flashblocks_consumer_subscriptions_active {value}");

        metrics::with_local_recorder(&recorder, || {
            let (_tx, rx) = broadcast::channel::<u32>(2);
            let updates = flashblock_updates(rx, "test");
            let rendered = handle.render();
            assert!(rendered.lines().any(|line| line == active(1)), "{rendered}");

            drop(updates);
            let rendered = handle.render();
            assert!(rendered.lines().any(|line| line == active(0)), "{rendered}");
        });
    }
}
