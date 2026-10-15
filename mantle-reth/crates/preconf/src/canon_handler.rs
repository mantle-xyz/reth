//! Canonical-state listener that keeps [`PreconfTxSet`] in sync with the chain.
//!
//! Subscribes to a provider's [`CanonStateSubscriptions::canonical_state_stream`]
//! and drives best-effort cleanups on committed / reverted chain events:
//!
//! - **Committed chain**: publishes the persisted block height (the ruler the retention period is
//!   measured against — see `commitments::SEAL_DEPTH`) and records every hash that has a promise
//!   record as committed at its block's height.
//!
//!   `Commitments::sweep` then runs once per notification, dropping records that are out of reorg
//!   reach and have no fifo entry left. It is a record-cache sweep, not a pass over commitments
//!   parked in a terminal state: one that cannot be honoured is removed when it ends.
//!
//!   The per-sender nonce-frontier `forward()` deliberately does **not** run here: it runs at
//!   `PayloadJob` start (`builder::payload_builder::sync_fifo_forward_to_head`), because the async
//!   fanout of `CanonStateNotification` races the next FCU — a new job could otherwise observe a
//!   stale `Success` entry and replay it via `reset_success_to_waiting`.
//! - **Reverted chain**: `fifo.claims().uncommit` is called for every reverted transaction — see
//!   [`Commitments::uncommit`](crate::commitments::Commitments::uncommit) for what that one call
//!   does and why its return value is the `reorg_drift` signal. Re-injection happens here: each
//!   commitment the reverted chain carried is pushed back with `PreconfSource::Replay`. The
//!   client-observed `block_height` may drift for reorged commitments; op-geth has the same
//!   behavior.
//!
//! Lifecycle: instantiated once at node startup when preconf is enabled,
//! then spawned as a `spawn_critical_task` on the reth task executor.
//! Returns when the broadcast subscription's sender side closes (typically
//! at node shutdown).

use std::{marker::PhantomData, sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use op_alloy_consensus::OpTxEnvelope;
use reth_chain_state::{CanonStateNotification, CanonStateSubscriptions};
use reth_execution_types::Chain;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::BlockNumReader;
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tracing::{debug, error, warn};

use crate::{
    admission::op_envelope_to_alloy,
    commitments::ClaimKind,
    preconf_tx_set::PreconfTxSet,
    types::{PreconfSource, PushResult},
};

/// What one read of the canonical-state subscription produced.
///
/// The gap is a variant rather than an error because it is neither a failure
/// nor an end: the subscription is still live and the next read returns the
/// notification after the ones that were dropped.
enum CanonUpdate<N: NodePrimitives> {
    /// A notification to process.
    Notification(CanonStateNotification<N>),
    /// `skipped` notifications were overwritten before this task read them.
    Gap { skipped: u64 },
    /// The sender is gone — node shutdown.
    Closed,
}

/// Reads one update, keeping a dropped-notification gap distinguishable from
/// the end of the stream.
///
/// Deliberately not `CanonStateSubscriptions::canonical_state_stream`: that
/// helper's `Item` is the notification itself, so a gap is swallowed inside it
/// and this handler cannot see — let alone report — that it happened.
async fn receive_canonical<N: NodePrimitives>(
    receiver: &mut Receiver<CanonStateNotification<N>>,
) -> CanonUpdate<N> {
    match receiver.recv().await {
        Ok(notification) => CanonUpdate::Notification(notification),
        Err(RecvError::Lagged(skipped)) => CanonUpdate::Gap { skipped },
        Err(RecvError::Closed) => CanonUpdate::Closed,
    }
}

/// Long-running async task bridging `CanonStateNotification` events to
/// [`PreconfTxSet`] cleanup.
///
/// Generic over the canonical-state subscription source `Pr`. The `N`
/// parameter is `Pr::Primitives` — kept as a separate type parameter so
/// trait bounds on the transaction type (`Transaction`, recovery) can
/// be expressed without re-projecting `<Pr::Primitives as ...>` everywhere.
pub struct PreconfCanonHandler<Pr, N> {
    provider: Pr,
    fifo: Arc<PreconfTxSet>,
    _n: PhantomData<fn() -> N>,
}

// Manual `Debug` impl: skip the provider (which would force `Pr: Debug` on
// every call site) and the phantom marker.
impl<Pr, N> std::fmt::Debug for PreconfCanonHandler<Pr, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfCanonHandler").field("fifo", &self.fifo).finish_non_exhaustive()
    }
}

impl<Pr, N> PreconfCanonHandler<Pr, N>
where
    Pr: CanonStateSubscriptions<Primitives = N> + BlockNumReader + 'static,
    N: NodePrimitives,
    N::SignedTx: Transaction + TxHashRef,
    // The queue holds alloy envelopes; a reverted commitment has to be
    // converted before it can go back in. Satisfied for any OP-stack
    // primitives by `impl From<OpTransactionSigned> for OpTxEnvelope`.
    OpTxEnvelope: From<N::SignedTx>,
{
    /// Construct a handler bound to `provider`'s canonical-state stream.
    ///
    /// Takes no journal handle: `mark_committed` on `Commitments` (reached
    /// through `fifo.claims()`) owns the retention decision, and the
    /// reorg-drift signal is `uncommit`'s return value.
    pub const fn new(provider: Pr, fifo: Arc<PreconfTxSet>) -> Self {
        Self { provider, fifo, _n: PhantomData }
    }

    /// Run the listener loop. Returns when the canonical-state stream
    /// terminates (typically at node shutdown, when the provider's
    /// broadcast sender is dropped).
    pub async fn run(self) {
        let mut receiver = self.provider.subscribe_to_canonical_state();
        loop {
            let notif = match receive_canonical(&mut receiver).await {
                CanonUpdate::Notification(notif) => notif,
                // Dropped notifications cost this handler the two things it
                // does per notification that have no catch-up pass:
                // `mark_committed` for the blocks it did not see, and
                // `observe_reorg` for a reorg it did not see. Both can leave a
                // promised commitment without a record, which a later reorg
                // then cannot put back. Loud rather than silent, because
                // nothing downstream can tell this happened.
                CanonUpdate::Gap { skipped } => {
                    warn!(
                        target: "mantle::preconf::canon",
                        skipped,
                        "canonical state notifications dropped before the preconf handler read \
                         them; commitments in those blocks are not recorded as committed"
                    );
                    metrics::counter!(
                        "mantle.canon.notifications_lagged_total",
                        "subscriber" => "preconf_canon"
                    )
                    .increment(skipped);
                    continue;
                }
                CanonUpdate::Closed => break,
            };

            // Covers the whole body below, `fifo.snapshot().await` included:
            // that call takes the same lock the admission path holds, and lock
            // contention is the only plausible way this handler falls behind.
            let started = Instant::now();
            // Reverted chain — withdraw the "seen on chain" observation and
            // emit the drift signal; see `observe_reorg`.
            if let Some(old) = notif.reverted() {
                self.observe_reorg(&old).await;
            }

            // Committed chain — record every promised hash as committed at its
            // block height on `Commitments` (`fifo.claims()`). The owned-clone iter
            // (`clone_transactions_recovered`) is needed because the borrowed
            // `&Tx` variants require `&Tx: alloy_consensus::Transaction`, gated
            // by `Transaction: 'static`, which does not fire for non-static
            // references. The clones are block-cadence and small.
            //
            // Publish the ruler first, before anything reads it this round:
            // `last_block_number()` (**on disk**), deliberately not
            // `best_block_number()` — retention means "durable and buried", and
            // an in-memory canonical block is lost on a non-graceful exit (see
            // `commitments::SEAL_DEPTH`). A read error is logged and skipped
            // rather than propagated: a stale watermark only delays releases,
            // while tearing down this task would stop the fifo cleanup and the
            // sweep too.
            match self.provider.last_block_number() {
                Ok(height) => self.fifo.claims().observe_persisted(height),
                Err(e) => warn!(
                    target: "mantle::preconf::canon",
                    ?e,
                    "could not read the persisted block height; retention checks keep using the previous watermark"
                ),
            }

            // Record which of our commitments this chain contains, with the
            // height of the block each was in. Every hash in the block is fed
            // in, ours and everyone else's; the filter `mark_committed` applies
            // to them is load-bearing — see `Commitments::mark_committed`.
            let committed = notif.committed();
            let mut commitments = 0usize;
            for block in committed.blocks_iter() {
                let height = block.number();
                for recovered in block.clone_transactions_recovered() {
                    if self.fifo.claims().mark_committed(recovered.inner().tx_hash(), height) {
                        commitments += 1;
                    }
                }
            }
            drop(committed);

            if commitments > 0 {
                debug!(
                    target: "mantle::preconf::canon",
                    commitments,
                    "recorded preconf commitments as committed"
                );
            }

            // Record-cache sweep. Runs unconditionally: its target is the leak
            // `drop_hash` cannot reach — a record with no fifo entry to be
            // removed from, left behind by `restore_preconf_state`'s
            // `OnChain::No` and `OnChain::Unknown` arms. Criterion is fifo
            // membership plus reorg reach; see `Commitments::sweep`.
            //
            // **Must stay below the `mark_committed` loop above.** A commitment
            // whose block just became canonical has usually lost its fifo entry
            // already, so what holds its record is depth rather than age. For a
            // promise made more than `commitments::SEAL_DEPTH` blocks ago,
            // `sweep`'s promise-recoverable arm has already lapsed, which leaves
            // `committed_height` as the only thing holding it — and
            // `mark_committed` sets that from *this* notification. Hoisted above
            // the loop, the sweep could drop a commitment in the very
            // notification meant to record it as on chain.
            let live: alloy_primitives::map::foldhash::HashSet<_> =
                self.fifo.snapshot().await.into_iter().collect();
            let dropped = self.fifo.claims().sweep(&live);
            if dropped > 0 {
                debug!(
                    target: "mantle::preconf::canon",
                    dropped,
                    live = live.len(),
                    "swept stale commitment records",
                );
            }

            metrics::histogram!(
                "mantle.canon.notification_duration",
                "subscriber" => "preconf_canon"
            )
            .record(started.elapsed());
        }
        debug!(target: "mantle::preconf::canon", "canonical state stream closed");
    }

    async fn observe_reorg(&self, old: &Chain<N>) {
        // `clone_transactions_recovered` for the same `Transaction: 'static`
        // reason as the committed-side iteration above.
        //
        // `block_number` is the tip of the reverted chain, not the per-tx block:
        // a multi-block reorg tags every warning with this tip, which is precise
        // enough for reorg-drift aggregation and avoids an outer loop over
        // blocks.
        let block_number = old.tip().number();
        for recovered in old.blocks_iter().flat_map(|block| block.clone_transactions_recovered()) {
            let hash = *recovered.inner().tx_hash();
            // One call, two answers: whether there is an observation to withdraw,
            // and how strong the claim behind it was — see `Commitments::uncommit`.
            let Some(kind) = self.fifo.claims().uncommit(&hash) else { continue };

            // Only a promise is drift. An announced transaction went out on the
            // slice stream, which every consumer reconciles for itself, so a
            // reorg taking it back is ordinary chain behaviour — and counting
            // it here would put every transaction in the reverted blocks on a
            // metric that exists to report broken preconfirmations.
            if kind == ClaimKind::Promised {
                warn!(
                    target: "mantle::preconf::canon",
                    ?hash,
                    block = block_number,
                    "reverted block contains a preconf commitment; it keeps its nonce and will be \
                     re-applied (reorg_drift)"
                );
                metrics::counter!("preconf.canon.reorg_drift_total").increment(1);
            }

            // And put it back, because nothing else will. The queue entry
            // is gone by now — it was removed when the commitment landed —
            // so `uncommit` alone would leave a promise nobody is going to
            // keep.
            //
            // The transaction comes from the reverted chain itself, which
            // carries it whole; there is no need to ask the journal, and
            // no window in which the chain has dropped it but the journal
            // has not caught up.
            //
            // `Replay`, not `Rpc`: the receipt went out before the reorg,
            // so this is a commitment being honoured rather than a fresh
            // request, and the dispatch gates that protect a waiting
            // client must not apply to it.
            let signer = recovered.signer();
            let (signed, _) = recovered.into_parts();
            match op_envelope_to_alloy(OpTxEnvelope::from(signed)) {
                Some(envelope) => {
                    match self
                        .fifo
                        .push_if_absent(Arc::new(envelope), signer, PreconfSource::Replay)
                        .await
                    {
                        PushResult::Inserted => {}
                        // The queue still holds it, so it is already owed and
                        // this call had nothing to add.
                        PushResult::AlreadyExists => debug!(
                            target: "mantle::preconf::canon",
                            ?hash,
                            "a reverted commitment was still queued; nothing to put back"
                        ),
                        // Unreachable while the bookkeeping holds: `uncommit`
                        // keeps the slot, so admission refuses anything else on
                        // that nonce for as long as the record stands. Reaching
                        // here means the record was released early — a reorg
                        // deeper than `SEAL_DEPTH`, or a `mark_committed` that
                        // never ran because its notification was dropped.
                        //
                        // Reported rather than resolved: both transactions may
                        // have had a receipt go out, so there is no longer a
                        // choice that keeps every promise. Someone has to know.
                        PushResult::ConflictActive(incumbent) => {
                            error!(
                                target: "mantle::preconf::canon",
                                ?hash,
                                ?incumbent,
                                sender = ?signer,
                                "a reverted commitment's nonce is held by another live entry; \
                                 it cannot be put back"
                            );
                            metrics::counter!("preconf.canon.reinject_conflict_total").increment(1);
                        }
                    }
                }
                // A commitment is always one of the user types, so this
                // says the reverted block held a system transaction we had
                // somehow recorded as ours. Loud rather than silent: it
                // would mean the promise bookkeeping is wrong.
                None => error!(
                    target: "mantle::preconf::canon",
                    ?hash,
                    "a reverted commitment is not a user transaction; not replaying it"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reth_execution_types::Chain;
    use reth_optimism_primitives::OpPrimitives;
    use tokio::sync::broadcast;

    use super::*;

    fn notification() -> CanonStateNotification<OpPrimitives> {
        CanonStateNotification::Commit { new: Arc::new(Chain::default()) }
    }

    /// A gap is its own answer. Reading it as the end of the stream is what
    /// `canonical_state_stream` does for its callers, and it is why this
    /// handler could not report one.
    #[tokio::test]
    async fn a_dropped_notification_is_reported_as_a_gap() {
        let (sender, mut receiver) = broadcast::channel(2);
        for _ in 0..3 {
            sender.send(notification()).expect("the receiver is still alive");
        }

        let CanonUpdate::Gap { skipped } = receive_canonical(&mut receiver).await else {
            panic!("the overwritten notification has to be reported as a gap");
        };
        assert_eq!(skipped, 1);

        assert!(
            matches!(receive_canonical(&mut receiver).await, CanonUpdate::Notification(_)),
            "and the subscription keeps delivering afterwards",
        );
    }

    /// The other answer the caller acts on, kept apart from the gap.
    #[tokio::test]
    async fn a_dropped_sender_is_reported_as_closed() {
        let (sender, mut receiver) = broadcast::channel::<CanonStateNotification<OpPrimitives>>(2);
        drop(sender);

        assert!(matches!(receive_canonical(&mut receiver).await, CanonUpdate::Closed));
    }
}
