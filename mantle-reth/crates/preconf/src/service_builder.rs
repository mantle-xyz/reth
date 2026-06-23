//! Service-level wiring for the preconf subsystem.
//!
//! [`PreconfServiceBuilder`] owns the cross-component shared state — a
//! validated [`Arc<PreconfConfig>`] and a single [`Arc<PreconfTxSet>`] —
//! and exposes typed factories for the per-task handlers that consume
//! them:
//!
//! - [`PreconfServiceBuilder::canon_handler`] — to subscribe canonical-
//!   state notifications and forward the fifo past sealed block nonces.
//! - [`PreconfServiceBuilder::rpc_handler`] — to back the local
//!   `eth_sendRawTransactionWithPreconf` entry on the sequencer node.
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

use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::TransactionPool;

use crate::{
    PreconfCanonHandler, PreconfConfig, PreconfJournal, PreconfRpcHandler, PreconfTxSet,
    builder::builder::{PreconfApplierFactory, default_applier_factory},
    config::PreconfConfigError,
    journal::JournalError,
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

/// Owns and hands out the shared preconf handles for a single node
/// instance. Construct once at startup; clone the `Arc` returned by the
/// accessors freely.
#[derive(Clone)]
pub struct PreconfServiceBuilder {
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
    /// Optional commitment journal. `None` ⇒ restart safety is off —
    /// promised but unsealed commitments are lost on crash. Enable
    /// via [`PreconfServiceBuilder::with_journal`] before installing
    /// the service builder in the node.
    journal: Option<Arc<PreconfJournal>>,
    /// Applier factory the service builder hands to every produced
    /// [`PreconfPayloadJobGenerator`]. Defaults to
    /// [`default_applier_factory`] (which yields a fresh
    /// [`PromiseApplier`](crate::builder::PromiseApplier) per slot).
    /// Operators replace this via
    /// [`PreconfServiceBuilder::with_applier_factory`] when an
    /// EVM-backed applier is available.
    applier_factory: PreconfApplierFactory,
}

// Manual Debug — `applier_factory` wraps a trait object without
// `Debug`. The cfg / fifo / journal fields keep the dump informative.
impl std::fmt::Debug for PreconfServiceBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfServiceBuilder")
            .field("cfg", &self.cfg)
            .field("fifo", &self.fifo)
            .field("journal", &self.journal)
            .finish_non_exhaustive()
    }
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
        Ok(Self { cfg, fifo, journal: None, applier_factory: default_applier_factory() })
    }

    /// Replace the applier factory. Any
    /// [`PreconfPayloadJobGenerator`](crate::builder::PreconfPayloadJobGenerator)
    /// constructed via [`Self::generator`] after this call will use
    /// the new factory; existing generators keep the one they
    /// captured.
    pub fn with_applier_factory(mut self, factory: PreconfApplierFactory) -> Self {
        self.applier_factory = factory;
        self
    }

    /// Borrow the configured applier factory. Used by callers that
    /// build a payload-job generator themselves (e.g. the cli crate
    /// when it wraps the OP basic payload job generator) and need to
    /// thread the factory through.
    pub fn applier_factory(&self) -> &PreconfApplierFactory {
        &self.applier_factory
    }

    /// Open or create the on-disk commitment journal at `path` and
    /// install it on this builder. Subsequent calls to
    /// [`Self::rpc_handler`] / [`Self::canon_handler`] will thread the
    /// journal handle into the produced handlers.
    ///
    /// The journal file is opened in append mode; existing contents
    /// are preserved. Callers expecting to recover state from a
    /// previous run must invoke [`PreconfJournal::load`] (or
    /// [`crate::restore_preconf_state`]) themselves on the handle
    /// returned by [`Self::journal`] before the listener starts
    /// emitting events.
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

    /// Construct a canonical-state handler bound to `provider`. The
    /// caller is responsible for spawning the returned handler's
    /// [`run`](PreconfCanonHandler::run) future on its task executor.
    ///
    /// The generic `N` matches `Pr::Primitives` — for OP-stack nodes
    /// this is `OpPrimitives`; the bound `N::SignedTx: Transaction +
    /// TxHashRef` is satisfied automatically.
    pub fn canon_handler<Pr, N>(&self, provider: Pr) -> PreconfCanonHandler<Pr, N>
    where
        Pr: CanonStateSubscriptions<Primitives = N> + 'static,
        N: NodePrimitives,
        N::SignedTx: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    {
        PreconfCanonHandler::new(provider, self.fifo.clone(), self.journal.clone())
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
        )
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
    fn new_accepts_default_and_shares_handles_via_clone() {
        let svc =
            PreconfServiceBuilder::new(PreconfConfig::default()).expect("default config validates");
        let svc2 = svc.clone();
        // Both clones see the same fifo / cfg via Arc pointer equality.
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
}
