//! `WebSocket` subscription to the flashblock stream.

use std::{sync::Arc, time::Duration};

use futures::{SinkExt as _, StreamExt};
use mantle_reth_flashblocks_types::{MAX_DECOMPRESSED_FLASHBLOCK_BYTES, MantleFlashblockPayload};
use tokio::{
    sync::mpsc,
    time::{Instant, interval_at},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::protocol::{Message, WebSocketConfig},
};
use tracing::{error, info, trace, warn};
use url::Url;

use crate::{FlashblocksReceiver, metrics::Metrics};

/// Position of the last delivered flashblock, used to resume after a reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamPosition {
    block_number: u64,
    flashblock_index: u64,
}

/// Subscribes to flashblocks via `WebSocket` and forwards them to the receiver.
#[derive(Debug)]
pub struct FlashblocksSubscriber<Receiver> {
    flashblocks_state: Arc<Receiver>,
    ws_url: Url,
    ping_interval: Duration,
}

/// Appends `block_number` / `flashblock_index` resume parameters to `ws_url`.
///
/// Returns the base URL unchanged when no position is known or the result does
/// not parse.
fn resume_url(ws_url: &Url, position: Option<StreamPosition>) -> Url {
    let Some(StreamPosition { block_number, flashblock_index }) = position else {
        return ws_url.clone();
    };

    let base = ws_url.as_str();
    let separator = if ws_url.query().is_some() { "&" } else { "?" };
    let with_params =
        format!("{base}{separator}block_number={block_number}&flashblock_index={flashblock_index}");

    with_params.parse().unwrap_or_else(|e| {
        warn!(error = %e, url = %with_params, "failed to parse resume URL, falling back to base URL");
        ws_url.clone()
    })
}

impl<Receiver> FlashblocksSubscriber<Receiver>
where
    Receiver: FlashblocksReceiver + Send + Sync + 'static,
{
    /// Max duration of backoff before reconnecting to upstream.
    pub const MAX_BACKOFF: Duration = MAX_BACKOFF;

    /// Creates a new flashblocks subscriber.
    pub const fn new(
        flashblocks_state: Arc<Receiver>,
        ws_url: Url,
        ping_interval: Duration,
    ) -> Self {
        Self { ws_url, flashblocks_state, ping_interval }
    }

    /// Starts the `WebSocket` subscription to receive flashblocks.
    pub fn start(&mut self) {
        info!(message = "starting flashblocks subscription", url = %self.ws_url);

        let ws_url = self.ws_url.clone();
        let ping_period = self.ping_interval;

        let (sender, mut mailbox) = mpsc::channel(100);

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            let mut last_position: Option<StreamPosition> = None;

            loop {
                let connect_url = resume_url(&ws_url, last_position);
                let config = WebSocketConfig::default()
                    .max_message_size(Some(MAX_DECOMPRESSED_FLASHBLOCK_BYTES))
                    .max_frame_size(Some(MAX_DECOMPRESSED_FLASHBLOCK_BYTES));
                match connect_async_with_config(connect_url.as_str(), Some(config), false).await {
                    Ok((ws_stream, _)) => {
                        backoff = Duration::from_secs(1);
                        info!(message = "websocket connection established", url = %connect_url);

                        let mut ping_interval =
                            interval_at(Instant::now() + ping_period, ping_period);
                        let mut awaiting_pong_resp = false;

                        let (mut write, mut read) = ws_stream.split();

                        'conn: loop {
                            tokio::select! {
                                Some(msg) = read.next() => {
                                    Metrics::upstream_messages().increment(1);

                                    match msg {
                                        Ok(msg @ (Message::Binary(_) | Message::Text(_))) => {
                                            let bytes = msg.into_data();
                                            match MantleFlashblockPayload::try_decode_message(bytes.as_ref()) {
                                                Ok(payload) => {
                                                    // Recorded before dispatch so a drop right
                                                    // after resumes past this slice.
                                                    last_position = Some(StreamPosition {
                                                        block_number: payload.metadata.block_number,
                                                        flashblock_index: payload.index,
                                                    });
                                                    let _ = sender.send(payload).await.map_err(|e| {
                                                        error!(message = "failed to publish flashblock to channel", error = %e);
                                                    });
                                                }
                                                Err(e) => {
                                                    Metrics::upstream_decode_errors().increment(1);
                                                    error!(message = "error decoding flashblock message", error = %e);
                                                }
                                            }
                                        }
                                        Ok(Message::Close(_)) => {
                                            info!(message = "websocket connection closed by upstream");
                                            backoff = Self::sleep(backoff).await;
                                            break 'conn;
                                        }
                                        Ok(Message::Pong(data)) => {
                                            trace!(target: "flashblocks::subscription", ?data, "received pong from upstream");
                                            awaiting_pong_resp = false
                                        }
                                        Err(e) => {
                                            Metrics::upstream_errors().increment(1);
                                            error!(message = "error receiving message", error = %e);
                                            backoff = Self::sleep(backoff).await;
                                            break 'conn;
                                        }
                                        _ => {}
                                    }
                                },
                                _ = ping_interval.tick() => {
                                    if awaiting_pong_resp {
                                        warn!(
                                            target: "flashblocks::subscription",
                                            ?backoff,
                                            timeout = ?ping_period,
                                            "no pong response from upstream, reconnecting",
                                        );

                                        backoff = Self::sleep(backoff).await;
                                        break 'conn;
                                    }

                                    trace!(target: "flashblocks::subscription", "sending ping to upstream");

                                    if let Err(error) = write.send(Message::Ping(Default::default())).await {
                                        warn!(
                                            target: "flashblocks::subscription",
                                            ?backoff,
                                            %error,
                                            "websocket connection lost, reconnecting",
                                        );

                                        backoff = Self::sleep(backoff).await;
                                        break 'conn;
                                    }
                                    awaiting_pong_resp = true
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            message = "websocket connection error, retrying",
                            backoff_duration = ?backoff,
                            error = %e
                        );

                        backoff = Self::sleep(backoff).await;
                    }
                }
            }
        });

        let flashblocks_state = Arc::clone(&self.flashblocks_state);
        tokio::spawn(async move {
            while let Some(payload) = mailbox.recv().await {
                flashblocks_state.on_flashblock_received(payload);
            }
        });
    }

    /// Sleeps for the given backoff duration and returns the next, capped value.
    async fn sleep(backoff: Duration) -> Duration {
        Metrics::reconnect_attempts().increment(1);
        tokio::time::sleep(backoff).await;
        next_backoff(backoff)
    }
}

/// Longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// The wait after `current`: doubled, capped at [`MAX_BACKOFF`].
fn next_backoff(current: Duration) -> Duration {
    std::cmp::min(current.saturating_mul(2), MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q06_decode_errors_are_counted_without_changing_resume_or_network_errors() {
        use metrics_exporter_prometheus::PrometheusBuilder;
        use tokio::net::TcpListener;
        use tokio_tungstenite::{accept_async, accept_hdr_async};

        struct Receiver(mpsc::UnboundedSender<MantleFlashblockPayload>);
        impl FlashblocksReceiver for Receiver {
            fn on_flashblock_received(&self, payload: MantleFlashblockPayload) {
                self.0.send(payload).unwrap();
            }
        }

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                Metrics::init();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("ws://{}/ws", listener.local_addr().unwrap()).parse().unwrap();
                let payload = MantleFlashblockPayload { index: 3, ..Default::default() };
                let json = serde_json::to_string(&payload).unwrap();
                let (uri_tx, uri_rx) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut websocket = accept_async(stream).await.unwrap();
                    websocket.send(Message::text("invalid JSON")).await.unwrap();
                    websocket.send(Message::text(json)).await.unwrap();
                    websocket.send(Message::binary(vec![0xff; 32])).await.unwrap();
                    websocket.send(Message::Ping(Default::default())).await.unwrap();
                    websocket.close(None).await.unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut uri = String::new();
                    let _websocket = accept_hdr_async(
                        stream,
                        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                            uri = request.uri().to_string();
                            Ok(response)
                        },
                    )
                    .await
                    .unwrap();
                    uri_tx.send(uri).unwrap();
                    std::future::pending::<()>().await;
                });
                let (tx, mut rx) = mpsc::unbounded_channel();
                FlashblocksSubscriber::new(Arc::new(Receiver(tx)), url, Duration::from_secs(600))
                    .start();
                let received =
                    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
                assert_eq!(received, payload, "a valid slice after malformed data still arrives");
                let uri =
                    tokio::time::timeout(Duration::from_secs(5), uri_rx).await.unwrap().unwrap();
                assert_eq!(uri, "/ws?block_number=0&flashblock_index=3");
                let scrape = handle.render();
                assert!(
                    scrape.contains("flashblocks_consumer_upstream_decode_errors_total 2"),
                    "{scrape}"
                );
                assert!(
                    scrape.contains("flashblocks_consumer_upstream_errors_total 0"),
                    "{scrape}"
                );
                assert!(rx.try_recv().is_err(), "malformed data is never delivered");
                server.abort();
            })
        });
    }

    /// The schedule a dead upstream produces: 1, 2, 4, 8, then 10 for good.
    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let secs = |n: u64| Duration::from_secs(n);
        for (current, next) in [(1, 2), (2, 4), (4, 8), (8, 10), (10, 10), (7, 10)] {
            assert_eq!(next_backoff(secs(current)), secs(next), "after {current}s");
        }
    }

    #[test]
    fn resume_url_without_position_is_unchanged() {
        let url: Url = "ws://localhost:9999/ws".parse().unwrap();
        assert_eq!(resume_url(&url, None), url);
    }

    #[test]
    fn resume_url_appends_query_when_absent() {
        let url: Url = "ws://localhost:9999/ws".parse().unwrap();
        let resumed =
            resume_url(&url, Some(StreamPosition { block_number: 100, flashblock_index: 5 }));
        assert_eq!(resumed.as_str(), "ws://localhost:9999/ws?block_number=100&flashblock_index=5");
    }

    #[test]
    fn resume_url_extends_existing_query() {
        let url: Url = "ws://localhost:9999/ws?token=abc".parse().unwrap();
        let resumed =
            resume_url(&url, Some(StreamPosition { block_number: 200, flashblock_index: 3 }));
        assert_eq!(
            resumed.as_str(),
            "ws://localhost:9999/ws?token=abc&block_number=200&flashblock_index=3"
        );
    }

    #[test]
    fn resume_url_uses_index_zero() {
        let url: Url = "ws://localhost:9999/ws".parse().unwrap();
        let resumed =
            resume_url(&url, Some(StreamPosition { block_number: 7, flashblock_index: 0 }));
        assert_eq!(resumed.as_str(), "ws://localhost:9999/ws?block_number=7&flashblock_index=0");
    }
}
