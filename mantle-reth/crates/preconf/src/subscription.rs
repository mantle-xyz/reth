//! JSON-RPC subscription handler for `eth_subscribe("newPreconfTransaction")`.
//!
//! Implements the [`PreconfSubscribeApiServer`] trait auto-derived from
//! [`mantle_reth_rpc_ext::PreconfSubscribeApi`]. On subscribe, the
//! handler fans a fresh [`broadcast::Receiver`] out through a
//! [`jsonrpsee`] `SubscriptionSink`, forwarding every
//! [`PreconfTxEvent`] the builder publishes to that subscriber.
//!
//! Broadcast lag (`RecvError::Lagged`) is surfaced as a warn log +
//! continue: the subscriber caught up automatically once the sink
//! drains — no need to close the subscription. Reconciliation of
//! dropped events is client-side (a fresh subscription re-syncs from
//! that point forward).

use std::sync::Arc;

use async_trait::async_trait;
use jsonrpsee::{PendingSubscriptionSink, SubscriptionMessage, SubscriptionSink};
use mantle_reth_rpc_ext::{PreconfSubscribeApiServer, PreconfTxEvent};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Cheap handle a jsonrpsee module holds. Subscribers each obtain
/// their own `broadcast::Receiver` from [`Self::subscribe_events`] so
/// slow consumers cannot back-pressure others.
pub struct PreconfSubscriptionHandler {
    events: broadcast::Sender<PreconfTxEvent>,
}

impl std::fmt::Debug for PreconfSubscriptionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfSubscriptionHandler")
            .field("subscribers", &self.events.receiver_count())
            .finish_non_exhaustive()
    }
}

impl PreconfSubscriptionHandler {
    /// Construct a handler that fans events from the shared
    /// broadcast channel out to individual `eth_subscribe` clients.
    pub const fn new(events: broadcast::Sender<PreconfTxEvent>) -> Self {
        Self { events }
    }
}

#[async_trait]
impl PreconfSubscribeApiServer for PreconfSubscriptionHandler {
    async fn subscribe_new_preconf_transaction(
        &self,
        pending: PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        let rx = self.events.subscribe();
        let sink = pending.accept().await?;
        // Detach the pump so the trait method returns promptly. The
        // spawned task lives as long as the subscription is open —
        // when the client unsubscribes / disconnects, `sink.send`
        // returns `Err(Disconnected)` and we break out.
        tokio::spawn(pump_events(sink, rx));
        Ok(())
    }
}

async fn pump_events(
    sink: SubscriptionSink,
    mut rx: broadcast::Receiver<PreconfTxEvent>,
) {
    // Cache the sink's identity — jsonrpsee 0.26 needs (method, id, payload)
    // per `SubscriptionMessage::new`, and both stay constant across sends
    // on the same sink.
    let method = sink.method_name().to_owned();
    let sub_id = sink.subscription_id();
    loop {
        match rx.recv().await {
            Ok(event) => {
                let msg = match SubscriptionMessage::new(&method, sub_id.clone(), &event) {
                    Ok(m) => m,
                    Err(err) => {
                        warn!(
                            target: "mantle::preconf::subscribe",
                            ?err,
                            "failed to encode preconf event; dropping"
                        );
                        continue;
                    }
                };
                if sink.send(msg).await.is_err() {
                    debug!(
                        target: "mantle::preconf::subscribe",
                        "subscriber disconnected; closing pump"
                    );
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    target: "mantle::preconf::subscribe",
                    skipped,
                    "subscriber lagged behind broadcast channel; events dropped"
                );
                // Continue the loop — `Lagged` leaves the receiver
                // positioned at the newest still-buffered item, so we
                // resume delivery from there.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!(
                    target: "mantle::preconf::subscribe",
                    "broadcast sender dropped; closing pump"
                );
                break;
            }
        }
    }
}

// Suppress unused-import warnings if the trait ever prunes required
// bounds; the `Arc` import remains reachable for callers threading
// through service_builder factory helpers.
#[allow(dead_code)]
type _ArcHint = Arc<PreconfSubscriptionHandler>;
