//! Pool-validator decoration that enforces preconf-specific rules **before**
//! the wrapped (Mantle / OP / Eth) validator runs.
//!
//! Two checks are added on top of whatever inner validator is wrapped:
//!
//! 1. **Replacement guard**: a tx whose `(sender, nonce)` collides with a
//!    non-`Timeout` `PreconfTxSet` entry of a different hash is rejected.
//!    `Timeout` entries release the slot — the existing fifo record is
//!    actively removed so the new tx can proceed cleanly.
//!
//! 2. **Per-tx gas ceiling (operator hardening)**: preconf-eligible txs
//!    whose `gas_limit` exceeds `cfg.preconf_max_gas_per_tx` are rejected.
//!    Non-preconf txs pass through unaffected.
//!
//! All other concerns (signature, balance, basefee, EIP-155, `MetaTx`, ...)
//! are delegated to the inner validator unchanged.

use std::{any::Any, sync::Arc};

use alloy_consensus::Transaction;
use reth_optimism_txpool::OpPooledTx;
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{
    EthPoolTransaction, PoolTransaction, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator, error::InvalidPoolTransactionError,
};

use crate::{config::PreconfConfig, preconf_tx_set::PreconfTxSet, types::PreconfStatus};

/// Replacement attempt blocked because an active preconf commitment already
/// occupies `(sender, nonce)`.
#[derive(thiserror::Error, Debug)]
#[error("cannot replace active preconf commitment for the same (sender, nonce)")]
pub struct ReplaceActivePreconf;

impl reth_transaction_pool::error::PoolTransactionError for ReplaceActivePreconf {
    fn is_bad_transaction(&self) -> bool {
        // `is_bad_transaction == true` triggers, in reth's network layer:
        //   1. P2P reputation hit on the announcing peer
        //      (`ReputationChangeKind::BadTransactions`, weight ≈ -16384;
        //      ~4 hits and the peer is below the ban threshold)
        //   2. The tx hash is added to the `bad_imports` cache, so future
        //      announcements of the same hash from any peer are rejected
        //      without re-running validation
        //   3. (Skipped while the node is syncing)
        //
        // A replacement collision is not the sender's fault — they have no
        // way to know that another tx with the same (sender, nonce) is in
        // flight. Returning false here keeps both the peer and the hash
        // unblocked so the legitimate retry path (e.g., the same-hash
        // re-submit after `Timeout`) still works.
        //
        // Same treatment as op-geth's `ErrPreconfInProcess`: log it, do
        // not punish.
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A preconf-eligible tx's `gas_limit` exceeded the configured per-tx ceiling.
#[derive(thiserror::Error, Debug)]
#[error("preconf-eligible tx gas limit exceeds `preconf_max_gas_per_tx`")]
pub struct PreconfGasLimitExceeded;

impl reth_transaction_pool::error::PoolTransactionError for PreconfGasLimitExceeded {
    fn is_bad_transaction(&self) -> bool {
        // The preconf feature is opened only to internal trusted clients —
        // no public peer ever submits via this path. Returning true would
        // cause reth to apply a P2P `BadTransactions` reputation hit and
        // cache the tx hash in `bad_imports`, both of which would punish
        // our own internal RPC infrastructure on any misconfiguration.
        //
        // We therefore treat `preconf_max_gas_per_tx` the same way reth
        // treats its own `MaxTxGasLimitExceeded`: a node-local policy
        // bound that rejects the tx without penalizing the announcing
        // peer. The validation outcome still rejects the tx; only the
        // reputation side-effect is suppressed.
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Validator decorator that enforces preconf-specific rules before delegating.
///
/// Constructed via [`Self::new`] and threaded into the pool validation chain.
/// Typically the inner validator is `MantleTransactionValidator<OpTransactionValidator<...>>`.
///
/// Cheap to clone — `Arc`-shares the config and the fifo handle.
#[derive(Debug, Clone)]
pub struct PreconfAwareValidator<V> {
    inner: V,
    cfg: Arc<PreconfConfig>,
    fifo: Arc<PreconfTxSet>,
}

impl<V> PreconfAwareValidator<V> {
    /// Wrap an inner validator with preconf checks.
    pub const fn new(inner: V, cfg: Arc<PreconfConfig>, fifo: Arc<PreconfTxSet>) -> Self {
        Self { inner, cfg, fifo }
    }

    /// Borrow the wrapped validator.
    pub const fn inner(&self) -> &V {
        &self.inner
    }
}

impl<V> TransactionValidator for PreconfAwareValidator<V>
where
    V: TransactionValidator,
    V::Transaction: EthPoolTransaction + OpPooledTx,
{
    type Transaction = V::Transaction;
    type Block = V::Block;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        let sender = transaction.sender();
        let nonce = transaction.nonce();
        let tx_hash = *transaction.hash();
        let to = Transaction::to(&transaction);
        let is_preconf_eligible = self.cfg.is_preconf_tx(&sender, to.as_ref());

        // Replacement guard: only reclaimable terminal states release
        // the (sender, nonce) slot — `Timeout` (client deadline),
        // `Canceled` (F1 pre-apply reject), and `Failed` (reth builder
        // pre-execute reject; tx NOT on chain). `Waiting` / `Success`
        // block replacement (`Success` is on-chain or in-flight, so
        // replacement would double-apply).
        if let Some(existing) = self.fifo.find_by_sender_nonce(&sender, nonce).await
            && existing.hash != tx_hash
        {
            if !matches!(
                existing.status,
                PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed
            ) {
                return TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::Other(Box::new(ReplaceActivePreconf)),
                );
            }
            // Slot is Timeout / Canceled / Failed — drop the stale fifo
            // entry so the new tx can occupy the slot cleanly. The
            // replacement only proceeds via the rest of the validator
            // chain; we do not re-push here — the pool listener will
            // pick up the new tx once it lands in the pool.
            self.fifo.remove(&existing.hash).await;
        }

        // Per-tx gas ceiling: applies only to preconf-eligible txs.
        // Non-preconf txs are intentionally left to the upstream (reth /
        // OP) validator's own gas-limit checks
        if is_preconf_eligible && transaction.gas_limit() > self.cfg.preconf_max_gas_per_tx {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Other(Box::new(PreconfGasLimitExceeded)),
            );
        }

        self.inner.validate_transaction(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_transaction_pool::error::PoolTransactionError;

    // `is_bad_transaction` returns reth's "should the sender be penalized"
    // signal — NOT a generic "is the tx invalid" predicate. Tests below
    // assert the chosen reputation policy for each error variant.

    #[test]
    fn replace_active_preconf_does_not_penalize_sender() {
        // Sender has no way to know another tx with the same
        // (sender, nonce) is in flight — don't penalize.
        let err = ReplaceActivePreconf;
        assert!(!err.is_bad_transaction());
        assert!(err.as_any().is::<ReplaceActivePreconf>());
    }

    #[test]
    fn preconf_gas_limit_exceeded_does_not_penalize_sender() {
        // Preconf is gated to internal trusted clients only — penalizing
        // the announcing peer would punish our own RPC infrastructure on
        // misconfiguration. The tx is still rejected; reputation is not
        // touched.
        let err = PreconfGasLimitExceeded;
        assert!(!err.is_bad_transaction());
        assert!(err.as_any().is::<PreconfGasLimitExceeded>());
    }

    // Full validate_transaction tests need a concrete `OpPooledTransaction`
    // + a stub inner validator + a populated `PreconfTxSet`. The OP-tx
    // construction across all envelope variants is non-trivial scaffolding;
    // the decorator's branching logic is otherwise a straight read against
    // the `PreconfTxSet` state machine (already covered by 40 tests in
    // `preconf_tx_set::tests`). End-to-end tests of the validator are
    // deferred to an integration suite that spins up a real pool.
}
