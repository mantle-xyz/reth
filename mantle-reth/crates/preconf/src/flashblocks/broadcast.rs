//! Serving one subscriber: catch it up from the archive, then hand it the
//! live stream.

use std::{collections::HashSet, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::{net::TcpStream, sync::broadcast};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::flashblocks::{FlashblockPosition, FlashblockRingBuffer, publisher::PositionedSlice};

/// How long a subscriber gets to complete the `WebSocket` handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a single catch-up send may take before the subscriber is treated
/// as unable to keep up.
const REPLAY_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// What catching a subscriber up achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Slices handed over before joining the live stream.
    pub sent: usize,
    /// The requested position had already fallen out of the archive, so the
    /// subscriber is missing slices that no longer exist here. It has to close
    /// that gap from the canonical chain instead.
    pub preceded_by_gap: bool,
    /// The requested position sits past everything published. The subscriber's
    /// block was rebuilt out from under it, or it is not talking to this
    /// producer. Nothing is owed either way, but neither cause is routine.
    pub ahead_of_archive: bool,
}

/// Why a subscription ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionExit {
    /// The publisher is shutting down.
    Cancelled,
    /// The subscriber fell far enough behind that slices were dropped, so it
    /// is disconnected to reconnect and replay rather than served a stream
    /// with holes in it.
    Lagged(u64),
    /// The subscriber went away.
    ClientClosed,
    /// Nothing will be published again.
    ChannelClosed,
    /// Writing to the subscriber failed.
    SendFailed,
}

/// Read the slice a reconnecting subscriber last received from its handshake
/// URI: `?block_number=X&flashblock_index=Y`.
///
/// Returns `None` for a first-time connection, and for anything malformed —
/// the query arrives from the network, so an unusable one means "start from
/// live" rather than an error.
pub fn parse_resume_position(uri: &str) -> Option<FlashblockPosition> {
    let query = uri.split_once('?').map(|(_, query)| query)?;

    let param = |name: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.parse::<u64>().ok())
    };

    Some(FlashblockPosition {
        block_number: param("block_number")?,
        flashblock_index: param("flashblock_index")?,
    })
}

/// Serve one subscriber for the life of its connection.
///
/// Takes the sender rather than a receiver so the subscription can only be
/// opened once the handshake is done. Subscribing earlier would let a slow
/// handshake accumulate more slices than the channel holds, and the
/// subscriber would be dropped as lagging the moment it started reading —
/// then reconnect into the same trap. Anything published before this point
/// is the archive's job.
pub(crate) async fn serve_subscriber(
    connection: TcpStream,
    pipe: &broadcast::Sender<PositionedSlice>,
    ring: Arc<RwLock<FlashblockRingBuffer>>,
    cancel: CancellationToken,
) {
    // The resume position lives in the upgrade request, which the handshake
    // consumes: once it returns, the stream no longer carries it. The callback
    // writes it out on the way past.
    let mut resume_from = None;
    let handshake = accept_hdr_async(connection, |request: &Request, response: Response| {
        resume_from = parse_resume_position(&request.uri().to_string());
        Ok(response)
    });

    let mut stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            warn!(target: "mantle::preconf::flashblocks", %error, "subscriber handshake failed");
            return;
        }
        Err(_) => {
            warn!(target: "mantle::preconf::flashblocks", "subscriber handshake timed out");
            return;
        }
    };

    let mut receiver = pipe.subscribe();

    if let Some(cutoff) = resume_from {
        let replayed = tokio::select! {
            () = cancel.cancelled() => return,
            outcome = replay(&mut stream, &mut receiver, &ring, cutoff) => outcome,
        };
        match replayed {
            Ok(outcome) => {
                if outcome.preceded_by_gap {
                    warn!(
                        target: "mantle::preconf::flashblocks",
                        block_number = cutoff.block_number,
                        flashblock_index = cutoff.flashblock_index,
                        "subscriber resumed from before the archive; it must close the gap from canonical",
                    );
                }
                if outcome.ahead_of_archive {
                    warn!(
                        target: "mantle::preconf::flashblocks",
                        block_number = cutoff.block_number,
                        flashblock_index = cutoff.flashblock_index,
                        "subscriber resumed from past everything published; its block was rebuilt or it is on the wrong producer",
                    );
                }
                debug!(
                    target: "mantle::preconf::flashblocks",
                    sent = outcome.sent,
                    "caught subscriber up",
                );
            }
            Err(exit) => {
                debug!(target: "mantle::preconf::flashblocks", ?exit, "catch-up abandoned");
                return;
            }
        }
    }

    let exit = pump_live(&mut stream, &mut receiver, &cancel).await;
    debug!(target: "mantle::preconf::flashblocks", ?exit, "subscription ended");
}

/// Hand over everything published after `cutoff`, in two phases.
///
/// The archive is snapshotted first, then whatever arrived live while that
/// snapshot was being sent is drained and deduplicated against it. Without the
/// second phase a slice published mid-catch-up would be missed: it is too late
/// for the snapshot and too early for the live loop.
async fn replay(
    stream: &mut WebSocketStream<TcpStream>,
    receiver: &mut broadcast::Receiver<PositionedSlice>,
    ring: &RwLock<FlashblockRingBuffer>,
    cutoff: FlashblockPosition,
) -> Result<ReplayOutcome, SubscriptionExit> {
    let (snapshot, preceded_by_gap, ahead_of_archive) = {
        let ring = ring.read();
        (ring.entries_after(&cutoff), ring.precedes_window(&cutoff), ring.exceeds_window(&cutoff))
    };

    let mut delivered: HashSet<FlashblockPosition> =
        HashSet::with_capacity(snapshot.len().saturating_add(receiver.len()));
    let mut sent = 0usize;

    for (position, text) in snapshot {
        delivered.insert(position);
        send_with_timeout(stream, text).await?;
        sent += 1;
    }

    // Drain without awaiting, so nothing new can interleave: an empty channel
    // is the termination condition, not a fixed count.
    let mut pending = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok((position, text)) => {
                if delivered.insert(position) {
                    pending.push(text);
                }
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                return Err(SubscriptionExit::Lagged(skipped));
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                return Err(SubscriptionExit::ChannelClosed);
            }
        }
    }

    for text in pending {
        send_with_timeout(stream, text).await?;
        sent += 1;
    }

    Ok(ReplayOutcome { sent, preceded_by_gap, ahead_of_archive })
}

async fn send_with_timeout(
    stream: &mut WebSocketStream<TcpStream>,
    text: tokio_tungstenite::tungstenite::Utf8Bytes,
) -> Result<(), SubscriptionExit> {
    match tokio::time::timeout(REPLAY_SEND_TIMEOUT, stream.send(Message::Text(text))).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(SubscriptionExit::SendFailed),
    }
}

/// Forward live slices until something ends the subscription.
pub(crate) async fn pump_live(
    stream: &mut WebSocketStream<TcpStream>,
    receiver: &mut broadcast::Receiver<PositionedSlice>,
    cancel: &CancellationToken,
) -> SubscriptionExit {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return SubscriptionExit::Cancelled,

            published = receiver.recv() => match published {
                Ok((_, text)) => {
                    if stream.send(Message::Text(text)).await.is_err() {
                        return SubscriptionExit::SendFailed;
                    }
                }
                // Dropping a lagging subscriber is deliberate: a stream with
                // holes in it is worse than no stream, and reconnecting
                // replays from the archive.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return SubscriptionExit::Lagged(skipped);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return SubscriptionExit::ChannelClosed;
                }
            },

            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => return SubscriptionExit::ClientClosed,
                Some(Err(_)) => return SubscriptionExit::ClientClosed,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, num::NonZeroUsize, sync::Arc, time::Duration};

    use futures_util::StreamExt;
    use parking_lot::RwLock;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::broadcast,
    };
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, accept_async, client_async,
        tungstenite::{Message, Utf8Bytes},
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        FlashblockRingBuffer, PositionedSlice, SubscriptionExit, parse_resume_position, pump_live,
        replay,
    };
    use crate::flashblocks::FlashblockPosition;

    fn pos(block_number: u64, flashblock_index: u64) -> FlashblockPosition {
        FlashblockPosition { block_number, flashblock_index }
    }

    fn slice(position: FlashblockPosition) -> PositionedSlice {
        (
            position,
            Utf8Bytes::from(format!("{}-{}", position.block_number, position.flashblock_index)),
        )
    }

    /// A connected `WebSocket` pair over loopback: the server half is what the
    /// publisher writes to, the client half is what a subscriber reads.
    async fn socket_pair()
    -> (WebSocketStream<TcpStream>, WebSocketStream<MaybeTlsStream<TcpStream>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.expect("accept");
            accept_async(connection).await.expect("server handshake")
        });

        let connection = TcpStream::connect(addr).await.expect("connect");
        let (client, _) = client_async(format!("ws://{addr}/"), MaybeTlsStream::Plain(connection))
            .await
            .expect("client handshake");

        (server.await.expect("server task"), client)
    }

    /// Drain what the subscriber has actually been sent, stopping once the
    /// stream goes quiet.
    async fn drain(
        client: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        expected: usize,
    ) -> Vec<String> {
        let mut seen = Vec::with_capacity(expected);
        for _ in 0..expected {
            match tokio::time::timeout(Duration::from_millis(500), client.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => seen.push(text.as_str().to_owned()),
                _ => break,
            }
        }
        seen
    }

    fn ring_with(
        positions: impl IntoIterator<Item = FlashblockPosition>,
    ) -> Arc<RwLock<FlashblockRingBuffer>> {
        let ring = FlashblockRingBuffer::new(NonZeroUsize::new(16).expect("non-zero"));
        let ring = Arc::new(RwLock::new(ring));
        for position in positions {
            let (position, text) = slice(position);
            ring.write().push(position, text);
        }
        ring
    }

    /// A slice published while the archive is still being replayed belongs to
    /// neither phase on its own: too late for the snapshot, too early for the
    /// live loop. The drain is what keeps it from being dropped.
    #[tokio::test]
    async fn catch_up_delivers_what_arrived_while_the_archive_was_replayed() {
        let (mut server, mut client) = socket_pair().await;
        let ring = ring_with([pos(10, 1), pos(10, 2)]);
        let (pipe, mut receiver) = broadcast::channel(8);
        // Published after this subscriber's receiver existed, but before its
        // catch-up ran.
        pipe.send(slice(pos(10, 3))).expect("receiver is alive");

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 0))
            .await
            .expect("catch-up completes");

        assert_eq!(outcome.sent, 3);
        assert_eq!(drain(&mut client, 4).await, ["10-1", "10-2", "10-3"]);
    }

    /// The same slice can sit in the archive and in the channel at once. It
    /// must be handed over once.
    #[tokio::test]
    async fn catch_up_does_not_repeat_a_slice_held_in_both_phases() {
        let (mut server, mut client) = socket_pair().await;
        let ring = ring_with([pos(10, 1), pos(10, 2)]);
        let (pipe, mut receiver) = broadcast::channel(8);
        pipe.send(slice(pos(10, 2))).expect("receiver is alive");
        pipe.send(slice(pos(10, 3))).expect("receiver is alive");

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 0))
            .await
            .expect("catch-up completes");

        assert_eq!(outcome.sent, 3, "10-2 is in both the archive and the channel");
        assert_eq!(drain(&mut client, 4).await, ["10-1", "10-2", "10-3"]);
    }

    #[tokio::test]
    async fn a_resume_position_inside_the_archive_reports_no_gap() {
        let (mut server, _client) = socket_pair().await;
        let ring = ring_with([pos(10, 1), pos(10, 2)]);
        let (_pipe, mut receiver) = broadcast::channel(8);

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 1))
            .await
            .expect("catch-up completes");

        assert!(!outcome.preceded_by_gap);
    }

    /// The subscriber asked to resume from before anything still held. It is
    /// given the whole window, but the gap in front of that window is real and
    /// only it can close it, from the canonical chain.
    #[tokio::test]
    async fn a_resume_position_older_than_the_archive_is_reported_as_a_gap() {
        let (mut server, mut client) = socket_pair().await;
        let ring = ring_with([pos(10, 5), pos(10, 6)]);
        let (_pipe, mut receiver) = broadcast::channel(8);

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 1))
            .await
            .expect("catch-up completes");

        assert!(outcome.preceded_by_gap);
        assert!(!outcome.ahead_of_archive);
        assert_eq!(
            drain(&mut client, 3).await,
            ["10-5", "10-6"],
            "what is still held is handed over rather than withheld",
        );
    }

    /// A subscriber naming the newest slice it holds is up to date: there is
    /// nothing after it to hand over, and it simply joins the live stream.
    #[tokio::test]
    async fn a_resume_position_at_the_head_of_the_archive_replays_nothing() {
        let (mut server, _client) = socket_pair().await;
        let ring = ring_with([pos(10, 1), pos(10, 2)]);
        let (_pipe, mut receiver) = broadcast::channel(8);

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 2))
            .await
            .expect("catch-up completes");

        assert_eq!(outcome.sent, 0);
        assert!(!outcome.preceded_by_gap);
        assert!(!outcome.ahead_of_archive, "being level with the head is not being ahead of it");
    }

    /// A subscriber can name a slice ahead of anything published: its block was
    /// rebuilt and the slices it holds were abandoned, or it is talking to a
    /// different producer. There is nothing to replay either way.
    #[tokio::test]
    async fn a_resume_position_ahead_of_the_archive_replays_nothing() {
        let (mut server, _client) = socket_pair().await;
        let ring = ring_with([pos(10, 1), pos(10, 2)]);
        let (_pipe, mut receiver) = broadcast::channel(8);

        let outcome = replay(&mut server, &mut receiver, &ring, pos(99, 0))
            .await
            .expect("catch-up completes");

        assert_eq!(outcome.sent, 0);
        assert!(
            !outcome.preceded_by_gap,
            "the gap flag reports a window that moved past the subscriber, not one ahead of it",
        );
        assert!(outcome.ahead_of_archive, "nothing published reaches that position");
    }

    /// After a restart the archive is empty, so there is nothing to replay
    /// whatever the subscriber asks for. It joins live and finds out from the
    /// first slice's predecessor that the producer started over.
    #[tokio::test]
    async fn an_empty_archive_replays_nothing_for_any_position() {
        let (mut server, _client) = socket_pair().await;
        let ring = ring_with([]);
        let (_pipe, mut receiver) = broadcast::channel(8);

        let outcome = replay(&mut server, &mut receiver, &ring, pos(10, 5))
            .await
            .expect("catch-up completes");

        assert_eq!(outcome.sent, 0);
        assert!(!outcome.preceded_by_gap);
        assert!(!outcome.ahead_of_archive, "with nothing archived there is no head to be ahead of",);
    }

    /// A subscriber slow enough to be dropped from the channel is disconnected
    /// rather than served a stream with holes in it; reconnecting replays from
    /// the archive.
    #[tokio::test]
    async fn a_subscriber_that_falls_behind_the_channel_is_disconnected() {
        let (mut server, _client) = socket_pair().await;
        let (pipe, mut receiver) = broadcast::channel(2);
        for index in 0..5 {
            pipe.send(slice(pos(10, index))).expect("receiver is alive");
        }

        let exit = pump_live(&mut server, &mut receiver, &CancellationToken::new()).await;

        assert!(matches!(exit, SubscriptionExit::Lagged(_)), "got {exit:?}");
    }

    #[tokio::test]
    async fn a_cancelled_publisher_ends_the_subscription() {
        let (mut server, _client) = socket_pair().await;
        let (_pipe, mut receiver) = broadcast::channel::<PositionedSlice>(8);
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert_eq!(
            pump_live(&mut server, &mut receiver, &cancel).await,
            SubscriptionExit::Cancelled
        );
    }

    #[tokio::test]
    async fn a_subscriber_going_away_ends_the_subscription() {
        let (mut server, client) = socket_pair().await;
        let (_pipe, mut receiver) = broadcast::channel::<PositionedSlice>(8);
        drop(client);

        assert_eq!(
            pump_live(&mut server, &mut receiver, &CancellationToken::new()).await,
            SubscriptionExit::ClientClosed
        );
    }

    #[test]
    fn a_resume_position_is_read_from_the_query() {
        let position = parse_resume_position("/?block_number=12&flashblock_index=3");

        assert_eq!(position, Some(FlashblockPosition { block_number: 12, flashblock_index: 3 }));
    }

    #[test]
    fn the_parameters_may_arrive_in_either_order() {
        let position = parse_resume_position("/ws?flashblock_index=3&block_number=12");

        assert_eq!(position, Some(FlashblockPosition { block_number: 12, flashblock_index: 3 }));
    }

    /// A subscriber connecting for the first time names no position and just
    /// joins the live stream.
    #[test]
    fn a_connection_without_parameters_asks_for_no_replay() {
        assert_eq!(parse_resume_position("/"), None);
        assert_eq!(parse_resume_position(""), None);
        assert_eq!(parse_resume_position("/ws"), None);
    }

    /// Both halves are needed to name a position, so half a request is not
    /// silently rounded into a whole one.
    #[test]
    fn half_a_position_is_not_a_position() {
        assert_eq!(parse_resume_position("/?block_number=12"), None);
        assert_eq!(parse_resume_position("/?flashblock_index=3"), None);
    }

    /// The query is attacker-controlled, so nothing in it may panic.
    #[test]
    fn unparseable_parameters_are_ignored_rather_than_fatal() {
        for query in [
            "/?block_number=abc&flashblock_index=3",
            "/?block_number=12&flashblock_index=-1",
            "/?block_number=99999999999999999999999&flashblock_index=0",
            "/?block_number=&flashblock_index=",
            "/?block_number&flashblock_index",
            "/?=12&=3",
            "/?&&&",
        ] {
            assert_eq!(parse_resume_position(query), None, "{query} must not yield a position");
        }
    }

    #[test]
    fn a_repeated_parameter_takes_the_first_occurrence() {
        let position =
            parse_resume_position("/?block_number=12&block_number=99&flashblock_index=3");

        assert_eq!(position, Some(FlashblockPosition { block_number: 12, flashblock_index: 3 }));
    }
}
