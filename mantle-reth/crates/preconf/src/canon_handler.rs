//! Canonical-state listener that keeps [`PreconfTxSet`] in sync with the chain.
//!
//! Subscribes to a provider's [`CanonStateSubscriptions::canonical_state_stream`]
//! and drives two best-effort cleanups:
//!
//! - **Committed chain**: each sealed block's transactions advance the
//!   per-sender nonce frontier. For every distinct sender we compute
//!   `max_nonce_in_chain + 1` and call [`PreconfTxSet::forward`], which
//!   drops fifo entries whose nonce sits strictly below the new frontier.
//!   Without this, preconf entries that have already landed on chain
//!   would leak into subsequent slots until `clean_timeout` ages them
//!   out.
//! - **Reverted chain**: a reorg currently produces a warn log for every
//!   reverted tx whose hash still lives in the fifo. The intended
//!   `reorg_drift_total` metric keys off the preconf journal's
//!   `contains(&hash)` (a persistent record of what was actually
//!   promised). The journal subsystem is not yet wired in, so the fifo
//!   membership check serves as a temporary undercounting proxy —
//!   logged through the same code path so the swap is a one-line change.
//!
//! Lifecycle: instantiated once at node startup when preconf is enabled,
//! then spawned as a `spawn_critical_task` on the reth task executor.
//! Returns when the broadcast subscription's sender side closes (typically
//! at node shutdown).

use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use alloy_consensus::{BlockHeader, Transaction, transaction::TxHashRef};
use alloy_primitives::Address;
use futures::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_execution_types::Chain;
use reth_primitives_traits::NodePrimitives;
use tracing::{debug, trace, warn};

use crate::preconf_tx_set::PreconfTxSet;

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

// Manual `Debug` impl: skip the provider (which would force `Pr: Debug`
// on every call site) and the phantom marker.
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
    pub const fn new(provider: Pr, fifo: Arc<PreconfTxSet>) -> Self {
        Self { provider, fifo, _n: PhantomData }
    }

    /// Run the listener loop. Returns when the underlying broadcast
    /// subscription closes (node shutdown).
    pub async fn run(self) {
        let mut stream = self.provider.canonical_state_stream();
        while let Some(notif) = stream.next().await {
            // Reverted chain — observability only until the journal
            // subsystem is wired in.
            if let Some(old) = notif.reverted() {
                self.observe_reorg(&old).await;
            }

            // Committed chain — forward fifo entries past each sender's
            // new nonce frontier. The owned-clone iter
            // (`clone_transactions_recovered`) is used because the
            // borrowed `&Tx` variants would require
            // `&Tx: alloy_consensus::Transaction`, which is gated by
            // `Transaction: 'static` and so does not fire for non-static
            // references. Tx clones for canonical notifications are
            // low-frequency (block cadence) and small (consensus tx with
            // no sidecars).
            let committed = notif.committed();
            let pairs: Vec<(Address, u64)> = committed
                .blocks_iter()
                .flat_map(|block| block.clone_transactions_recovered())
                .map(|r| (r.signer(), r.inner().nonce()))
                .collect();
            drop(committed);
            let frontier = aggregate_nonce_frontier(pairs);
            for (sender, next_nonce) in frontier {
                trace!(
                    target: "mantle::preconf::canon",
                    ?sender, ?next_nonce,
                    "forward fifo cleanup for sealed sender"
                );
                self.fifo.forward(&sender, next_nonce).await;
            }
        }
        debug!(target: "mantle::preconf::canon", "canonical state stream closed");
    }

    async fn observe_reorg(&self, old: &Chain<N>) {
        // `clone_transactions_recovered` for the same `Transaction: 'static`
        // reason as the committed-side iteration above.
        let block_number = old.tip().number();
        for recovered in old.blocks_iter().flat_map(|block| block.clone_transactions_recovered()) {
            let hash = *recovered.inner().tx_hash();
            // Once the preconf journal is wired in, this membership test
            // becomes `journal.contains(&hash)`. Until then the fifo
            // membership check is the only signal available; it
            // undercounts (entries already forward-cleaned) but never
            // overcounts, so the resulting reorg-drift signal is safe to
            // act on operationally.
            if self.fifo.contains(&hash).await {
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

/// Reduce a `(sender, observed_nonce)` stream to a `sender → next_nonce`
/// map, where `next_nonce = max(observed_nonce) + 1` per sender.
///
/// This is exactly the argument [`PreconfTxSet::forward`] expects (drops
/// entries whose nonce is strictly less than the supplied value).
///
/// Taking already-extracted `(Address, u64)` pairs instead of recovered
/// txs avoids leaking `NodePrimitives`/lifetime generics into the helper
/// signature, which keeps the unit tests trivial.
fn aggregate_nonce_frontier(
    items: impl IntoIterator<Item = (Address, u64)>,
) -> HashMap<Address, u64> {
    let mut frontier: HashMap<Address, u64> = HashMap::new();
    for (sender, nonce) in items {
        let next_nonce = nonce.saturating_add(1);
        frontier
            .entry(sender)
            .and_modify(|cur| {
                if next_nonce > *cur {
                    *cur = next_nonce;
                }
            })
            .or_insert(next_nonce);
    }
    frontier
}

#[cfg(test)]
mod tests {
    //! The free `aggregate_nonce_frontier` helper is the only piece of
    //! the handler exercisable in isolation. Listener-loop tests need a
    //! real `CanonStateSubscriptions` impl emitting `CanonStateNotification`
    //! with the OP `NodePrimitives` family — same scaffolding cost as
    //! the pool listener tests, deferred to end-to-end coverage by the
    //! same rationale.

    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn aggregates_single_sender_takes_max_plus_one() {
        let a = addr(1);
        let frontier = aggregate_nonce_frontier([(a, 3), (a, 7), (a, 5)]);
        assert_eq!(frontier.len(), 1);
        assert_eq!(frontier[&a], 8); // max(3,7,5) + 1
    }

    #[test]
    fn aggregates_multi_sender_independent_frontiers() {
        let (a, b) = (addr(1), addr(2));
        let frontier = aggregate_nonce_frontier([(a, 2), (b, 9), (a, 4), (b, 0)]);
        assert_eq!(frontier[&a], 5); // max(2, 4) + 1
        assert_eq!(frontier[&b], 10); // max(9, 0) + 1
    }

    #[test]
    fn aggregates_empty_iterator_yields_empty_map() {
        let frontier = aggregate_nonce_frontier(std::iter::empty::<(Address, u64)>());
        assert!(frontier.is_empty());
    }

    #[test]
    fn aggregates_saturating_add_handles_max_nonce() {
        let a = addr(1);
        let frontier = aggregate_nonce_frontier([(a, u64::MAX)]);
        // saturating_add: u64::MAX + 1 saturates to u64::MAX. forward()
        // then drops every entry with `nonce < u64::MAX` (i.e. all but
        // pathological u64::MAX entries) — acceptable for what is already
        // an impossible-on-chain scenario.
        assert_eq!(frontier[&a], u64::MAX);
    }

    #[test]
    fn aggregates_first_seen_lower_then_higher() {
        let a = addr(1);
        let frontier = aggregate_nonce_frontier([(a, 5), (a, 3)]);
        // First insertion is `5+1=6`. Second tries `3+1=4` which is less,
        // so the entry stays at 6 — `and_modify` only overwrites on strict
        // greater-than.
        assert_eq!(frontier[&a], 6);
    }
}
