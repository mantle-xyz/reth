//! The upstream-to-downstream hand-off.
//!
//! An upstream [`WebsocketSubscriber`](crate::WebsocketSubscriber) calls a
//! handler for every message it decodes; that handler publishes onto the
//! broadcast channel each [`Registry`](crate::Registry) client reads from.

use std::io::Write;

use axum::extract::ws::Message;
use tokio::sync::broadcast::Sender;
use tracing::{error, trace};

use crate::metrics::Metrics;

/// Builds the handler the upstream subscribers hand every decoded message to.
///
/// With `enable_compression` set, each payload is brotli-compressed once here
/// and the compressed frame is shared by every client, rather than compressed
/// per client on the way out. Clients are then served `Binary` frames; without
/// it they are served the payload bytes as-is.
///
/// A compression failure drops that one message and leaves the stream running:
/// the alternative is killing the fan-out for every client over a single
/// unencodable payload.
///
/// The caller is expected to hold one receiver open for the lifetime of the
/// channel, otherwise a moment with no clients closes it. That held receiver
/// is not a client, so it is discounted from the connection gauge.
pub fn upstream_listener(
    sender: Sender<Message>,
    enable_compression: bool,
) -> impl Fn(String) + Clone + Send + Sync + 'static {
    move |data: String| {
        trace!(target: "ws-proxy", bytes = data.len(), "received upstream data");
        Metrics::active_connections().set(sender.receiver_count().saturating_sub(1) as f64);

        let message_data = if enable_compression {
            let mut compressed = Vec::new();
            {
                let mut compressor = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
                if let Err(error) = compressor.write_all(data.as_bytes()) {
                    error!(target: "ws-proxy", %error, "failed to compress upstream data");
                    return;
                }
            }
            Metrics::bytes_compressed().increment(compressed.len() as u64);
            compressed
        } else {
            data.into_bytes()
        };

        match sender.send(message_data.into()) {
            Ok(_) => Metrics::broadcast_queue_size().set(sender.len() as f64),
            Err(error) => error!(target: "ws-proxy", %error, "failed to broadcast upstream data"),
        }
    }
}
