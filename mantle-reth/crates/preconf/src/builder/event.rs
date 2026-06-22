//! Internal builder-loop event types.
//!
//! The preconf payload job's inner builder loop is a long-running
//! `tokio::select!` over heterogeneous async sources. Reifying each source's
//! wake-up into a `BuilderEvent` variant keeps the loop body small and
//! makes the dispatch tree easy to read.
//!
//! These types are not part of any public API — they are dropped between
//! the source futures and the apply / sweep handlers inside the same
//! crate.
//!
//! Variants are deliberately data-light. The hash for a `Preconf` event
//! is enough to look up the full entry via [`PreconfTxSet::get_tx`]; the
//! variants do not carry full transactions or receipts. This avoids
//! cloning across the dispatch boundary and keeps the enum small enough
//! that `match` codegen is a jump table.

use alloy_primitives::TxHash;

/// One wake-up of the builder loop.
///
/// Not `Clone` — events are consumed exactly once by the dispatch
/// handler. `Debug` is provided only for tracing.
#[derive(Debug)]
pub enum BuilderEvent {
    /// A new (or revived) preconf-eligible tx is sitting in the fifo and
    /// the broadcast notifier woke us. The handler should look up the
    /// entry by `hash` and run `apply_preconf_tx`.
    Preconf(TxHash),

    /// The broadcast subscription lagged behind the publisher (fifo
    /// produced events faster than this loop drained them). The handler
    /// must reconcile by walking [`PreconfTxSet::snapshot`] and applying
    /// every still-pending hash, since some `Preconf(hash)` notifications
    /// have been dropped by the channel.
    BroadcastLagged,

    /// The sweep ticker fired. The handler should pull a batch from
    /// `pool.best_transactions()` and try to extend the in-flight block
    /// with non-preconf work.
    SweepTick,

    /// The cancel signal flipped — the job is being torn down. The
    /// handler should release in-flight state and break out of the
    /// loop.
    Cancel,
}

impl BuilderEvent {
    /// Short human-readable label used in tracing spans / log messages.
    /// Kept inline so log statements don't need to match on the variant
    /// themselves.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Preconf(_) => "preconf",
            Self::BroadcastLagged => "broadcast_lagged",
            Self::SweepTick => "sweep_tick",
            Self::Cancel => "cancel",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn label_matches_variant() {
        assert_eq!(BuilderEvent::Preconf(B256::ZERO).label(), "preconf");
        assert_eq!(BuilderEvent::BroadcastLagged.label(), "broadcast_lagged");
        assert_eq!(BuilderEvent::SweepTick.label(), "sweep_tick");
        assert_eq!(BuilderEvent::Cancel.label(), "cancel");
    }

    #[test]
    fn preconf_carries_hash() {
        let h = B256::from([0x42; 32]);
        match BuilderEvent::Preconf(h) {
            BuilderEvent::Preconf(got) => assert_eq!(got, h),
            other => panic!("expected Preconf, got {other:?}"),
        }
    }

    #[test]
    fn debug_is_useful_for_tracing() {
        // Just smoke-test that Debug doesn't panic and yields something
        // non-empty — exact format isn't part of the contract.
        let s = format!("{:?}", BuilderEvent::SweepTick);
        assert!(!s.is_empty());
    }
}
