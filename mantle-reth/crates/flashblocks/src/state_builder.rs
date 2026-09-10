//! Local replay of flashblock transactions into pending state.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database as AlloyDatabase,
    block::{StateDB, SystemCaller},
};
use alloy_primitives::B256;
use alloy_rpc_types_eth::{TransactionTrait, state::StateOverride};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use op_revm::{
    L1BlockInfo, OpHaltReason, constants::L1_BLOCK_CONTRACT, estimate_tx_compressed_size,
};
use reth_evm::{Evm, FromRecoveredTx};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::{OpPrimitives, OpReceipt};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use revm::{
    Database, DatabaseCommit,
    context::{
        Block as _,
        result::{ExecutionResult, ResultAndState},
    },
    state::EvmState,
};

use crate::{ExecutionError, PendingBlocks, StateProcessorError, UnifiedReceiptBuilder};

/// Result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: OpTransactionReceipt,
    /// The updated EVM state.
    pub state: EvmState,
    /// The execution result of the transaction.
    pub result: ExecutionResult<OpHaltReason>,
    /// Per-transaction EVM execution time, if known.
    pub execution_time_us: Option<u128>,
}

#[derive(Debug)]
struct CachedTransactionExecution {
    receipt: OpTransactionReceipt,
    state: EvmState,
    result: ExecutionResult<OpHaltReason>,
    execution_time_us: Option<u128>,
}

/// Executes or fetches cached values for transactions in a flashblock.
#[derive(Debug)]
pub struct PendingStateBuilder<E, ChainSpec> {
    cumulative_gas_used: u64,
    next_log_index: usize,

    evm: E,
    pending_block: Block<OpTxEnvelope, Header>,
    l1_block_info: L1BlockInfo,
    receipt_builder: UnifiedReceiptBuilder<ChainSpec>,
    chain_spec: ChainSpec,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
}

impl<E, ChainSpec, DB> PendingStateBuilder<E, ChainSpec>
where
    E: Evm<DB = DB, HaltReason = OpHaltReason>,
    DB: Database + DatabaseCommit,
    E::Tx: FromRecoveredTx<OpTxEnvelope>,
    ChainSpec: OpHardforks + reth_chainspec::EthChainSpec + Clone,
{
    /// Creates a new pending state builder.
    pub fn new(
        chain_spec: ChainSpec,
        evm: E,
        pending_block: Block<OpTxEnvelope, Header>,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        l1_block_info: L1BlockInfo,
        state_overrides: StateOverride,
    ) -> Self {
        Self {
            pending_block,
            evm,
            cumulative_gas_used: 0,
            next_log_index: 0,
            prev_pending_blocks,
            l1_block_info,
            state_overrides,
            chain_spec: chain_spec.clone(),
            receipt_builder: UnifiedReceiptBuilder::new(chain_spec),
        }
    }

    /// Consumes the builder and returns the database and state overrides.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.evm.into_db(), self.state_overrides)
    }

    /// Returns a mutable reference to the underlying database.
    pub fn db_mut(&mut self) -> &mut DB {
        self.evm.db_mut()
    }

    /// Seeds block-level offsets when appending transactions to an already-executed block.
    ///
    /// Omitting this leaves every receipt in the slice with a `cumulative_gas_used`
    /// and log index restarting from zero.
    pub const fn set_execution_offsets(&mut self, cumulative_gas_used: u64, next_log_index: usize) {
        self.cumulative_gas_used = cumulative_gas_used;
        self.next_log_index = next_log_index;
    }

    /// Returns the cumulative gas used for the current pending block.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }

    /// Returns the next log index for the current pending block.
    pub const fn next_log_index(&self) -> usize {
        self.next_log_index
    }

    /// Executes a single transaction and updates internal state.
    ///
    /// Must be called in block order for each transaction.
    pub fn execute_transaction(
        &mut self,
        idx: usize,
        transaction: Recovered<OpTxEnvelope>,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        let effective_gas_price = if transaction.is_deposit() {
            0
        } else {
            self.pending_block
                .base_fee_per_gas
                .map(|base_fee| {
                    transaction.effective_tip_per_gas(base_fee).unwrap_or_default() +
                        base_fee as u128
                })
                .unwrap_or_else(|| transaction.max_fee_per_gas())
        };

        let cached_execution = self.prev_pending_blocks.as_ref().and_then(|p| {
            Some(CachedTransactionExecution {
                receipt: p.get_receipt(tx_hash)?.clone(),
                state: p.get_transaction_state(&tx_hash)?,
                result: p.get_transaction_result(&tx_hash)?.clone(),
                execution_time_us: p.get_execution_time(&tx_hash),
            })
        });

        if let Some(cached_execution) = cached_execution {
            self.execute_with_cached_data(transaction, cached_execution, idx, effective_gas_price)
        } else {
            self.execute_with_evm(transaction, idx, effective_gas_price)
        }
    }

    /// Applies EIP-4788 and EIP-2935 pre-execution changes to the EVM.
    ///
    /// Must be called once per block, before executing any transactions, so cached
    /// execution results match what the validator computes. Mantle disables OP
    /// Canyon's create2-deployer force-deploy, so that step is absent here.
    pub fn apply_pre_execution_changes(
        &mut self,
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
    ) -> Result<(), StateProcessorError>
    where
        DB: AlloyDatabase + StateDB,
        ChainSpec: Clone,
    {
        let spec = self.receipt_builder.chain_spec();
        let mut system_caller = SystemCaller::new(spec.clone());
        system_caller
            .apply_blockhashes_contract_call(parent_hash, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        system_caller
            .apply_beacon_root_contract_call(parent_beacon_block_root, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;

        Ok(())
    }

    /// Builds transaction result from cached receipt and state data.
    fn execute_with_cached_data(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        cached_execution: CachedTransactionExecution,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let CachedTransactionExecution { receipt, state, result, execution_time_us } =
            cached_execution;

        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let OpReceipt::Deposit(deposit_receipt) = &receipt.inner.inner.receipt else {
                return Err(ExecutionError::DepositReceiptMismatch.into());
            };

            (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
        } else {
            (None, None)
        };

        let rpc_transaction = Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: transaction,
                block_hash: None,
                block_number: Some(self.pending_block.number),
                block_timestamp: Some(self.pending_block.timestamp),
                transaction_index: Some(idx as u64),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
        };

        self.cumulative_gas_used = self
            .cumulative_gas_used
            .checked_add(receipt.inner.gas_used)
            .ok_or(ExecutionError::GasOverflow)?;
        self.next_log_index += receipt.inner.logs().len();

        for address in state.keys() {
            self.evm.db_mut().basic(*address).map_err(|err| {
                StateProcessorError::Execution(ExecutionError::EvmEnv(err.to_string()))
            })?;
        }
        self.evm.db_mut().commit(state.clone());

        Ok(ExecutedPendingTransaction {
            rpc_transaction,
            receipt,
            state,
            result,
            execution_time_us,
        })
    }

    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &Recovered<OpTxEnvelope>,
    ) -> Result<u64, StateProcessorError> {
        let encoded =
            estimate_tx_compressed_size(tx_env.encoded_2718().as_ref()).saturating_div(1_000_000);

        // Preload the L1 block contract; fetching the scalar panics on a cold cache.
        self.evm.db_mut().basic(L1_BLOCK_CONTRACT).map_err(|err| {
            StateProcessorError::Execution(ExecutionError::EvmEnv(err.to_string()))
        })?;

        let da_footprint_gas_scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(self.evm.db_mut())
            .map_err(|err| StateProcessorError::Execution(ExecutionError::EvmEnv(err.to_string())))?
            .into();

        Ok(encoded.saturating_mul(da_footprint_gas_scalar))
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<OpTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();
        let is_deposit = transaction.is_deposit();

        let da_footprint_used = if self
            .chain_spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to()) &&
            !is_deposit
        {
            self.jovian_da_footprint_estimation(&transaction)?
        } else {
            0
        };

        let start = Instant::now();
        let transact_result = self.evm.transact(&transaction);
        let elapsed_us = start.elapsed().as_micros();

        match transact_result {
            Ok(ResultAndState { state, result }) => {
                let gas_used = result.tx_gas_used();
                for (addr, acc) in &state {
                    let existing_override = self.state_overrides.entry(*addr).or_default();
                    existing_override.balance = Some(acc.info.balance);
                    existing_override.nonce = Some(acc.info.nonce);
                    existing_override.code = acc.info.code.clone().map(|code| code.bytes());

                    let existing =
                        existing_override.state_diff.get_or_insert_with(Default::default);
                    let changed_slots = acc
                        .storage
                        .iter()
                        .map(|(&key, slot)| (B256::from(key), B256::from(slot.present_value)));

                    existing.extend(changed_slots);
                }

                self.cumulative_gas_used = self
                    .cumulative_gas_used
                    .checked_add(gas_used)
                    .ok_or(ExecutionError::GasOverflow)?;

                let receipt = self.receipt_builder.build(
                    &mut self.evm,
                    &transaction,
                    &result,
                    self.cumulative_gas_used,
                    self.pending_block.timestamp,
                )?;

                let meta = TransactionMeta {
                    tx_hash,
                    index: idx as u64,
                    block_hash: B256::ZERO,
                    block_number: self.pending_block.number,
                    base_fee: self.pending_block.base_fee_per_gas,
                    excess_blob_gas: self.pending_block.excess_blob_gas,
                    timestamp: self.pending_block.timestamp,
                };

                let sender = transaction.signer();
                let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx: Recovered::new_unchecked(&transaction, sender),
                    gas_used,
                    next_log_index: self.next_log_index,
                    meta,
                };

                // No SDM accounting here: the refund is produced by an EVM inspector
                // this path never arms, so gas and balances follow legacy accounting.
                // Only correct while SDM stays disabled — see T3 in the plan discussion.
                let mut op_receipt = OpReceiptBuilder::new(
                    self.receipt_builder.chain_spec(),
                    input,
                    &mut self.l1_block_info,
                    None,
                )
                .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
                .build();

                op_receipt.inner.blob_gas_used = Some(da_footprint_used);
                self.next_log_index += receipt.logs().len();

                let (deposit_receipt_version, deposit_nonce) = if is_deposit {
                    let OpReceipt::Deposit(deposit_receipt) = &op_receipt.inner.inner.receipt
                    else {
                        return Err(ExecutionError::DepositReceiptMismatch.into());
                    };

                    (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
                } else {
                    (None, None)
                };

                let rpc_transaction = Transaction {
                    inner: alloy_rpc_types_eth::Transaction {
                        inner: transaction,
                        block_hash: None,
                        block_number: Some(self.pending_block.number),
                        block_timestamp: Some(self.pending_block.timestamp),
                        transaction_index: Some(idx as u64),
                        effective_gas_price: Some(effective_gas_price),
                    },
                    deposit_nonce,
                    deposit_receipt_version,
                };
                self.evm.db_mut().commit(state.clone());

                Ok(ExecutedPendingTransaction {
                    rpc_transaction,
                    receipt: op_receipt,
                    state,
                    result,
                    execution_time_us: Some(elapsed_us),
                })
            }
            Err(e) => Err(ExecutionError::TransactionFailed {
                tx_hash,
                sender: transaction.signer(),
                reason: format!("{e:?}"),
            }
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Sealed, Signed, transaction::Recovered};
    use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
    use alloy_op_hardforks::{MANTLE_MAINNET_ARSIA_TIMESTAMP, MANTLE_MAINNET_SKADI_TIMESTAMP};
    use alloy_primitives::{Address, Signature, TxKind, U256, address, uint};
    use alloy_rpc_types_engine::PayloadId;
    use mantle_reth_chainspec::MANTLE_MAINNET;
    use mantle_reth_flashblocks_types::{MantleFlashblockMetadata, MantleFlashblockPayload};
    use op_alloy_consensus::TxDeposit;
    use op_alloy_rpc_types_engine::{OpFlashblockPayloadBase, OpFlashblockPayloadDelta};
    use reth_evm::ConfigureEvm;
    use reth_optimism_chainspec::OpChainSpec;
    use reth_optimism_evm::OpEvmConfig;
    use reth_revm::State;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use super::*;

    /// EIP-4788 ring buffer length, hardcoded in the contract bytecode.
    const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;
    const POST_SKADI_TIMESTAMP: u64 = MANTLE_MAINNET_SKADI_TIMESTAMP + 1;
    const PRE_SKADI_TIMESTAMP: u64 = MANTLE_MAINNET_SKADI_TIMESTAMP - 1;
    const POST_ARSIA_TIMESTAMP: u64 = MANTLE_MAINNET_ARSIA_TIMESTAMP + 1;

    const L1_BLOCK_ADDRESS: Address =
        Address::new([0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15]);
    const DA_FOOTPRINT_GAS_SCALAR_SLOT: U256 = uint!(8_U256);

    fn chain_spec() -> Arc<OpChainSpec> {
        Arc::clone(&MANTLE_MAINNET)
    }

    fn funded_sender_db(sender: Address) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );
        db
    }

    fn seed_da_footprint_scalar(db: &mut InMemoryDB, scalar: u16) {
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&scalar.to_be_bytes());
        db.insert_account_info(L1_BLOCK_ADDRESS, AccountInfo::default());
        db.insert_account_storage(
            L1_BLOCK_ADDRESS,
            DA_FOOTPRINT_GAS_SCALAR_SLOT,
            U256::from_be_bytes(slot_value),
        )
        .expect("failed to insert L1 block storage");
    }

    fn make_db_with_beacon_roots_contract() -> State<InMemoryDB> {
        let mut db = State::builder().with_database(InMemoryDB::default()).build();
        let code = Bytecode::new_raw(BEACON_ROOTS_CODE.clone());
        let code_hash = code.hash_slow();
        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            AccountInfo { code: Some(code), code_hash, nonce: 1, ..Default::default() },
        );
        db
    }

    fn test_header(timestamp: u64) -> Header {
        Header {
            number: 1,
            timestamp,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        }
    }

    fn create_legacy_tx() -> Recovered<OpTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(5000),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };
        let envelope = OpTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        Recovered::new_unchecked(envelope, Address::ZERO)
    }

    fn create_deposit_tx(sender: Address) -> Recovered<OpTxEnvelope> {
        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0xdd),
            from: sender,
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Default::default(),
            eth_tx_value: None,
        };
        Recovered::new_unchecked(
            OpTxEnvelope::Deposit(alloy_consensus::Sealed::new_unchecked(deposit, B256::ZERO)),
            sender,
        )
    }

    fn builder_over<DB>(
        spec: Arc<OpChainSpec>,
        db: DB,
        header: Header,
    ) -> PendingStateBuilder<
        impl Evm<HaltReason = OpHaltReason, DB = DB, Tx: FromRecoveredTx<OpTxEnvelope>>,
        Arc<OpChainSpec>,
    >
    where
        DB: Database + DatabaseCommit + AlloyDatabase + StateDB + std::fmt::Debug,
    {
        let evm_config = OpEvmConfig::optimism(Arc::clone(&spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header, body: BlockBody::<OpTxEnvelope>::default() };
        PendingStateBuilder::new(
            spec,
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        )
    }

    fn cached_pending_blocks(
        header: &Header,
        tx_hash: B256,
        executed: &ExecutedPendingTransaction,
        execution_time_us: u128,
    ) -> Arc<PendingBlocks> {
        let mut builder = crate::PendingBlocksBuilder::new();
        builder.with_header(Sealed::new_unchecked(header.clone(), B256::ZERO));
        builder.with_flashblocks([MantleFlashblockPayload {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(OpFlashblockPayloadBase {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: header.number,
                gas_limit: header.gas_limit,
                timestamp: header.timestamp,
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            }),
            diff: OpFlashblockPayloadDelta {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                gas_used: executed.receipt.inner.gas_used,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: MantleFlashblockMetadata {
                block_number: header.number,
                ..MantleFlashblockMetadata::default()
            },
        }]);
        builder.with_receipt(tx_hash, executed.receipt.clone());
        builder.with_transaction_state(tx_hash, executed.state.clone());
        builder.with_transaction_result(tx_hash, executed.result.clone());
        builder.with_execution_time(tx_hash, execution_time_us);
        Arc::new(builder.build().expect("should build cached pending blocks"))
    }

    #[test]
    fn apply_pre_execution_changes_stores_beacon_root_in_eip4788_contract() {
        let header = test_header(POST_SKADI_TIMESTAMP);
        let mut builder =
            builder_over(chain_spec(), make_db_with_beacon_roots_contract(), header.clone());

        let parent_beacon_block_root = B256::from([0xab; 32]);
        builder
            .apply_pre_execution_changes(B256::ZERO, Some(parent_beacon_block_root))
            .expect("apply_pre_execution_changes should succeed");

        let (db, _) = builder.into_db_and_state_overrides();

        // EIP-4788 slot = timestamp % BUFFER_LENGTH + BUFFER_LENGTH.
        let timestamp_idx = header.timestamp % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let root_slot = U256::from(timestamp_idx + BEACON_ROOTS_HISTORY_BUFFER_LENGTH);
        let beacon_account = db
            .cache
            .accounts
            .get(&BEACON_ROOTS_ADDRESS)
            .expect("beacon roots contract should be in cache after commit");
        let storage = &beacon_account
            .account
            .as_ref()
            .expect("beacon roots account should be populated")
            .storage;
        let stored_root = *storage.get(&root_slot).expect("beacon root slot should be written");

        assert_eq!(
            stored_root,
            U256::from_be_bytes(parent_beacon_block_root.0),
            "EIP-4788 should store parent_beacon_block_root at the timestamp-indexed slot"
        );
    }

    #[test]
    fn apply_pre_execution_changes_pre_ecotone_with_no_beacon_root_is_noop() {
        let mut builder = builder_over(
            chain_spec(),
            make_db_with_beacon_roots_contract(),
            test_header(PRE_SKADI_TIMESTAMP),
        );

        builder
            .apply_pre_execution_changes(B256::ZERO, None)
            .expect("should succeed pre-Ecotone with no beacon root");

        let (db, _) = builder.into_db_and_state_overrides();

        let beacon_account = db.cache.accounts.get(&BEACON_ROOTS_ADDRESS);
        let has_storage_writes =
            beacon_account.and_then(|a| a.account.as_ref()).is_some_and(|a| !a.storage.is_empty());
        assert!(!has_storage_writes, "EIP-4788 contract should not be called pre-Ecotone");
    }

    #[test]
    fn cached_execute_transaction_preserves_timing_from_prev_pending_blocks() {
        let spec = chain_spec();
        let header = test_header(POST_SKADI_TIMESTAMP);

        let mut first_builder =
            builder_over(Arc::clone(&spec), funded_sender_db(Address::ZERO), header.clone());
        let tx = create_legacy_tx();
        let tx_hash = tx.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx).expect("transaction execution failed");

        let prev = cached_pending_blocks(&header, tx_hash, &first_result, 1_234);

        let evm_config = OpEvmConfig::optimism(Arc::clone(&spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(InMemoryDB::default(), evm_env);
        let mut second_builder = PendingStateBuilder::new(
            spec,
            evm,
            Block { header, body: BlockBody::<OpTxEnvelope>::default() },
            Some(prev),
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let cached_result = second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached transaction execution failed");

        assert_eq!(cached_result.execution_time_us, Some(1_234));
    }

    #[test]
    fn flashblock_tx_has_nonzero_blob_gas_used_when_jovian_active() {
        let mut db = funded_sender_db(Address::ZERO);
        seed_da_footprint_scalar(&mut db, 100);

        let mut builder = builder_over(chain_spec(), db, test_header(POST_ARSIA_TIMESTAMP));
        let result = builder
            .execute_transaction(0, create_legacy_tx())
            .expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert!(
            blob_gas_used > 0,
            "blob_gas_used should be > 0 for a non-deposit tx once Jovian is active, got {blob_gas_used}"
        );
    }

    #[test]
    fn flashblock_tx_has_zero_blob_gas_used_when_jovian_inactive() {
        let mut builder = builder_over(
            chain_spec(),
            funded_sender_db(Address::ZERO),
            test_header(POST_SKADI_TIMESTAMP),
        );
        let result = builder
            .execute_transaction(0, create_legacy_tx())
            .expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(blob_gas_used, 0, "blob_gas_used should be 0 while Jovian is inactive");
    }

    #[test]
    fn flashblock_deposit_tx_has_zero_blob_gas_used_when_jovian_active() {
        let deposit_sender = address!("0x1234567890123456789012345678901234567890");
        let mut db = funded_sender_db(deposit_sender);
        seed_da_footprint_scalar(&mut db, 100);

        let mut builder = builder_over(chain_spec(), db, test_header(POST_ARSIA_TIMESTAMP));
        let result = builder
            .execute_transaction(0, create_deposit_tx(deposit_sender))
            .expect("deposit execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(blob_gas_used, 0, "deposit transactions are exempt from the DA footprint");
    }

    #[test]
    fn cached_execute_commits_state_so_subsequent_fresh_txs_see_updated_nonce() {
        let spec = chain_spec();
        let sender = Address::ZERO;
        let header = test_header(POST_SKADI_TIMESTAMP);

        let db = State::builder().with_database(funded_sender_db(sender)).build();
        let mut first_builder = builder_over(Arc::clone(&spec), db, header.clone());

        let tx_a = create_legacy_tx();
        let tx_a_hash = tx_a.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx_a).expect("first execution failed");

        let (first_db, _) = first_builder.into_db_and_state_overrides();
        let nonce_after_tx_a = first_db
            .cache
            .accounts
            .get(&sender)
            .and_then(|a| a.account_info())
            .map(|info| info.nonce)
            .expect("sender should be in cache after tx A");
        assert_eq!(nonce_after_tx_a, 1, "fresh execution should increment the nonce to 1");

        let prev = cached_pending_blocks(&header, tx_a_hash, &first_result, 1);

        let evm_config = OpEvmConfig::optimism(Arc::clone(&spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(
            State::builder().with_database(funded_sender_db(sender)).build(),
            evm_env,
        );
        let mut second_builder = PendingStateBuilder::new(
            spec,
            evm,
            Block { header, body: BlockBody::<OpTxEnvelope>::default() },
            Some(prev),
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        second_builder.execute_transaction(0, create_legacy_tx()).expect("cached execution failed");

        let (second_db, _) = second_builder.into_db_and_state_overrides();
        let nonce_after_cached = second_db
            .cache
            .accounts
            .get(&sender)
            .and_then(|a| a.account_info())
            .map(|info| info.nonce)
            .expect("sender should be in cache after cached replay");

        assert_eq!(
            nonce_after_cached, 1,
            "cached replay must commit state so later fresh txs see the advanced nonce"
        );
    }
}
