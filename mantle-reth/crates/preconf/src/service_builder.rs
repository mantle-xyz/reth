//! Service-level wiring for the preconf subsystem.
//!
//! [`PreconfServiceBuilder`] is the **application-level owner** of the
//! preconf subsystem's cross-component shared state — a validated
//! [`Arc<PreconfConfig>`], a single [`Arc<PreconfTxSet>`], and an
//! optional [`Arc<PreconfJournal>`] — and exposes typed factories for
//! the per-task handlers that consume them:
//!
//! - [`PreconfServiceBuilder::canon_handler`] — to subscribe canonical-
//!   state notifications and forward the fifo past sealed block nonces.
//! - [`PreconfServiceBuilder::rpc_handler`] — to back the local
//!   `eth_sendRawTransactionWithPreconf` entry on the sequencer node.
//!
//! Distinct from [`crate::payload_service_builder::MantlePreconfServiceBuilder`],
//! which is a **reth node-builder trait impl** (`PayloadServiceBuilder`)
//! that reth's components plumbing calls into. The two collaborate: the
//! app-level builder here is constructed first, then its handles feed
//! the reth-facing builder + the cli pool wiring.
//!
//! The builder does **not** spawn anything itself. Reth nodes hold the
//! task executor at the integration layer (the node builder's
//! `extend_rpc_modules` closure has both `provider` and
//! `task_executor` in scope), so the spawn call site lives there. This
//! keeps the service builder free of reth `TaskExecutor` and lifecycle
//! plumbing.
//!
//! Pool integration lives on [`MantlePoolBuilder::with_preconf`](crate)
//! and is driven directly from the cli crate's component-builder code —
//! it consumes the same `cfg` and `fifo` handles via
//! [`PreconfServiceBuilder::cfg`] / [`PreconfServiceBuilder::fifo`].

use std::{path::Path, sync::Arc};

use mantle_reth_rpc_ext::PreconfTxEvent;
use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::TransactionPool;
use tokio::sync::{OnceCell, broadcast};

use crate::{
    PreconfCanonHandler, PreconfConfig, PreconfJournal, PreconfRpcHandler, PreconfTxSet,
    config::PreconfConfigError,
    journal::{EventPublisher, JournalError, RestorePool, RestoredSet, restore_preconf_state},
};
use thiserror::Error;

/// Errors surfaced by [`PreconfServiceBuilder::from_config`] — either
/// the config itself is invalid, or opening the journal file failed.
#[derive(Debug, Error)]
pub enum PreconfServiceError {
    /// Configuration validation failed (see [`PreconfConfig::validate`]).
    #[error(transparent)]
    Config(#[from] PreconfConfigError),
    /// Journal file could not be opened or created.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// Errors surfaced by [`PreconfServiceBuilder::start`] — the restore
/// phase only ever propagates journal-load failures (which are already
/// converted to warn-and-empty inside `restore_preconf_state`), so this
/// enum is currently a placeholder for future variants (e.g. explicit
/// timeout / cancellation).
#[derive(Debug, Error)]
pub enum PreconfStartError {}

/// Owns and hands out the shared preconf handles for a single node
/// instance. Construct once at startup; wrap in `Arc` at the CLI layer
/// to distribute to consumers (pool builder, canon handler, RPC).
///
/// Startup ordering — the fields separate into two phases:
///
/// - **Eagerly constructed** in [`Self::new`] / [`Self::from_config`]:
///   `cfg`, `fifo`, `journal`, and the `event_broadcast` channel. These
///   are usable as soon as the builder exists.
/// - **Set once by [`Self::start`]** (called from the pool builder
///   after the pool is up but before the pool listener + canon handler
///   are spawned): `restored` (the [`RestoredSet`] produced by
///   [`restore_preconf_state`]) and `event_publisher` (the broadcast
///   wrapper that filters restored hashes). Both use [`OnceCell`] so
///   `start` is idempotent — calling it twice is a no-op after the
///   first successful call.
#[derive(Debug)]
pub struct PreconfServiceBuilder {
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
    /// Optional commitment journal. `None` ⇒ restart safety is off —
    /// promised but unsealed commitments are lost on crash. Enable
    /// via [`PreconfServiceBuilder::with_journal`] before installing
    /// the service builder in the node.
    journal: Option<Arc<PreconfJournal>>,
    /// Broadcast channel for `newPreconfTransaction` subscribers.
    /// Constructed eagerly so the RPC subscription API can take a
    /// receiver before [`Self::start`] is called; the publish path
    /// wraps this same sender in the [`EventPublisher`] created during
    /// `start` (which layers the `RestoredSet` suppression on top).
    event_broadcast: broadcast::Sender<PreconfTxEvent>,
    /// Set once by [`Self::start`]. `Arc<RestoredSet>` is the set of
    /// hashes replayed from the journal at startup — the canon handler
    /// consults `RestoredSet::take` on every sealed hash to drop it
    /// from the suppression set.
    restored: OnceCell<Arc<RestoredSet>>,
    /// Set once by [`Self::start`]. Wraps `event_broadcast` with the
    /// `RestoredSet` filter so duplicated events for restored txs are
    /// suppressed. Downstream (dispatch loop, RPC handler) publish
    /// through this handle rather than the raw sender.
    event_publisher: OnceCell<Arc<EventPublisher>>,
}

impl PreconfServiceBuilder {
    /// Convenience: validate the config and, if `cfg.journal_path` is
    /// `Some`, open the journal file in one step.
    ///
    /// Equivalent to chaining [`Self::new`] + [`Self::with_journal`]
    /// manually, but driven entirely off the config so production
    /// callers do not need to know whether persistence is on. The
    /// `journal_path` field is the single source of truth — set it to
    /// enable persistence, leave it `None` to disable.
    pub async fn from_config(cfg: PreconfConfig) -> Result<Self, PreconfServiceError> {
        let path = cfg.journal_path.clone();
        let builder = Self::new(cfg)?;
        match path {
            Some(p) => Ok(builder.with_journal(p).await?),
            None => Ok(builder),
        }
    }

    /// Validate `cfg` and construct the shared fifo with the
    /// configured broadcast capacity. Persistence is disabled —
    /// chain [`Self::with_journal`] (or use [`Self::from_config`]) to
    /// enable it.
    ///
    /// Returns [`PreconfConfigError`] if any field is out of range
    /// (zero timeouts, zero gas limits, ...). The config is not stored
    /// until validation succeeds, so a failed call leaves no
    /// partially-initialised state.
    pub fn new(cfg: PreconfConfig) -> Result<Self, PreconfConfigError> {
        // `validate` takes ownership and returns the validated config back
        // on success — destructure the broadcast cap before wrapping in Arc.
        let cfg = cfg.validate()?;
        let broadcast_cap = cfg.broadcast_cap;
        let cfg = Arc::new(cfg);
        let fifo = Arc::new(PreconfTxSet::new(broadcast_cap));
        let (event_broadcast, _) = broadcast::channel(broadcast_cap);
        Ok(Self {
            cfg,
            fifo,
            journal: None,
            event_broadcast,
            restored: OnceCell::new(),
            event_publisher: OnceCell::new(),
        })
    }

    /// Open or create the on-disk commitment journal at `path` and
    /// install it on this builder. Subsequent calls to
    /// [`Self::rpc_handler`] / [`Self::canon_handler`] will thread the
    /// journal handle into the produced handlers.
    ///
    /// The journal file is opened in append mode; existing contents
    /// are preserved. Recovery from a previous run
    /// ([`crate::restore_preconf_state`] + [`crate::RestoredSet`] +
    /// [`crate::EventPublisher`]) is not wired into this builder yet;
    /// the pieces exist as standalone helpers but the assembly hook
    /// belongs with the payload-service startup path — tracked as
    /// R5/D1 in the review plan. Until that lands, restart replay
    /// does not happen and commitments made prior to restart are not
    /// re-emitted.
    pub async fn with_journal(mut self, path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let journal = PreconfJournal::open(path).await?;
        self.journal = Some(Arc::new(journal));
        Ok(self)
    }

    /// Shared config handle. Cheap to clone — internally an `Arc`.
    pub fn cfg(&self) -> &Arc<PreconfConfig> {
        &self.cfg
    }

    /// Shared fifo handle. Cheap to clone — internally an `Arc`.
    pub fn fifo(&self) -> &Arc<PreconfTxSet> {
        &self.fifo
    }

    /// Journal handle if persistence is enabled. `None` when the
    /// builder was constructed without [`Self::with_journal`].
    pub fn journal(&self) -> Option<&Arc<PreconfJournal>> {
        self.journal.as_ref()
    }

    /// Fresh subscriber for the `newPreconfTransaction` broadcast
    /// channel. Called by the RPC subscription layer at trait
    /// registration time to produce a `broadcast::Receiver` per
    /// subscriber. The channel is live from `new` onward — subscribing
    /// before [`Self::start`] is fine (nothing is published until the
    /// builder path fires), and past events are not replayed (broadcast
    /// semantics).
    pub fn subscribe_events(&self) -> broadcast::Receiver<PreconfTxEvent> {
        self.event_broadcast.subscribe()
    }

    /// Publish handle threaded into `LoopState` so the dispatch loop
    /// can emit events after each fifo status transition. `None` until
    /// [`Self::start`] has been called; the pool builder's start-hook
    /// enforces this ordering.
    pub fn event_publisher(&self) -> Option<Arc<EventPublisher>> {
        self.event_publisher.get().cloned()
    }

    /// The [`RestoredSet`] built during [`Self::start`]. `None` until
    /// start has been called; the canon handler factory pulls a clone
    /// out to consult `take` on each sealed hash.
    pub fn restored_set(&self) -> Option<Arc<RestoredSet>> {
        self.restored.get().cloned()
    }

    /// Startup hook: journal → restore → construct `RestoredSet` +
    /// `EventPublisher` — idempotent. Call once from the pool builder
    /// after the pool is up, **before** the pool listener + canon
    /// handler are spawned (so the RestoredSet is populated when they
    /// start consuming events).
    ///
    /// If no journal is configured, the restored set is empty and the
    /// publisher forwards every event without suppression.
    ///
    /// Subsequent calls are a no-op — the [`OnceCell`] fields short-
    /// circuit. This lets `MantlePoolBuilder::build_pool` call it
    /// unconditionally without worrying about double-invocation across
    /// runtime reloads.
    pub async fn start<P>(&self, pool: &P) -> Result<(), PreconfStartError>
    where
        P: RestorePool + Clone + 'static,
    {
        if self.event_publisher.get().is_some() {
            return Ok(());
        }
        let restored = if let Some(journal) = self.journal.as_ref() {
            restore_preconf_state(journal, pool, &self.fifo).await
        } else {
            RestoredSet::empty()
        };
        // OnceCell::set errors when already set — a concurrent start()
        // won the race; either way both callers see the same value.
        let _ = self.restored.set(restored.clone());
        let publisher =
            Arc::new(EventPublisher::from_sender(self.event_broadcast.clone(), restored));
        let _ = self.event_publisher.set(publisher);

        // R3/SLA-1: register the pool-eviction callback on the fifo.
        // Every non-on-chain terminal transition (mark_timeout /
        // mark_canceled / mark_failed) now synchronously removes the
        // tx from the pool, closing the window where a
        // client-observed failure could be contradicted by a later
        // on-chain landing via the pool iterator.
        let pool_for_evict = pool.clone();
        self.fifo.set_pool_eviction_callback(Arc::new(move |hash| {
            pool_for_evict.remove_transactions(vec![hash]);
        }));

        Ok(())
    }

    /// Construct a canonical-state handler bound to `provider` + `pool`.
    /// The caller is responsible for spawning the returned handler's
    /// [`run`](PreconfCanonHandler::run) future on its task executor.
    ///
    /// The generic `N` matches `Pr::Primitives` — for OP-stack nodes
    /// this is `OpPrimitives`; the bound `N::SignedTx: Transaction +
    /// TxHashRef` is satisfied automatically. `P` is the transaction
    /// pool; the handler uses it to `remove_transactions` on hashes
    /// evicted by `PreconfTxSet::clean_reclaimable`, so a Timeout / Canceled preconf
    /// tx cannot land on chain after the client already saw `Timeout`.
    pub fn canon_handler<Pr, P, N>(
        &self,
        provider: Pr,
        pool: P,
    ) -> PreconfCanonHandler<Pr, P, N>
    where
        Pr: CanonStateSubscriptions<Primitives = N> + 'static,
        P: TransactionPool + 'static,
        N: NodePrimitives,
        N::SignedTx: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    {
        // If `start` has run and produced a RestoredSet, use it; otherwise
        // fall back to an empty set so `take` is a no-op. This keeps the
        // factory callable both before and after start (e.g. tests that
        // wire a canon handler without going through the pool builder).
        let restored = self.restored.get().cloned().unwrap_or_else(RestoredSet::empty);
        PreconfCanonHandler::new(provider, pool, self.fifo.clone(), self.journal.clone(), restored)
    }

    /// Construct the local-sequencer RPC handler. Returned by value so
    /// the caller can decide whether to wrap in `Arc` (the path through
    /// `MantleRpcExt::new` expects `Arc<dyn DynPreconfHandler>`).
    pub fn rpc_handler<P, Pr>(&self, pool: P, provider: Pr) -> PreconfRpcHandler<P, Pr>
    where
        P: TransactionPool + 'static,
        Pr: StateProviderFactory + 'static,
    {
        PreconfRpcHandler::new(
            pool,
            provider,
            self.fifo.clone(),
            self.cfg.clone(),
            self.journal.clone(),
            self.event_publisher(),
        )
    }

    /// Construct the `eth_subscribe("newPreconfTransaction")` handler.
    /// Callers register it into the reth RPC module via
    /// [`jsonrpsee::server::RpcModule::merge`] using the auto-derived
    /// `into_rpc` from the `PreconfSubscribeApi` trait.
    pub fn subscription_handler(&self) -> crate::PreconfSubscriptionHandler {
        crate::PreconfSubscriptionHandler::new(self.event_broadcast.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_invalid_config() {
        let cfg = PreconfConfig { broadcast_cap: 0, ..PreconfConfig::default() };
        assert!(PreconfServiceBuilder::new(cfg).is_err());
    }

    #[test]
    fn new_accepts_default_and_arc_shares_handles() {
        let svc = Arc::new(
            PreconfServiceBuilder::new(PreconfConfig::default()).expect("default config validates"),
        );
        let svc2 = Arc::clone(&svc);
        // Arc-shared instance: both refs see the same fifo / cfg.
        assert!(Arc::ptr_eq(svc.cfg(), svc2.cfg()));
        assert!(Arc::ptr_eq(svc.fifo(), svc2.fifo()));
    }

    #[test]
    fn fifo_broadcast_capacity_matches_config() {
        let cfg = PreconfConfig { broadcast_cap: 7, ..PreconfConfig::default() };
        let svc = PreconfServiceBuilder::new(cfg).unwrap();
        // No direct getter for the cap; subscribe to assert non-panic.
        let _rx = svc.fifo().subscribe();
    }

    #[tokio::test]
    async fn from_config_without_journal_path_disables_persistence() {
        let cfg = PreconfConfig::default();
        assert!(cfg.journal_path.is_none(), "default config should disable journal");
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();
        assert!(svc.journal().is_none(), "journal must be off when path is None");
    }

    #[tokio::test]
    async fn from_config_with_journal_path_opens_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let cfg = PreconfConfig { journal_path: Some(path.clone()), ..PreconfConfig::default() };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();
        let journal = svc.journal().expect("journal must be opened when path is set");
        assert_eq!(journal.path(), path);
        // File must exist on disk.
        assert!(path.exists());
    }

    #[tokio::test]
    async fn from_config_surfaces_invalid_config_as_typed_error() {
        let cfg = PreconfConfig { broadcast_cap: 0, ..PreconfConfig::default() };
        let err = PreconfServiceBuilder::from_config(cfg).await.unwrap_err();
        assert!(matches!(err, PreconfServiceError::Config(_)));
    }

    #[tokio::test]
    async fn from_config_surfaces_journal_open_failure_as_typed_error() {
        // Companion to `..._invalid_config_as_typed_error`: when the config is
        // valid but opening the journal fails (path under a non-existent
        // parent), the error must surface as `Journal(_)`, not panic or wrong
        // variant. Without this, the Journal branch of `?`-conversion has no
        // regression test.
        let cfg = PreconfConfig {
            // Parent dir `/nonexistent-mantle-preconf-test` should not be
            // creatable as a child of `/` for a non-root caller.
            journal_path: Some(std::path::PathBuf::from(
                "/nonexistent-mantle-preconf-test/preconf.jsonl",
            )),
            ..PreconfConfig::default()
        };
        let err = PreconfServiceBuilder::from_config(cfg).await.unwrap_err();
        assert!(
            matches!(err, PreconfServiceError::Journal(_)),
            "expected Journal variant, got {err:?}"
        );
    }

    #[tokio::test]
    async fn new_then_with_journal_two_step_chain() {
        // The two-step `new()` + `with_journal()` chain is documented as a
        // first-class API alternative to `from_config`. Without this test the
        // chain can silently break (e.g. if journal handle wiring through
        // `with_journal` ever regressed).
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let svc = PreconfServiceBuilder::new(PreconfConfig::default())
            .expect("default validates")
            .with_journal(&path)
            .await
            .expect("journal opens at fresh path");
        assert_eq!(svc.journal().expect("journal set").path(), path);
    }

    // ── start() lifecycle ────────────────────────────────────────

    /// Stub pool used to exercise `PreconfServiceBuilder::start` without
    /// standing up a real reth pool. `start`'s only interaction with the
    /// pool is via [`RestorePool::add_envelope`], which
    /// [`restore_preconf_state`] calls once per journal entry. Since
    /// tests here use empty journals, the stub can panic if reached —
    /// the empty-journal branch never invokes it.
    #[derive(Clone)]
    struct UnreachablePool;

    #[async_trait::async_trait]
    impl RestorePool for UnreachablePool {
        async fn contains(&self, _hash: &alloy_primitives::TxHash) -> bool {
            unreachable!("empty journal → restore_preconf_state must not call the pool")
        }
        async fn add_envelope(
            &self,
            _tx_rlp: &alloy_primitives::Bytes,
        ) -> Result<crate::journal::RestoredEnvelope, String> {
            unreachable!("empty journal → restore_preconf_state must not call the pool")
        }
        fn remove_transactions(&self, _hashes: Vec<alloy_primitives::TxHash>) {
            // No mark_* fires in these tests; the setup asserts on
            // publisher + restored_set only.
        }
    }

    #[tokio::test]
    async fn start_without_journal_creates_empty_restored_and_publisher() {
        let svc = PreconfServiceBuilder::new(PreconfConfig::default()).unwrap();
        assert!(svc.event_publisher().is_none(), "publisher unset before start");
        assert!(svc.restored_set().is_none(), "restored set unset before start");

        svc.start(&UnreachablePool).await.unwrap();

        // Both fields now set.
        let publisher = svc.event_publisher().expect("publisher set");
        let restored = svc.restored_set().expect("restored set set");
        assert_eq!(restored.len(), 0, "no journal ⇒ empty restored set");
        // Publisher forwards events (no suppression) — subscribe + publish sanity.
        let mut rx = svc.subscribe_events();
        publisher.publish(PreconfTxEvent {
            tx_hash: alloy_primitives::B256::ZERO,
            status: mantle_reth_rpc_ext::PreconfStatus::Success,
            reason: String::new(),
            block_height: 0,
            receipt: mantle_reth_rpc_ext::PreconfTxReceipt { logs: None },
        });
        assert!(rx.try_recv().is_ok(), "subscriber received forwarded event");
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let svc = PreconfServiceBuilder::new(PreconfConfig::default()).unwrap();
        svc.start(&UnreachablePool).await.unwrap();
        let publisher_1 = svc.event_publisher().unwrap();
        // Second call must be a no-op; publisher pointer stays.
        svc.start(&UnreachablePool).await.unwrap();
        let publisher_2 = svc.event_publisher().unwrap();
        assert!(Arc::ptr_eq(&publisher_1, &publisher_2));
    }

    /// R6/T5 — `start()` reads the journal from disk and pushes each
    /// promised entry into the fifo via the adapter. Covers the
    /// journal-enabled end-to-end wiring that
    /// `start_without_journal_...` cannot exercise.
    #[tokio::test]
    async fn start_with_populated_journal_restores_entries_to_fifo() {
        use crate::journal::{JournalEntry, RestoredEnvelope};
        use alloy_consensus::{Signed, TxLegacy};
        use alloy_primitives::{B256, Bytes, Signature, TxHash};

        // Recording pool — every add_envelope call fabricates a
        // synthetic envelope so restore_preconf_state has something to
        // push into the fifo, and asserts what got called. `Clone`
        // required because `start()` clones the pool into the
        // fifo-layer eviction callback (Arc'd shared state keeps the
        // clones observing the same call log).
        #[derive(Clone)]
        struct RecordingPool {
            add_calls: Arc<std::sync::Mutex<Vec<Bytes>>>,
        }
        #[async_trait::async_trait]
        impl RestorePool for RecordingPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            async fn add_envelope(
                &self,
                tx_rlp: &Bytes,
            ) -> Result<RestoredEnvelope, String> {
                self.add_calls.lock().unwrap().push(tx_rlp.clone());
                // Fabricate a deterministic legacy envelope keyed off
                // the first byte of the RLP so every restored entry
                // has a distinct hash.
                let seed = tx_rlp.first().copied().unwrap_or(0);
                let inner = TxLegacy { nonce: seed as u64, ..Default::default() };
                let signed = Signed::new_unchecked(
                    inner,
                    Signature::test_signature(),
                    B256::from([seed; 32]),
                );
                Ok(RestoredEnvelope {
                    envelope: alloy_consensus::TxEnvelope::Legacy(signed),
                    from: alloy_primitives::Address::from([seed; 20]),
                })
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {
                // No mark_* fires in this restore-only test; keep
                // no-op to satisfy the trait.
            }
        }

        // Set up a service builder with a journal already containing
        // two committed entries.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let cfg = PreconfConfig { journal_path: Some(path.clone()), ..PreconfConfig::default() };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();

        let journal = svc.journal().expect("journal on");
        journal
            .append_promised(&JournalEntry {
                hash: TxHash::from([0xA1; 32]),
                tx_rlp: Bytes::from(vec![0xA1; 4]),
                block_height: 10,
                committed_at_ms: 1,
            })
            .await
            .unwrap();
        journal
            .append_promised(&JournalEntry {
                hash: TxHash::from([0xA2; 32]),
                tx_rlp: Bytes::from(vec![0xA2; 4]),
                block_height: 11,
                committed_at_ms: 2,
            })
            .await
            .unwrap();

        let pool = RecordingPool { add_calls: Arc::new(std::sync::Mutex::new(Vec::new())) };
        svc.start(&pool).await.unwrap();

        // The adapter was invoked for each journal entry.
        assert_eq!(pool.add_calls.lock().unwrap().len(), 2, "one add per entry");
        // RestoredSet has both hashes (drives event suppression until
        // the canon handler drains via `take`).
        let restored = svc.restored_set().expect("restored set populated");
        assert_eq!(restored.len(), 2);
        // Fifo contains both restored envelopes.
        let snapshot = svc.fifo().snapshot().await;
        assert_eq!(snapshot.len(), 2);
    }
}
