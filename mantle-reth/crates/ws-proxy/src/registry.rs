//! Broadcast fan-out to connected downstream clients.

use std::time::Instant;

use axum::extract::ws::Message;
use futures::{SinkExt, stream::StreamExt};
use tokio::{
    sync::broadcast::{Sender, error::RecvError},
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

        let mut receiver = self.sender.subscribe();
        Metrics::new_connections().increment(1);

        let filter = client.filter.clone();
        let compressed = self.compressed;
        let client_id = client.id();
        let (mut ws_sender, ws_receiver) = client.websocket.split();

        let (pong_error_tx, mut pong_error_rx) = tokio::sync::oneshot::channel();
        let client_reader = self.start_reader(ws_receiver, client_id.clone(), pong_error_tx);

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

        client_reader.abort();
        Metrics::closed_connections().increment(1);

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

    /// Dropping a ping for a filtered client makes it miss its pong deadline
    /// and get disconnected, so every control frame must bypass the filter.
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
