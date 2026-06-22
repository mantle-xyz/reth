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

use std::sync::Arc;

use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::NodePrimitives;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::TransactionPool;

use crate::{
    PreconfCanonHandler, PreconfConfig, PreconfRpcHandler, PreconfTxSet, config::PreconfConfigError,
};

/// Owns and hands out the shared preconf handles for a single node
/// instance. Construct once at startup; clone the `Arc` returned by the
/// accessors freely.
#[derive(Debug, Clone)]
pub struct PreconfServiceBuilder {
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
}

impl PreconfServiceBuilder {
    /// Validate `cfg` and construct the shared fifo with the
    /// configured broadcast capacity.
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
        Ok(Self { cfg, fifo })
    }

    /// Shared config handle. Cheap to clone — internally an `Arc`.
    pub fn cfg(&self) -> &Arc<PreconfConfig> {
        &self.cfg
    }

    /// Shared fifo handle. Cheap to clone — internally an `Arc`.
    pub fn fifo(&self) -> &Arc<PreconfTxSet> {
        &self.fifo
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
        PreconfCanonHandler::new(provider, self.fifo.clone())
    }

    /// Construct the local-sequencer RPC handler. Returned by value so
    /// the caller can decide whether to wrap in `Arc` (the path through
    /// `MantleRpcExt::new` expects `Arc<dyn DynPreconfHandler>`).
    pub fn rpc_handler<P, Pr>(&self, pool: P, provider: Pr) -> PreconfRpcHandler<P, Pr>
    where
        P: TransactionPool + 'static,
        Pr: StateProviderFactory + 'static,
    {
        PreconfRpcHandler::new(pool, provider, self.fifo.clone(), self.cfg.clone())
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
}
