//! Local replay of flashblock transactions into pending state.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_eips::{
    Encodable2718,
    eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS},
    eip4788::BEACON_ROOTS_ADDRESS,
};
use alloy_evm::{
    Database as AlloyDatabase,
    block::{StateDB, SystemCaller},
};
use alloy_primitives::{Address, B256, Bytes, U256};
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
    state::{Account, EvmState},
};

use crate::{ExecutionError, PendingBlocks, StateProcessorError, UnifiedReceiptBuilder};

/// Length of the EIP-4788 beacon roots ring buffer, hardcoded in the contract.
const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;

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

        // `SystemCaller` commits straight to the database; read the slots back so the
        // pending overrides carry them too.
        let number: u64 = self.evm.block().number().saturating_to();
        let timestamp: u64 = self.evm.block().timestamp().saturating_to();
        let mut slots = Vec::new();
        if number != 0 && spec.is_prague_active_at_timestamp(timestamp) {
            let slot = (number - 1) % HISTORY_SERVE_WINDOW as u64;
            slots.push((HISTORY_STORAGE_ADDRESS, U256::from(slot)));
        }
        if number != 0 && spec.is_cancun_active_at_timestamp(timestamp) {
            let timestamp_slot = U256::from(timestamp % BEACON_ROOTS_HISTORY_BUFFER_LENGTH);
            slots.push((BEACON_ROOTS_ADDRESS, timestamp_slot));
            slots.push((
                BEACON_ROOTS_ADDRESS,
                timestamp_slot + U256::from(BEACON_ROOTS_HISTORY_BUFFER_LENGTH),
            ));
        }
        for (address, slot) in slots {
            let value = self
                .evm
                .db_mut()
                .storage(address, slot)
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
            write_storage_override(
                &mut self.state_overrides,
                address,
                [(B256::from(slot), B256::from(value))],
            );
        }

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

        // Mantle's token ratio is not carried in L1 attributes. Read it before
        // execution so an oracle update is charged using the old ratio and
        // subsequent transactions (including later slices) use the new one.
        if self.chain_spec.is_mantle() && !is_deposit {
            self.l1_block_info.token_ratio = self
                .evm
                .db_mut()
                .storage(
                    op_revm::constants::GAS_ORACLE_CONTRACT,
                    op_revm::constants::TOKEN_RATIO_SLOT,
                )
                .map_err(|err| ExecutionError::RpcReceiptBuild(err.to_string()))?;
        }
        self.l1_block_info.clear_tx_l1_cost();

        let da_footprint_used = if self
            .chain_spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to()) &&
            !is_deposit
        {
            Some(self.jovian_da_footprint_estimation(&transaction)?)
        } else {
            None
        };

        let start = Instant::now();
        let transact_result = self.evm.transact(&transaction);
        let elapsed_us = start.elapsed().as_micros();

        match transact_result {
            Ok(ResultAndState { state, result }) => {
                let gas_used = result.tx_gas_used();
                for (addr, acc) in &state {
                    write_account_override(&mut self.state_overrides, *addr, acc);
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
                    // A pending block has no hash, and the two upstream types
                    // spell that differently: this one is a bare `B256`, so
                    // zero is its only "unknown", while the `Transaction` above
                    // has `Option<B256>` and uses `None`. Receipts therefore
                    // report `0x00..0` where transactions report `null` for the
                    // same block. Reconciling them would mean lying in one.
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
                // Only correct while SDM stays disabled.
                let mut op_receipt = OpReceiptBuilder::new(
                    self.receipt_builder.chain_spec(),
                    input,
                    &mut self.l1_block_info,
                    None,
                )
                .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
                .build();

                // Only non-deposit transactions after Jovian carry `blob_gas_used`, as in
                // `OpReceiptBuilder`.
                if let Some(da_footprint_used) = da_footprint_used {
                    op_receipt.inner.blob_gas_used = Some(da_footprint_used);
                }
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

/// Folds one account's post-transaction state into the pending overrides.
fn write_account_override(overrides: &mut StateOverride, address: Address, account: &Account) {
    let entry = overrides.entry(address).or_default();
    if account.is_selfdestructed() {
        // revm zeroes only the balance here; nonce, code and storage are still
        // the pre-destruction values and must not reach the overrides.
        entry.balance = Some(U256::ZERO);
        entry.nonce = Some(0);
        entry.code = Some(Bytes::new());
        entry.state = Some(Default::default());
        entry.state_diff = None;
        return;
    }
    entry.balance = Some(account.info.balance);
    entry.nonce = Some(account.info.nonce);
    // Overrides must carry the unpadded original, for the reason
    // `PendingBlocks::get_code` gives.
    let code = account.info.code.clone().map(|code| code.original_bytes());
    // After a destruction the explicit empty code must survive: dropping it
    // would let the replaced storage sit under the pre-destruction code.
    entry.code = match code {
        Some(code) if !code.is_empty() => Some(code),
        _ if entry.state.is_some() => Some(Bytes::new()),
        code => code,
    };
    write_storage_override(
        overrides,
        address,
        account
            .storage
            .iter()
            .map(|(&key, slot)| (B256::from(key), B256::from(slot.present_value))),
    );
}

/// Writes storage slots into the pending overrides, touching nothing else.
fn write_storage_override(
    overrides: &mut StateOverride,
    address: Address,
    slots: impl IntoIterator<Item = (B256, B256)>,
) {
    let entry = overrides.entry(address).or_default();
    // A storage replacement must stay the only storage field: alloy-evm rejects
    // an account carrying both `state` and `state_diff`.
    match entry.state.as_mut() {
        Some(state) => state.extend(slots),
        None => entry.state_diff.get_or_insert_with(Default::default).extend(slots),
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Sealed, Signed, transaction::Recovered};
    use alloy_eips::{
        eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
        eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    };
    use alloy_op_hardforks::{MANTLE_MAINNET_ARSIA_TIMESTAMP, MANTLE_MAINNET_SKADI_TIMESTAMP};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256, address, bytes, uint};
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
        database::{EmptyDB, InMemoryDB},
        state::{Account, AccountInfo, Bytecode, EvmStorageSlot},
    };

    use super::*;

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

    fn legacy_tx(
        nonce: u64,
        to: TxKind,
        gas_limit: u64,
        value: U256,
        input: Bytes,
    ) -> Recovered<OpTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(5000),
            nonce,
            gas_price: 1_000_000_000,
            gas_limit,
            to,
            value,
            input,
        };
        let envelope = OpTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::ZERO,
        ));
        Recovered::new_unchecked(envelope, Address::ZERO)
    }

    /// Init code of a factory: CREATEs a child, then CALLs it. The child's init
    /// code (`PUSH1 7 PUSH1 1 SSTORE`, then return runtime `30ff`) writes slot 1
    /// and deploys `ADDRESS SELFDESTRUCT`, so the child destroys itself in the
    /// transaction that created it.
    const FACTORY_INIT_CODE: Bytes = bytes!(
        "6f60076001556130ff6000526002601ef3600052601060106000f060006000600060006000855af1505000"
    );
    const DESTROYED_RUNTIME: Bytes = bytes!("30ff");

    fn factory_child() -> Address {
        Address::ZERO.create(0).create(1)
    }

    fn create_factory_tx() -> Recovered<OpTxEnvelope> {
        legacy_tx(0, TxKind::Create, 300_000, U256::ZERO, FACTORY_INIT_CODE)
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
            eth_value: U256::ZERO,
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
                post_exec_tx: None,
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
    fn an_account_created_and_destroyed_in_one_transaction_is_cleared() {
        let mut builder = builder_over(
            chain_spec(),
            funded_sender_db(Address::ZERO),
            test_header(POST_SKADI_TIMESTAMP),
        );
        let executed = builder.execute_transaction(0, create_factory_tx()).expect("execution");
        let child = factory_child();

        assert!(executed.result.is_success());
        let account = &executed.state[&child];
        assert!(account.is_selfdestructed());
        assert!(account.is_created_locally());
        assert_eq!(account.info.nonce, 1);
        assert_eq!(account.info.code.as_ref().unwrap().original_bytes(), DESTROYED_RUNTIME);
        assert_eq!(account.storage[&U256::from(1)].present_value, U256::from(7));

        let (_, overrides) = builder.into_db_and_state_overrides();
        let entry = &overrides[&child];
        assert_eq!(entry.balance, Some(U256::ZERO));
        assert_eq!(entry.nonce, Some(0));
        assert_eq!(entry.code, Some(Bytes::new()));
        assert_eq!(entry.state, Some(Default::default()));
        assert_eq!(entry.state_diff, None);
    }

    #[test]
    fn an_account_written_after_its_destruction_keeps_one_storage_field() {
        let db = State::builder().with_database(funded_sender_db(Address::ZERO)).build();
        let mut builder = builder_over(chain_spec(), db, test_header(POST_SKADI_TIMESTAMP));
        let child = factory_child();
        builder.execute_transaction(0, create_factory_tx()).expect("create");
        builder
            .execute_transaction(
                1,
                legacy_tx(1, TxKind::Call(child), 21_000, U256::from(1), Bytes::new()),
            )
            .expect("transfer");

        let (_, overrides) = builder.into_db_and_state_overrides();
        let entry = &overrides[&child];
        assert_eq!(entry.balance, Some(U256::from(1)));
        assert_eq!(entry.nonce, Some(0));
        assert_eq!(entry.code, Some(Bytes::new()));
        assert_eq!(entry.state, Some(Default::default()));
        assert_eq!(entry.state_diff, None);

        let mut db = State::builder().with_database(EmptyDB::default()).build();
        alloy_evm::overrides::apply_state_overrides(overrides, &mut db)
            .expect("overrides must be accepted by the evm");
    }

    #[test]
    fn storage_written_after_a_destruction_extends_the_replacement() {
        let child = factory_child();
        let mut overrides = StateOverride::default();

        let mut destroyed = Account::from(AccountInfo { nonce: 1, ..Default::default() });
        destroyed.mark_touch();
        destroyed.mark_created_locally();
        destroyed.mark_selfdestructed_locally();
        destroyed.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(7), Default::default()),
        );
        write_account_override(&mut overrides, child, &destroyed);

        let mut live = Account::from(AccountInfo { nonce: 1, code: None, ..Default::default() });
        live.storage.insert(
            U256::from(2),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), Default::default()),
        );
        write_account_override(&mut overrides, child, &live);

        let entry = &overrides[&child];
        let expected = [(B256::from(U256::from(2)), B256::from(U256::from(9)))];
        assert_eq!(entry.state, Some(expected.into_iter().collect()));
        assert_eq!(entry.state_diff, None);
        assert_eq!(entry.code, Some(Bytes::new()));
        assert_eq!(entry.nonce, Some(1));
    }

    /// EIP-6780: `SELFDESTRUCT` outside the creating transaction only moves the balance; code,
    /// nonce and storage stay.
    #[test]
    fn a_preexisting_contract_that_selfdestructs_keeps_its_code_and_nonce() {
        let y = address!("0x000000000000000000000000000000000000dead");
        let mut db = funded_sender_db(Address::ZERO);
        let code = Bytecode::new_raw(bytes!("6000ff"));
        db.insert_account_info(
            y,
            AccountInfo {
                balance: U256::from(5),
                nonce: 1,
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
        db.insert_account_storage(y, U256::from(1), U256::from(7)).unwrap();
        let mut builder = builder_over(chain_spec(), db, test_header(POST_SKADI_TIMESTAMP));
        let executed = builder
            .execute_transaction(
                0,
                legacy_tx(0, TxKind::Call(y), 100_000, U256::ZERO, Bytes::new()),
            )
            .expect("execution");
        assert!(!executed.state[&y].is_selfdestructed());

        let (_, overrides) = builder.into_db_and_state_overrides();
        let entry = &overrides[&y];
        assert_eq!(entry.balance, Some(U256::ZERO));
        assert_eq!(entry.nonce, Some(1));
        assert_eq!(entry.code, Some(bytes!("6000ff")));
        assert_eq!(entry.state, None);
    }

    #[test]
    fn beacon_root_system_call_reaches_the_overrides() {
        let header = test_header(POST_SKADI_TIMESTAMP);
        let ts = header.timestamp;
        let mut builder = builder_over(chain_spec(), make_db_with_beacon_roots_contract(), header);
        let root = B256::from([0xab; 32]);
        builder.apply_pre_execution_changes(B256::ZERO, Some(root)).expect("system calls");

        let (_, overrides) = builder.into_db_and_state_overrides();
        let entry = &overrides[&BEACON_ROOTS_ADDRESS];
        let slot = ts % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let diff = entry.state_diff.as_ref().expect("state_diff");
        assert_eq!(diff[&B256::from(U256::from(slot))], B256::from(U256::from(ts)));
        assert_eq!(diff[&B256::from(U256::from(slot + BEACON_ROOTS_HISTORY_BUFFER_LENGTH))], root);
        assert_eq!(entry.balance, None);
        assert_eq!(entry.nonce, None);
        assert_eq!(entry.code, None);
    }

    #[test]
    fn block_hash_system_call_reaches_the_overrides() {
        let mut db = State::builder().with_database(InMemoryDB::default()).build();
        let code = Bytecode::new_raw(HISTORY_STORAGE_CODE.clone());
        db.insert_account(
            HISTORY_STORAGE_ADDRESS,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                nonce: 1,
                ..Default::default()
            },
        );
        let mut header = test_header(POST_SKADI_TIMESTAMP);
        header.number = 10_000;
        let mut builder = builder_over(chain_spec(), db, header);
        let parent_hash = B256::from([0xcd; 32]);
        builder
            .apply_pre_execution_changes(parent_hash, Some(B256::from([0xab; 32])))
            .expect("system calls");

        let (_, overrides) = builder.into_db_and_state_overrides();
        let entry = &overrides[&HISTORY_STORAGE_ADDRESS];
        let slot = B256::from(U256::from(9_999 % HISTORY_SERVE_WINDOW as u64));
        assert_eq!(entry.state_diff.as_ref().expect("state_diff")[&slot], parent_hash);
        assert_eq!(entry.balance, None);
        assert_eq!(entry.nonce, None);
        assert_eq!(entry.code, None);
    }

    #[test]
    fn system_call_slots_follow_a_storage_replacement() {
        let address = Address::repeat_byte(0x11);
        let mut overrides = StateOverride::default();
        overrides.entry(address).or_default().state = Some(Default::default());

        write_storage_override(
            &mut overrides,
            address,
            [(B256::repeat_byte(1), B256::repeat_byte(2))],
        );

        let entry = &overrides[&address];
        assert_eq!(entry.state.as_ref().unwrap()[&B256::repeat_byte(1)], B256::repeat_byte(2));
        assert_eq!(entry.state_diff, None);
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
    fn flashblock_tx_has_no_blob_gas_used_when_jovian_inactive() {
        let mut builder = builder_over(
            chain_spec(),
            funded_sender_db(Address::ZERO),
            test_header(POST_SKADI_TIMESTAMP),
        );
        let result = builder
            .execute_transaction(0, create_legacy_tx())
            .expect("transaction execution failed");

        assert_eq!(
            result.receipt.inner.blob_gas_used, None,
            "blob_gas_used must be None while Jovian is inactive"
        );
    }

    #[test]
    fn flashblock_deposit_tx_has_no_blob_gas_used_when_jovian_active() {
        let deposit_sender = address!("0x1234567890123456789012345678901234567890");
        let mut db = funded_sender_db(deposit_sender);
        seed_da_footprint_scalar(&mut db, 100);

        let mut builder = builder_over(chain_spec(), db, test_header(POST_ARSIA_TIMESTAMP));
        let result = builder
            .execute_transaction(0, create_deposit_tx(deposit_sender))
            .expect("deposit execution failed");

        assert_eq!(
            result.receipt.inner.blob_gas_used, None,
            "deposit receipts do not carry blob_gas_used, matching OpReceiptBuilder"
        );
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
