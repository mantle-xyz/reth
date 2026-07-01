use crate::{InvalidCrossTx, OpPooledTx, interop_filter::InteropFilterClient};
use alloy_consensus::{BlockHeader, Transaction};
use op_revm::L1BlockInfo;
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_optimism_evm::RethL1BlockInfo;
use reth_optimism_forks::OpHardforks;
use reth_primitives_traits::{
    Block, BlockBody, BlockTy, GotExpected, SealedBlock,
    transaction::error::InvalidTransactionError,
};
use reth_storage_api::{AccountInfoReader, BlockReaderIdExt, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator, error::InvalidPoolTransactionError,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// The timeout for cross-chain transaction validation against the interop filter.
pub(crate) const CHECK_ACCESS_LIST_TIMEOUT_SECS: u64 = 7200;

/// Gas reserved on every L2 block for the always-present L1-info deposit transaction.
///
/// Mirrors op-geth `l1InfoGasOverhead` (`core/txpool/validation.go`): the L1-info deposit is
/// injected into every L2 block, so a non-deposit transaction can never consume the full block
/// gas limit. Upstream reth caps the tx-pool gas limit at the full block limit, so without this
/// reservation a transaction with gas in `(block_gas_limit - overhead, block_gas_limit]` would be
/// admitted to the pool yet could never be included — it would sit there until it expires. We
/// reserve the overhead so such transactions are rejected up front, as geth does.
pub(crate) const MANTLE_L1_INFO_GAS_OVERHEAD: u64 = 1_000_000;

/// Effective per-transaction gas limit for the Mantle tx-pool: the block gas limit minus the
/// L1-info deposit reservation. Port of the Optimism branch of op-geth's `EffectiveGasLimit`
/// (`core/txpool/validation.go`). Saturates to `0` when the block limit is below the reservation.
pub(crate) fn mantle_effective_gas_limit(block_gas_limit: u64) -> u64 {
    block_gas_limit.saturating_sub(MANTLE_L1_INFO_GAS_OVERHEAD)
}

/// Tracks additional infos for the current block.
#[derive(Debug, Default)]
pub struct OpL1BlockInfo {
    /// The current L1 block info.
    l1_block_info: RwLock<L1BlockInfo>,
    /// Current block timestamp.
    timestamp: AtomicU64,
}

impl OpL1BlockInfo {
    /// Returns the most recent timestamp
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }
}

/// Validator for Optimism transactions.
#[derive(Debug, Clone)]
pub struct OpTransactionValidator<Client, Tx, Evm> {
    /// The type that performs the actual validation.
    inner: Arc<EthTransactionValidator<Client, Tx, Evm>>,
    /// Additional block info required for validation.
    block_info: Arc<OpL1BlockInfo>,
    /// If true, ensure that the transaction's sender has enough balance to cover the L1 gas fee
    /// derived from the tracked L1 block info that is extracted from the first transaction in the
    /// L2 block.
    require_l1_data_gas_fee: bool,
    /// Client used to check transaction validity with the interop filter.
    interop_client: Option<InteropFilterClient>,
    /// tracks activated forks relevant for transaction validation
    fork_tracker: Arc<OpForkTracker>,
}

impl<Client, Tx, Evm> OpTransactionValidator<Client, Tx, Evm> {
    /// Returns the configured chain spec
    pub fn chain_spec(&self) -> Arc<Client::ChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.inner.chain_spec()
    }

    /// Returns the configured client
    pub fn client(&self) -> &Client {
        self.inner.client()
    }

    /// Returns the current block timestamp.
    fn block_timestamp(&self) -> u64 {
        self.block_info.timestamp.load(Ordering::Relaxed)
    }

    /// Whether to ensure that the transaction's sender has enough balance to also cover the L1 gas
    /// fee.
    pub fn require_l1_data_gas_fee(self, require_l1_data_gas_fee: bool) -> Self {
        Self { require_l1_data_gas_fee, ..self }
    }

    /// Returns whether this validator also requires the transaction's sender to have enough balance
    /// to cover the L1 gas fee.
    pub const fn requires_l1_data_gas_fee(&self) -> bool {
        self.require_l1_data_gas_fee
    }
}

impl<Client, Tx, Evm> OpTransactionValidator<Client, Tx, Evm>
where
    Client:
        ChainSpecProvider<ChainSpec: OpHardforks> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + OpPooledTx,
    Evm: ConfigureEvm,
{
    /// Create a new [`OpTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx, Evm>) -> Self {
        let this = Self::with_block_info(inner, OpL1BlockInfo::default());
        if let Ok(Some(block)) =
            this.inner.client().block_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)
        {
            // genesis block has no txs, so we can't extract L1 info, we set the block info to empty
            // so that we will accept txs into the pool before the first block
            if block.header().number() == 0 {
                this.block_info.timestamp.store(block.header().timestamp(), Ordering::Relaxed);
            } else {
                this.update_l1_block_info(block.header(), block.body().transactions().first());
            }
        }

        this
    }

    /// Create a new [`OpTransactionValidator`] with the given [`OpL1BlockInfo`].
    pub fn with_block_info(
        inner: EthTransactionValidator<Client, Tx, Evm>,
        block_info: OpL1BlockInfo,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            block_info: Arc::new(block_info),
            require_l1_data_gas_fee: true,
            interop_client: None,
            fork_tracker: Arc::new(OpForkTracker { interop: AtomicBool::from(false) }),
        }
    }

    /// Sets the interop filter client and safety level.
    pub fn with_interop(mut self, interop_client: InteropFilterClient) -> Self {
        self.interop_client = Some(interop_client);
        self
    }

    /// Update the L1 block info for the given header and system transaction, if any.
    ///
    /// Note: this supports optional system transaction, in case this is used in a dev setup
    pub fn update_l1_block_info<H, T>(&self, header: &H, tx: Option<&T>)
    where
        H: BlockHeader,
        T: Transaction,
    {
        self.block_info.timestamp.store(header.timestamp(), Ordering::Relaxed);

        if let Some(Ok(l1_block_info)) = tx.map(reth_optimism_evm::extract_l1_info_from_tx) {
            *self.block_info.l1_block_info.write() = l1_block_info;
        }

        // [MANTLE] token_ratio is not in L1 info calldata; read from GAS_ORACLE_CONTRACT state.
        if self.chain_spec().is_mantle() &&
            let Ok(state) = self.client().latest() &&
            let Ok(Some(ratio)) = state.storage(
                op_revm::constants::GAS_ORACLE_CONTRACT,
                op_revm::constants::TOKEN_RATIO_SLOT.into(),
            )
        {
            self.block_info.l1_block_info.write().token_ratio = ratio;
        }

        if self.chain_spec().is_interop_active_at_timestamp(header.timestamp()) {
            self.fork_tracker.interop.store(true, Ordering::Relaxed);
        }
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`OpTransactionValidator::validate_one_with_state`], but creates
    /// a new state provider internally.
    pub async fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_state(origin, transaction, &mut None).await
    }

    /// Validates a single transaction with a provided state provider.
    ///
    /// This allows reusing the same state provider across multiple transaction validations.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one_with_state`], but in
    /// addition applies OP validity checks:
    /// - ensures tx is not eip4844
    /// - ensures cross chain transactions are valid wrt locally configured safety level
    /// - ensures that the account has enough balance to cover the L1 gas cost
    pub async fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        if transaction.is_eip4844() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        // Interop cross tx validation
        match self.is_valid_cross_tx(&transaction).await {
            Some(Err(err)) => {
                let err = match err {
                    InvalidCrossTx::CrossChainTxPreInterop => {
                        InvalidTransactionError::TxTypeNotSupported.into()
                    }
                    err => InvalidPoolTransactionError::Other(Box::new(err)),
                };
                return TransactionValidationOutcome::Invalid(transaction, err);
            }
            Some(Ok(_)) => {
                // valid interop tx
                transaction
                    .set_interop_deadline(self.block_timestamp() + CHECK_ACCESS_LIST_TIMEOUT_SECS);
            }
            _ => {}
        }

        // [MANTLE] Reserve gas for the L1-info deposit present in every L2 block, matching
        // op-geth `EffectiveGasLimit` (`core/txpool/validation.go`). Upstream reth caps at the
        // full block gas limit, so without this a tx with gas_limit in
        // `(block_gas_limit - overhead, block_gas_limit]` would be admitted but could never be
        // included (the L1-info deposit always consumes part of the block), leaving it stuck.
        if self.chain_spec().is_mantle() {
            let gas_limit = transaction.gas_limit();
            let effective_limit = mantle_effective_gas_limit(self.inner.block_gas_limit());
            if gas_limit > effective_limit {
                return TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidPoolTransactionError::ExceedsGasLimit(gas_limit, effective_limit),
                );
            }
        }

        let outcome = self.inner.validate_one_with_state(origin, transaction, state);

        self.apply_op_checks(outcome)
    }

    /// Performs the necessary opstack specific checks based on top of the regular eth outcome.
    fn apply_op_checks(
        &self,
        outcome: TransactionValidationOutcome<Tx>,
    ) -> TransactionValidationOutcome<Tx> {
        if !self.requires_l1_data_gas_fee() {
            // no need to check L1 gas fee
            return outcome;
        }
        // ensure that the account has enough balance to cover the L1 gas cost
        if let TransactionValidationOutcome::Valid {
            balance,
            state_nonce,
            transaction: valid_tx,
            propagate,
            bytecode_hash,
            authorities,
        } = outcome
        {
            let mut l1_block_info = self.block_info.l1_block_info.read().clone();

            let encoded = valid_tx.transaction().encoded_2718();

            let cost_addition = match l1_block_info.l1_tx_data_fee(
                self.chain_spec(),
                self.block_timestamp(),
                &encoded,
                false,
            ) {
                Ok(cost) => cost,
                Err(err) => {
                    return TransactionValidationOutcome::Error(*valid_tx.hash(), Box::new(err));
                }
            };
            let cost = valid_tx.transaction().cost().saturating_add(cost_addition);

            // Checks for max cost
            if cost > balance {
                return TransactionValidationOutcome::Invalid(
                    valid_tx.into_transaction(),
                    InvalidTransactionError::InsufficientFunds(
                        GotExpected { got: balance, expected: cost }.into(),
                    )
                    .into(),
                );
            }

            return TransactionValidationOutcome::Valid {
                balance,
                state_nonce,
                transaction: valid_tx,
                propagate,
                bytecode_hash,
                authorities,
            };
        }
        outcome
    }

    /// Wrapper for is valid cross tx
    pub async fn is_valid_cross_tx(&self, tx: &Tx) -> Option<Result<(), InvalidCrossTx>> {
        // We don't need to check for deposit transaction in here, because they won't come from
        // txpool
        self.interop_client
            .as_ref()?
            .is_valid_cross_tx(
                tx.access_list(),
                tx.hash(),
                self.block_info.timestamp.load(Ordering::Relaxed),
                Some(CHECK_ACCESS_LIST_TIMEOUT_SECS),
                self.fork_tracker.is_interop_activated(),
            )
            .await
    }
}

impl<Client, Tx, Evm> TransactionValidator for OpTransactionValidator<Client, Tx, Evm>
where
    Client:
        ChainSpecProvider<ChainSpec: OpHardforks> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + OpPooledTx,
    Evm: ConfigureEvm,
{
    type Transaction = Tx;
    type Block = BlockTy<Evm::Primitives>;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(
            new_tip_block.header(),
            new_tip_block.body().transactions().first(),
        );
    }
}

/// Keeps track of whether certain forks are activated
#[derive(Debug)]
pub(crate) struct OpForkTracker {
    /// Tracks if interop is activated at the block's timestamp.
    interop: AtomicBool,
}

impl OpForkTracker {
    /// Returns `true` if Lagoon fork is activated.
    pub(crate) fn is_interop_activated(&self) -> bool {
        self.interop.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{MANTLE_L1_INFO_GAS_OVERHEAD, mantle_effective_gas_limit};

    #[test]
    fn effective_gas_limit_subtracts_l1_info_overhead() {
        // 30M block → 29M effective; 60M block → 59M (matches op-geth EffectiveGasLimit).
        assert_eq!(
            mantle_effective_gas_limit(30_000_000),
            30_000_000 - MANTLE_L1_INFO_GAS_OVERHEAD
        );
        assert_eq!(mantle_effective_gas_limit(60_000_000), 59_000_000);
    }

    #[test]
    fn effective_gas_limit_saturates_below_overhead() {
        // A block limit at or below the reservation leaves no room for user txs.
        assert_eq!(mantle_effective_gas_limit(MANTLE_L1_INFO_GAS_OVERHEAD), 0);
        assert_eq!(mantle_effective_gas_limit(500_000), 0);
        assert_eq!(mantle_effective_gas_limit(0), 0);
    }
}
