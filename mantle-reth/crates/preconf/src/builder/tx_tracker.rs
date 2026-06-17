//! Cross-iteration + cross-path dedup for the payload builder.
//!
//! During a single `PayloadJob` the builder may see the same tx hash from
//! multiple sources within one block-build cycle:
//!
//! - the preconf fifo broadcast (event-driven, may also fire from
//!   `broadcast::Receiver::Lagged` reconcile-via-snapshot)
//! - the pool sweep arm (normal-path `best_transactions`)
//! - the start-of-job replay of any commitments that accumulated in the
//!   fifo while no payload job was active
//!
//! `BuilderTxTracker` records the set of hashes that have already been
//! committed to the block-under-construction or deliberately excluded from
//! it, so any later visit of the same hash can be short-circuited before
//! invoking `BlockBuilder::execute_transaction*`.
//!
//! Lifecycle: instantiated per `PayloadJob`; never shared across blocks.
//! On a sealed block the owning `BuilderState` is dropped along with the
//! tracker, and the next job starts with a fresh empty tracker.

use alloy_primitives::TxHash;
use std::collections::HashSet;

/// Cross-iteration tx dedup for the payload builder.
///
/// `committed` and `excluded` are treated as **disjoint** by convention —
/// callers should not record the same hash in both. The tracker does not
/// enforce this; it's a discipline issue at the call sites in the builder
/// loop. Recording the same hash twice in the same set is a no-op (returns
/// `false` from the second call).
#[derive(Debug, Default)]
pub struct BuilderTxTracker {
    /// Hashes that have been successfully applied to the block-under-build.
    committed: HashSet<TxHash>,
    /// Hashes the builder decided not to include in this block (e.g.,
    /// per-block gas budget exceeded, pre-apply deadline expired, EVM revert
    /// the builder chose to surface as a `Failed` preconf).
    excluded: HashSet<TxHash>,
}

impl BuilderTxTracker {
    /// New, empty tracker. Identical to `Default::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `hash` as committed. Returns `true` if it was newly inserted,
    /// `false` if already present (idempotent).
    pub fn record_committed(&mut self, hash: TxHash) -> bool {
        self.committed.insert(hash)
    }

    /// Records `hash` as excluded. Returns `true` if it was newly inserted,
    /// `false` if already present (idempotent).
    pub fn record_excluded(&mut self, hash: TxHash) -> bool {
        self.excluded.insert(hash)
    }

    /// `true` iff `hash` is in the committed set.
    pub fn is_committed(&self, hash: &TxHash) -> bool {
        self.committed.contains(hash)
    }

    /// `true` iff `hash` is in the excluded set.
    pub fn is_excluded(&self, hash: &TxHash) -> bool {
        self.excluded.contains(hash)
    }

    /// `true` iff `hash` is in either set. Used by `collect_new_best` to
    /// filter the pool's `best_transactions` iterator in one pass.
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.is_committed(hash) || self.is_excluded(hash)
    }

    /// Number of committed hashes — exposed for metrics / debug only.
    pub fn committed_len(&self) -> usize {
        self.committed.len()
    }

    /// Number of excluded hashes — exposed for metrics / debug only.
    pub fn excluded_len(&self) -> usize {
        self.excluded.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    #[test]
    fn new_tracker_is_empty() {
        let t = BuilderTxTracker::new();
        assert!(!t.contains(&h(1)));
        assert!(!t.is_committed(&h(1)));
        assert!(!t.is_excluded(&h(1)));
        assert_eq!(t.committed_len(), 0);
        assert_eq!(t.excluded_len(), 0);
    }

    #[test]
    fn record_committed_then_visible_in_committed_and_contains() {
        let mut t = BuilderTxTracker::new();
        assert!(t.record_committed(h(1)));
        assert!(t.is_committed(&h(1)));
        assert!(t.contains(&h(1)));
        assert!(!t.is_excluded(&h(1)));
        assert_eq!(t.committed_len(), 1);
        assert_eq!(t.excluded_len(), 0);
    }

    #[test]
    fn record_excluded_then_visible_in_excluded_and_contains() {
        let mut t = BuilderTxTracker::new();
        assert!(t.record_excluded(h(2)));
        assert!(t.is_excluded(&h(2)));
        assert!(t.contains(&h(2)));
        assert!(!t.is_committed(&h(2)));
    }

    #[test]
    fn double_record_is_idempotent() {
        let mut t = BuilderTxTracker::new();
        assert!(t.record_committed(h(1)));
        assert!(!t.record_committed(h(1))); // second time: already present
        assert_eq!(t.committed_len(), 1);

        assert!(t.record_excluded(h(2)));
        assert!(!t.record_excluded(h(2)));
        assert_eq!(t.excluded_len(), 1);
    }

    #[test]
    fn committed_and_excluded_are_independent_sets() {
        // Recording the same hash in both sets is allowed (sets are
        // disjoint by convention, not by enforcement). Both lookups
        // return true; both length counters are 1.
        let mut t = BuilderTxTracker::new();
        assert!(t.record_committed(h(1)));
        assert!(t.record_excluded(h(1)));
        assert!(t.is_committed(&h(1)));
        assert!(t.is_excluded(&h(1)));
        assert_eq!(t.committed_len(), 1);
        assert_eq!(t.excluded_len(), 1);
    }

    #[test]
    fn different_hashes_dont_alias() {
        let mut t = BuilderTxTracker::new();
        t.record_committed(h(1));
        t.record_excluded(h(2));
        assert!(t.is_committed(&h(1)));
        assert!(!t.is_committed(&h(2)));
        assert!(t.is_excluded(&h(2)));
        assert!(!t.is_excluded(&h(1)));
    }

    #[test]
    fn default_equals_new() {
        let a = BuilderTxTracker::default();
        let b = BuilderTxTracker::new();
        assert_eq!(a.committed_len(), b.committed_len());
        assert_eq!(a.excluded_len(), b.excluded_len());
    }
}
