//! Unified receipt builder that handles deposit and non-deposit transactions.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use op_alloy_consensus::{OpDepositReceipt, OpTxEnvelope, OpTxType};
use reth_evm::Evm;
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::OpReceipt;
use revm::{Database, context::result::ExecutionResult};

/// Error type for receipt building operations.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptBuildError {
    /// Failed to load the deposit sender's account from the database.
    #[error("failed to load deposit account")]
    DepositAccountLoad,
}

/// A receipt builder that handles both deposit and non-deposit transactions
/// without requiring error handling at the call site.
#[derive(Debug, Clone)]
pub struct UnifiedReceiptBuilder<C> {
    chain_spec: C,
}

impl<C> UnifiedReceiptBuilder<C> {
    /// Creates a new unified receipt builder with the given chain specification.
    pub const fn new(chain_spec: C) -> Self {
        Self { chain_spec }
    }

    /// Returns a reference to the chain specification.
    pub const fn chain_spec(&self) -> &C {
        &self.chain_spec
    }
}

impl<C: OpHardforks> UnifiedReceiptBuilder<C> {
    /// Builds a receipt for any transaction type, handling deposit receipts internally.
    ///
    /// # Errors
    /// Returns [`ReceiptBuildError`] if the deposit sender's account cannot be
    /// loaded from the database.
    pub fn build<E>(
        &self,
        evm: &mut E,
        transaction: &Recovered<OpTxEnvelope>,
        result: &ExecutionResult<E::HaltReason>,
        cumulative_gas_used: u64,
        timestamp: u64,
    ) -> Result<OpReceipt, ReceiptBuildError>
    where
        E: Evm,
        E::DB: Database,
    {
        let tx_type = transaction.tx_type();

        let receipt = Receipt {
            status: Eip658Value::Eip658(result.is_success()),
            cumulative_gas_used,
            logs: result.logs().to_vec(),
        };

        if tx_type == OpTxType::Deposit {
            let deposit_nonce = if self.chain_spec.is_regolith_active_at_timestamp(timestamp) {
                Some(
                    evm.db_mut()
                        .basic(transaction.signer())
                        .map_err(|_| ReceiptBuildError::DepositAccountLoad)?
                        .map(|acc| acc.nonce)
                        .unwrap_or_default(),
                )
            } else {
                None
            };

            // Always `None` on Mantle: MNT is the native gas token and ETH is an
            // ERC-20, so deposit receipts do not follow OP Canyon's
            // `deposit_receipt_version` semantics. Mirrors `alloy-op-evm`.
            Ok(OpReceipt::Deposit(OpDepositReceipt {
                inner: receipt,
                deposit_nonce,
                deposit_receipt_version: None,
            }))
        } else {
            Ok(match tx_type {
                OpTxType::Legacy => OpReceipt::Legacy(receipt),
                OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
                OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
                OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
                OpTxType::PostExec => OpReceipt::PostExec(receipt),
                OpTxType::Deposit => unreachable!(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, Bytes, Log, LogData, Signature, TxKind, U256, address};
    use mantle_reth_chainspec::MANTLE_MAINNET;
    use op_alloy_consensus::TxDeposit;
    use op_revm::OpHaltReason;
    use reth_evm::ConfigureEvm;
    use reth_optimism_chainspec::OpChainSpec;
    use reth_optimism_evm::OpEvmConfig;
    use revm::{
        context::result::{Output, ResultGas, SuccessReason},
        database::InMemoryDB,
    };

    use super::*;

    fn chain_spec() -> Arc<OpChainSpec> {
        Arc::clone(&MANTLE_MAINNET)
    }

    fn create_legacy_tx() -> Recovered<OpTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1000000000,
            gas_limit: 21000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let envelope = OpTxEnvelope::Legacy(alloy_consensus::Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        Recovered::new_unchecked(envelope, Address::ZERO)
    }

    fn create_deposit_tx() -> Recovered<OpTxEnvelope> {
        let deposit = TxDeposit {
            source_hash: B256::ZERO,
            from: address!("0x1234567890123456789012345678901234567890"),
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::new(),
            eth_tx_value: None,
        };
        let sealed = alloy_consensus::Sealed::new_unchecked(deposit, B256::ZERO);
        Recovered::new_unchecked(
            OpTxEnvelope::Deposit(sealed),
            address!("0x1234567890123456789012345678901234567890"),
        )
    }

    fn create_success_result() -> ExecutionResult<OpHaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default()
                .with_total_gas_spent(21_000)
                .with_refunded(0)
                .with_floor_gas(0),
            logs: vec![Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], Bytes::new()),
            }],
            output: Output::Call(Bytes::new()),
        }
    }

    fn create_revert_result(gas_spent: u64) -> ExecutionResult<OpHaltReason> {
        ExecutionResult::Revert {
            gas: ResultGas::default()
                .with_total_gas_spent(gas_spent)
                .with_refunded(0)
                .with_floor_gas(0),
            logs: vec![],
            output: Bytes::new(),
        }
    }

    fn create_test_evm(
        chain_spec: Arc<OpChainSpec>,
        db: &mut InMemoryDB,
    ) -> impl Evm<HaltReason = OpHaltReason, DB = &mut InMemoryDB> + '_ {
        let evm_config = OpEvmConfig::optimism(chain_spec);
        let header = Header::default();
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        evm_config.evm_with_env(db, evm_env)
    }

    #[test]
    fn test_unified_receipt_builder_creation() {
        let spec = chain_spec();
        let builder = UnifiedReceiptBuilder::new(Arc::clone(&spec));
        assert!(Arc::ptr_eq(builder.chain_spec(), &spec));
    }

    #[test]
    fn test_build_legacy_receipt() {
        let spec = chain_spec();
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(Arc::clone(&spec), &mut db);

        let builder = UnifiedReceiptBuilder::new(spec);
        let receipt = builder
            .build(&mut evm, &create_legacy_tx(), &create_success_result(), 21000, 0)
            .expect("build should succeed");

        let OpReceipt::Legacy(inner) = receipt else { panic!("expected legacy receipt") };
        assert!(inner.status.coerce_status());
        assert_eq!(inner.cumulative_gas_used, 21000);
        assert_eq!(inner.logs.len(), 1);
    }

    #[test]
    fn test_build_deposit_receipt() {
        let spec = chain_spec();
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(Arc::clone(&spec), &mut db);

        let builder = UnifiedReceiptBuilder::new(spec);
        let receipt = builder
            .build(&mut evm, &create_deposit_tx(), &create_success_result(), 21000, 0)
            .expect("build should succeed");

        let OpReceipt::Deposit(deposit) = receipt else { panic!("expected deposit receipt") };
        assert!(deposit.inner.status.coerce_status());
        assert_eq!(deposit.inner.cumulative_gas_used, 21000);
    }

    #[test]
    fn test_deposit_receipt_version_is_never_set() {
        let spec = chain_spec();
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(Arc::clone(&spec), &mut db);

        let builder = UnifiedReceiptBuilder::new(spec);
        // Far past any OP fork activation; Mantle still leaves the field unset.
        let receipt = builder
            .build(&mut evm, &create_deposit_tx(), &create_success_result(), 21000, 2_000_000_000)
            .expect("build should succeed");

        let OpReceipt::Deposit(deposit) = receipt else { panic!("expected deposit receipt") };
        assert_eq!(deposit.deposit_receipt_version, None);
    }

    #[test]
    fn test_build_failed_transaction_receipt() {
        let spec = chain_spec();
        let mut db = InMemoryDB::default();
        let mut evm = create_test_evm(Arc::clone(&spec), &mut db);

        let builder = UnifiedReceiptBuilder::new(spec);
        let receipt = builder
            .build(&mut evm, &create_legacy_tx(), &create_revert_result(10_000), 10000, 0)
            .expect("build should succeed");

        let OpReceipt::Legacy(inner) = receipt else { panic!("expected legacy receipt") };
        assert!(!inner.status.coerce_status());
        assert_eq!(inner.cumulative_gas_used, 10000);
        assert!(inner.logs.is_empty());
    }
}
