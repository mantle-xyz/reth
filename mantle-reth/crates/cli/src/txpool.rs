//! Mantle-specific transaction pool validation.
//!
//! Wraps [`OpTransactionValidator`] to reject transactions that are valid on OP chains
//! but explicitly disabled on Mantle:
//!
//! - EIP-155 unprotected transactions (legacy txs without `chain_id`)
//! - Legacy `MetaTx` transactions (disabled since `MantleEverest`)

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Typed2718;
use alloy_op_hardforks::is_mantle_meta_tx;
use reth_optimism_txpool::OpPooledTx;
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{
    EthPoolTransaction, TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
    error::InvalidPoolTransactionError,
};
use std::any::Any;

/// Legacy Mantle `MetaTx` transactions are permanently disabled since `MantleEverest`.
#[derive(thiserror::Error, Debug)]
#[error("meta tx is disabled")]
pub struct MetaTxDisabled;

impl reth_transaction_pool::error::PoolTransactionError for MetaTxDisabled {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// EIP-155 unprotected transactions (missing `chain_id`) are rejected on Mantle.
#[derive(thiserror::Error, Debug)]
#[error("only replay-protected (EIP-155) transactions allowed over RPC")]
pub struct UnprotectedTxDisabled;

impl reth_transaction_pool::error::PoolTransactionError for UnprotectedTxDisabled {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Wraps an inner [`TransactionValidator`] and adds Mantle-specific rejection rules:
///
/// - EIP-155 unprotected legacy transactions (type 0 without `chain_id`)
/// - Legacy `MetaTx` transactions (disabled since `MantleEverest`)
///
/// All other transactions are forwarded to the inner validator unchanged.
#[derive(Debug, Clone)]
pub struct MantleTransactionValidator<V> {
    inner: V,
}

impl<V> MantleTransactionValidator<V> {
    /// Creates a new [`MantleTransactionValidator`] wrapping the given inner validator.
    pub const fn new(inner: V) -> Self {
        Self { inner }
    }

    /// Returns a reference to the inner validator.
    pub fn inner(&self) -> &V {
        &self.inner
    }
}

impl<V> TransactionValidator for MantleTransactionValidator<V>
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
        // [MANTLE] Reject EIP-155 unprotected transactions (no chain_id)
        if transaction.ty() == 0 && transaction.chain_id().is_none() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Other(Box::new(UnprotectedTxDisabled)),
            );
        }

        // [MANTLE] Reject legacy MetaTx transactions (disabled since MantleEverest)
        if is_mantle_meta_tx(transaction.input()) {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::Other(Box::new(MetaTxDisabled)),
            );
        }

        self.inner.validate_transaction(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
    }
}
