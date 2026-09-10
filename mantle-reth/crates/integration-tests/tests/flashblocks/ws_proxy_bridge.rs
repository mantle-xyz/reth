//! The `ws-proxy` / consumer link: sequencer → proxy → consumer.
//!
//! Both halves were built against the same wire format but had never been run
//! against each other. This suite assembles all three hops in-process — a stub
//! sequencer, a real [`mantle_reth_ws_proxy`] instance on an ephemeral port,
//! and a real [`FlashblocksSubscriber`] — and drives frames end to end. The
//! proxy is built from the library rather than its binary, so the compression,
//! fan-out and resume paths under test are the ones the binary runs.
//!
//! What this cannot cover: no node is launched, so the slices stop at the
//! subscriber's [`FlashblocksReceiver`] and never reach a `pending` block.
//! The address/topic filter is not exercised either — it lives only on the
//! `/ws/{api_key}/filter` route, and reads `metadata.receipts`, which the
//! Mantle wire format leaves permanently empty.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use mantle_reth_flashblocks::{FlashblocksReceiver, FlashblocksSubscriber};
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use mantle_reth_ws_proxy::{
    Authentication, InMemoryRateLimit, Message, RateLimit, RateLimitError, Registry, Server,
    SubscriberOptions, Ticket, TrustedProxyConfig, Uri, WebsocketSubscriber, upstream_listener,
};
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message as WsMessage,
        handshake::server::{Request, Response},
    },
};
use tokio_util::sync::{CancellationToken, DropGuard};
use url::Url;

use crate::helpers::base_slice;

/// Awaits one item, failing the test rather than hanging.
async fn recv_next<T>(receiver: &mut UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), receiver.recv())
        .await
        .expect("an item should arrive")
        .expect("the channel should stay open")
}

/// Slices the consumer delivered, in order.
#[derive(Debug)]
struct Recorder(UnboundedSender<MantleFlashblockPayload>);

impl FlashblocksReceiver for Recorder {
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload) {
        let _ = self.0.send(flashblock);
    }
}

/// What the test tells the stub sequencer to do next.
enum Upstream {
    /// Send this frame on the live connection.
    Frame(WsMessage),
    /// Close the live connection, forcing the proxy to reconnect.
    Close,
}

/// An `index == 0` base slice for `block_number`, as the sequencer sends it.
fn slice_frame(block_number: u64) -> Upstream {
    Upstream::Frame(WsMessage::text(
        serde_json::to_string(&base_slice(block_number)).expect("serialise slice"),
    ))
}

/// A stub sequencer the test drives frame by frame.
///
/// Serves one connection at a time — the proxy holds a single upstream
/// connection — and reports every connection's request URI so a reconnect's
/// resume parameters can be asserted on. Inbound frames are polled but
/// ignored, which is what lets tungstenite answer the proxy's keepalive pings.
async fn stub_sequencer() -> (Uri, UnboundedSender<Upstream>, UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the stub sequencer");
    let addr = listener.local_addr().expect("stub sequencer address");
    let uri: Uri = format!("ws://{addr}/ws").parse().expect("stub sequencer uri");

    let (frame_tx, frame_rx) = unbounded_channel();
    let frames = Arc::new(tokio::sync::Mutex::new(frame_rx));
    let (uri_tx, uri_rx) = unbounded_channel();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let uri_tx = uri_tx.clone();
            let frames = Arc::clone(&frames);

            tokio::spawn(async move {
                let mut websocket =
                    accept_hdr_async(stream, |request: &Request, response: Response| {
                        let _ = uri_tx.send(request.uri().to_string());
                        Ok(response)
                    })
                    .await?;

                // Held for the life of the connection, so a reconnect picks up
                // the queue where this connection left it.
                let mut frames = frames.lock().await;

                loop {
                    tokio::select! {
                        instruction = frames.recv() => match instruction {
                            Some(Upstream::Frame(frame)) => websocket.send(frame).await?,
                            Some(Upstream::Close) | None => break,
                        },
                        inbound = websocket.next() => match inbound {
                            // Polling is the point: it lets tungstenite pong.
                            Some(Ok(_)) => {}
                            _ => return Ok(()),
                        },
                    }
                }

                websocket.close(None).await
            });
        }
    });

    (uri, frame_tx, uri_rx)
}

/// Counts the connections the proxy admits, so a downstream reconnect is
/// observable. Delegates the decision itself to a real [`InMemoryRateLimit`].
///
/// Only acquisitions are counted: the [`Ticket`] handed back carries the inner
/// limiter, so releases bypass this wrapper.
#[derive(Debug)]
struct CountingRateLimit {
    admitted: Arc<AtomicUsize>,
    inner: Arc<InMemoryRateLimit>,
}

impl RateLimit for CountingRateLimit {
    fn try_acquire(self: Arc<Self>, addr: IpAddr) -> Result<Ticket, RateLimitError> {
        self.admitted.fetch_add(1, Ordering::SeqCst);
        Arc::clone(&self.inner).try_acquire(addr)
    }

    fn release(&self, addr: IpAddr) {
        self.inner.release(addr);
    }
}

/// A running proxy: the URL consumers connect to, plus the instrumentation the
/// assertions read.
struct Proxy {
    /// The unauthenticated `/ws` endpoint. Registered only when the proxy was
    /// started without keys; with keys, use [`Proxy::endpoint`].
    url: Url,
    /// The listen address, for building endpoints other than `/ws`.
    addr: SocketAddr,
    /// Downstream connections admitted since start-up.
    admitted: Arc<AtomicUsize>,
    /// The fan-out channel, read for its receiver count to tell when a client
    /// has attached.
    sender: broadcast::Sender<Message>,
    /// Keeps the broadcast channel alive across moments with no clients, as
    /// [`upstream_listener`] documents its caller must.
    _held_receiver: broadcast::Receiver<Message>,
    /// Shuts the subscriber and server down when the test ends.
    _shutdown: DropGuard,
}

impl Proxy {
    /// The endpoint at `path`, e.g. `ws/<key>` for an authenticated route.
    fn endpoint(&self, path: &str) -> Url {
        format!("ws://{}/{path}", self.addr).parse().expect("proxy endpoint")
    }

    /// Waits until a downstream client is attached to the fan-out.
    ///
    /// The proxy keeps no history, so a client sees only what is broadcast
    /// after it attaches. Sending a slice before that point races the fan-out
    /// instead of testing it, and the slice is legitimately gone.
    ///
    /// `Registry::subscribe` takes its receiver before reading any frame, so a
    /// count above the one held by [`Proxy::_held_receiver`] means the next
    /// broadcast will reach the client.
    async fn await_client(&self) {
        for _ in 0..200 {
            if self.sender.receiver_count() > 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no downstream client attached to the proxy");
    }
}

/// Assembles an unauthenticated proxy against `upstream`, on an ephemeral port.
async fn spawn_proxy(upstream: Uri, enable_compression: bool) -> Proxy {
    spawn_proxy_with_auth(upstream, enable_compression, None).await
}

/// Assembles a proxy against `upstream`, on an ephemeral port.
///
/// Client pings are left off: what matters here is whether the *consumer's*
/// keepalive is answered, and the proxy's own client-ping path would mask it.
///
/// `authentication` present makes the server drop `/ws` and serve
/// `/ws/{api_key}` instead — `--public-access-enabled` is left off, which is
/// the configuration the consumer has to cope with by putting the key in its
/// own URL.
async fn spawn_proxy_with_auth(
    upstream: Uri,
    enable_compression: bool,
    authentication: Option<Authentication>,
) -> Proxy {
    let (sender, _held_receiver) = broadcast::channel::<Message>(20);
    let token = CancellationToken::new();
    let fan_out = sender.clone();

    let mut subscriber = WebsocketSubscriber::new(
        upstream,
        upstream_listener(sender.clone(), enable_compression),
        SubscriberOptions::default(),
    );
    let subscriber_token = token.clone();
    tokio::spawn(async move { subscriber.run(subscriber_token).await });

    let admitted = Arc::new(AtomicUsize::new(0));
    let rate_limiter: Arc<dyn RateLimit> = Arc::new(CountingRateLimit {
        admitted: Arc::clone(&admitted),
        inner: Arc::new(InMemoryRateLimit::new(100, 10)),
    });

    let server = Server::new(
        "127.0.0.1:0".parse().expect("proxy listen address"),
        Registry::new(sender, enable_compression, false, 30_000, Duration::from_millis(1_000)),
        rate_limiter,
        authentication,
        TrustedProxyConfig::new("X-Forwarded-For".to_owned(), Vec::new()),
        false,
    );

    let bound = server.bind().await.expect("bind the proxy");
    let addr = bound.local_addr().expect("proxy address");
    let server_token = token.clone();
    tokio::spawn(async move { server.serve(bound, server_token).await });

    Proxy {
        url: format!("ws://{addr}/ws").parse().expect("proxy url"),
        addr,
        admitted,
        sender: fan_out,
        _held_receiver,
        _shutdown: token.drop_guard(),
    }
}

/// Starts a consumer against `url` and returns the slices it delivers.
fn subscribe(url: Url, ping_interval: Duration) -> UnboundedReceiver<MantleFlashblockPayload> {
    let (tx, rx) = unbounded_channel();
    let mut subscriber = FlashblocksSubscriber::new(Arc::new(Recorder(tx)), url, ping_interval);
    subscriber.start();
    rx
}

/// Long enough that the consumer's keepalive never fires during a test.
const NO_PINGS: Duration = Duration::from_secs(600);

/// Uncompressed slices survive both hops, in order.
#[tokio::test(flavor = "multi_thread")]
async fn plaintext_slices_reach_the_consumer_through_the_proxy() {
    let (upstream, sequencer, _uris) = stub_sequencer().await;
    let proxy = spawn_proxy(upstream, false).await;
    let mut slices = subscribe(proxy.url.clone(), NO_PINGS);
    proxy.await_client().await;

    sequencer.send(slice_frame(1)).expect("stub sequencer accepts frames");
    sequencer.send(slice_frame(2)).expect("stub sequencer accepts frames");

    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 1);
    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 2);
}

/// With `--enable-compression` the proxy sends brotli `Binary` frames. The
/// consumer sniffs the encoding rather than being told it, so this is the pair
/// of implementations agreeing without a negotiation step — and the one hop
/// where an encoding mismatch would drop every slice silently.
#[tokio::test(flavor = "multi_thread")]
async fn brotli_frames_reach_the_consumer_through_the_proxy() {
    let (upstream, sequencer, _uris) = stub_sequencer().await;
    let proxy = spawn_proxy(upstream, true).await;
    let mut slices = subscribe(proxy.url.clone(), NO_PINGS);
    proxy.await_client().await;

    sequencer.send(slice_frame(3)).expect("stub sequencer accepts frames");

    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 3);
}

/// The consumer pings the proxy and reconnects if no pong comes back within
/// one interval. The proxy's client reader treats an inbound ping as a no-op,
/// so the pong can only come from the websocket layer underneath it — and it
/// has to come while the stream is idle, which is exactly when nothing is
/// being written to that socket.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_consumer_is_ponged_and_keeps_its_connection() {
    let (upstream, sequencer, _uris) = stub_sequencer().await;
    let proxy = spawn_proxy(upstream, false).await;
    let mut slices = subscribe(proxy.url.clone(), Duration::from_millis(200));
    proxy.await_client().await;

    sequencer.send(slice_frame(1)).expect("stub sequencer accepts frames");
    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 1);

    // Several ping intervals with no traffic in either direction.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    sequencer.send(slice_frame(2)).expect("stub sequencer accepts frames");
    assert_eq!(
        recv_next(&mut slices).await.metadata.block_number,
        2,
        "the consumer should still be delivering after an idle period"
    );
    assert_eq!(
        proxy.admitted.load(Ordering::SeqCst),
        1,
        "an unanswered ping would have forced the consumer to reconnect",
    );
}

/// When the sequencer drops the proxy, the proxy resumes on its own behalf.
/// The consumer's connection is unaffected and it sees no gap.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_resumes_upstream_without_disturbing_the_consumer() {
    let (upstream, sequencer, mut uris) = stub_sequencer().await;
    let proxy = spawn_proxy(upstream, false).await;
    let mut slices = subscribe(proxy.url.clone(), NO_PINGS);
    proxy.await_client().await;

    assert_eq!(recv_next(&mut uris).await, "/ws", "the first connection has no position to resume");

    sequencer.send(slice_frame(5)).expect("stub sequencer accepts frames");
    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 5);

    sequencer.send(Upstream::Close).expect("stub sequencer accepts instructions");
    assert_eq!(
        recv_next(&mut uris).await,
        "/ws?block_number=5&flashblock_index=0",
        "the proxy's reconnect must name the last slice it received",
    );

    sequencer.send(slice_frame(6)).expect("stub sequencer accepts frames");
    assert_eq!(
        recv_next(&mut slices).await.metadata.block_number,
        6,
        "the consumer keeps its connection across an upstream reconnect",
    );
    assert_eq!(
        proxy.admitted.load(Ordering::SeqCst),
        1,
        "the consumer should not have reconnected"
    );
}

/// The consumer appends `block_number`/`flashblock_index` when it reconnects.
/// The proxy's `/ws` route declares no query parameters and holds no history,
/// so such a request is accepted and then ignored: the client is attached to
/// the live stream and the slices it missed are not replayed. Resume is a
/// property of the sequencer, and the proxy does not stand in for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_resume_request_from_the_consumer_is_accepted_and_ignored() {
    let (upstream, sequencer, _uris) = stub_sequencer().await;
    let proxy = spawn_proxy(upstream, false).await;

    // Delivered before any client is attached, so it is genuinely missed.
    sequencer.send(slice_frame(10)).expect("stub sequencer accepts frames");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resuming =
        format!("{}?block_number=10&flashblock_index=0", proxy.url).parse().expect("resume url");
    let mut slices = subscribe(resuming, NO_PINGS);
    proxy.await_client().await;

    sequencer.send(slice_frame(11)).expect("stub sequencer accepts frames");
    assert_eq!(
        recv_next(&mut slices).await.metadata.block_number,
        11,
        "a resume request must not be rejected, and must not replay slice 10",
    );
}

/// The consumer has no argument for an API key, but the proxy's authentication
/// is path-based, so the key goes in `--flashblocks.websocket-url` itself.
///
/// The reconnect is the part that could silently break: `resume_url` appends
/// its position parameters to the configured URL, and if it rebuilt the URL
/// rather than appending, the key would be dropped and every reconnect after
/// the first would 404 — leaving a node that works until its first upstream
/// hiccup. Forcing an upstream close here exercises that path.
///
/// Base's `tests/integration.rs` covers `/ws/{api_key}` with a bare websocket
/// client only; nothing on either side covers a consumer reconnecting through
/// an authenticated route.
#[tokio::test(flavor = "multi_thread")]
async fn the_consumer_authenticates_by_url_and_keeps_the_key_across_a_reconnect() {
    const KEY: &str = "consumer-key";

    let (upstream, sequencer, mut uris) = stub_sequencer().await;
    let authentication =
        Authentication::new(HashMap::from([(KEY.to_owned(), "mantle-rpc".to_owned())]));
    let proxy = spawn_proxy_with_auth(upstream, false, Some(authentication)).await;

    let mut slices = subscribe(proxy.endpoint(&format!("ws/{KEY}")), NO_PINGS);
    proxy.await_client().await;

    assert_eq!(recv_next(&mut uris).await, "/ws");
    sequencer.send(slice_frame(1)).expect("stub sequencer accepts frames");
    assert_eq!(recv_next(&mut slices).await.metadata.block_number, 1);

    // Drop the upstream so the proxy reconnects; the consumer's own connection
    // to the proxy is untouched and must keep delivering.
    sequencer.send(Upstream::Close).expect("stub sequencer accepts instructions");
    assert_eq!(recv_next(&mut uris).await, "/ws?block_number=1&flashblock_index=0");

    sequencer.send(slice_frame(2)).expect("stub sequencer accepts frames");
    assert_eq!(
        recv_next(&mut slices).await.metadata.block_number,
        2,
        "the authenticated consumer connection must survive an upstream reconnect"
    );
    assert_eq!(
        proxy.admitted.load(Ordering::SeqCst),
        1,
        "the consumer should not have reconnected, so its key was used exactly once",
    );
}

/// A consumer pointed at `/ws` when the proxy requires keys gets no stream at
/// all. The failure is a rejected upgrade, which says nothing about a missing
/// key — the node simply never serves flashblock-backed `pending`.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_without_the_key_in_its_url_never_receives_a_slice() {
    let (upstream, sequencer, _uris) = stub_sequencer().await;
    let authentication =
        Authentication::new(HashMap::from([("some-key".to_owned(), "app".to_owned())]));
    let proxy = spawn_proxy_with_auth(upstream, false, Some(authentication)).await;

    let mut slices = subscribe(proxy.url.clone(), NO_PINGS);

    // Let the subscriber attempt its connection and back off.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sequencer.send(slice_frame(1)).expect("stub sequencer accepts frames");
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(slices.try_recv().is_err(), "an unauthenticated consumer must receive nothing");
    assert_eq!(
        proxy.admitted.load(Ordering::SeqCst),
        0,
        "the rate limiter should never have been consulted: the route does not exist",
    );
}
