//! The `WebSocket` endpoint slices are published on.

use std::{
    future::Future,
    io,
    net::{SocketAddr, TcpListener as StdTcpListener},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use alloy_rpc_types_engine::PayloadId;
use mantle_reth_flashblocks_types::{MAX_DECOMPRESSED_FLASHBLOCK_BYTES, MantleFlashblockPayload};
use parking_lot::RwLock;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast,
};
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::flashblocks::{
    FlashblockPosition, FlashblockRingBuffer, broadcast::serve_subscriber,
    config::FlashblockProducerConfig,
};

/// A slice paired with where it sits in the stream and which build produced it.
///
/// The build is carried because position alone does not identify a slice: a
/// rebuild restarts its indices, so two different slices can hold the same
/// `(block, index)`. Anything that treats a slice as a key — catch-up's
/// de-duplication, the archive's supersede rule — needs both halves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedSlice {
    /// Where the slice sits in the stream.
    pub position: FlashblockPosition,
    /// The payload job that produced it.
    pub payload_id: PayloadId,
    /// The serialized slice, shared with the archive.
    pub text: Utf8Bytes,
}

/// Publishes slices to whoever is connected, and keeps the recent ones so a
/// subscriber that reconnects can be caught up.
///
/// Owns the listener: dropping it closes the endpoint and every open
/// subscription. Hand [`handle`](Self::handle) to whatever produces slices.
#[derive(Debug)]
pub struct MantleFlashblocksPublisher {
    handle: PublisherHandle,
    cancel: CancellationToken,
    local_addr: SocketAddr,
}

/// Cloneable half of the publisher: enough to publish, nothing that would
/// keep the endpoint alive.
#[derive(Debug, Clone)]
pub struct PublisherHandle {
    pipe: broadcast::Sender<PositionedSlice>,
    ring: Arc<RwLock<FlashblockRingBuffer>>,
}

impl MantleFlashblocksPublisher {
    /// Bind the endpoint, returning the publisher and the loop that accepts
    /// subscribers.
    ///
    /// Binding is synchronous, and happens here rather than inside the accept
    /// loop: a port already in use then stops the node at startup with a clear
    /// message, instead of leaving it running with an endpoint nobody serves
    /// from inside a task nobody is watching. It also means binding does not
    /// quietly require a runtime to already exist.
    ///
    /// The loop is handed back rather than spawned so that how it runs — and
    /// what happens if it stops — stays a decision for whoever owns the node.
    /// Nothing is served until the loop is driven.
    pub fn bind(
        cfg: &FlashblockProducerConfig,
    ) -> io::Result<(Self, impl Future<Output = ()> + Send + use<>)> {
        let ring_capacity = NonZeroUsize::new(cfg.ring_capacity)
            .ok_or_else(|| io::Error::other("flashblocks ring capacity must be non-zero"))?;

        // Bound the archive below by the live channel; see
        // `FlashblockProducerConfigError::RingSmallerThanBroadcast`.
        debug_assert!(cfg.ring_capacity >= cfg.broadcast_capacity);

        let (pipe, _) = broadcast::channel(cfg.broadcast_capacity.max(1));
        let ring = Arc::new(RwLock::new(FlashblockRingBuffer::new(ring_capacity)));
        let cancel = CancellationToken::new();

        let std_listener = StdTcpListener::bind(SocketAddr::new(cfg.addr, cfg.port))?;
        std_listener.set_nonblocking(true)?;
        let local_addr = std_listener.local_addr()?;
        let listener = TcpListener::from_std(std_listener)?;

        let accepting =
            accept_loop(listener, pipe.clone(), Arc::clone(&ring), cancel.child_token());

        info!(target: "mantle::preconf::flashblocks", %local_addr, "flashblocks publisher bound");

        Ok((Self { handle: PublisherHandle { pipe, ring }, cancel, local_addr }, accepting))
    }

    /// Address actually bound, which differs from the configured one when the
    /// port was left to the OS.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A handle for publishing slices.
    pub fn handle(&self) -> PublisherHandle {
        self.handle.clone()
    }
}

impl Drop for MantleFlashblocksPublisher {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl PublisherHandle {
    /// Serialize a slice, archive it, and hand it to every live subscriber.
    ///
    /// Archiving happens first: a subscriber connecting a moment later must
    /// find the slice waiting for it rather than fall in the gap between the
    /// two. Returns the serialized size.
    pub fn publish(&self, payload: &MantleFlashblockPayload) -> Result<usize, serde_json::Error> {
        let json = serde_json::to_string(payload)?;
        let size = json.len();
        // Against the decoder's `MAX_DECOMPRESSED_FLASHBLOCK_BYTES` ceiling, and
        // the input to whether compression is worth adding to the wire.
        metrics::histogram!("flashblocks.byte_size").record(size as f64);
        // Block-size limits keep a slice under the ceiling, so this is not
        // expected to fire. Published anyway: consumers and the ws-proxy refuse
        // it on their side, and this is where the broken assumption shows.
        if size > MAX_DECOMPRESSED_FLASHBLOCK_BYTES {
            metrics::counter!("flashblocks.oversized_slices_total").increment(1);
            warn!(
                target: "mantle::preconf::flashblocks",
                block_number = payload.metadata.block_number,
                index = payload.index,
                size,
                limit = MAX_DECOMPRESSED_FLASHBLOCK_BYTES,
                "slice is over the decoder's size limit; subscribers will refuse it",
            );
        }
        let text = Utf8Bytes::from(json);
        let position = FlashblockPosition {
            block_number: payload.metadata.block_number,
            flashblock_index: payload.index,
        };

        self.ring.write().push(position, payload.payload_id, text.clone());

        // No subscribers is the normal case, not a failure: the slice is
        // archived either way.
        let _ = self.pipe.send(PositionedSlice { position, payload_id: payload.payload_id, text });

        Ok(size)
    }

    /// Where the most recently published slice sits, across every block this
    /// producer has published. `None` means nothing has been published yet.
    pub fn latest_position(&self) -> Option<FlashblockPosition> {
        self.ring.read().latest()
    }

    /// Subscribers currently connected.
    ///
    /// Test-only, and used as a readiness barrier rather than an assertion:
    /// publishing before the subscriber has attached drops the slice, so the
    /// tests wait on this before they publish.
    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.pipe.receiver_count()
    }
}

/// Where the accept loop takes connections from. A trait so tests can make
/// accepting fail.
trait Accept: Send + 'static {
    fn accept(&self) -> impl Future<Output = io::Result<TcpStream>> + Send;
}

impl Accept for TcpListener {
    async fn accept(&self) -> io::Result<TcpStream> {
        Ok(Self::accept(self).await?.0)
    }
}

/// Delay after the first failed accept; each further failure doubles it.
const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(10);
/// Longest delay between accepts while they keep failing.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// How long to wait before accepting again after a failure.
///
/// Without a wait, a failure that does not consume the pending connection —
/// `EMFILE` is the usual one — leaves the listener readable, and the loop
/// retries at full speed: one core busy and a warning per attempt.
#[derive(Debug, Default)]
struct AcceptBackoff {
    next: Option<Duration>,
}

impl AcceptBackoff {
    /// Record a failure and return how long to wait before the next accept.
    fn failed(&mut self) -> Duration {
        let delay = self.next.unwrap_or(ACCEPT_BACKOFF_FIRST);
        self.next = Some(delay.saturating_mul(2).min(ACCEPT_BACKOFF_MAX));
        delay
    }

    /// Record a success: the next failure waits the shortest delay again.
    const fn succeeded(&mut self) {
        self.next = None;
    }
}

/// Accept subscribers until cancelled, serving each on its own task.
async fn accept_loop<L: Accept>(
    listener: L,
    pipe: broadcast::Sender<PositionedSlice>,
    ring: Arc<RwLock<FlashblockRingBuffer>>,
    cancel: CancellationToken,
) {
    let mut backoff = AcceptBackoff::default();
    loop {
        let connection = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(connection) => {
                    backoff.succeeded();
                    connection
                }
                Err(error) => {
                    let delay = backoff.failed();
                    warn!(target: "mantle::preconf::flashblocks", %error, ?delay, "failed to accept subscriber");
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(delay) => continue,
                    }
                }
            },
        };

        let pipe = pipe.clone();
        let ring = Arc::clone(&ring);
        let cancel = cancel.child_token();

        tokio::spawn(async move {
            serve_subscriber(connection, &pipe, ring, cancel).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use futures_util::StreamExt;
    use mantle_reth_flashblocks_types::MantleFlashblockPayload;
    use tokio_tungstenite::tungstenite::Message;

    use super::MantleFlashblocksPublisher;
    use crate::flashblocks::config::FlashblockProducerConfig;

    /// Bind on an OS-assigned port so tests never collide, and drive the
    /// accept loop the way the node wiring will.
    fn publisher(ring: usize, channel: usize) -> MantleFlashblocksPublisher {
        let (publisher, accepting) = MantleFlashblocksPublisher::bind(&FlashblockProducerConfig {
            addr: Ipv4Addr::LOCALHOST.into(),
            port: 0,
            ring_capacity: ring,
            broadcast_capacity: channel,
            ..Default::default()
        })
        .expect("bind on an ephemeral port");
        tokio::spawn(accepting);
        publisher
    }

    fn slice(block_number: u64, index: u64) -> MantleFlashblockPayload {
        let mut payload = MantleFlashblockPayload { index, ..Default::default() };
        payload.metadata.block_number = block_number;
        payload
    }

    /// A slice over the decoder's ceiling is counted. Consumers and the
    /// ws-proxy refuse it, and block-size limits are meant to keep it from
    /// happening, so the counter is the alarm that the assumption broke.
    #[test]
    fn a_slice_over_the_decoder_ceiling_is_counted() {
        use mantle_reth_flashblocks_types::MAX_DECOMPRESSED_FLASHBLOCK_BYTES;

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let metrics = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let publisher = publisher(8, 8);
                let handle = publisher.handle();

                handle.publish(&slice(1, 0)).expect("an ordinary slice");
                assert!(
                    !metrics.render().contains("flashblocks_oversized_slices_total 1"),
                    "an ordinary slice is not counted",
                );

                // Hex encoding doubles the bytes, so half the ceiling is enough.
                let mut oversized = slice(1, 1);
                oversized.diff.transactions =
                    vec![vec![0u8; MAX_DECOMPRESSED_FLASHBLOCK_BYTES / 2 + 1].into()];
                handle.publish(&oversized).expect("still published");
                assert!(
                    metrics.render().contains("flashblocks_oversized_slices_total 1"),
                    "{}",
                    metrics.render(),
                );
            });
        });
    }

    async fn connect(
        publisher: &MantleFlashblocksPublisher,
        query: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://{}{query}", publisher.local_addr());
        let (stream, _) = tokio_tungstenite::connect_async(url).await.expect("subscriber connects");
        stream
    }

    /// Read the slices a subscriber receives, identified by position.
    async fn received(
        stream: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        count: usize,
    ) -> Vec<(u64, u64)> {
        let mut positions = Vec::with_capacity(count);
        for _ in 0..count {
            let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a slice arrives before the timeout")
                .expect("the stream is still open")
                .expect("the frame is well formed");
            let Message::Text(text) = message else { panic!("expected a text frame") };
            let decoded: MantleFlashblockPayload =
                serde_json::from_str(&text).expect("slices are published as valid json");
            positions.push((decoded.metadata.block_number, decoded.index));
        }
        positions
    }

    #[tokio::test]
    async fn a_published_slice_reaches_a_connected_subscriber() {
        let publisher = publisher(8, 8);
        let mut subscriber = connect(&publisher, "").await;
        // The accept task must have subscribed before publishing, or the
        // slice would only exist in the archive.
        while publisher.handle().subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }

        publisher.handle().publish(&slice(10, 0)).expect("serializes");
        publisher.handle().publish(&slice(10, 1)).expect("serializes");

        assert_eq!(received(&mut subscriber, 2).await, [(10, 0), (10, 1)]);
    }

    /// A subscriber that publishes nothing to us still gets the archive, so a
    /// connection opened after the fact is not silently empty.
    #[tokio::test]
    async fn a_reconnecting_subscriber_is_caught_up_from_the_archive() {
        let publisher = publisher(8, 8);
        for index in 0..4 {
            publisher.handle().publish(&slice(10, index)).expect("serializes");
        }

        let mut subscriber = connect(&publisher, "/?block_number=10&flashblock_index=1").await;

        assert_eq!(received(&mut subscriber, 2).await, [(10, 2), (10, 3)]);
    }

    /// End-to-end wiring: a resume request survives the handshake, reaches
    /// the archive, and the connection then carries live slices. The seam
    /// between the two phases is covered by the `broadcast` tests, which can
    /// force the interleaving this one cannot.
    #[tokio::test]
    async fn a_resumed_connection_serves_the_archive_and_then_the_live_stream() {
        let publisher = publisher(8, 8);
        for index in 0..3 {
            publisher.handle().publish(&slice(10, index)).expect("serializes");
        }

        let mut subscriber = connect(&publisher, "/?block_number=10&flashblock_index=0").await;
        while publisher.handle().subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }
        publisher.handle().publish(&slice(10, 3)).expect("serializes");

        assert_eq!(received(&mut subscriber, 3).await, [(10, 1), (10, 2), (10, 3)]);
    }

    #[tokio::test]
    async fn a_first_time_subscriber_gets_only_what_comes_next() {
        let publisher = publisher(8, 8);
        publisher.handle().publish(&slice(10, 0)).expect("serializes");

        let mut subscriber = connect(&publisher, "").await;
        while publisher.handle().subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }
        publisher.handle().publish(&slice(10, 1)).expect("serializes");

        assert_eq!(
            received(&mut subscriber, 1).await,
            [(10, 1)],
            "no replay was asked for, so the earlier slice stays in the archive",
        );
    }

    /// Binding reserves the port but serves nobody until the accept loop is
    /// driven, so wiring that forgets to run it fails loudly rather than
    /// looking healthy.
    #[tokio::test]
    async fn binding_without_driving_the_accept_loop_serves_nobody() {
        let (publisher, _accepting) = MantleFlashblocksPublisher::bind(&FlashblockProducerConfig {
            addr: Ipv4Addr::LOCALHOST.into(),
            port: 0,
            ..Default::default()
        })
        .expect("bind on an ephemeral port");

        let url = format!("ws://{}", publisher.local_addr());
        let handshake =
            tokio::time::timeout(Duration::from_millis(300), tokio_tungstenite::connect_async(url))
                .await;

        assert!(handshake.is_err(), "no handshake can complete while nothing accepts");
    }

    /// Dropping the publisher must take the endpoint with it, or a restart
    /// would fail to bind.
    #[tokio::test]
    async fn dropping_the_publisher_closes_the_endpoint() {
        let publisher = publisher(8, 8);
        let addr = publisher.local_addr();
        let mut subscriber = connect(&publisher, "").await;

        drop(publisher);

        let closed = tokio::time::timeout(Duration::from_secs(5), subscriber.next()).await;
        assert!(closed.is_ok(), "the subscription ends rather than hanging");
        assert!(tokio::net::TcpListener::bind(addr).await.is_ok(), "the port is free again",);
    }

    /// An accept that always fails, counting attempts. After `limit` attempts it
    /// never returns, so a loop with no backoff stops instead of hanging the
    /// paused-clock test.
    struct FailingAccept {
        attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        limit: usize,
    }

    impl super::Accept for FailingAccept {
        async fn accept(&self) -> std::io::Result<tokio::net::TcpStream> {
            let n = self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.limit {
                std::future::pending::<()>().await;
            }
            Err(std::io::Error::other("too many open files"))
        }
    }

    fn failing_loop(
        limit: usize,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let attempts = std::sync::Arc::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (pipe, _) = tokio::sync::broadcast::channel(1);
        let ring = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::flashblocks::FlashblockRingBuffer::new(std::num::NonZeroUsize::MIN),
        ));
        let handle = tokio::spawn(super::accept_loop(
            FailingAccept { attempts: std::sync::Arc::clone(&attempts), limit },
            pipe,
            ring,
            cancel.clone(),
        ));
        (attempts, cancel, handle)
    }

    #[test]
    fn accept_backoff_doubles_up_to_the_cap_and_resets_on_success() {
        let mut backoff = super::AcceptBackoff::default();
        let delays: Vec<u64> = (0..9).map(|_| backoff.failed().as_millis() as u64).collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 320, 640, 1000, 1000]);

        backoff.succeeded();
        assert_eq!(backoff.failed(), super::ACCEPT_BACKOFF_FIRST);
    }

    /// Failed accepts are retried after the backoff delays, not straight away.
    /// The delays sum to 1270ms by the eighth attempt, so one second fits seven.
    #[tokio::test(start_paused = true)]
    async fn a_failing_accept_is_retried_with_backoff() {
        let (attempts, cancel, handle) = failing_loop(100);

        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 7);
        cancel.cancel();
        handle.await.unwrap();
    }

    /// Shutdown does not wait out the delay.
    #[tokio::test(start_paused = true)]
    async fn cancelling_during_the_backoff_stops_the_loop_at_once() {
        let (_attempts, cancel, handle) = failing_loop(100);
        // Into the capped delays.
        tokio::time::sleep(Duration::from_secs(3)).await;

        let cancelled_at = tokio::time::Instant::now();
        cancel.cancel();
        handle.await.unwrap();

        assert_eq!(tokio::time::Instant::now(), cancelled_at);
    }
}
