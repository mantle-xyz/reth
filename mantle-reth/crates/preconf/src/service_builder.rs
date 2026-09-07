//! Service-level wiring for the preconf subsystem.
//!
//! [`PreconfServiceBuilder`] is the **application-level owner** of the
//! preconf subsystem's cross-component shared state — a validated
//! [`Arc<PreconfConfig>`], a single [`Arc<PreconfClassifier>`], a single
//! [`Arc<PreconfTxSet>`], and a mandatory [`Arc<PreconfJournal>`] — and
//! exposes typed factories for the per-task handlers that consume them:
//!
//! - [`PreconfServiceBuilder::canon_handler`] — to subscribe canonical- state notifications and
//!   forward the fifo past sealed block nonces.
//! - [`PreconfServiceBuilder::rpc_handler`] — to back the local `eth_sendRawTransactionWithPreconf`
//!   entry on the sequencer node.
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

use std::sync::Arc;

use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::TransactionPool;

use crate::{
    PreconfCanonHandler, PreconfClassifier, PreconfConfig, PreconfJournal, PreconfRpcHandler,
    PreconfTxSet,
    config::PreconfConfigError,
    journal::{JournalError, RestorePool, restore_preconf_state},
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
    /// `cfg.journal_path` was `None` at construction time. The journal is
    /// mandatory; the CLI layer must resolve the datadir-relative default
    /// into `Some(..)` before calling [`PreconfServiceBuilder::from_config`].
    #[error("journal_path must be resolved before building the preconf service")]
    MissingJournalPath,
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
/// The commitment journal is **mandatory**: it is opened eagerly in
/// [`Self::from_config`] and every produced handler gets a real
/// [`Arc<PreconfJournal>`]. There is no persistence-disabled mode.
#[derive(Debug)]
pub struct PreconfServiceBuilder {
    cfg: Arc<PreconfConfig>,
    /// Owns the allowlists and the frozen per-tx verdicts. Built
    /// here so every consumer — validator, pool listener, payload builder, RPC
    /// handler — shares one instance; two classifiers would mean two answers.
    classifier: Arc<PreconfClassifier>,
    fifo: Arc<PreconfTxSet>,
    /// Commitment journal — always present. Opened at construction from
    /// `cfg.journal_path` (which the CLI layer resolves to the
    /// datadir-relative default when unset).
    journal: Arc<PreconfJournal>,
}

impl PreconfServiceBuilder {
    /// Validate `cfg`, build the shared fifo, and open the commitment
    /// journal in one step.
    ///
    /// `cfg.journal_path` must be resolved to `Some(..)` by the caller —
    /// the CLI layer fills the datadir-relative default when the operator
    /// did not pass `--preconf.journal-path`. The journal is mandatory when
    /// preconf is enabled, so a `None` path here is a wiring bug and returns
    /// [`PreconfServiceError::MissingJournalPath`].
    ///
    /// The journal file is opened in append mode; existing contents are
    /// preserved and replayed into the fifo when [`Self::start`] runs.
    pub async fn from_config(cfg: PreconfConfig) -> Result<Self, PreconfServiceError> {
        // `validate` takes ownership and returns the validated config back.
        let cfg = cfg.validate()?;
        let broadcast_cap = cfg.broadcast_cap;
        let journal_path =
            cfg.journal_path.clone().ok_or(PreconfServiceError::MissingJournalPath)?;
        let journal = PreconfJournal::open(&journal_path, cfg.journal_max_size).await?;
        // Built from the validated config, before it is shared: the classifier
        // reads `all_preconfs` and the verdict grace period off it.
        let classifier = Arc::new(PreconfClassifier::from_config(&cfg));
        let cfg = Arc::new(cfg);
        let fifo = Arc::new(PreconfTxSet::new(broadcast_cap));
        Ok(Self { cfg, classifier, fifo, journal: Arc::new(journal) })
    }

    /// Shared config handle. Cheap to clone — internally an `Arc`.
    pub fn cfg(&self) -> &Arc<PreconfConfig> {
        &self.cfg
    }

    /// Shared classifier handle — the single owner of the allowlists.
    /// Cheap to clone — internally an `Arc`.
    pub fn classifier(&self) -> &Arc<PreconfClassifier> {
        &self.classifier
    }

    /// Shared fifo handle. Cheap to clone — internally an `Arc`.
    pub fn fifo(&self) -> &Arc<PreconfTxSet> {
        &self.fifo
    }

    /// Shared journal handle. Always present (see type-level docs).
    pub fn journal(&self) -> &Arc<PreconfJournal> {
        &self.journal
    }

    /// Startup hook: replay the journal into the fifo and register the
    /// fifo → pool eviction callback.
    ///
    /// Call once from the pool builder after the pool is up, **before**
    /// the pool listener + canon handler are spawned so the fifo state
    /// is populated when they start consuming events.
    pub async fn start<P, C>(&self, pool: &P, chain: &C) -> Result<(), PreconfStartError>
    where
        P: RestorePool + Clone + 'static,
        C: crate::journal::CommitmentChainView,
    {
        restore_preconf_state(&self.journal, pool, chain, &self.fifo, &self.classifier).await;

        // Every non-on-chain terminal transition (mark_timeout /
        // mark_canceled / mark_failed) synchronously removes the tx
        // from the pool, closing the window where a client-observed
        // failure could be contradicted by a later on-chain landing
        // via the pool iterator.
        let pool_for_evict = pool.clone();
        self.fifo.set_pool_eviction_callback(Arc::new(move |hash| {
            pool_for_evict.remove_transactions(vec![hash]);
        }));

        // Every fifo removal path also drops the frozen verdict: on most of
        // them, once the commitment record is gone there is nothing left for
        // the verdict to protect. `forward` is the exception — it removes on
        // "the sender's nonce moved past this entry", which neither identifies
        // the tx that advanced the nonce nor is irrevocable, so the commitment
        // may still be live.
        // Registered as a callback rather than a direct reference so the fifo
        // stays ignorant of the classifier (the sweep in the other
        // direction is driven by the canonical-state handler, which holds both).
        let classifier_for_evict = self.classifier.clone();
        self.fifo.set_verdict_eviction_callback(Arc::new(move |hash| {
            classifier_for_evict.release_unless_committed(&hash);
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
    ///
    /// `Pr` must also be a `BlockNumReader`: the handler publishes
    /// `last_block_number()` (the **persisted** tip) to the classifier each
    /// notification, which is the ruler the retention period is measured
    /// against — see `classifier::SEAL_DEPTH`.
    pub fn canon_handler<Pr, P, N>(&self, provider: Pr, pool: P) -> PreconfCanonHandler<Pr, P, N>
    where
        Pr: CanonStateSubscriptions<Primitives = N> + reth_storage_api::BlockNumReader + 'static,
        P: TransactionPool + 'static,
        N: NodePrimitives,
        N::SignedTx: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    {
        PreconfCanonHandler::new(provider, pool, self.fifo.clone(), self.classifier.clone())
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
            self.classifier.clone(),
            self.journal.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a service builder with a fresh journal under a temp dir.
    /// Returns the `TempDir` so the caller keeps it alive for the test.
    async fn svc_with_temp_journal() -> (tempfile::TempDir, PreconfServiceBuilder) {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = PreconfConfig {
            journal_path: Some(dir.path().join("preconf.jsonl")),
            ..PreconfConfig::default()
        };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();
        (dir, svc)
    }

    #[tokio::test]
    async fn from_config_arc_shares_handles() {
        let (_dir, svc) = svc_with_temp_journal().await;
        let svc = Arc::new(svc);
        let svc2 = Arc::clone(&svc);
        // Arc-shared instance: both refs see the same fifo / cfg / classifier.
        assert!(Arc::ptr_eq(svc.cfg(), svc2.cfg()));
        assert!(Arc::ptr_eq(svc.fifo(), svc2.fifo()));
        assert!(Arc::ptr_eq(svc.classifier(), svc2.classifier()));
    }

    #[tokio::test]
    async fn fifo_broadcast_capacity_matches_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = PreconfConfig {
            broadcast_cap: 7,
            journal_path: Some(dir.path().join("preconf.jsonl")),
            ..PreconfConfig::default()
        };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();
        // No direct getter for the cap; subscribe to assert non-panic.
        let _rx = svc.fifo().subscribe();
    }

    #[tokio::test]
    async fn from_config_without_journal_path_errors() {
        // The journal is mandatory — an unresolved (`None`) path is a wiring
        // bug (the CLI layer must fill the datadir default first).
        let cfg = PreconfConfig::default();
        assert!(cfg.journal_path.is_none(), "default config leaves path unresolved");
        let err = PreconfServiceBuilder::from_config(cfg).await.unwrap_err();
        assert!(matches!(err, PreconfServiceError::MissingJournalPath), "got {err:?}");
    }

    #[tokio::test]
    async fn from_config_with_journal_path_opens_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let cfg = PreconfConfig { journal_path: Some(path.clone()), ..PreconfConfig::default() };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();
        assert_eq!(svc.journal().path(), path);
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

    // ── start() lifecycle ────────────────────────────────────────

    /// Stub pool used to exercise `PreconfServiceBuilder::start` without
    /// standing up a real reth pool. `start`'s only interaction with the
    /// pool is via [`RestorePool::add_envelope`], which
    /// [`restore_preconf_state`] calls once per journal entry. Since
    /// tests here use empty journals, the stub can panic if reached —
    /// the empty-journal branch never invokes it.
    #[derive(Clone)]
    struct UnreachablePool;

    /// Same reasoning for the chain view: with an empty journal there is no entry
    /// whose nonce could have been consumed, so nothing asks.
    struct UnreachableChain;

    impl crate::journal::CommitmentChainView for UnreachableChain {
        fn commitment_on_chain(&self, _hash: &alloy_primitives::TxHash) -> crate::journal::OnChain {
            unreachable!("empty journal → restore_preconf_state must not consult the chain")
        }
    }

    #[async_trait::async_trait]
    impl RestorePool for UnreachablePool {
        async fn contains(&self, _hash: &alloy_primitives::TxHash) -> bool {
            unreachable!("empty journal → restore_preconf_state must not call the pool")
        }
        fn recover_slot(
            &self,
            _tx_rlp: &alloy_primitives::Bytes,
        ) -> Option<(alloy_primitives::Address, u64)> {
            unreachable!("empty journal → restore_preconf_state must not call the pool")
        }
        async fn add_envelope(
            &self,
            _tx_rlp: &alloy_primitives::Bytes,
        ) -> Result<crate::journal::RestoredEnvelope, crate::journal::RestoreSkip> {
            unreachable!("empty journal → restore_preconf_state must not call the pool")
        }
        fn remove_transactions(&self, _hashes: Vec<alloy_primitives::TxHash>) {
            // No mark_* fires in these tests; the setup asserts only
            // on fifo state.
        }
    }

    #[tokio::test]
    async fn start_with_empty_journal_is_noop_on_fifo() {
        let (_dir, svc) = svc_with_temp_journal().await;
        svc.start(&UnreachablePool, &UnreachableChain).await.unwrap();
        // Empty journal ⇒ nothing was replayed into the fifo.
        assert_eq!(svc.fifo().snapshot().await.len(), 0);
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let (_dir, svc) = svc_with_temp_journal().await;
        // Calling twice must not panic; both calls succeed.
        svc.start(&UnreachablePool, &UnreachableChain).await.unwrap();
        svc.start(&UnreachablePool, &UnreachableChain).await.unwrap();
    }

    /// `start()` reads the journal from disk and pushes each
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
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(alloy_primitives::Address, u64)> {
                // Same `(from, nonce)` `add_envelope` fabricates below.
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((alloy_primitives::Address::from([seed; 20]), u64::from(seed)))
            }
            async fn add_envelope(
                &self,
                tx_rlp: &Bytes,
            ) -> Result<RestoredEnvelope, crate::journal::RestoreSkip> {
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

        // Fresh commit timestamps, so the entries are ordinary in-flight
        // commitments rather than anything a rotation would drop.
        let now_ms =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
                as u64;
        let journal = svc.journal();
        journal
            .append_promised(&JournalEntry {
                hash: TxHash::from([0xA1; 32]),
                tx_rlp: Bytes::from(vec![0xA1; 4]),
                block_height: 10,
                committed_at_ms: now_ms,
            })
            .await
            .unwrap();
        journal
            .append_promised(&JournalEntry {
                hash: TxHash::from([0xA2; 32]),
                tx_rlp: Bytes::from(vec![0xA2; 4]),
                block_height: 11,
                committed_at_ms: now_ms,
            })
            .await
            .unwrap();

        let pool = RecordingPool { add_calls: Arc::new(std::sync::Mutex::new(Vec::new())) };
        // This pool admits everything, so the chain is never consulted either.
        svc.start(&pool, &UnreachableChain).await.unwrap();

        // The adapter was invoked for each journal entry.
        assert_eq!(pool.add_calls.lock().unwrap().len(), 2, "one add per entry");
        // Fifo contains both restored envelopes.
        let snapshot = svc.fifo().snapshot().await;
        assert_eq!(snapshot.len(), 2);
    }

    /// A restore that cannot re-admit an entry gives the commitment up, and the
    /// journal line goes with it — `start`'s prune runs *after* the replay pass,
    /// keyed on the records that pass established, so there is no window in which
    /// the file and the classifier disagree.
    #[tokio::test]
    async fn start_prunes_an_entry_whose_commitment_cannot_be_honoured() {
        use crate::journal::{JournalEntry, RestoreSkip, RestoredEnvelope};
        use alloy_primitives::{Address, Bytes, TxHash};

        #[derive(Clone)]
        struct RejectingPool;

        #[async_trait::async_trait]
        impl RestorePool for RejectingPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                Some((Address::from([0xB1; 20]), u64::from(tx_rlp[0])))
            }
            async fn add_envelope(&self, _tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
                Err(RestoreSkip::Rejected("stub refuses to admit".into()))
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
        }

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let cfg = PreconfConfig { journal_path: Some(path.clone()), ..PreconfConfig::default() };
        let svc = PreconfServiceBuilder::from_config(cfg).await.unwrap();

        svc.journal()
            .append_promised(&JournalEntry {
                hash: TxHash::from([0xB1; 32]),
                tx_rlp: Bytes::from(vec![0xB1; 4]),
                block_height: 5,
                committed_at_ms: 1,
            })
            .await
            .unwrap();

        // `RestoreSkip::Rejected` never consults the chain, so `UnreachableChain`
        // is still the right stub here.
        svc.start(&RejectingPool, &UnreachableChain).await.unwrap();

        assert!(
            svc.fifo().snapshot().await.is_empty(),
            "a commitment the pool refuses reaches no fifo entry",
        );
        let (remaining, _) = svc.journal().load().await.unwrap();
        assert!(remaining.is_empty(), "and its journal line is pruned along with its record");
    }
}
