//! The `WebSocket` endpoint slices are published on.

use std::{
    future::Future,
    io,
    net::{SocketAddr, TcpListener as StdTcpListener},
    num::NonZeroUsize,
    sync::Arc,
};

use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use parking_lot::RwLock;
use tokio::{net::TcpListener, sync::broadcast};
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::flashblocks::{
    FlashblockPosition, FlashblockRingBuffer, broadcast::serve_subscriber,
    config::FlashblockProducerConfig,
};

/// A slice paired with where it sits in the stream.
pub type PositionedSlice = (FlashblockPosition, Utf8Bytes);

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
    /// The loop is handed back rather than spawned so that how it runs — and
    /// what happens if it stops — stays a decision for whoever owns the node,
    /// and so binding does not quietly require a runtime to already exist.
    /// Nothing is served until the loop is driven.
    pub fn bind(
        cfg: &FlashblockProducerConfig,
    ) -> io::Result<(Self, impl Future<Output = ()> + Send + use<>)> {
        let ring_capacity = NonZeroUsize::new(cfg.ring_capacity)
            .ok_or_else(|| io::Error::other("flashblocks ring capacity must be non-zero"))?;

        // Bound the archive below by the live channel: a subscriber may lag by
        // a whole channel before it is dropped, and it must be able to resume
        // from wherever it fell behind.
        debug_assert!(cfg.ring_capacity >= cfg.broadcast_capacity);

        let (pipe, _) = broadcast::channel(cfg.broadcast_capacity.max(1));
        let ring = Arc::new(RwLock::new(FlashblockRingBuffer::new(ring_capacity)));
        let cancel = CancellationToken::new();

        // Bind synchronously so a port clash surfaces at startup rather than
        // inside a task nobody is watching.
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
        let text = Utf8Bytes::from(json);
        let position = FlashblockPosition {
            block_number: payload.metadata.block_number,
            flashblock_index: payload.index,
        };

        self.ring.write().push(position, text.clone());

        // No subscribers is the normal case, not a failure: the slice is
        // archived either way.
        let _ = self.pipe.send((position, text));

        Ok(size)
    }

    /// Subscribers currently connected.
    pub fn subscriber_count(&self) -> usize {
        self.pipe.receiver_count()
    }
}

/// Accept subscribers until cancelled, serving each on its own task.
async fn accept_loop(
    listener: TcpListener,
    pipe: broadcast::Sender<PositionedSlice>,
    ring: Arc<RwLock<FlashblockRingBuffer>>,
    cancel: CancellationToken,
) {
    loop {
        let connection = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((connection, _)) => connection,
                Err(error) => {
                    warn!(target: "mantle::preconf::flashblocks", %error, "failed to accept subscriber");
                    continue;
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
}
