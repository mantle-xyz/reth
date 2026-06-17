//! Cross-iteration state carry-forward for the payload builder.
//!
//! ## Why this exists
//!
//! The preconf payload builder runs as a long-lived async job. Each call to
//! `build_increment` crosses a [`tokio::task::spawn_blocking`] boundary so
//! the EVM work runs on the blocking thread pool. Across that boundary the
//! revm [`State<DB>`] cannot be carried directly (it borrows the underlying
//! [`StateProvider`]), so we must extract the *carryable bits* —
//! [`CacheState`] (cached account / storage reads) and the running
//! [`TransitionState`] — at the end of each iteration and rebuild a fresh
//! `State<DB>` at the start of the next.
//!
//! Carrying both fields avoids the cost that would otherwise dominate:
//! re-reading every touched account from the underlying database on every
//! iteration.
//!
//! ## What this is not
//!
//! This module does **not** implement per-tx speculative rollback. The
//! preconf design uses a single source state — once a tx is applied to the
//! cache it is part of the in-flight block. A separate concern is the
//! flashblock seal boundary used by op-rbuilder (`transition_state`
//! snapshot/restore around `merge_transitions`), which lives in a different
//! code path and is added only when flashblocks support lands.
//!
//! ## Reference
//!
//! Equivalent pattern in op-rbuilder's `builders/builder_tx.rs:275`:
//! ```ignore
//! State::builder()
//!     .with_database(state)
//!     .with_cached_prestate(db.cache.clone())
//!     .with_bundle_update()
//!     .build()
//! ```
//! (Used there for a one-off simulation State, but the carry-forward
//! mechanics are identical.)
//!
//! [`State<DB>`]: reth_revm::State
//! [`CacheState`]: reth_revm::db::CacheState
//! [`TransitionState`]: reth_revm::db::TransitionState
//! [`StateProvider`]: https://docs.rs/reth-storage-api/latest/reth_storage_api/trait.StateProvider.html

use reth_revm::{
    Database, State,
    db::{CacheState, TransitionState},
};

/// State that survives between `build_increment` calls.
///
/// Holds the bits of a revm `State<DB>` that we want to reuse on the next
/// iteration without re-reading from the underlying database:
///
/// - `cache` — accumulated account / storage reads (and any pending writes)
/// - `transition` — running per-block transitions, carried forward so the
///   sealing path can produce a consistent `BundleState` covering the full
///   set of txs applied across the iteration boundary
///
/// Held by the long-running builder's owning state struct (e.g.,
/// `BuilderState`). Lifecycle: created with [`CarriedState::empty`] when
/// the `PayloadJob` starts, then [`extract`](Self::extract) /
/// [`into_state`](Self::into_state) on every iteration.
#[derive(Debug, Default)]
pub struct CarriedState {
    /// Accumulated account / storage cache.
    pub cache: CacheState,
    /// Running `transition_state`. `None` after a `merge_transitions` call
    /// followed by extract; typically `Some` between iterations.
    pub transition: Option<TransitionState>,
}

impl CarriedState {
    /// Fresh carry-state with empty cache and no transitions. Use this on
    /// `PayloadJob` startup.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Drain the carryable fields out of a revm `State<DB>`.
    ///
    /// After this call the source `State` has its `cache` reset to the
    /// `CacheState` default and its `transition_state` set to `None` — it
    /// should be dropped, not used for further EVM execution.
    pub fn extract<DB>(state: &mut State<DB>) -> Self {
        Self { cache: std::mem::take(&mut state.cache), transition: state.transition_state.take() }
    }

    /// Build a fresh revm `State<DB>` that resumes from this carry-state.
    ///
    /// The returned state is constructed via revm's
    /// [`StateBuilder::with_cached_prestate`] and `with_bundle_update`, so
    /// it accumulates a new `BundleState` (the previous bundle is *not*
    /// carried — bundles are scoped to a single sealed payload).
    ///
    /// Consumes `self` because both fields are moved into the new `State`.
    ///
    /// [`StateBuilder::with_cached_prestate`]: reth_revm::db::StateBuilder::with_cached_prestate
    pub fn into_state<DB: Database>(self, db: DB) -> State<DB> {
        let mut state = State::builder()
            .with_database(db)
            .with_cached_prestate(self.cache)
            .with_bundle_update()
            .build();
        state.transition_state = self.transition;
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_revm::database_interface::EmptyDB;

    /// Helper: pull the cache out of a freshly-built state by routing it
    /// through `extract` / `into_state` once.
    fn freshly_built_state() -> State<EmptyDB> {
        CarriedState::empty().into_state(EmptyDB::default())
    }

    #[test]
    fn empty_is_default() {
        let a = CarriedState::empty();
        let b = CarriedState::default();
        assert_eq!(a.cache, b.cache);
        assert_eq!(a.transition, b.transition);
        assert!(a.transition.is_none());
    }

    #[test]
    fn extract_drains_source_state() {
        let mut state = freshly_built_state();
        // Seed the transition_state with a non-default value so we can
        // observe it being moved out.
        state.transition_state = Some(TransitionState::default());

        let carried = CarriedState::extract(&mut state);

        // Source state now empty.
        assert_eq!(state.cache, CacheState::default());
        assert!(state.transition_state.is_none());
        // Carried received the transition.
        assert!(carried.transition.is_some());
    }

    #[test]
    fn extract_then_into_state_roundtrip_preserves_fields() {
        // Build a state, drain it, build a new state from the drained
        // carry — the new state's cache and transition_state must equal
        // the original.
        let mut state = freshly_built_state();
        state.transition_state = Some(TransitionState::default());
        let original_cache = state.cache.clone();
        let original_transition = state.transition_state.clone();

        let carried = CarriedState::extract(&mut state);
        let new_state = carried.into_state(EmptyDB::default());

        assert_eq!(new_state.cache, original_cache);
        assert_eq!(new_state.transition_state, original_transition);
    }

    #[test]
    fn into_state_uses_bundle_update() {
        // `with_bundle_update` means the state will track bundles. We
        // verify indirectly: bundle_state is initialized to default
        // (empty), and the state can produce a `BundleState` via
        // `take_bundle()` without panicking.
        let mut state = CarriedState::empty().into_state(EmptyDB::default());
        let bundle = state.take_bundle();
        assert!(bundle.is_empty());
    }

    #[test]
    fn empty_into_state_yields_default_cache() {
        let state = CarriedState::empty().into_state(EmptyDB::default());
        assert_eq!(state.cache, CacheState::default());
        assert!(state.transition_state.is_none());
    }

    #[test]
    fn carried_transition_none_after_extract_if_source_had_none() {
        // Default state has transition_state = None — extract must reflect
        // that (no silent fallback to Some(default)).
        let mut state = freshly_built_state();
        assert!(state.transition_state.is_none());

        let carried = CarriedState::extract(&mut state);
        assert!(carried.transition.is_none());
    }
}
