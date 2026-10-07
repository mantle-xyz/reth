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
pub struct PreconfCanonHandler<Pr, N> {
    provider: Pr,
    fifo: Arc<PreconfTxSet>,
    /// Commitment journal (mandatory). Every sealed tx is marked via
    /// [`PreconfJournal::mark_sealed`] so periodic rotation can drop the
    /// entry; the reverted-chain observer keys the `reorg_drift` warning
    /// off [`PreconfJournal::contains`].
    journal: Arc<PreconfJournal>,
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
    Pr: CanonStateSubscriptions<Primitives = N> + 'static,
    N: NodePrimitives,
    N::SignedTx: Transaction + TxHashRef,
{
    /// Construct a handler bound to `provider`'s canonical-state stream.
    /// The `journal` is mandatory — it drives sealed-set bookkeeping and
    /// the reverted-chain `reorg_drift` signal.
    pub const fn new(provider: Pr, fifo: Arc<PreconfTxSet>, journal: Arc<PreconfJournal>) -> Self {
        Self { provider, fifo, journal, _n: PhantomData }
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

            // A `Success` entry that survived this block without landing was
            // carried by a builder block nobody adopted. Demote it so it stops
            // being untouchable: still must-land, but now overturnable if every
            // build fails it, which is what keeps a wedged promise from pinning
            // its `(sender, nonce)` slot forever.
            let landed: std::collections::HashSet<_> = sealed_hashes.iter().copied().collect();
            let demoted = self.fifo.demote_unlanded_promises(&landed).await;
            if demoted > 0 {
                debug!(
                    target: "mantle::preconf::canon",
                    demoted,
                    "demoted {demoted} unadopted preconf commitments to replay"
                );
            }

            // No reclaimable sweep here any more: a terminal outcome removes
            // the entry in the same critical section that records it, so only
            // `Waiting` and `Success` persist. The build's pre-apply deadline
            // gate is what bounds how long a stuck `Waiting` entry holds its
            // `(sender, nonce)` slot.

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
