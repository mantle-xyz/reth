use alloy_consensus::{Eip658Value, Receipt};
use core::fmt;
use op_alloy_consensus::{MantleTxStoredReceipt, OpDepositReceipt, OpTxType};
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use revm::L1BlockInfo;
use revm_primitives::ExecutionResult;

/// Context for building a receipt.
#[derive(Debug)]
pub struct ReceiptBuilderCtx<'a, T> {
    /// Transaction
    pub tx: &'a T,
    /// Result of transaction execution.
    pub result: ExecutionResult,
    /// Cumulative gas used.
    pub cumulative_gas_used: u64,
    /// L1 block information. Since Mantle's token ratio is updated in GasOracles, it's not
    /// possible to retrieve the token ratio through extract_l1_info
    pub l1_block_info: Option<L1BlockInfo>,
}

/// Type that knows how to build a receipt based on execution result.
pub trait OpReceiptBuilder<T>: fmt::Debug + Send + Sync + Unpin + 'static {
    /// Receipt type.
    type Receipt: Send + Sync + Clone + Unpin + 'static;

    /// Builds a receipt given a transaction and the result of the execution.
    ///
    /// Note: this method should return `Err` if the transaction is a deposit transaction. In that
    /// case, the `build_deposit_receipt` method will be called.
    fn build_receipt<'a>(
        &self,
        ctx: ReceiptBuilderCtx<'a, T>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, T>>;

    /// Builds receipt for a deposit transaction.
    fn build_deposit_receipt(&self, inner: OpDepositReceipt) -> Self::Receipt;
}

/// Basic builder for receipts of [`OpTransactionSigned`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BasicOpReceiptBuilder;

impl OpReceiptBuilder<OpTransactionSigned> for BasicOpReceiptBuilder {
    type Receipt = OpReceipt;

    fn build_receipt<'a>(
        &self,
        ctx: ReceiptBuilderCtx<'a, OpTransactionSigned>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, OpTransactionSigned>> {
        match ctx.tx.tx_type() {
            OpTxType::Deposit => Err(ctx),
            ty => {
                let l1_block_info = ctx.l1_block_info.unwrap();
                let receipt = MantleTxStoredReceipt {
                    inner: Receipt {
                        // Success flag was added in `EIP-658: Embedding transaction status code in
                        // receipts`.
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: ctx.cumulative_gas_used,
                        logs: ctx.result.into_logs(),
                    },
                    l1_base_fee: l1_block_info.l1_base_fee.try_into().ok(),
                    l1_fee_overhead: l1_block_info.l1_fee_overhead.map(|v| v.try_into().unwrap()),
                    l1_base_fee_scalar: l1_block_info.l1_base_fee_scalar.try_into().ok(),
                    token_ratio: l1_block_info.token_ratio.map(|v| v.try_into().unwrap()),
                };

                Ok(match ty {
                    OpTxType::Legacy => OpReceipt::Legacy(receipt),
                    OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
                    OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
                    OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
                    OpTxType::Deposit => unreachable!(),
                })
            }
        }
    }

    fn build_deposit_receipt(&self, inner: OpDepositReceipt) -> Self::Receipt {
        OpReceipt::Deposit(inner)
    }
}
