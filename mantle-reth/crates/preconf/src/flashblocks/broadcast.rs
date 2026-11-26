//! Serving one subscriber: catch it up from the archive, then hand it the
//! live stream.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_rpc_types_engine::PayloadId;
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

/// Bounds a live write even when the peer never drains its socket buffer.
const LIVE_SEND_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The subscriber fell far enough behind that the channel dropped slices
    /// for it. It is disconnected rather than served a stream with holes in
    /// it, which is worse than no stream; its reconnect replays from the
    /// archive, so this is recoverable. Carries the number of slices skipped.
    Lagged(u64),
    /// The subscriber went away.
    ClientClosed,
    /// Nothing will be published again.
    ChannelClosed,
    /// Writing to the subscriber failed.
    SendFailed,
    /// A write timed out. Carries the known unsent count: the current slice
    /// plus the live queue, or the remainder of the current replay batch.
    SendTimedOut(u64),
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
            debug!(target: "mantle::preconf::flashblocks", %error, "subscriber handshake failed");
            return;
        }
        Err(_) => {
            warn!(target: "mantle::preconf::flashblocks", "subscriber handshake timed out");
            return;
        }
    };

    let subscribers = Subscription::join();
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
                subscribers.leave(&exit);
                debug!(target: "mantle::preconf::flashblocks", ?exit, "catch-up abandoned");
                return;
            }
        }
    }

    let exit = pump_live(&mut stream, &mut receiver, &cancel).await;
    subscribers.leave(&exit);
    debug!(target: "mantle::preconf::flashblocks", ?exit, "subscription ended");
}

/// Counts one successfully handshaken subscription for as long as it is held.
///
/// A guard rather than a pair of calls: the subscription has several ways out —
/// a replay that could not be delivered, cancellation, the client going
/// away — and a gauge that only comes down on some of them drifts upward until
/// it says the endpoint is busier than it is.
struct Subscription;

impl Subscription {
    fn join() -> Self {
        metrics::gauge!("flashblocks.subscribers").increment(1.0);
        Self
    }

    /// Record how the subscription ended, before dropping the count.
    fn leave(self, exit: &SubscriptionExit) {
        if let SubscriptionExit::Lagged(skipped) | SubscriptionExit::SendTimedOut(skipped) = exit {
            // Recoverable on its own (see `SubscriptionExit::Lagged`), but a
            // rising rate says the channel is too small for the traffic, or
            // the subscriber too slow.
            metrics::counter!("flashblocks.slow_subscriber_dropped_total").increment(1);
            metrics::counter!("flashblocks.slices_missed_by_slow_subscribers_total")
                .increment(*skipped);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        metrics::gauge!("flashblocks.subscribers").decrement(1.0);
    }
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
    if preceded_by_gap {
        // Count the attempt when detected, even if a later send fails.
        metrics::counter!("flashblocks.resume_before_archive_total").increment(1);
    }

    // Keyed by build as well as position: a rebuild reuses positions, so
    // position alone would read its slices as repeats of the ones they replace
    // and withhold the base that says the block started over.
    let mut delivered: HashSet<(FlashblockPosition, PayloadId)> =
        HashSet::with_capacity(snapshot.len().saturating_add(receiver.len()));
    let mut sent = 0usize;

    let snapshot_len = snapshot.len();
    for (index, (position, payload_id, text)) in snapshot.into_iter().enumerate() {
        delivered.insert((position, payload_id));
        send_with_timeout(stream, text, (snapshot_len - index) as u64).await?;
        sent += 1;
    }

    // Drain without awaiting, so nothing new can interleave: an empty channel
    // is the termination condition, not a fixed count.
    let mut pending = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(slice) => {
                if delivered.insert((slice.position, slice.payload_id)) {
                    pending.push(slice.text);
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

    let pending_len = pending.len();
    for (index, text) in pending.into_iter().enumerate() {
        send_with_timeout(stream, text, (pending_len - index) as u64).await?;
        sent += 1;
    }

    Ok(ReplayOutcome { sent, preceded_by_gap, ahead_of_archive })
}

async fn send_with_timeout(
    stream: &mut WebSocketStream<TcpStream>,
    text: tokio_tungstenite::tungstenite::Utf8Bytes,
    remaining: u64,
) -> Result<(), SubscriptionExit> {
    match tokio::time::timeout(REPLAY_SEND_TIMEOUT, stream.send(Message::Text(text))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SubscriptionExit::SendFailed),
        Err(_) => Err(SubscriptionExit::SendTimedOut(remaining)),
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
                Ok(slice) => {
                    let sent = tokio::select! {
                        () = cancel.cancelled() => return SubscriptionExit::Cancelled,
                        sent = tokio::time::timeout(
                            LIVE_SEND_TIMEOUT,
                            stream.send(Message::Text(slice.text)),
                        ) => sent,
                    };
                    match sent {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => return SubscriptionExit::SendFailed,
                        Err(_) => return SubscriptionExit::SendTimedOut(
                            (receiver.len() as u64).saturating_add(1),
                        ),
                    }
                }
                // Ending the subscription here is deliberate; see
                // `SubscriptionExit::Lagged`.
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

    use alloy_rpc_types_engine::PayloadId;
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

    /// The build these fixtures publish under unless a case needs two.
    fn one_build() -> PayloadId {
        PayloadId::default()
    }

    fn slice(position: FlashblockPosition) -> PositionedSlice {
        PositionedSlice {
            position,
            payload_id: one_build(),
            text: Utf8Bytes::from(format!(
                "{}-{}",
                position.block_number, position.flashblock_index
            )),
        }
    }

    /// The same position under a named build, with the build in the text so an
    /// assertion says which of the two a subscriber was handed.
    fn slice_of(
        position: FlashblockPosition,
        payload_id: PayloadId,
        build: &str,
    ) -> PositionedSlice {
        PositionedSlice {
            position,
            payload_id,
            text: Utf8Bytes::from(format!(
                "{}-{}@{build}",
                position.block_number, position.flashblock_index
            )),
        }
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
            let s = slice(position);
            ring.write().push(s.position, s.payload_id, s.text);
        }
        ring
    }

    #[test]
    fn q02_only_completed_handshakes_count_as_subscribers() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                metrics::gauge!("flashblocks.subscribers").set(0.0);
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                let connection = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
                let (server, _) = listener.accept().await.unwrap();
                let (pipe, _) = broadcast::channel(8);
                let cancel = CancellationToken::new();
                let serving = super::serve_subscriber(server, &pipe, ring_with([]), cancel.clone());
                tokio::pin!(serving);
                assert!(futures_util::poll!(&mut serving).is_pending());
                assert!(
                    handle.render().contains("flashblocks_subscribers 0"),
                    "an idle TCP connection is not a subscription"
                );

                let handshake = client_async("ws://localhost/", connection);
                let (client, _) = tokio::select! {
                    result = handshake => result.unwrap(),
                    () = &mut serving => panic!("subscription ended during handshake"),
                };
                assert!(futures_util::poll!(&mut serving).is_pending());
                assert!(handle.render().contains("flashblocks_subscribers 1"));
                cancel.cancel();
                serving.await;
                assert!(handle.render().contains("flashblocks_subscribers 0"));
                drop(client);
            })
        });
    }

    #[test]
    fn q03_replay_timeout_counts_the_disconnect_and_known_unsent_slices() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                let addr = listener.local_addr().unwrap();
                let ring = ring_with([]);
                ring.write().push(pos(10, 1), one_build(), "x".repeat(32 * 1024 * 1024).into());
                ring.write().push(pos(10, 2), one_build(), "tail".into());
                let (pipe, _) = broadcast::channel(8);
                let server = tokio::spawn(async move {
                    let (connection, _) = listener.accept().await.unwrap();
                    super::serve_subscriber(connection, &pipe, ring, CancellationToken::new())
                        .await;
                });
                let connection = TcpStream::connect(addr).await.unwrap();
                let (_client, _) = client_async(
                    format!("ws://{addr}/?block_number=10&flashblock_index=0"),
                    connection,
                )
                .await
                .unwrap();
                tokio::time::timeout(Duration::from_secs(7), server).await.unwrap().unwrap();
                let scrape = handle.render();
                assert!(scrape.contains("flashblocks_slow_subscriber_dropped_total 1"), "{scrape}");
                assert!(scrape.contains("flashblocks_resume_before_archive_total 1\n"), "{scrape}");
                assert!(
                    scrape.contains("flashblocks_slices_missed_by_slow_subscribers_total 2"),
                    "{scrape}"
                );
                assert!(scrape.contains("flashblocks_subscribers 0"), "{scrape}");
            })
        });
    }

    #[tokio::test]
    async fn q03_pending_replay_timeout_reports_the_remaining_deduplicated_batch() {
        let (mut server, _client) = socket_pair().await;
        let ring = ring_with([]);
        let (pipe, mut receiver) = broadcast::channel(8);
        let mut large = slice(pos(10, 1));
        large.text = "x".repeat(32 * 1024 * 1024).into();
        pipe.send(large.clone()).unwrap();
        pipe.send(large).unwrap();
        pipe.send(slice(pos(10, 2))).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(7),
            replay(&mut server, &mut receiver, &ring, pos(10, 0)),
        )
        .await
        .unwrap();
        assert_eq!(result, Err(SubscriptionExit::SendTimedOut(2)));
    }

    /// What the drain phase of [`replay`] exists for: a slice published while
    /// the archive is still being sent must still reach the subscriber.
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

    /// A rebuild lands while the snapshot is being handed over, republishing
    /// positions the snapshot already holds. Those are different slices: the
    /// index restarts per build, so sharing a position says nothing about
    /// sharing content. Dropping them as repeats withholds the replacement's
    /// base — the one slice that tells a consumer the block started over.
    #[tokio::test]
    async fn catch_up_does_not_mistake_a_rebuild_for_a_repeat() {
        let (mut server, mut client) = socket_pair().await;
        let ring = ring_with([pos(9, 9), pos(10, 0), pos(10, 1)]);
        let (pipe, mut receiver) = broadcast::channel(8);

        let rebuild = PayloadId::new([0xff; 8]);
        for index in 0..=2 {
            pipe.send(slice_of(pos(10, index), rebuild, "B")).expect("receiver is alive");
        }

        let outcome =
            replay(&mut server, &mut receiver, &ring, pos(9, 9)).await.expect("catch-up completes");

        assert_eq!(
            drain(&mut client, 6).await,
            ["10-0", "10-1", "10-0@B", "10-1@B", "10-2@B"],
            "the rebuild's slices are not the archived ones they share a position with",
        );
        assert_eq!(outcome.sent, 5);
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

    #[test]
    fn resume_before_archive_counts_attempts_without_changing_replay() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                crate::seed_preconf_metrics();
                assert!(handle.render().contains("flashblocks_resume_before_archive_total 0\n"));
                for (cutoff, empty, expected_count, expected_messages) in [
                    (pos(10, 5), false, 0, vec!["10-6"]),
                    (pos(10, 6), false, 0, vec![]),
                    (pos(11, 0), false, 0, vec![]),
                    (pos(10, 1), false, 1, vec!["10-5", "10-6"]),
                    (pos(10, 0), false, 2, vec!["10-5", "10-6"]),
                    (pos(10, 5), false, 2, vec!["10-6"]),
                    (pos(10, 0), true, 2, vec![]),
                ] {
                    let (mut server, mut client) = socket_pair().await;
                    let positions = if empty { vec![] } else { vec![pos(10, 5), pos(10, 6)] };
                    let ring = ring_with(positions);
                    let (_pipe, mut receiver) = broadcast::channel(8);
                    let outcome = replay(&mut server, &mut receiver, &ring, cutoff).await.unwrap();
                    assert_eq!(outcome.sent, expected_messages.len());
                    assert_eq!(
                        drain(&mut client, expected_messages.len() + 1).await,
                        expected_messages
                    );
                    let scrape = handle.render();
                    assert!(
                        scrape.contains(&format!(
                            "flashblocks_resume_before_archive_total {expected_count}\n"
                        )),
                        "resume {cutoff:?}, empty archive {empty}: {scrape}"
                    );
                }
            })
        });
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
    /// first slice's predecessor that the history is not continuous.
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

    /// Falling behind the channel must end the subscription, not silently skip
    /// the dropped slices — see [`SubscriptionExit::Lagged`].
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

    /// A peer that stops reading can block send before recv reports lag.
    #[tokio::test]
    async fn a_subscriber_that_never_reads_is_disconnected() {
        let (mut server, _client) = socket_pair().await;
        let (pipe, mut receiver) = broadcast::channel(2);
        let mut large = slice(pos(10, 0));
        large.text = "x".repeat(32 * 1024 * 1024).into();
        pipe.send(large).unwrap();

        let exit = tokio::time::timeout(
            Duration::from_secs(7),
            pump_live(&mut server, &mut receiver, &CancellationToken::new()),
        )
        .await
        .expect("a blocked live send must have a deadline");
        assert_eq!(exit, SubscriptionExit::SendTimedOut(1));
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_blocked_live_send() {
        let (mut server, _client) = socket_pair().await;
        let (pipe, mut receiver) = broadcast::channel(2);
        let mut large = slice(pos(10, 0));
        large.text = "x".repeat(32 * 1024 * 1024).into();
        pipe.send(large).unwrap();
        let cancel = CancellationToken::new();
        let mut pump = Box::pin(pump_live(&mut server, &mut receiver, &cancel));
        assert!(futures_util::poll!(&mut pump).is_pending());
        cancel.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .expect("shutdown must interrupt the blocked socket write");
        assert_eq!(exit, SubscriptionExit::Cancelled);
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
