//! Canonical-state listener that keeps [`PreconfTxSet`] in sync with the chain.
//!
//! Subscribes to a provider's [`CanonStateSubscriptions::canonical_state_stream`]
//! and drives best-effort cleanups on committed / reverted chain events:
//!
//! - **Committed chain**: publishes the persisted block height (the ruler the retention period is
//!   measured against — see `classifier::SEAL_DEPTH`), records every hash that has a promise record
//!   as committed at its block's height, and runs [`PreconfTxSet::clean_reclaimable`] to evict
//!   `Timeout` / `Canceled` / `Failed` entries — three "not on chain" states that must not linger
//!   on senders who never post another nonce. Evicted hashes are then `remove_transactions`-ed from
//!   the pool so a preconf tx that already surfaced a not-on-chain wire signal to the client cannot
//!   silently land on chain later (which would corrupt off-chain reconciliation).
//!
//!   **Nonce-frontier `forward()` moved out**: the per-sender fifo
//!   forward that used to run here now runs synchronously at
//!   `PayloadJob` start (see
//!   `builder::payload_builder::sync_fifo_forward_to_head`). Rationale:
//!   the async fanout of `CanonStateNotification` raced with the next
//!   FCU — a new `PayloadJob` could observe stale `Success` entries and
//!   incorrectly replay them via `reset_success_to_waiting`, silently
//!   double-counting `preconf_gas_used` in the fresh slot. Running the
//!   forward from the `PayloadJob` prologue guarantees fifo consistency
//!   with the parent block state before any dispatch decision.
//! - **Reverted chain**: `classifier.uncommit` withdraws the "seen on chain" observation for every
//!   reverted transaction, **keeping the promise record and the `(sender, nonce)` slot** — the
//!   commitment is live again and must still refuse a same-nonce replacement. Its return value is
//!   the `reorg_drift` signal (it replaces the old proxies: `journal.contains`, and fifo membership
//!   when persistence was off). No recovery action is taken here: reorg reinject is delegated to
//!   the reth pool's own reset flow (`transaction-pool/src/maintain.rs` re-admits pruned txs via
//!   `add_external_transactions`), which the preconf pool listener picks up on the next new-pending
//!   event and pushes into the fifo with `PreconfSource::Replay`. The client-observed
//!   `block_height` may drift for reorged commitments; op-geth has the same behavior.
//!
//! Lifecycle: instantiated once at node startup when preconf is enabled,
//! then spawned as a `spawn_critical_task` on the reth task executor.
//! Returns when the broadcast subscription's sender side closes (typically
//! at node shutdown).

use std::{marker::PhantomData, sync::Arc, time::Duration};

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use futures::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_execution_types::Chain;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::BlockNumReader;
use reth_transaction_pool::TransactionPool;
use tracing::{debug, warn};

use crate::{PreconfClassifier, preconf_tx_set::PreconfTxSet};

/// Max age for an unconsumed `pending_responders` slot before the canon sweep
/// drops it. Well beyond any realistic `preconf_timeout` so an in-flight
/// responder is never evicted early (see
/// [`PreconfTxSet::expire_pending_responders`]).
const PENDING_RESPONDER_TTL: Duration = Duration::from_secs(60);

/// Long-running async task bridging `CanonStateNotification` events to
/// [`PreconfTxSet`] cleanup.
///
/// Generic over the canonical-state subscription source `Pr`. The `N`
/// parameter is `Pr::Primitives` — kept as a separate type parameter so
/// trait bounds on the transaction type (`Transaction`, recovery) can
/// be expressed without re-projecting `<Pr::Primitives as ...>` everywhere.
pub struct PreconfCanonHandler<Pr, P, N> {
    provider: Pr,
    /// Transaction pool. Used to `remove_transactions` the hashes evicted
    /// by [`PreconfTxSet::clean_reclaimable`] so a Timeout or Canceled
    /// preconf tx does NOT quietly land on chain later (which would
    /// violate the client's bookkeeping — see design note in the run
    /// loop).
    pool: P,
    fifo: Arc<PreconfTxSet>,
    /// Verdict cache. Swept once per canonical notification against the
    /// fifo's live set — see [`PreconfClassifier::sweep`]. This handler is the
    /// only place that holds both, which is why the sweep lives here.
    classifier: Arc<PreconfClassifier>,
    _n: PhantomData<fn() -> N>,
}

// Manual `Debug` impl: skip the provider / pool (which would force
// `Pr: Debug` / `P: Debug` on every call site) and the phantom marker.
impl<Pr, P, N> std::fmt::Debug for PreconfCanonHandler<Pr, P, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfCanonHandler").field("fifo", &self.fifo).finish_non_exhaustive()
    }
}

impl<Pr, P, N> PreconfCanonHandler<Pr, P, N>
where
    Pr: CanonStateSubscriptions<Primitives = N> + BlockNumReader + 'static,
    P: TransactionPool + 'static,
    N: NodePrimitives,
    N::SignedTx: Transaction + TxHashRef,
{
    /// Construct a handler bound to `provider`'s canonical-state stream.
    ///
    /// Takes no journal handle. Both of its former uses are gone: sealed hashes
    /// are now recorded as `mark_committed` on the classifier (which owns the
    /// retention decision), and the reorg-drift signal comes from `uncommit`'s
    /// return value instead of `journal.contains`. That also removes the
    /// behavioural split this handler used to have between persistence being on
    /// and off.
    pub const fn new(
        provider: Pr,
        pool: P,
        fifo: Arc<PreconfTxSet>,
        classifier: Arc<PreconfClassifier>,
    ) -> Self {
        Self { provider, pool, fifo, classifier, _n: PhantomData }
    }

    /// Run the listener loop. Returns when the canonical-state stream
    /// terminates (typically at node shutdown, when the provider's
    /// broadcast sender is dropped).
    pub async fn run(self) {
        let mut stream = self.provider.canonical_state_stream();
        while let Some(notif) = stream.next().await {
            // Reverted chain — withdraw the "seen on chain" observation and
            // emit the drift signal; see `observe_reorg`.
            if let Some(old) = notif.reverted() {
                self.observe_reorg(&old).await;
            }

            // Committed chain — record every promised hash as committed at its
            // block height on the classifier. The owned-clone iter
            // (`clone_transactions_recovered`) is used because the
            // borrowed `&Tx` variants would require
            // `&Tx: alloy_consensus::Transaction`, which is gated by
            // `Transaction: 'static` and so does not fire for non-static
            // references. Tx clones for canonical notifications are
            // low-frequency (block cadence) and small (consensus tx with
            // no sidecars).
            //
            // The sender-nonce frontier that used to drive per-sender
            // `fifo.forward()` here now runs in `PayloadJob` prologue
            // (`sync_fifo_forward_to_head`) — see module docs.
            // Publish the ruler before anything reads it this round. This is
            // `last_block_number()` (**on disk**), deliberately not
            // `best_block_number()`: the retention period and the journal's
            // discard gate both mean "durable and buried", and an in-memory
            // canonical block is lost on a non-graceful exit. See
            // `classifier::SEAL_DEPTH`.
            //
            // A read error is logged and skipped rather than propagated: a stale
            // watermark only delays releases, while tearing down this task would
            // stop the fifo cleanup and the sweep as well.
            match self.provider.last_block_number() {
                Ok(height) => self.classifier.observe_persisted(height),
                Err(e) => warn!(
                    target: "mantle::preconf::canon",
                    ?e,
                    "could not read the persisted block height; retention checks keep using the previous watermark"
                ),
            }

            // Record which of our commitments this chain contains, **with the
            // height of the block each was in** — the retention period is a block
            // depth, so a flat list of hashes would not do. `mark_committed`
            // filters: it ignores any hash without a promise record, which is the
            // overwhelming majority here (ordinary user transactions). Without
            // that filter this loop would pin every sender's nonce in the block.
            let committed = notif.committed();
            let mut commitments = 0usize;
            for block in committed.blocks_iter() {
                let height = block.number();
                for recovered in block.clone_transactions_recovered() {
                    if self.classifier.mark_committed(recovered.inner().tx_hash(), height) {
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

            // Housekeeping: evict `Timeout` / `Canceled` / `Failed`
            // entries in one pass — all three are "not on chain,
            // reclaimable" and must NOT linger, or the (sender, nonce)
            // slot they hold would block future preconf submissions
            // from the same sender. `sync_fifo_forward_to_head` at
            // PayloadJob start only drops entries whose nonce trails
            // the sealed frontier; reclaimable entries whose sender
            // never posts another nonce would otherwise stay
            // indefinitely. Running per-notification (~ per sealed
            // block, so ~2s on OP L2) matches op-geth's cadence without
            // requiring a separate background task.
            //
            // Pool-side removal mirrors op-geth: after fifo eviction we
            // `remove_transactions` the same hashes from the pool so a
            // preconf tx that already surfaced Timeout or Canceled to
            // the client CANNOT quietly land on chain later. That silent
            // late-inclusion would break off-chain reconciliation
            // (client accounts it as "failed", chain shows "success").
            //
            // The two calls are not atomic: between fifo eviction and
            // pool removal a concurrent build_payload could theoretically
            // pick up an evicted tx from the pool iterator. The window
            // is µs-scale (both are same-task sequential calls, no await
            // between them beyond mutex acquisition) and has not been
            // observed in devnet — a followup tracks it.
            let evicted = self.fifo.clean_reclaimable().await;
            if !evicted.is_empty() {
                let pool_removed = self.pool.remove_transactions(evicted.clone());
                debug!(
                    target: "mantle::preconf::canon",
                    fifo_count = evicted.len(),
                    pool_count = pool_removed.len(),
                    "clean_reclaimable evicted {} fifo entries; removed {} from pool",
                    evicted.len(),
                    pool_removed.len(),
                );
            }

            // Backstop GC for orphaned RPC responders (see
            // `PreconfTxSet::expire_pending_responders`).
            let expired = self.fifo.expire_pending_responders(PENDING_RESPONDER_TTL).await;
            if expired > 0 {
                debug!(
                    target: "mantle::preconf::canon",
                    expired,
                    "swept orphaned pending preconf responders",
                );
            }

            // Verdict-cache sweep. Runs unconditionally (not only when
            // `evicted` is non-empty): its target is the leak `drop_hash`
            // cannot reach — a tx classified at admission that then sits in
            // `Queued`, never emits a `Pending` event and so never gets a fifo
            // entry at all. Criterion is fifo membership plus a grace period;
            // see `PreconfClassifier::sweep` for why LRU would be wrong here.
            //
            // Independent of the responder sweep above: that one bounds the
            // fifo's `pending_responders`, this one bounds the verdict cache.
            // Neither subsumes the other.
            //
            // **Ordering invariant: this must stay below the `mark_committed`
            // loop above.** `sweep` retains a record either because it is in
            // `live`, or because it is young, or because `committed_height` puts
            // it inside the reorg-retention depth — and nothing in it exempts a
            // record merely for being `promised`. A commitment whose block just
            // became canonical has by then usually lost its fifo entry (its
            // nonce advanced, so `forward` dropped it), so `committed_height` is
            // the only thing left holding it. `mark_committed` is what sets that
            // field, and it is set from *this* notification. Hoist the sweep
            // above that loop and a commitment can be swept in the very
            // notification that was supposed to record it as on chain.
            let live: alloy_primitives::map::foldhash::HashSet<_> =
                self.fifo.snapshot().await.into_iter().collect();
            let dropped = self.classifier.sweep(&live);
            if dropped > 0 {
                debug!(
                    target: "mantle::preconf::canon",
                    dropped,
                    live = live.len(),
                    "swept stale preconf verdicts",
                );
            }
        }
        debug!(target: "mantle::preconf::canon", "canonical state stream closed");
    }

    async fn observe_reorg(&self, old: &Chain<N>) {
        // `clone_transactions_recovered` for the same `Transaction: 'static`
        // reason as the committed-side iteration above.
        //
        // `block_number` records the tip of the reverted chain, not the
        // per-tx block. When a reorg spans multiple blocks every warn
        // log tag under this tip — precise enough for reorg-drift
        // metric aggregation; a per-tx block resolution would require
        // walking `old.blocks_iter()` with an outer loop over blocks.
        let block_number = old.tip().number();
        for recovered in old.blocks_iter().flat_map(|block| block.clone_transactions_recovered()) {
            let hash = *recovered.inner().tx_hash();
            // One call does both jobs, and they are the same job seen twice.
            //
            // It withdraws the "seen on chain" observation — the commitment is
            // live again, so the retention clock that was counting toward
            // forgetting it must stop. The promise record and, crucially, the
            // `(sender, nonce)` slot are **kept**: that is what refuses the
            // same-nonce replacement a reorg invites, and it is why this handler
            // no longer has to race anything. Nothing else happens here.
            //
            // And its return value is exactly the reorg-drift predicate: a
            // reverted transaction we had recorded as committed is drift; one we
            // never observed is not. The old proxies — `journal.contains` (an
            // `.await` on a different lock) and, without persistence, fifo
            // membership (which undercounts once `forward` has cleaned an entry)
            // — are both replaced by the thing that actually knows.
            if self.classifier.uncommit(&hash) {
                warn!(
                    target: "mantle::preconf::canon",
                    ?hash,
                    block = block_number,
                    "reverted block contains a preconf commitment; it keeps its nonce and will be \
                     re-applied (reorg_drift)"
                );
                metrics::counter!("preconf.canon.reorg_drift_total").increment(1);
            }
        }
    }
}

// Note: `aggregate_nonce_frontier` and its unit tests used to live here.
// The per-sender fifo forward driven by that helper has moved to
// `builder::payload_builder::sync_fifo_forward_to_head` (see module docs
// for the rationale — eliminates the canon vs new-PayloadJob race).
