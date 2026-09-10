//! The proxy's downstream server in isolation: routing, admission, keepalive.
//!
//! Six cases are ported from Base's `websocket-proxy/tests/integration.rs`,
//! three have no Base counterpart. All need neither an upstream nor a consumer
//! — the harness owns the broadcast channel and connects bare websocket
//! clients — so they pin server behaviour independently of the flashblock
//! pipeline `ws_proxy_bridge.rs` drives.
//!
//! Kept in this crate rather than the proxy's own `tests/`, where Base keeps
//! them: `just test-ci` runs `cargo test --workspace --lib`, which skips
//! integration targets, and its nextest step names only this crate. A suite
//! placed the way Base places it would never run in CI.
//!
//! What this cannot cover: `/healthz` (a constant handler, and unlike Base
//! this harness does not use it as a readiness probe — `Server::bind` already
//! returns with the port listening); and whether slices decode end to end,
//! which is `ws_proxy_bridge.rs`.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use mantle_reth_ws_proxy::{
    Authentication, InMemoryRateLimit, Message, RateLimit, Registry, Server, TrustedProxyConfig,
};
use tokio::{
    sync::{
        broadcast,
        mpsc::{UnboundedReceiver, unbounded_channel},
    },
    task::JoinHandle,
};
use tokio_tungstenite::{connect_async, tungstenite::Error as WsError};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::helpers::base_slice;

/// An address that appears nowhere in the broadcast slices, so a filter on it
/// matches no data frame.
const UNMATCHED: &str = "0x00000000000000000000000000000000000000ff";

/// How the harness configures the server under test.
struct Config {
    authentication: Option<Authentication>,
    /// Concurrent connections the whole instance admits.
    instance_limit: usize,
    /// Concurrent connections a single client IP admits.
    ///
    /// Deliberately a separate field: `InMemoryRateLimit::new` takes the two
    /// limits as adjacent `usize` parameters, so passing one value for both
    /// would make every admission assertion blind to their order.
    per_ip_limit: usize,
    /// Whether the registry enforces pong deadlines on clients.
    client_ping_enabled: bool,
    client_pong_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            authentication: None,
            instance_limit: 100,
            per_ip_limit: 100,
            client_ping_enabled: false,
            client_pong_timeout_ms: 30_000,
        }
    }
}

impl Config {
    /// Registers `keys` as `<key> -> app-<key>`.
    fn with_keys(mut self, keys: &[&str]) -> Self {
        let table =
            keys.iter().map(|k| ((*k).to_owned(), format!("app-{k}"))).collect::<HashMap<_, _>>();
        self.authentication = Some(Authentication::new(table));
        self
    }
}

/// A proxy server with no upstream attached; the test publishes slices itself.
struct Harness {
    addr: SocketAddr,
    sender: broadcast::Sender<Message>,
    /// Kept alive so the broadcast channel outlives moments with no clients.
    _held_receiver: broadcast::Receiver<Message>,
    _shutdown: DropGuard,
}

/// A connected client recording the frames it receives.
///
/// Polling the socket is what lets tungstenite answer pings, so a recording
/// client is also a well-behaved one. Dropping it aborts the reader task,
/// which closes the connection.
struct Client {
    frames: UnboundedReceiver<String>,
    task: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A client that never polls its socket, so tungstenite never answers a ping.
struct DeafClient {
    task: JoinHandle<()>,
}

impl Drop for DeafClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Harness {
    async fn start(config: Config) -> Self {
        let (sender, _held_receiver) = broadcast::channel::<Message>(20);
        let token = CancellationToken::new();

        let rate_limiter: Arc<dyn RateLimit> =
            Arc::new(InMemoryRateLimit::new(config.instance_limit, config.per_ip_limit));
        let server = Server::new(
            "127.0.0.1:0".parse().expect("listen address"),
            Registry::new(
                sender.clone(),
                false,
                config.client_ping_enabled,
                config.client_pong_timeout_ms,
                Duration::from_millis(1_000),
            ),
            rate_limiter,
            config.authentication,
            TrustedProxyConfig::new("X-Forwarded-For".to_owned(), Vec::new()),
            false,
        );

        let bound = server.bind().await.expect("bind the proxy");
        let addr = bound.local_addr().expect("proxy address");
        let server_token = token.clone();
        tokio::spawn(async move { server.serve(bound, server_token).await });

        Self { addr, sender, _held_receiver, _shutdown: token.drop_guard() }
    }

    /// Whether a websocket upgrade at `path` succeeds.
    async fn can_connect(&self, path: &str) -> bool {
        connect_async(format!("ws://{}/{path}", self.addr)).await.is_ok()
    }

    /// The rejection body for an upgrade that must fail.
    ///
    /// The two rate limits report distinguishable reasons, which is what makes
    /// it possible to assert *which* limit fired rather than only that one did.
    async fn rejection_body(&self, path: &str) -> String {
        match connect_async(format!("ws://{}/{path}", self.addr)).await {
            Err(WsError::Http(response)) => {
                String::from_utf8_lossy(response.body().as_deref().unwrap_or_default()).into_owned()
            }
            Err(other) => panic!("expected an HTTP rejection, got {other}"),
            Ok(_) => panic!("the upgrade should have been rejected"),
        }
    }

    /// Connects a recording client, or `None` if the upgrade was rejected.
    async fn connect_client(&self, path: &str) -> Option<Client> {
        let (stream, _) = connect_async(format!("ws://{}/{path}", self.addr)).await.ok()?;
        let (tx, frames) = unbounded_channel();

        let task = tokio::spawn(async move {
            let (_write, mut read) = stream.split();
            while let Some(Ok(msg)) = read.next().await {
                if msg.is_text() || msg.is_binary() {
                    let _ = tx.send(String::from_utf8_lossy(&msg.into_data()).into_owned());
                }
            }
        });

        Some(Client { frames, task })
    }

    /// Connects a client that holds the socket open without reading it.
    async fn connect_deaf_client(&self, path: &str) -> DeafClient {
        let (stream, _) = connect_async(format!("ws://{}/{path}", self.addr))
            .await
            .expect("deaf client connects");

        DeafClient {
            task: tokio::spawn(async move {
                let _held = stream;
                std::future::pending::<()>().await;
            }),
        }
    }

    /// Clients currently attached to the fan-out, discounting the held receiver.
    fn client_count(&self) -> usize {
        self.sender.receiver_count().saturating_sub(1)
    }

    /// Waits until exactly `expected` clients are attached.
    async fn await_clients(&self, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.client_count() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("expected {expected} clients, found {}", self.client_count());
    }

    /// Broadcasts a slice for `block_number` as a text frame.
    fn broadcast_slice(&self, block_number: u64) {
        let json = serde_json::to_string(&base_slice(block_number)).expect("serialise slice");
        self.sender.send(Message::Text(json.into())).expect("a receiver is held open");
    }

    /// Broadcasts a ping, as the binary's client-ping task does.
    fn broadcast_ping(&self) {
        self.sender.send(Message::Ping(Vec::new().into())).expect("a receiver is held open");
    }
}

/// Awaits one frame, failing the test rather than hanging.
async fn next_frame(client: &mut Client) -> String {
    tokio::time::timeout(Duration::from_secs(10), client.frames.recv())
        .await
        .expect("a frame should arrive")
        .expect("the channel should stay open")
}

// ---------------------------------------------------------------- routing

/// Configuring any key set drops the public `/ws` route entirely.
///
/// Asserted with an **empty** key table, which is the sharper form: the route
/// disappears because authentication is configured at all, not because some
/// particular key was rejected.
#[tokio::test(flavor = "multi_thread")]
async fn configuring_authentication_removes_the_public_endpoint() {
    let harness = Harness::start(Config {
        authentication: Some(Authentication::none()),
        ..Config::default()
    })
    .await;

    assert!(!harness.can_connect("ws").await, "`/ws` must not be served once keys are configured");
}

/// Known keys are admitted, unknown ones rejected.
#[tokio::test(flavor = "multi_thread")]
async fn only_known_keys_are_admitted() {
    let harness = Harness::start(Config::default().with_keys(&["key1", "key2", "key3"])).await;

    for key in ["key1", "key2", "key3"] {
        assert!(harness.can_connect(&format!("ws/{key}")).await, "`{key}` should be admitted");
    }
    assert!(!harness.can_connect("ws/key4").await, "an unregistered key must be rejected");
}

/// The complement of [`configuring_authentication_removes_the_public_endpoint`]:
/// with no keys the authenticated route does not exist either. The two routing
/// tables are disjoint, so no single configuration serves both — which is why
/// a consumer's URL has to match the mode the proxy was started in.
///
/// No Base counterpart.
#[tokio::test(flavor = "multi_thread")]
async fn without_keys_only_the_public_endpoint_exists() {
    let harness = Harness::start(Config::default()).await;

    assert!(harness.can_connect("ws").await, "`/ws` is the sole route when no keys are set");
    assert!(
        !harness.can_connect("ws/anything").await,
        "`/ws/{{api_key}}` must not be served when no keys are set"
    );
}

// ------------------------------------------------------------- fan-out

/// Every attached client receives every slice, in order. This is the whole
/// purpose of the component, and the only case that exercises more than one
/// client at a time.
#[tokio::test(flavor = "multi_thread")]
async fn all_clients_receive_every_slice() {
    let harness = Harness::start(Config::default()).await;

    let mut one = harness.connect_client("ws").await.expect("first client connects");
    let mut two = harness.connect_client("ws").await.expect("second client connects");
    harness.await_clients(2).await;

    harness.broadcast_slice(1);
    harness.broadcast_slice(2);

    for client in [&mut one, &mut two] {
        for expected in [1, 2] {
            let frame = next_frame(client).await;
            assert!(
                frame.contains(&format!("\"block_number\":{expected}")),
                "expected slice {expected}, got {frame}"
            );
        }
    }
}

/// The instance-wide connection limit is enforced at upgrade time.
///
/// The per-IP limit is left wide open so that only the instance limit can
/// fire, and the rejection reason is asserted rather than just the rejection:
/// `InMemoryRateLimit::new` takes the two limits as adjacent `usize`
/// parameters, and asserting only "some limit rejected it" would pass just as
/// well with them swapped.
#[tokio::test(flavor = "multi_thread")]
async fn the_instance_connection_limit_rejects_the_surplus_client() {
    let harness =
        Harness::start(Config { instance_limit: 3, per_ip_limit: 100, ..Config::default() }).await;

    let mut admitted = Vec::new();
    for _ in 0..3 {
        admitted.push(harness.connect_client("ws").await.expect("within the limit"));
    }
    harness.await_clients(3).await;

    let rejection = harness.rejection_body("ws").await;
    assert!(
        rejection.contains("Global limit"),
        "the fourth client must be rejected by the instance limit, got: {rejection}"
    );

    harness.broadcast_slice(1);
    for client in &mut admitted {
        assert!(next_frame(client).await.contains("\"block_number\":1"));
    }
}

/// The per-IP limit is enforced independently of the instance limit.
///
/// Every test client connects from `127.0.0.1`, so the instance limit is set
/// well above the per-IP one to make the per-IP limit the only one that can
/// fire. Swapping the two constructor arguments makes the surplus client hit
/// the instance limit instead and reports `Global limit`, so this case and
/// [`the_instance_connection_limit_rejects_the_surplus_client`] fail together
/// if the wiring is reversed.
///
/// Base covers the per-IP limit only in `rate_limit.rs`'s unit tests, which do
/// not reach `websocket_handler`.
#[tokio::test(flavor = "multi_thread")]
async fn the_per_ip_connection_limit_rejects_a_third_connection_from_one_address() {
    let harness =
        Harness::start(Config { instance_limit: 5, per_ip_limit: 2, ..Config::default() }).await;

    let _one = harness.connect_client("ws").await.expect("first from this IP");
    let _two = harness.connect_client("ws").await.expect("second from this IP");
    harness.await_clients(2).await;

    let rejection = harness.rejection_body("ws").await;
    assert!(
        rejection.contains("IP limit exceeded"),
        "the third connection from one IP must hit the per-IP limit, got: {rejection}"
    );
}

/// A departed client frees its slot: the fan-out drops it and a new client
/// takes its place. Without this a long-running proxy would reach its limit
/// and refuse every subsequent connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_departed_client_frees_its_slot() {
    let harness = Harness::start(Config { instance_limit: 3, ..Config::default() }).await;

    let mut one = harness.connect_client("ws").await.expect("first client connects");
    let two = harness.connect_client("ws").await.expect("second client connects");
    let three = harness.connect_client("ws").await.expect("third client connects");
    harness.await_clients(3).await;

    // At the limit, so admission must fail before anything is released.
    assert!(harness.connect_client("ws").await.is_none(), "the instance is at its limit");

    drop(three);
    harness.await_clients(2).await;

    let mut four = harness.connect_client("ws").await.expect("the freed slot is reusable");
    harness.await_clients(3).await;

    harness.broadcast_slice(9);
    assert!(next_frame(&mut one).await.contains("\"block_number\":9"));
    assert!(next_frame(&mut four).await.contains("\"block_number\":9"));

    drop(two);
}

// ------------------------------------------------------------- keepalive

/// A client that never answers a ping is disconnected once its pong deadline
/// passes.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresponsive_client_is_disconnected() {
    let harness = Harness::start(Config {
        client_ping_enabled: true,
        client_pong_timeout_ms: 500,
        ..Config::default()
    })
    .await;

    let _deaf = harness.connect_deaf_client("ws").await;
    harness.await_clients(1).await;

    harness.broadcast_ping();
    harness.await_clients(0).await;
}

/// A client that set a filter must still be pinged.
///
/// The filter is evaluated per frame, and a ping carries no payload to match.
/// Running control frames through the filter would drop them for every
/// filtered client, which does not merely lose a frame: the client never
/// pongs, its deadline expires, and it is evicted every `pong_timeout`. The
/// data frames broadcast here deliberately match nothing, so the only frames
/// that can keep the connection alive are the pings.
///
/// No Base counterpart — Base feeds control frames to the filter, so this
/// combination is broken there and could not have been asserted.
#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_client_is_still_pinged_and_survives() {
    const KEY: &str = "third-party";

    let harness = Harness::start(Config {
        client_ping_enabled: true,
        client_pong_timeout_ms: 600,
        ..Config::default().with_keys(&[KEY])
    })
    .await;

    let mut filtered = harness
        .connect_client(&format!("ws/{KEY}/filter?addresses={UNMATCHED}"))
        .await
        .expect("a filtered client connects");
    harness.await_clients(1).await;

    // Three pong deadlines' worth of pings, interleaved with data the filter
    // rejects.
    for block in 1..=6 {
        harness.broadcast_ping();
        harness.broadcast_slice(block);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    assert_eq!(
        harness.client_count(),
        1,
        "a filtered client must receive pings, otherwise it is evicted every pong timeout",
    );
    assert!(
        filtered.frames.try_recv().is_err(),
        "the filter must still suppress data frames that match nothing",
    );
}
