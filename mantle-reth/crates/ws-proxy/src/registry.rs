//! Broadcast fan-out to connected downstream clients.

use std::time::Instant;

use axum::extract::ws::Message;
use futures::{SinkExt, stream::StreamExt};
use tokio::{
    sync::broadcast::{Receiver, Sender, error::RecvError},
    time::{Duration, interval, timeout},
};
use tracing::{debug, info, trace, warn};

use crate::{client::ClientConnection, filter::FilterType, metrics::Metrics};

fn get_message_size(msg: &Message) -> u64 {
    match msg {
        Message::Text(text) => text.len() as u64,
        Message::Binary(data) | Message::Ping(data) | Message::Pong(data) => data.len() as u64,
        Message::Close(_) => 0,
    }
}

/// Whether a broadcast message should reach a client with the given filter.
///
/// Control frames bypass the filter. They carry no payload to match, so
/// running them through it would drop them for every filtered client — and
/// dropping pings is not merely a lost frame: the client never pongs, its
/// pong deadline expires, and [`Registry::start_reader`] disconnects it. A
/// subscriber that set any filter would be evicted every `pong_timeout`.
///
/// Data frames are matched on their own bytes. Both encodings are handled
/// because the broadcast channel carries whatever the upstream listener puts
/// on it; feeding an empty slice for one of them would silently drop every
/// message of that kind.
fn should_forward(filter: &FilterType, msg: &Message, compressed: bool) -> bool {
    match msg {
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) => true,
        Message::Binary(data) => filter.matches(data, compressed),
        Message::Text(text) => filter.matches(text.as_bytes(), compressed),
    }
}

/// Owns a real client's receiver and keeps metrics correct on every exit,
/// including cancellation of the subscription task.
struct ClientSubscription {
    receiver: Option<Receiver<Message>>,
    reader: Option<tokio::task::JoinHandle<()>>,
    sender: Sender<Message>,
}

impl ClientSubscription {
    fn new(sender: Sender<Message>) -> Self {
        let receiver = Some(sender.subscribe());
        Metrics::active_connections().increment(1.0);
        Metrics::new_connections().increment(1);
        Self { receiver, reader: None, sender }
    }

    async fn recv(&mut self) -> Result<Message, RecvError> {
        let result = self.receiver.as_mut().expect("receiver exists until drop").recv().await;
        Metrics::refresh_broadcast_queue_size(&self.sender);
        result
    }
}

impl Drop for ClientSubscription {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        // Release this receiver before sampling the remaining clients' backlog.
        drop(self.receiver.take());
        Metrics::refresh_broadcast_queue_size(&self.sender);
        Metrics::active_connections().decrement(1.0);
        Metrics::closed_connections().increment(1);
    }
}

/// Manages broadcast subscriptions for connected `WebSocket` clients.
#[derive(Clone, Debug)]
pub struct Registry {
    sender: Sender<Message>,
    compressed: bool,
    ping_enabled: bool,
    pong_timeout_ms: u64,
    send_timeout_ms: Duration,
}

impl Registry {
    /// Creates a new registry with the given broadcast sender and configuration.
    pub const fn new(
        sender: Sender<Message>,
        compressed: bool,
        ping_enabled: bool,
        pong_timeout_ms: u64,
        send_timeout_ms: Duration,
    ) -> Self {
        Self { sender, compressed, ping_enabled, pong_timeout_ms, send_timeout_ms }
    }

    /// Subscribes a client to the broadcast channel and forwards matching messages.
    pub async fn subscribe(&self, client: ClientConnection) {
        info!(message = "subscribing client", client = client.id());

        let mut receiver = ClientSubscription::new(self.sender.clone());

        let filter = client.filter.clone();
        let compressed = self.compressed;
        let client_id = client.id();
        let (mut ws_sender, ws_receiver) = client.websocket.split();

        let (pong_error_tx, mut pong_error_rx) = tokio::sync::oneshot::channel();
        receiver.reader = Some(self.start_reader(ws_receiver, client_id.clone(), pong_error_tx));

        loop {
            tokio::select! {
                broadcast_result = receiver.recv() => {
                    match broadcast_result {
                        Ok(msg) => {
                            if should_forward(&filter, &msg, compressed) {
                                trace!(message = "filter matched for client", client = client_id, filter = ?filter);

                                let send_start = Instant::now();
                                let msg_size = get_message_size(&msg);
                                let send_result = timeout(self.send_timeout_ms, ws_sender.send(msg)).await;
                                let send_duration = send_start.elapsed();

                                Metrics::message_send_duration().record(send_duration);

                                match send_result {
                                    Ok(Ok(())) => {
                                        // Success - message sent
                                        trace!(message = "message sent to client", client = client_id);
                                        Metrics::sent_messages().increment(1);
                                        Metrics::bytes_broadcasted().increment(msg_size);
                                    }
                                    Ok(Err(e)) => {
                                        // Send failed (connection error)
                                        warn!(
                                            message = "failed to send data to client",
                                            client = client_id,
                                            error = e.to_string()
                                        );
                                        Metrics::failed_messages().increment(1);
                                        break;
                                    }
                                    Err(_) => {
                                        // Timeout - client too slow
                                        warn!(
                                            message = "send timeout - disconnecting slow client",
                                            client = client_id,
                                            timeout_ms = self.send_timeout_ms.as_millis()
                                        );
                                        Metrics::failed_messages().increment(1);
                                        break;
                                    }
                                }
                            } else {
                                trace!(client_id = %client_id, "Filter did not match");
                            }
                        }
                        Err(RecvError::Closed) => {
                            info!(message = "upstream connection closed", client = client_id);
                            break;
                        }
                        Err(RecvError::Lagged(_)) => {
                            info!(message = "client is lagging", client = client_id);
                            Metrics::lagged_connections().increment(1);
                            break;
                        }
                    }
                }

                _ = &mut pong_error_rx => {
                    debug!(message = "client reader signaled disconnect", client = client_id);
                    break;
                }
            }
        }

        info!(message = "client disconnected", client = client_id);
    }

    fn start_reader(
        &self,
        ws_receiver: futures::stream::SplitStream<axum::extract::ws::WebSocket>,
        client_id: String,
        pong_error_tx: tokio::sync::oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let ping_enabled = self.ping_enabled;
        let pong_timeout_ms = self.pong_timeout_ms;

        tokio::spawn(async move {
            let mut ws_receiver = ws_receiver;
            let mut last_pong = Instant::now();
            let mut timeout_checker = interval(Duration::from_millis(pong_timeout_ms / 4));
            let pong_timeout = Duration::from_millis(pong_timeout_ms);

            loop {
                tokio::select! {
                    msg = ws_receiver.next() => {
                        match msg {
                            Some(Ok(Message::Pong(_))) => {
                                if ping_enabled {
                                    trace!(message = "received pong from client", client = client_id);
                                    last_pong = Instant::now();
                                }
                            }
                            Some(Ok(Message::Close(_))) => {
                                trace!(message = "received close from client", client = client_id);
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            Some(Ok(Message::Ping(_))) => {
                                // This is a no-op, but needs to be handled separately from the
                                // catch-all [`Some(Ok(_))`] arm below as otherwise client pings would be unexpected
                                // behavior and cause the connection to be shut down
                                trace!(message = "received ping from client", client = client_id);
                            }
                            Some(Ok(_)) => {
                                debug!(
                                    message = "unexpected inbound frame from client, disconnecting",
                                    client = client_id,
                                );
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            Some(Err(e)) => {
                                trace!(
                                    message = "error receiving from client",
                                    client = client_id,
                                    error = e.to_string()
                                );
                                let _ = pong_error_tx.send(());
                                return;
                            }
                            None => {
                                trace!(message = "client connection closed", client = client_id);
                                let _ = pong_error_tx.send(());
                                return;
                            }
                        }
                    }

                    _ = timeout_checker.tick() => {
                        if ping_enabled && last_pong.elapsed() > pong_timeout  {
                            debug!(
                                message = "client pong timeout, disconnecting",
                                client = client_id,
                                elapsed_ms = last_pong.elapsed().as_millis()
                            );
                            Metrics::client_pong_disconnects().increment(1);
                            let _ = pong_error_tx.send(());
                            return;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_metrics_follow_clients_without_upstream_traffic() {
        use crate::{InMemoryRateLimit, RateLimit};
        use axum::{Router, extract::WebSocketUpgrade, routing::get};
        use metrics_exporter_prometheus::PrometheusBuilder;
        use std::sync::Arc;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                Metrics::init();
                let (sender, _) = tokio::sync::broadcast::channel(4);
                let registry = Registry::new(sender, false, false, 1000, Duration::from_secs(1));
                let limiter = Arc::new(InMemoryRateLimit::new(2, 2));
                let (task_tx, mut tasks) = tokio::sync::mpsc::unbounded_channel();
                let router = Router::new().route(
                    "/",
                    get(move |upgrade: WebSocketUpgrade| {
                        let registry = registry.clone();
                        let ticket =
                            limiter.clone().try_acquire("127.0.0.1".parse().unwrap()).unwrap();
                        let task_tx = task_tx.clone();
                        async move {
                            upgrade.on_upgrade(move |socket| async move {
                                let client = ClientConnection::new(
                                    "127.0.0.1".parse().unwrap(),
                                    ticket,
                                    socket,
                                    FilterType::None,
                                );
                                task_tx
                                    .send(tokio::spawn(
                                        async move { registry.subscribe(client).await },
                                    ))
                                    .unwrap();
                            })
                        }
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("ws://{}/", listener.local_addr().unwrap());
                let server =
                    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
                for cancel in [false, true] {
                    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
                    let task = tasks.recv().await.unwrap();
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while !handle.render().contains("flashblocks_proxy_active_connections 1") {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("the connection must be counted without an upstream message");
                    if cancel {
                        task.abort();
                        assert!(task.await.unwrap_err().is_cancelled());
                    } else {
                        socket.close(None).await.unwrap();
                        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
                    }
                    let scrape = handle.render();
                    assert!(scrape.contains("flashblocks_proxy_active_connections 0"), "{scrape}");
                }
                assert!(handle.render().contains("flashblocks_proxy_closed_connections 2"));
                server.abort();
            })
        });
    }

    #[test]
    fn backlog_tracks_the_slowest_real_client_and_clears_on_receive_or_drop() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                Metrics::init();
                let (sender, _) = tokio::sync::broadcast::channel(4);
                let publish = crate::upstream_listener(sender.clone(), false);
                let assert_depth = |depth| {
                    let scrape = handle.render();
                    assert!(
                        scrape
                            .contains(&format!("flashblocks_proxy_broadcast_queue_size {depth}\n")),
                        "{scrape}"
                    );
                };
                publish("no clients".into());
                assert_depth(0);
                let mut fast = ClientSubscription::new(sender.clone());
                let mut slow = ClientSubscription::new(sender.clone());
                for _ in 0..3 {
                    publish("slice".into());
                }
                assert_depth(3);
                for _ in 0..3 {
                    fast.recv().await.unwrap();
                }
                assert_depth(3);
                slow.recv().await.unwrap();
                assert_depth(2);
                drop(slow);
                assert_depth(0);
                publish("slice".into());
                assert_depth(1);
                fast.recv().await.unwrap();
                assert_depth(0);
                drop(fast);
                publish("no clients".into());
                assert_depth(0);
                // The channel remains usable after its last receiver disconnects.
                let mut later = ClientSubscription::new(sender);
                publish("new slice".into());
                assert_eq!(later.recv().await.unwrap(), Message::Binary("new slice".into()));
                assert_depth(0);
                for _ in 0..7 {
                    publish("overflow".into());
                }
                assert_depth(4);
                assert!(matches!(later.recv().await, Err(RecvError::Lagged(3))));
                drop(later);
                assert_depth(0);
            })
        });
    }

    const WATCHED: &str = "0x4200000000000000000000000000000000000010";

    /// A slice whose only log is emitted by [`WATCHED`], in the flat
    /// `OpReceipt` encoding the Mantle producer uses.
    fn slice_mentioning_watched() -> Vec<u8> {
        format!(
            r#"{{"index":0,"metadata":{{"block_number":1,"receipts":{{"0x11":{{"type":"0x7e","logs":[{{"address":"{WATCHED}","topics":[]}}]}}}}}}}}"#
        )
        .into_bytes()
    }

    fn slice_mentioning_nothing() -> Vec<u8> {
        br#"{"index":0,"metadata":{"block_number":1,"receipts":{}}}"#.to_vec()
    }

    /// Locks the control-frame bypass in [`should_forward`]: a filtered client
    /// must still receive every ping, pong and close.
    #[test]
    fn control_frames_bypass_the_filter() {
        let filter = FilterType::new_addresses(vec![WATCHED.to_owned()]);

        for msg in [
            Message::Ping(Vec::new().into()),
            Message::Pong(Vec::new().into()),
            Message::Close(None),
        ] {
            assert!(
                should_forward(&filter, &msg, false),
                "a control frame must reach a filtered client: {msg:?}"
            );
        }
    }

    #[test]
    fn data_frames_are_matched_on_their_own_bytes() {
        let filter = FilterType::new_addresses(vec![WATCHED.to_owned()]);

        let matching = slice_mentioning_watched();
        let other = slice_mentioning_nothing();

        assert!(should_forward(&filter, &Message::Binary(matching.clone().into()), false));
        assert!(!should_forward(&filter, &Message::Binary(other.clone().into()), false));

        // The channel carries whatever the upstream listener produced. Text
        // must be matched on its bytes, not on an empty slice.
        let as_text = String::from_utf8(matching).expect("the fixture is UTF-8");
        assert!(should_forward(&filter, &Message::Text(as_text.into()), false));

        let other_text = String::from_utf8(other).expect("the fixture is UTF-8");
        assert!(!should_forward(&filter, &Message::Text(other_text.into()), false));
    }

    #[test]
    fn an_absent_filter_forwards_every_frame() {
        for msg in
            [Message::Binary(slice_mentioning_nothing().into()), Message::Ping(Vec::new().into())]
        {
            assert!(should_forward(&FilterType::None, &msg, false));
        }
    }

    #[test]
    fn message_size_counts_the_payload_only() {
        assert_eq!(get_message_size(&Message::Text("abcd".into())), 4);
        assert_eq!(get_message_size(&Message::Binary(vec![0u8; 7].into())), 7);
        assert_eq!(get_message_size(&Message::Close(None)), 0);
    }
}
