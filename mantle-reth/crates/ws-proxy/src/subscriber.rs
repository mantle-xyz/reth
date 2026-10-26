//! Persistent upstream websocket connection with backoff and resume.

use std::{
    io::Read,
    time::{Duration, Instant},
};

use axum::http::Uri;
use backoff::{ExponentialBackoff, backoff::Backoff};
use futures::{SinkExt, StreamExt};
use mantle_reth_flashblocks_types::{
    MAX_DECOMPRESSED_FLASHBLOCK_BYTES as MAX_SLICE_BYTES, redact_ws_url, ws_url_endpoint,
};
use tokio::{select, sync::oneshot};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Error, Error::ConnectionClosed, Message, protocol::WebSocketConfig},
};
use tokio_util::{bytes, sync::CancellationToken};
use tracing::{error, info, trace, warn};

use crate::metrics::Metrics;

/// Configuration options for a [`WebsocketSubscriber`].
#[derive(Debug, Clone)]
pub struct SubscriberOptions {
    /// Maximum duration between reconnection attempts.
    pub max_backoff_interval: Duration,
    /// Initial duration between reconnection attempts before exponential increase.
    pub backoff_initial_interval: Duration,
    /// Interval at which ping frames are sent to the upstream server.
    pub ping_interval: Duration,
    /// Maximum time to wait for a pong response before considering the connection dead.
    pub pong_timeout: Duration,
    /// Grace period after initial connection before enforcing pong deadlines.
    pub initial_grace_period: Duration,
}

impl SubscriberOptions {
    /// Sets the maximum backoff interval.
    pub const fn with_max_backoff_interval(mut self, max_backoff_interval: Duration) -> Self {
        self.max_backoff_interval = max_backoff_interval;
        self
    }

    /// Sets the ping interval.
    pub const fn with_ping_interval(mut self, ping_interval: Duration) -> Self {
        self.ping_interval = ping_interval;
        self
    }

    /// Sets the pong timeout.
    pub const fn with_pong_timeout(mut self, pong_timeout: Duration) -> Self {
        self.pong_timeout = pong_timeout;
        self
    }

    /// Sets the initial backoff interval.
    pub const fn with_backoff_initial_interval(
        mut self,
        backoff_initial_interval: Duration,
    ) -> Self {
        self.backoff_initial_interval = backoff_initial_interval;
        self
    }

    /// Sets the initial grace period.
    pub const fn with_initial_grace_period(mut self, initial_grace_period: Duration) -> Self {
        self.initial_grace_period = initial_grace_period;
        self
    }
}

impl Default for SubscriberOptions {
    fn default() -> Self {
        Self {
            max_backoff_interval: Duration::from_secs(5),
            backoff_initial_interval: Duration::from_millis(500),
            ping_interval: Duration::from_secs(1),
            pong_timeout: Duration::from_secs(2),
            initial_grace_period: Duration::from_secs(5),
        }
    }
}

/// Maintains a persistent websocket connection to an upstream server, automatically
/// reconnecting with exponential backoff and monitoring liveness via ping/pong.
///
/// Tracks the position of the last received flashblock message. On reconnect,
/// appends `block_number` and `flashblock_index` query parameters to the URI
/// so the upstream publisher can replay missed entries.
pub struct WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    uri: Uri,
    /// `uri` for logs: the upstream's API key and query hidden.
    uri_redacted: String,
    /// `uri`'s `host:port`, for metric labels.
    uri_label: String,
    handler: F,
    backoff: ExponentialBackoff,
    options: SubscriberOptions,
    last_position: Option<(u64, u64)>,
}

impl<F> std::fmt::Debug for WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebsocketSubscriber")
            .field("uri", &self.uri_redacted)
            .field("options", &self.options)
            .field("last_position", &self.last_position)
            .finish_non_exhaustive()
    }
}

impl<F> WebsocketSubscriber<F>
where
    F: Fn(String) + Send + Sync + 'static,
{
    /// Creates a new subscriber targeting the given URI with the provided message handler.
    pub fn new(uri: Uri, handler: F, options: SubscriberOptions) -> Self {
        let backoff = ExponentialBackoff {
            initial_interval: options.backoff_initial_interval,
            max_interval: options.max_backoff_interval,
            max_elapsed_time: None,
            ..Default::default()
        };

        let uri_redacted = redact_ws_url(&uri.to_string());
        let uri_label = ws_url_endpoint(&uri.to_string());
        Self { uri, uri_redacted, uri_label, handler, backoff, options, last_position: None }
    }

    /// Runs the subscriber loop, reconnecting on failure until the token is cancelled.
    pub async fn run(&mut self, token: CancellationToken) {
        info!(message = "starting upstream subscription", uri = %self.uri_redacted);
        loop {
            select! {
                _ = token.cancelled() => {
                    info!(
                        message = "cancelled upstream subscription",
                        uri = %self.uri_redacted
                    );
                    return;
                }
                result = self.connect_and_listen() => {
                    match result {
                        Ok(()) => {
                            info!(
                                message = "upstream connection closed",
                                uri = %self.uri_redacted
                            );
                        }
                        Err(e) => {
                            error!(
                                message = "upstream websocket error",
                                uri = %self.uri_redacted,
                                error = e.to_string()
                            );
                            Metrics::upstream_errors().increment(1);

                            if let Some(duration) = self.backoff.next_backoff() {
                                warn!(
                                    message = "reconnecting",
                                    uri = %self.uri_redacted,
                                    seconds = duration.as_secs()
                                );
                                select! {
                                    _ = token.cancelled() => {
                                        info!(
                                            message = "cancelled subscriber during backoff",
                                            uri = %self.uri_redacted
                                        );
                                        return
                                    }
                                    _ = tokio::time::sleep(duration) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Builds the connection URI, appending resume position query parameters
    /// if a previous position is tracked.
    fn connect_uri(&self) -> Uri {
        let Some((block_number, flashblock_index)) = self.last_position else {
            return self.uri.clone();
        };

        let base = self.uri.to_string();
        let separator = if self.uri.query().is_some() { "&" } else { "?" };
        let with_params = format!(
            "{base}{separator}block_number={block_number}&flashblock_index={flashblock_index}"
        );

        with_params.parse().unwrap_or_else(|e| {
            warn!(error = %e, uri = %self.uri_redacted, "Failed to parse reconnect URI, falling back to base URI");
            self.uri.clone()
        })
    }

    async fn connect_and_listen(&mut self) -> Result<(), Error> {
        let connect_uri = self.connect_uri();
        info!(
            message = "connecting to websocket",
            uri = %self.uri_redacted,
            resume_from = ?self.last_position,
        );

        Metrics::upstream_connection_attempts().increment(1);

        // Refusing to buffer a frame we would refuse to read; see `MAX_SLICE_BYTES`.
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SLICE_BYTES))
            .max_frame_size(Some(MAX_SLICE_BYTES));
        let (ws_stream, _) =
            match connect_async_with_config(&connect_uri, Some(config), false).await {
                Ok(connection) => {
                    Metrics::upstream_connection_successes().increment(1);
                    connection
                }
                Err(e) => {
                    Metrics::upstream_connection_failures().increment(1);
                    return Err(e);
                }
            };

        info!(message = "websocket connection established", uri = %self.uri_redacted);

        Metrics::upstream_connections().increment(1);
        self.backoff.reset();

        let (mut write, mut read) = ws_stream.split();

        let (ping_error_tx, mut ping_error_rx) = oneshot::channel();
        let options = self.options.clone();
        let mut pong_deadline = Instant::now() + options.initial_grace_period;

        let ping_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(options.ping_interval);
            loop {
                interval.tick().await;
                Metrics::ping_attempts().increment(1);
                if let Err(e) = write.send(Message::Ping(bytes::Bytes::new())).await {
                    error!(message = "failed to send ping to upstream", error = e.to_string());
                    Metrics::ping_failures().increment(1);
                    let _ = ping_error_tx.send(e);
                    break;
                }
                Metrics::ping_sent().increment(1);
            }
        });

        let mut deadline_check = tokio::time::interval(Duration::from_millis(50));

        let result = loop {
            select! {
                _ = deadline_check.tick() => {
                    if Instant::now() >= pong_deadline {
                        error!(
                            message = "pong timeout from upstream",
                            uri = %self.uri_redacted
                        );
                        break Err(ConnectionClosed);
                    }
                }
                // `result =`, not `Ok(..) =`: a pattern that fails to match
                // leaves the branch disabled only for *this* `select!`, and
                // the loop builds a new one every iteration — while
                // `oneshot`'s `poll` clears its inner state on both the Ok and
                // the Err path, so the next poll panics with "called after
                // complete". The deadline tick below guarantees there is a
                // next iteration, so a dropped sender turned into a panic
                // inside about 50ms.
                result = &mut ping_error_rx => {
                    match result {
                        Ok(ping_err) => break Err(ping_err),
                        // The ping task is gone without having reported
                        // anything. Nothing left to keep this connection for.
                        Err(_) => break Err(ConnectionClosed),
                    }
                }
                message = read.next() => {
                    let Some(msg) = message else {
                        break Ok(());
                    };
                    if let Err(e) = self.handle_message(msg, &mut pong_deadline, options.pong_timeout).await {
                        break Err(e);
                    }
                }
            }
        };

        ping_task.abort();
        Metrics::upstream_connections().decrement(1);
        result
    }

    async fn handle_message(
        &mut self,
        message: Result<Message, Error>,
        pong_deadline: &mut Instant,
        pong_timeout: Duration,
    ) -> Result<(), Error> {
        let msg = match message {
            Ok(msg) => msg,
            Err(e) => {
                error!(
                    message = "error receiving message",
                    uri = %self.uri_redacted,
                    error = e.to_string()
                );
                return Err(e);
            }
        };

        match msg {
            Message::Text(text) => {
                if text.len() > MAX_SLICE_BYTES {
                    warn!(
                        message = "dropping oversized text message",
                        uri = %self.uri_redacted,
                        size = text.len(),
                        max = MAX_SLICE_BYTES
                    );
                    return Ok(());
                }

                trace!(
                    message = "received text message",
                    uri = %self.uri_redacted,
                    payload = text.as_str()
                );
                self.deliver(text.to_string());
            }
            Message::Binary(data) => {
                trace!(message = "received binary message", uri = %self.uri_redacted, size = data.len());

                match decode_binary_slice(&data) {
                    Some(text) => self.deliver(text),
                    None => warn!(
                        message = "dropping undecodable binary message",
                        uri = %self.uri_redacted,
                        size = data.len()
                    ),
                }
            }
            Message::Pong(_) => {
                trace!(message = "received pong from upstream", uri = %self.uri_redacted);
                *pong_deadline = Instant::now() + pong_timeout;
            }
            Message::Close(_) => {
                info!(message = "received close frame from upstream", uri = %self.uri_redacted);
                return Err(ConnectionClosed);
            }
            _ => {}
        }

        Ok(())
    }

    /// Records the resume position, then hands the slice to the listener.
    ///
    /// The position is recorded **before** dispatch so that a connection
    /// dropping immediately afterwards resumes from after this slice rather
    /// than replaying it.
    fn deliver(&mut self, text: String) {
        Metrics::upstream_messages(self.uri_label.clone()).increment(1);

        if let Some(pos) = parse_flashblock_position(&text) {
            self.last_position = Some(pos);
        }

        (self.handler)(text);
    }
}

/// The fields [`parse_flashblock_position`] needs, and nothing else, so that
/// deserializing a slice does not materialize the entire JSON tree.
#[derive(serde::Deserialize)]
struct PositionExtract {
    index: u64,
    metadata: MetadataExtract,
}

#[derive(serde::Deserialize)]
struct MetadataExtract {
    block_number: u64,
}

/// Extracts `(block_number, flashblock_index)` from a flashblock JSON payload.
///
/// Reads `metadata.block_number` and the top-level `index` field, matching the
/// wire format of `MantleFlashblockPayload`. Returns `None` if the JSON is not
/// a valid flashblock payload or either field is missing.
fn parse_flashblock_position(data: &str) -> Option<(u64, u64)> {
    let extract: PositionExtract = serde_json::from_str(data).ok()?;
    Some((extract.metadata.block_number, extract.index))
}

/// Recovers the slice JSON from a binary frame.
///
/// Mirrors the consumer's `MantleFlashblockPayload::try_decode_message`: a
/// first non-whitespace byte of `{` means the frame is already JSON, anything
/// else is brotli. Dropping binary frames instead, as Base's proxy does, would
/// starve every subscriber: a compressed slice is never valid UTF-8 and so
/// never arrives as [`Message::Text`].
fn decode_binary_slice(data: &[u8]) -> Option<String> {
    if data.len() > MAX_SLICE_BYTES {
        warn!(size = data.len(), max = MAX_SLICE_BYTES, "binary frame exceeds the limit");
        return None;
    }

    let looks_like_json = data.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    if looks_like_json {
        return String::from_utf8(data.to_vec()).ok();
    }

    let mut decompressed = Vec::new();
    // Read at most one byte past the ceiling into `decompressed`: enough to
    // tell "exactly at the limit" from "over it" without materialising the
    // whole expansion. Mirrors the consumer's `decompress`. The decoder's own
    // window is separate and not bounded by this.
    if let Err(error) = brotli::Decompressor::new(data, 4096)
        .take(MAX_SLICE_BYTES as u64 + 1)
        .read_to_end(&mut decompressed)
    {
        warn!(%error, "failed to brotli-decompress a binary frame");
        return None;
    }

    if decompressed.len() > MAX_SLICE_BYTES {
        warn!(
            size = decompressed.len(),
            max = MAX_SLICE_BYTES,
            "decompressed frame exceeds the limit"
        );
        return None;
    }

    String::from_utf8(decompressed).ok()
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use axum::http::Uri;
    use futures::SinkExt;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::broadcast,
        time::{Duration, sleep, timeout},
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::*;

    struct MockServer {
        addr: SocketAddr,
        message_sender: broadcast::Sender<String>,
        shutdown: CancellationToken,
    }

    impl MockServer {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, _) = broadcast::channel::<String>(100);
            let shutdown = CancellationToken::new();
            let shutdown_clone = shutdown.clone();
            let tx_clone = tx.clone();

            tokio::spawn(async move {
                loop {
                    select! {
                        _ = shutdown_clone.cancelled() => {
                            break;
                        }
                        accept_result = listener.accept() => {
                            match accept_result {
                                Ok((stream, _)) => {
                                    let tx = tx_clone.clone();
                                    let shutdown = shutdown_clone.clone();
                                    tokio::spawn(async move {
                                        Self::handle_connection(stream, tx, shutdown).await;
                                    });
                                }
                                Err(e) => {
                                    eprintln!("Failed to accept: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
            });

            Self { addr, message_sender: tx, shutdown }
        }

        async fn handle_connection(
            stream: TcpStream,
            tx: broadcast::Sender<String>,
            shutdown: CancellationToken,
        ) {
            let ws_stream = match accept_async(stream).await {
                Ok(ws_stream) => ws_stream,
                Err(e) => {
                    eprintln!("Failed to accept websocket: {e}");
                    return;
                }
            };

            let (mut ws_sender, _) = ws_stream.split();

            let mut rx = tx.subscribe();

            loop {
                select! {
                    _ = shutdown.cancelled() => {
                        break;
                    }
                    msg = rx.recv() => {
                        match msg {
                            Ok(text) => {
                                if let Err(e) = ws_sender.send(Message::Text(text.into())).await {
                                    eprintln!("Error sending message: {e}");
                                    break;
                                }
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }
                }
            }
        }

        async fn send_message(
            &self,
            msg: &str,
        ) -> Result<usize, broadcast::error::SendError<String>> {
            self.message_sender.send(msg.to_string())
        }

        async fn shutdown(self) {
            self.shutdown.cancel();
        }

        fn uri(&self) -> Uri {
            format!("ws://{}", self.addr).parse().expect("Failed to parse URI")
        }
    }

    #[tokio::test]
    async fn test_ping_pong_reconnection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri: Uri = format!("ws://{addr}").parse().unwrap();

        let shutdown = CancellationToken::new();
        let shutdown_server = shutdown.clone();

        let connection_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let connection_count_server = Arc::clone(&connection_count);

        tokio::spawn(async move {
            loop {
                select! {
                    _ = shutdown_server.cancelled() => break,
                    accept_result = listener.accept() => {
                        if let Ok((stream, _)) = accept_result {
                            connection_count_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let shutdown_inner = shutdown_server.clone();
                            tokio::spawn(async move {
                                let _ws_stream = match accept_async(stream).await {
                                    Ok(ws) => ws,
                                    Err(_) => return,
                                };


                                // Become completely unresponsive - don't read any messages
                                select! {
                                    _ = shutdown_inner.cancelled() => ()
                                }
                            });
                        }
                    }
                }
            }
        });

        let listener_fn = move |_data: String| {
            // Handler for received messages - not needed for this test
        };

        let options = SubscriberOptions::default()
            .with_backoff_initial_interval(Duration::from_millis(100))
            .with_ping_interval(Duration::from_millis(100))
            .with_pong_timeout(Duration::from_millis(200))
            .with_initial_grace_period(Duration::from_millis(50));

        let mut subscriber = WebsocketSubscriber::new(uri, listener_fn, options);

        let subscriber_task = {
            let token_clone = shutdown.clone();
            tokio::spawn(async move {
                subscriber.run(token_clone).await;
            })
        };

        // This needs to take into account the poll interval, pong deadline and the backoff
        // interval.
        sleep(Duration::from_secs(1)).await;

        let connections = connection_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            connections >= 2,
            "Expected at least 2 connection attempts due to ping timeout, got {connections}"
        );

        shutdown.cancel();
        let _ = timeout(Duration::from_secs(1), subscriber_task).await;
    }

    /// An upstream that ends the stream without a close frame is reconnected on
    /// the backoff schedule, not as fast as a TCP handshake completes.
    ///
    /// A load balancer draining, a sequencer restarting and a middlebox idle
    /// timeout all look alike from here: the connection is accepted, the
    /// handshake completes, and then the stream simply ends. tungstenite
    /// reports that as an error rather than as end-of-stream, so `run` takes
    /// its `Err` arm and consults the backoff. `run` also has an `Ok(())` arm
    /// that reconnects without one; this is what holds that arm out of reach of
    /// the abrupt-close cases it would otherwise cover.
    #[tokio::test]
    async fn an_upstream_that_ends_the_stream_is_reconnected_on_the_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri: Uri = format!("ws://{addr}").parse().unwrap();

        let shutdown = CancellationToken::new();
        let shutdown_server = shutdown.clone();

        let accepts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let accepts_server = Arc::clone(&accepts);

        tokio::spawn(async move {
            loop {
                select! {
                    _ = shutdown_server.cancelled() => break,
                    accept_result = listener.accept() => {
                        if let Ok((stream, _)) = accept_result {
                            accepts_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            // Complete the handshake, then drop the stream.
                            tokio::spawn(async move { drop(accept_async(stream).await) });
                        }
                    }
                }
            }
        });

        // Keepalive far longer than the test, so the only thing driving a
        // reconnect is the stream ending.
        let options = SubscriberOptions::default()
            .with_backoff_initial_interval(Duration::from_millis(200))
            .with_ping_interval(Duration::from_secs(30))
            .with_pong_timeout(Duration::from_secs(30))
            .with_initial_grace_period(Duration::from_millis(50));

        let mut subscriber = WebsocketSubscriber::new(uri, |_data: String| {}, options);
        let subscriber_task = {
            let token = shutdown.clone();
            tokio::spawn(async move {
                subscriber.run(token).await;
            })
        };

        sleep(Duration::from_secs(1)).await;
        let attempts = accepts.load(std::sync::atomic::Ordering::SeqCst);

        shutdown.cancel();
        let _ = timeout(Duration::from_secs(1), subscriber_task).await;

        // A 200ms initial interval, doubling, cannot fit many attempts into a
        // second. Loose enough to survive a slow machine, tight enough that an
        // unthrottled loop (hundreds) cannot pass.
        assert!(
            attempts >= 2,
            "the premise: the subscriber has to be reconnecting at all, got {attempts}"
        );
        assert!(
            attempts <= 8,
            "reconnects must be paced by the backoff, got {attempts} in one second"
        );
    }

    #[tokio::test]
    async fn test_multiple_subscribers_single_listener() {
        let server1 = MockServer::new().await;
        let server2 = MockServer::new().await;

        let received_messages = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received_messages);

        let listener = move |data: String| {
            if let Ok(mut messages) = received_clone.lock() {
                messages.push(data);
            }
        };

        let token = CancellationToken::new();
        let token_clone1 = token.clone();
        let token_clone2 = token.clone();

        let uri1 = server1.uri();
        let listener_clone1 = listener.clone();

        let mut subscriber1 =
            WebsocketSubscriber::new(uri1.clone(), listener_clone1, SubscriberOptions::default());

        let uri2 = server2.uri();
        let listener_clone2 = listener.clone();

        let mut subscriber2 =
            WebsocketSubscriber::new(uri2.clone(), listener_clone2, SubscriberOptions::default());

        let task1 = tokio::spawn(async move {
            subscriber1.run(token_clone1).await;
        });

        let task2 = tokio::spawn(async move {
            subscriber2.run(token_clone2).await;
        });

        sleep(Duration::from_millis(500)).await;

        let _ = server1.send_message("Message from server 1").await;
        let _ = server2.send_message("Message from server 2").await;

        sleep(Duration::from_millis(500)).await;

        let _ = server1.send_message("Another message from server 1").await;
        let _ = server2.send_message("Another message from server 2").await;

        sleep(Duration::from_millis(500)).await;

        // Cancel the token to shut down subscribers
        token.cancel();
        let _ = timeout(Duration::from_secs(1), task1).await;
        let _ = timeout(Duration::from_secs(1), task2).await;

        server1.shutdown().await;
        server2.shutdown().await;

        let messages = match received_messages.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        assert_eq!(messages.len(), 4);

        assert!(messages.contains(&"Message from server 1".to_string()));
        assert!(messages.contains(&"Message from server 2".to_string()));
        assert!(messages.contains(&"Another message from server 1".to_string()));
        assert!(messages.contains(&"Another message from server 2".to_string()));

        assert!(!messages.is_empty());
    }

    /// The compressing-upstream case `decode_binary_slice` exists for.
    #[test]
    fn a_brotli_binary_frame_is_decompressed() {
        let json = r#"{"index":4,"metadata":{"block_number":123},"diff":{}}"#;

        let mut compressed = Vec::new();
        {
            let mut compressor = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            compressor.write_all(json.as_bytes()).expect("compresses");
        }

        assert_eq!(decode_binary_slice(&compressed).as_deref(), Some(json));
    }

    /// The other half of the sniff in `decode_binary_slice`: an uncompressed
    /// slice framed as binary must be passed through, not run through brotli.
    #[test]
    fn an_uncompressed_binary_frame_is_passed_through() {
        let json = r#"  {"index":0,"metadata":{"block_number":1},"diff":{}}"#;
        assert_eq!(decode_binary_slice(json.as_bytes()).as_deref(), Some(json));
    }

    #[test]
    fn undecodable_binary_frames_are_rejected() {
        assert_eq!(decode_binary_slice(&[0xff, 0xfe, 0xfd]), None, "neither JSON nor brotli");
        assert_eq!(
            decode_binary_slice(&vec![b'x'; MAX_SLICE_BYTES + 1]),
            None,
            "over the size limit"
        );
    }

    /// A compression bomb clears the pre-decompression check — the frame is
    /// kilobytes — so only the check after decompression can refuse it.
    ///
    /// That the decoder is bounded, and so never materializes the expansion,
    /// is not observable here: an unbounded decoder returns `None` too, just
    /// after allocating the whole thing. Boundedness rests on the `take` in
    /// `decode_binary_slice`, not on this test.
    #[test]
    fn a_frame_that_expands_past_the_ceiling_is_rejected() {
        let mut bomb = Vec::new();
        {
            let mut compressor = brotli::CompressorWriter::new(&mut bomb, 4096, 5, 22);
            compressor.write_all(&vec![b'a'; MAX_SLICE_BYTES * 2]).expect("compresses");
        }

        assert!(
            bomb.len() < MAX_SLICE_BYTES,
            "the frame itself must clear the pre-decompression check, got {} bytes",
            bomb.len()
        );
        assert_eq!(decode_binary_slice(&bomb), None, "expands past the ceiling");
    }

    /// The ceiling has to cover the frame type the producer actually sends.
    /// Mantle's publisher writes `Message::Text`, so a ceiling that only
    /// guards `Message::Binary` guards nothing on the path in use.
    #[tokio::test]
    async fn an_oversized_text_frame_is_refused_like_an_oversized_binary_one() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&delivered);
        let mut subscriber = WebsocketSubscriber::new(
            "ws://127.0.0.1:1".parse().expect("a uri"),
            move |text: String| recorded.lock().unwrap().push(text.len()),
            SubscriberOptions::default(),
        );
        let mut pong_deadline = Instant::now() + Duration::from_secs(60);

        let oversized = "x".repeat(MAX_SLICE_BYTES + 1);
        subscriber
            .handle_message(
                Ok(Message::Text(oversized.into())),
                &mut pong_deadline,
                Duration::from_secs(1),
            )
            .await
            .expect("an oversized frame is dropped, not a connection error");

        assert!(
            delivered.lock().unwrap().is_empty(),
            "an oversized text frame must not reach the subscribers",
        );

        // The premise: the same path does forward a frame within the ceiling,
        // so the assertion above is about the size and not about the path.
        subscriber
            .handle_message(
                Ok(Message::Text("{}".to_string().into())),
                &mut pong_deadline,
                Duration::from_secs(1),
            )
            .await
            .expect("a frame within the ceiling is handled");

        assert_eq!(delivered.lock().unwrap().as_slice(), &[2]);
    }

    #[test]
    fn parse_flashblock_position_valid() {
        let payload = r#"{"index":4,"metadata":{"block_number":123},"diff":{}}"#;
        assert_eq!(parse_flashblock_position(payload), Some((123, 4)));
    }

    #[test]
    fn parse_flashblock_position_missing_metadata() {
        let payload = r#"{"index":0,"diff":{}}"#;
        assert_eq!(parse_flashblock_position(payload), None);
    }

    #[test]
    fn parse_flashblock_position_invalid_json() {
        assert_eq!(parse_flashblock_position("not json"), None);
    }

    #[test]
    fn parse_flashblock_position_missing_index() {
        let payload = r#"{"metadata":{"block_number":123}}"#;
        assert_eq!(parse_flashblock_position(payload), None);
    }

    #[test]
    fn connect_uri_without_position() {
        let uri: Uri = "ws://localhost:9999/ws".parse().unwrap();
        let subscriber =
            WebsocketSubscriber::new(uri.clone(), |_: String| {}, SubscriberOptions::default());
        assert_eq!(subscriber.connect_uri(), uri);
    }

    #[test]
    fn connect_uri_with_position() {
        let uri: Uri = "ws://localhost:9999/ws".parse().unwrap();
        let mut subscriber =
            WebsocketSubscriber::new(uri, |_: String| {}, SubscriberOptions::default());
        subscriber.last_position = Some((100, 5));
        let connect_uri = subscriber.connect_uri();
        assert_eq!(
            connect_uri.to_string(),
            "ws://localhost:9999/ws?block_number=100&flashblock_index=5"
        );
    }

    #[test]
    fn connect_uri_with_existing_query() {
        let uri: Uri = "ws://localhost:9999/ws?token=abc".parse().unwrap();
        let mut subscriber =
            WebsocketSubscriber::new(uri, |_: String| {}, SubscriberOptions::default());
        subscriber.last_position = Some((200, 3));
        let connect_uri = subscriber.connect_uri();
        assert_eq!(
            connect_uri.to_string(),
            "ws://localhost:9999/ws?token=abc&block_number=200&flashblock_index=3"
        );
    }

    /// The upstream may be another proxy's `/ws/<key>` endpoint: neither the
    /// key nor a query token may reach a log line or a metric label.
    #[test]
    fn the_upstream_key_stays_out_of_logs_and_labels() {
        let uri: Uri = "ws://proxy:8545/ws/secret-key?token=secret-token".parse().unwrap();
        let subscriber =
            WebsocketSubscriber::new(uri, |_: String| {}, SubscriberOptions::default());

        assert_eq!(subscriber.uri_redacted, "ws://proxy:8545/ws/***");
        assert_eq!(subscriber.uri_label, "proxy:8545");
        let debug = format!("{subscriber:?}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
