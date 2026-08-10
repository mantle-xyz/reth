//! Canonical-state listener that keeps [`PreconfTxSet`] in sync with the chain.
//!
//! Subscribes to a provider's [`CanonStateSubscriptions::canonical_state_stream`]
//! and drives best-effort cleanups on committed / reverted chain events:
//!
//! - **Committed chain**: journals the sealed hashes (so the rejournal loop can drop them on its
//!   next rotate tick) and runs [`PreconfTxSet::clean_reclaimable`] to evict `Timeout` / `Canceled`
//!   / `Failed` entries — three "not on chain" states that must not linger on senders who never
//!   post another nonce. Evicted hashes are then `remove_transactions`-ed from the pool so a
//!   preconf tx that already surfaced a not-on-chain wire signal to the client cannot silently land
//!   on chain later (which would corrupt off-chain reconciliation).
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
//! - **Reverted chain**: a reorg produces a warn log for every reverted tx whose hash is tracked
//!   (journal `sealed` set when persistence is enabled, fifo membership as fallback). This handler
//!   performs no recovery action — reorg reinject is delegated to the reth pool's own reset flow
//!   (`transaction-pool/src/maintain.rs` re-admits pruned txs via `add_external_transactions`),
//!   which the preconf pool listener picks up on the next new-pending event and pushes into the
//!   fifo with `PreconfSource::Replay` (see the listener's `journal.contains` check). The
//!   client-observed `block_height` may drift for reorged commitments; op-geth has the same
//!   behavior.
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
use reth_transaction_pool::TransactionPool;
use tracing::{debug, warn};

use crate::{PreconfJournal, preconf_tx_set::PreconfTxSet};

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
    /// Commitment journal (mandatory). Every sealed tx is marked via
    /// [`PreconfJournal::mark_sealed`] so periodic rotation can drop the
    /// entry; the reverted-chain observer keys the `reorg_drift` warning
    /// off [`PreconfJournal::contains`].
    journal: Arc<PreconfJournal>,
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
    Pr: CanonStateSubscriptions<Primitives = N> + 'static,
    P: TransactionPool + 'static,
    N: NodePrimitives,
    N::SignedTx: Transaction + TxHashRef,
{
    /// Construct a handler bound to `provider`'s canonical-state stream.
    /// The `journal` is mandatory — it drives sealed-set bookkeeping and
    /// the reverted-chain `reorg_drift` signal.
    pub const fn new(
        provider: Pr,
        pool: P,
        fifo: Arc<PreconfTxSet>,
        journal: Arc<PreconfJournal>,
    ) -> Self {
        Self { provider, pool, fifo, journal, _n: PhantomData }
    }

    /// Run the listener loop. Returns when the canonical-state stream
    /// terminates (typically at node shutdown, when the provider's
    /// broadcast sender is dropped).
    pub async fn run(self) {
        let mut stream = self.provider.canonical_state_stream();
        while let Some(notif) = stream.next().await {
            // Reverted chain — observability only until the journal
            // subsystem is wired in.
            if let Some(old) = notif.reverted() {
                self.observe_reorg(&old).await;
            }

            // Committed chain — collect sealed hashes for journal
            // marking. The owned-clone iter
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
            let committed = notif.committed();
            let mut sealed_hashes: Vec<alloy_primitives::TxHash> = Vec::new();
            for recovered in
                committed.blocks_iter().flat_map(|block| block.clone_transactions_recovered())
            {
                sealed_hashes.push(*recovered.inner().tx_hash());
            }
            drop(committed);

            // Mark sealed hashes in the journal so the rotation loop
            // can drop them on its next tick.
            self.journal.mark_sealed_batch(sealed_hashes.iter().copied()).await;

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
            // Every preconf commitment that survived to a sealed block is
            // tracked in the journal, so `contains` is a precise reorg-drift
            // signal.
            let tracked = self.journal.contains(&hash).await;
            if tracked {
                warn!(
                    target: "mantle::preconf::canon",
                    ?hash,
                    block = block_number,
                    "reverted block contains tracked preconf tx (reorg_drift)"
                );
            }
        }
    }
}

// Note: `aggregate_nonce_frontier` and its unit tests used to live here.
// The per-sender fifo forward driven by that helper has moved to
// `builder::payload_builder::sync_fifo_forward_to_head` (see module docs
// for the rationale — eliminates the canon vs new-PayloadJob race).
