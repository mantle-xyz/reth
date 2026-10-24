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
//!   does and why its return value is the `reorg_drift` signal. Each commitment the reverted chain
//!   carried is put back as `PreconfSource::Replay` through [`PreconfTxSet::push_if_absent`]. A
//!   build already running may stand on the reverted block; it does not execute the entry, because
//!   a replayed commitment runs only on the canonical chain (`ReplayParentCheck` in
//!   `builder::payload_builder`). A transaction the new chain also carries is skipped; the
//!   committed-chain pass records it at its new height. The client-observed `block_height` may
//!   drift for reorged commitments; op-geth has the same behavior.
//!
//! Lifecycle: instantiated once at node startup when preconf is enabled,
//! then spawned as a `spawn_critical_task` on the reth task executor.
//! Returns when the broadcast subscription's sender side closes (typically
//! at node shutdown).

use std::{marker::PhantomData, sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use alloy_primitives::{TxHash, map::HashSet};
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
                let new_chain: HashSet<TxHash> =
                    notif.committed().transaction_hashes().copied().collect();
                let outcome = reinject_reverted(&self.fifo, &old, &new_chain).await;
                // Counted from what the pass actually found, so the series and
                // the tests are reading the same numbers.
                if outcome.drift > 0 {
                    metrics::counter!("preconf.canon.reorg_drift_total").increment(outcome.drift);
                }
                if outcome.conflicts > 0 {
                    metrics::counter!("preconf.canon.reinject_conflict_total")
                        .increment(outcome.conflicts);
                }
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
}

/// What one reverted chain held, as [`reinject_reverted`] found it.
///
/// Returned rather than counted in place so the caller owns the metrics and a
/// test can read the same numbers the series report.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReorgOutcome {
    /// Commitments a client was promised, which the reorg took back.
    drift: u64,
    /// Transactions put back in the queue.
    reinjected: u64,
    /// Transactions the new chain also carries, so left alone.
    on_new_chain: u64,
    /// Transactions whose `(sender, nonce)` is held by a different live entry,
    /// so they cannot go back.
    conflicts: u64,
}

/// Withdraws the on-chain observation for every transaction a reorg took back,
/// and puts the ones this node owes back in the queue.
///
/// A transaction `new_chain` also carries is skipped: it is still on chain, and
/// the committed-chain pass in [`PreconfCanonHandler::run`] records it at its
/// new height. Withdrawing it would count a false `reorg_drift`.
///
/// Free rather than a method: it needs the queue and nothing else, and a
/// handler cannot be built without a provider.
async fn reinject_reverted<N>(
    fifo: &PreconfTxSet,
    old: &Chain<N>,
    new_chain: &HashSet<TxHash>,
) -> ReorgOutcome
where
    N: NodePrimitives,
    N::SignedTx: Transaction + TxHashRef,
    OpTxEnvelope: From<N::SignedTx>,
{
    let mut outcome = ReorgOutcome::default();

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
        if new_chain.contains(&hash) {
            outcome.on_new_chain += 1;
            continue;
        }
        // One call, two answers: whether there is an observation to withdraw,
        // and how strong the claim behind it was — see `Commitments::uncommit`.
        let Some(kind) = fifo.claims().uncommit(&hash) else { continue };

        // Only a promise is drift. An announced transaction went out on the
        // slice stream, which every consumer reconciles for itself, so a
        // reorg taking it back is ordinary chain behaviour — and counting
        // it here would put every transaction in the reverted blocks on a
        // metric that exists to report broken preconfirmations.
        if kind == ClaimKind::Promised {
            outcome.drift += 1;
            warn!(
                target: "mantle::preconf::canon",
                ?hash,
                block = block_number,
                "reverted block contains a preconf commitment; it keeps its nonce and will be \
                 re-applied (reorg_drift)"
            );
        }

        // Put it back, because nothing else in the preconf path will: its
        // queue entry was removed when it landed. The reverted chain carries
        // the transaction whole, so the journal is not needed.
        //
        // `Replay`, not `Rpc`: the receipt went out before the reorg, so the
        // dispatch gates that protect a waiting client must not apply to it.
        //
        // Announced like any other push. A build still standing on the
        // reverted block reads it and abandons itself instead of executing it
        // — see `ReplayParentCheck` in `builder::payload_builder`.
        //
        // Not the only way back. reth's pool maintenance re-adds the same
        // transaction to the ordinary pool on this notification. While the
        // queue holds it, the pool arm skips it (`apply_one_best_tx` checks
        // `status_of`); if the queue loses it, the pool arm executes it as an
        // ordinary transaction, since the slot gate there lets the slot's own
        // transaction through. Do not filter it out of the pool.
        let signer = recovered.signer();
        let (signed, _) = recovered.into_parts();
        match op_envelope_to_alloy(OpTxEnvelope::from(signed)) {
            Some(envelope) => {
                match fifo.push_if_absent(Arc::new(envelope), signer, PreconfSource::Replay).await {
                    PushResult::Inserted => outcome.reinjected += 1,
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
                        outcome.conflicts += 1;
                        error!(
                            target: "mantle::preconf::canon",
                            ?hash,
                            ?incumbent,
                            sender = ?signer,
                            "a reverted commitment's nonce is held by another live entry; \
                             it cannot be put back"
                        );
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

    outcome
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

#[cfg(test)]
mod reorg_tests {
    use std::collections::BTreeMap;

    use alloy_consensus::{BlockBody, Header, TxEip1559};
    use alloy_primitives::{Address, Signature, U256};
    use op_alloy_consensus::OpTypedTransaction;
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};
    use reth_primitives_traits::RecoveredBlock;

    use super::*;
    use crate::types::PreconfStatus;

    /// Height of the reverted block; nothing here depends on the value.
    const REVERTED_AT: u64 = 7;

    fn sender() -> Address {
        Address::from([1; 20])
    }

    /// Two calls with the same `nonce` and different `salt` differ in hash,
    /// which is what makes them rivals for one `(sender, nonce)`.
    fn op_tx(nonce: u64, salt: u8) -> OpTransactionSigned {
        let inner = TxEip1559 { nonce, value: U256::from(salt), ..Default::default() };
        OpTransactionSigned::new_unhashed(
            OpTypedTransaction::Eip1559(inner),
            Signature::test_signature(),
        )
    }

    /// A reverted chain of one block holding `tx` from [`sender`].
    fn reverted_chain(tx: OpTransactionSigned) -> Chain<OpPrimitives> {
        let body = BlockBody { transactions: vec![tx], ommers: vec![], withdrawals: None };
        let block = alloy_consensus::Block {
            header: Header { number: REVERTED_AT, ..Default::default() },
            body,
        };
        Chain::new(
            vec![RecoveredBlock::new_unhashed(block, vec![sender()])],
            ExecutionOutcome::default(),
            BTreeMap::default(),
        )
    }

    fn fifo() -> PreconfTxSet {
        PreconfTxSet::new(8)
    }

    /// The case the whole mechanism exists for: a receipt went to a client,
    /// the block was taken back, and nothing else would put it back — the
    /// queue entry went when it landed.
    #[tokio::test]
    async fn a_promised_commitment_is_drift_and_goes_back() {
        let fifo = fifo();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();
        assert_eq!(fifo.claims().mark_promised(hash, &sender(), 0, REVERTED_AT), Ok(()));
        assert!(fifo.claims().mark_committed(&hash, REVERTED_AT));

        let outcome = reinject_reverted(&fifo, &reverted_chain(tx), &HashSet::default()).await;

        assert_eq!(
            outcome,
            ReorgOutcome { drift: 1, reinjected: 1, on_new_chain: 0, conflicts: 0 }
        );
        assert_eq!(fifo.status_of(&hash).await, Some(PreconfStatus::Waiting));
        assert_eq!(
            fifo.claims().slot_owner(&sender(), 0),
            Some(hash),
            "the slot is kept, or a replacement could take the nonce",
        );
        assert_eq!(
            fifo.find_by_hash(&hash).await.map(|entry| entry.source),
            Some(PreconfSource::Replay),
            "it goes back as a commitment being honoured, not a fresh request: the dispatch \
             gates that protect a waiting client must not apply to it",
        );
    }

    /// The same commitment, but `mark_committed` never ran for it, as when its
    /// notification is dropped. It still goes back: the reverted block shows it
    /// was on chain. That holds only while its record lasts; past `SEAL_DEPTH`
    /// from the promise, `sweep` has released it and there is nothing to put
    /// back.
    ///
    /// Calls `reinject_reverted` directly, so the handler's `CanonUpdate::Gap`
    /// branch is not exercised here.
    #[tokio::test]
    async fn a_promise_whose_commit_notification_was_lost_still_goes_back() {
        let fifo = fifo();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();
        assert_eq!(fifo.claims().mark_promised(hash, &sender(), 0, REVERTED_AT), Ok(()));
        // No `mark_committed`.

        let outcome = reinject_reverted(&fifo, &reverted_chain(tx), &HashSet::default()).await;

        assert_eq!(
            outcome,
            ReorgOutcome { drift: 1, reinjected: 1, on_new_chain: 0, conflicts: 0 }
        );
        assert_eq!(fifo.status_of(&hash).await, Some(PreconfStatus::Waiting));
        assert_eq!(
            fifo.find_by_hash(&hash).await.map(|entry| entry.source),
            Some(PreconfSource::Replay),
        );
    }

    /// A transaction the new chain also carries is still on chain: it is not
    /// withdrawn, counted as drift, or put back. The committed-chain pass
    /// records it at its new height.
    #[tokio::test]
    async fn a_transaction_the_new_chain_also_carries_is_left_alone() {
        let fifo = fifo();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();
        assert_eq!(fifo.claims().mark_promised(hash, &sender(), 0, REVERTED_AT), Ok(()));
        assert!(fifo.claims().mark_committed(&hash, REVERTED_AT));
        let new_chain: HashSet<TxHash> = [hash].into_iter().collect();

        let outcome = reinject_reverted(&fifo, &reverted_chain(tx), &new_chain).await;

        assert_eq!(outcome, ReorgOutcome { on_new_chain: 1, ..Default::default() });
        assert_eq!(fifo.status_of(&hash).await, None, "not put back in the queue");
        assert!(
            !fifo.claims().release_unless_committed(&hash),
            "still recorded as committed, so a shallow record cannot be released",
        );
    }

    /// The re-injected entry is announced, so a build on the new head picks it
    /// up in the block it is building rather than the next one.
    #[tokio::test]
    async fn a_requeued_commitment_is_announced_to_running_builds() {
        let fifo = fifo();
        let mut running_build = fifo.subscribe();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();
        assert_eq!(fifo.claims().mark_promised(hash, &sender(), 0, REVERTED_AT), Ok(()));
        assert!(fifo.claims().mark_committed(&hash, REVERTED_AT));

        reinject_reverted(&fifo, &reverted_chain(tx), &HashSet::default()).await;

        assert_eq!(fifo.status_of(&hash).await, Some(PreconfStatus::Waiting), "it is queued");
        assert_eq!(running_build.try_recv().ok(), Some(hash), "and announced");
    }

    /// The build loop records every pool transaction it executes, so most of
    /// what a reverted block holds has a record here. Those go back too — the
    /// slice path pruned them from the pool — but they are not broken
    /// preconfirmations and must not be counted as drift.
    #[tokio::test]
    async fn an_executed_pool_transaction_goes_back_without_counting_as_drift() {
        let fifo = fifo();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();
        assert_eq!(fifo.claims().record_announced(hash, &sender(), 0, REVERTED_AT), Ok(()));
        assert!(fifo.claims().mark_committed(&hash, REVERTED_AT));

        let outcome = reinject_reverted(&fifo, &reverted_chain(tx), &HashSet::default()).await;

        assert_eq!(
            outcome,
            ReorgOutcome { drift: 0, reinjected: 1, on_new_chain: 0, conflicts: 0 }
        );
    }

    /// A transaction this node never claimed is somebody else's business.
    #[tokio::test]
    async fn a_transaction_we_never_recorded_is_left_alone() {
        let fifo = fifo();
        let tx = op_tx(0, 1);
        let hash = tx.tx_hash();

        let outcome = reinject_reverted(&fifo, &reverted_chain(tx), &HashSet::default()).await;

        assert_eq!(outcome, ReorgOutcome::default());
        assert_eq!(fifo.status_of(&hash).await, None);
    }

    /// The case that should not be reachable: the commitment's nonce is held
    /// by a different live entry, so it cannot go back. Both may have had a
    /// receipt go out, so nothing here can keep every promise — it is reported
    /// instead, and the report is what this pins.
    #[tokio::test]
    async fn a_nonce_held_by_another_live_entry_is_reported() {
        let fifo = fifo();
        let reverted = op_tx(0, 1);
        let rival = op_tx(0, 2);
        let reverted_hash = reverted.tx_hash();
        let rival_hash = rival.tx_hash();
        assert_ne!(reverted_hash, rival_hash, "the rival has to be a different transaction");

        // The rival occupies `(sender, 0)` in the queue.
        let rival_envelope = op_envelope_to_alloy(rival).expect("a user type");
        assert_eq!(
            fifo.push_if_absent(Arc::new(rival_envelope), sender(), PreconfSource::Rpc).await,
            PushResult::Inserted,
        );

        assert_eq!(fifo.claims().mark_promised(reverted_hash, &sender(), 0, REVERTED_AT), Ok(()));
        assert!(fifo.claims().mark_committed(&reverted_hash, REVERTED_AT));

        let outcome =
            reinject_reverted(&fifo, &reverted_chain(reverted), &HashSet::default()).await;

        assert_eq!(
            outcome,
            ReorgOutcome { drift: 1, reinjected: 0, on_new_chain: 0, conflicts: 1 },
            "drift is still reported — the promise was broken either way",
        );
        assert_eq!(fifo.status_of(&reverted_hash).await, None, "it could not go back");
    }
}
