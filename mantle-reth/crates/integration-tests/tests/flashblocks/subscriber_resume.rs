//! Reconnect and resume behaviour of [`FlashblocksSubscriber`] (E1, E4).
//!
//! A stub websocket server stands in for the sequencer: it records the request
//! URI of every connection, replays a scripted set of slices, then closes. No
//! node is launched — the subscriber only needs a [`FlashblocksReceiver`].
//!
//! What this cannot cover: whether a producer honours the resume parameters.
//! That is the other half of the contract and needs a producer.

use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use mantle_reth_flashblocks::{FlashblocksReceiver, FlashblocksSubscriber};
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use tokio::{
    net::TcpListener,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};
use url::Url;

use crate::helpers::base_slice;

/// Long enough that the keepalive never fires during a test.
const NO_PINGS: Duration = Duration::from_secs(600);

/// Slices delivered by the subscriber, in order.
#[derive(Debug)]
struct Recorder(UnboundedSender<MantleFlashblockPayload>);

impl FlashblocksReceiver for Recorder {
    fn on_flashblock_received(&self, flashblock: MantleFlashblockPayload) {
        let _ = self.0.send(flashblock);
    }
}

/// What one connection to the stub server sends before closing.
enum Script {
    /// Slices for these block numbers, each an `index == 0` base slice.
    Slices(Vec<u64>),
    /// A message that is neither JSON nor brotli, then a slice.
    Garbage(u64),
    /// A slice, then silence: the connection is never read again, so pings go
    /// unanswered.
    Deaf(u64),
}

/// Serves `script[n]` on the n-th connection, repeating the last entry, and
/// reports every connection's request URI.
async fn stub_server(script: Vec<Script>) -> (Url, UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the stub server");
    let url: Url = format!("ws://{}/ws", listener.local_addr().expect("stub address"))
        .parse()
        .expect("stub url");
    let (uri_tx, uri_rx) = unbounded_channel();

    let script = Arc::new(script);
    tokio::spawn(async move {
        let mut connection = 0usize;
        while let Ok((stream, _)) = listener.accept().await {
            let uri_tx = uri_tx.clone();
            let script = Arc::clone(&script);
            let step = connection.min(script.len().saturating_sub(1));
            connection += 1;

            // One task per connection: a step may hold its connection open for
            // the rest of the test while later connections are still served.
            tokio::spawn(async move {
                let mut websocket =
                    accept_hdr_async(stream, |request: &Request, response: Response| {
                        let _ = uri_tx.send(request.uri().to_string());
                        Ok(response)
                    })
                    .await?;

                match script.get(step) {
                    Some(Script::Slices(block_numbers)) => {
                        for block_number in block_numbers {
                            websocket.send(slice_message(*block_number)).await?;
                        }
                    }
                    Some(Script::Garbage(block_number)) => {
                        websocket.send(Message::binary(vec![0xff; 32])).await?;
                        websocket.send(slice_message(*block_number)).await?;
                        // Held open: the decode error must not drop the connection.
                        while websocket.next().await.is_some() {}
                        return Ok(());
                    }
                    Some(Script::Deaf(block_number)) => {
                        websocket.send(slice_message(*block_number)).await?;
                        // Never polled again, so tungstenite never answers a ping.
                        std::future::pending::<()>().await;
                    }
                    None => {}
                }

                websocket.close(None).await
            });
        }
    });

    (url, uri_rx)
}

/// An `index == 0` base slice for `block_number`, as the sequencer sends it.
fn slice_message(block_number: u64) -> Message {
    Message::text(serde_json::to_string(&base_slice(block_number)).expect("serialise slice"))
}

/// Starts a subscriber against `url` and returns the slices it delivers.
fn subscribe_every(
    url: Url,
    ping_interval: Duration,
) -> UnboundedReceiver<MantleFlashblockPayload> {
    let (tx, rx) = unbounded_channel();
    let mut subscriber = FlashblocksSubscriber::new(Arc::new(Recorder(tx)), url, ping_interval);
    subscriber.start();
    rx
}

/// [`subscribe_every`] with the keepalive effectively disabled.
fn subscribe(url: Url) -> UnboundedReceiver<MantleFlashblockPayload> {
    subscribe_every(url, NO_PINGS)
}

/// Awaits one item, failing the test rather than hanging.
async fn next<T>(receiver: &mut UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), receiver.recv())
        .await
        .expect("an item should arrive")
        .expect("the channel should stay open")
}

/// Slices from the stream reach the receiver in order.
#[tokio::test(flavor = "multi_thread")]
async fn slices_are_forwarded_in_order() {
    let (url, _uris) = stub_server(vec![Script::Slices(vec![1, 2])]).await;
    let mut slices = subscribe(url);

    assert_eq!(next(&mut slices).await.metadata.block_number, 1);
    assert_eq!(next(&mut slices).await.metadata.block_number, 2);
}

/// The first connection has no position to resume from.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_connection_carries_no_resume_parameters() {
    let (url, mut uris) = stub_server(vec![Script::Slices(vec![1])]).await;
    let _slices = subscribe(url);

    assert_eq!(next(&mut uris).await, "/ws");
}

/// After the upstream closes, the subscriber reconnects and asks to resume from
/// the last slice it saw.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_resumes_from_the_last_delivered_slice() {
    let (url, mut uris) =
        stub_server(vec![Script::Slices(vec![1, 2]), Script::Slices(vec![])]).await;
    let mut slices = subscribe(url);

    assert_eq!(next(&mut uris).await, "/ws");
    assert_eq!(next(&mut slices).await.metadata.block_number, 1);
    assert_eq!(next(&mut slices).await.metadata.block_number, 2);

    assert_eq!(
        next(&mut uris).await,
        "/ws?block_number=2&flashblock_index=0",
        "the reconnect must name the last slice received"
    );
}

/// Resume parameters are appended to a URL that already carries a query.
#[tokio::test(flavor = "multi_thread")]
async fn resume_parameters_extend_an_existing_query() {
    let (base, mut uris) = stub_server(vec![Script::Slices(vec![7]), Script::Slices(vec![])]).await;
    let url: Url = format!("{base}?token=abc").parse().expect("url with query");
    let _slices = subscribe(url);

    assert_eq!(next(&mut uris).await, "/ws?token=abc");
    assert_eq!(next(&mut uris).await, "/ws?token=abc&block_number=7&flashblock_index=0");
}

/// An undecodable message is skipped; the connection survives it.
#[tokio::test(flavor = "multi_thread")]
async fn an_undecodable_message_does_not_drop_the_connection() {
    let (url, mut uris) = stub_server(vec![Script::Garbage(4)]).await;
    let mut slices = subscribe(url);

    assert_eq!(next(&mut uris).await, "/ws");
    assert_eq!(next(&mut slices).await.metadata.block_number, 4, "the valid slice still arrives");

    // Longer than the first backoff, so a dropped connection would have shown
    // up as a second request by now.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(uris.try_recv().is_err(), "no reconnect should have been attempted");
}

/// An unanswered ping reconnects, and the reconnect still resumes.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_ping_reconnects_and_resumes() {
    let (url, mut uris) = stub_server(vec![Script::Deaf(9), Script::Slices(vec![])]).await;
    let mut slices = subscribe_every(url, Duration::from_millis(200));

    assert_eq!(next(&mut uris).await, "/ws");
    assert_eq!(next(&mut slices).await.metadata.block_number, 9);

    assert_eq!(next(&mut uris).await, "/ws?block_number=9&flashblock_index=0");
}
