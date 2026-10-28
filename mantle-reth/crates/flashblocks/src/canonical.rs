//! Driving a canonical-state subscription without losing the subscription.
//!
//! The channel behind [`CanonStateSubscriptions::subscribe_to_canonical_state`]
//! is a `tokio::sync::broadcast`: a receiver that falls behind its capacity has
//! the oldest notifications overwritten and is told so once, through
//! `RecvError::Lagged`. That error says the *stream* skipped ahead, not that it
//! ended — but the obvious `while let Ok(..)` and `while let Some(Ok(..))`
//! loops read it as an end and stop receiving for the rest of the process.
//!
//! Both readers here drop what was skipped and keep going. Dropping is safe for
//! this consumer because every canonical block is reconciled against the overlay
//! as it arrives rather than applied as a delta, so the next notification
//! reaches the right conclusion whatever came before it. What is *not* safe is
//! losing the subscription, which is why the gap is counted rather than
//! returned: a silent stop looks exactly like a quiet chain in every metric this
//! crate exports.
//!
//! [`CanonStateSubscriptions::subscribe_to_canonical_state`]: reth_chain_state::CanonStateSubscriptions::subscribe_to_canonical_state

use std::{any::Any, future::Future, panic::AssertUnwindSafe, time::Instant};

use futures::FutureExt;
use reth_chain_state::CanonStateNotification;
use reth_primitives_traits::NodePrimitives;
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tracing::{error, warn};

use crate::Metrics;

/// Counts notifications the broadcast dropped before `subscriber` read them.
///
/// One name across every canonical-state subscriber, labelled by which one, so
/// a single alert covers them all.
fn record_gap(subscriber: &'static str, skipped: u64) {
    warn!(
        target: "mantle::flashblocks::canonical",
        subscriber,
        skipped,
        "canonical state notifications dropped before this subscriber read them"
    );
    metrics::counter!("mantle.canon.notifications_lagged_total", "subscriber" => subscriber)
        .increment(skipped);
}

/// Hands every canonical-state notification to `on_notification` until the
/// channel closes, then returns.
pub async fn forward_canonical_state<N, F>(
    subscriber: &'static str,
    mut receiver: Receiver<CanonStateNotification<N>>,
    mut on_notification: F,
) where
    N: NodePrimitives,
    F: FnMut(CanonStateNotification<N>),
{
    loop {
        match receiver.recv().await {
            Ok(notification) => {
                // The whole of this subscriber's per-notification cost: fall
                // far enough behind on it and the broadcast starts dropping.
                let started = Instant::now();
                on_notification(notification);
                metrics::histogram!(
                    "mantle.canon.notification_duration",
                    "subscriber" => subscriber
                )
                .record(started.elapsed());
            }
            Err(RecvError::Lagged(skipped)) => record_gap(subscriber, skipped),
            Err(RecvError::Closed) => return,
        }
    }
}

/// Runs `forwarder`, catching a panic. Every exit is logged as an error and
/// counted in `flashblocks.consumer.canonical_forwarder_exited_total` with
/// `reason` `panic` or `closed`; the forwarder is not restarted. The executor
/// drops it on shutdown, so `closed` means the feed was lost on a running node.
pub async fn run_canonical_forwarder<F>(subscriber: &'static str, forwarder: F)
where
    F: Future<Output = ()>,
{
    match AssertUnwindSafe(forwarder).catch_unwind().await {
        Ok(()) => {
            Metrics::canonical_forwarder_exited("closed");
            error!(
                target: "mantle::flashblocks::canonical",
                subscriber,
                "canonical state notification channel closed; forwarder stopped"
            );
        }
        Err(panic) => {
            Metrics::canonical_forwarder_exited("panic");
            error!(
                target: "mantle::flashblocks::canonical",
                subscriber,
                panic = panic_message(&*panic),
                "canonical state forwarder panicked and stopped; it is not restarted"
            );
        }
    }
}

/// The text of a caught panic payload.
pub(crate) fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>")
}

/// Waits for the first notification `matches` accepts, or `None` once the
/// channel closes.
///
/// Separate from [`forward_canonical_state`] because the caller needs the
/// matching notification back and then stops listening; the predicate stays
/// synchronous so whatever the caller does with the match can `await` outside
/// this loop.
pub async fn await_canonical_match<N, F>(
    subscriber: &'static str,
    receiver: &mut Receiver<CanonStateNotification<N>>,
    mut matches: F,
) -> Option<CanonStateNotification<N>>
where
    N: NodePrimitives,
    F: FnMut(&CanonStateNotification<N>) -> bool,
{
    loop {
        match receiver.recv().await {
            Ok(notification) if matches(&notification) => return Some(notification),
            Ok(_) => {}
            Err(RecvError::Lagged(skipped)) => record_gap(subscriber, skipped),
            Err(RecvError::Closed) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use metrics_exporter_prometheus::PrometheusBuilder;
    use reth_execution_types::Chain;
    use reth_optimism_primitives::OpPrimitives;
    use tokio::sync::broadcast;

    use super::*;

    /// A notification carrying nothing: these tests are about the loop, not
    /// about what the loop hands on.
    fn notification() -> CanonStateNotification<OpPrimitives> {
        CanonStateNotification::Commit { new: Arc::new(Chain::default()) }
    }

    /// Overfills `capacity`, so the receiver's next read reports the gap.
    fn overfilled(
        capacity: usize,
        extra: usize,
    ) -> (
        broadcast::Sender<CanonStateNotification<OpPrimitives>>,
        Receiver<CanonStateNotification<OpPrimitives>>,
    ) {
        let (sender, receiver) = broadcast::channel(capacity);
        for _ in 0..capacity + extra {
            sender.send(notification()).expect("the receiver is still alive");
        }
        (sender, receiver)
    }

    /// The whole point: a gap costs the notifications it skipped, not the
    /// subscription.
    #[tokio::test]
    async fn a_gap_does_not_end_the_forwarding_loop() {
        let (sender, receiver) = overfilled(2, 1);
        let delivered = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&delivered);
        let task = tokio::spawn(forward_canonical_state("test", receiver, move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));

        // Closing the channel is the only thing that may end the loop.
        drop(sender);
        task.await.expect("the loop must end by the channel closing, not by panicking");

        assert_eq!(
            delivered.load(Ordering::SeqCst),
            2,
            "the two notifications still in the channel have to be delivered after the gap",
        );
    }

    fn forwarder_exits(forwarder: impl Future<Output = ()>) -> Option<String> {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(run_canonical_forwarder("test", forwarder));
        });
        handle
            .render()
            .lines()
            .find(|l| l.starts_with("flashblocks_consumer_canonical_forwarder_exited_total"))
            .map(str::to_owned)
    }

    /// A panicking forwarder ends quietly for its caller and is counted.
    #[test]
    fn a_panicking_forwarder_is_caught_and_counted() {
        let line = forwarder_exits(async { panic!("forwarder broke") });
        assert_eq!(
            line.as_deref(),
            Some("flashblocks_consumer_canonical_forwarder_exited_total{reason=\"panic\"} 1")
        );
    }

    /// A forwarder that returns is counted as `closed`.
    #[test]
    fn a_returning_forwarder_is_counted_as_closed() {
        let line = forwarder_exits(async {});
        assert_eq!(
            line.as_deref(),
            Some("flashblocks_consumer_canonical_forwarder_exited_total{reason=\"closed\"} 1")
        );
    }

    /// A match that arrives after a gap is still a match.
    #[tokio::test]
    async fn a_gap_does_not_hide_a_later_match() {
        let (sender, mut receiver) = overfilled(2, 1);

        let found = await_canonical_match("test", &mut receiver, |_| true).await;

        assert!(found.is_some(), "the notifications after the gap are still readable");
        drop(sender);
    }

    /// And the absence of a match is still reported as absence, so the caller
    /// can tell "not here" from "stopped looking".
    #[tokio::test]
    async fn a_closed_channel_reports_no_match() {
        let (sender, mut receiver) = broadcast::channel::<CanonStateNotification<OpPrimitives>>(2);
        sender.send(notification()).expect("the receiver is still alive");
        drop(sender);

        assert!(await_canonical_match("test", &mut receiver, |_| false).await.is_none());
    }
}
