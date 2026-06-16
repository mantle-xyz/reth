//! Builder apply interface.
//!
//! [`apply_preconf_tx`] is a thin wrapper around reth's
//! [`BlockBuilder::execute_transaction_with_result_closure`] that captures the
//! EVM execution result and converts it into a [`PreconfReceipt`] suitable
//! for delivery to the RPC client.
//!
//! The receipt-building logic itself lives in [`build_receipt`], a pure
//! function over [`ExecutionResult`]. End-to-end tests with a real
//! `OpBlockBuilder` are deferred to later integration tests; this module
//! unit-tests `build_receipt` directly against synthetic execution results.
//!
//! [`BlockBuilder::execute_transaction_with_result_closure`]: reth_evm::execute::BlockBuilder::execute_transaction_with_result_closure

use crate::types::{PreconfError, PreconfReceipt};
use alloy_evm::block::TxResult;
use alloy_primitives::{Bytes, TxHash};
use reth_evm::execute::{BlockBuilder, ExecutorTx};
use reth_revm::context::result::ExecutionResult;

/// Apply a single transaction via the supplied [`BlockBuilder`] and produce a
/// [`PreconfReceipt`] from the EVM execution result.
///
/// The supplied transaction is committed to the builder's running state on
/// success (`BlockBuilder::execute_transaction_with_result_closure` always
/// commits — see reth-evm `crates/evm/evm/src/execute.rs:334-344`).
///
/// Errors:
/// - [`PreconfError::BuilderRejected`] — the EVM rejected the transaction
///   (signature / nonce / balance / etc.). State is left unchanged.
/// - [`PreconfError::Internal`] — the closure was somehow never invoked. This
///   should not happen with a correctly-implemented `BlockBuilder`, but the
///   trait signature does not guarantee single-shot invocation, so we
///   defensively surface this as `Internal` rather than `panic!`.
pub fn apply_preconf_tx<B>(
    builder: &mut B,
    tx: impl ExecutorTx<B::Executor>,
    tx_hash: TxHash,
    block_height: u64,
) -> Result<PreconfReceipt, PreconfError>
where
    B: BlockBuilder,
{
    let mut captured: Option<PreconfReceipt> = None;
    builder
        .execute_transaction_with_result_closure(tx, |res| {
            let ras = res.result();
            captured = Some(build_receipt(tx_hash, block_height, &ras.result));
        })
        .map_err(|e| PreconfError::BuilderRejected(e.to_string()))?;
    captured.ok_or_else(|| PreconfError::Internal("BlockBuilder closure not invoked".into()))
}

/// Convert a revm [`ExecutionResult`] into a [`PreconfReceipt`].
///
/// Pure function — no side effects. Generic over the halt-reason type so
/// callers can pass either the stock revm `HaltReason` or the OP-stack
/// `OpHaltReason`; we format the reason via `Debug` (matching the wire-layer
/// `reason: String` shape exposed by `PreconfTxEvent`).
pub fn build_receipt<H: core::fmt::Debug>(
    tx_hash: TxHash,
    block_height: u64,
    result: &ExecutionResult<H>,
) -> PreconfReceipt {
    match result {
        ExecutionResult::Success { gas, logs, .. } => PreconfReceipt {
            tx_hash,
            block_height,
            status: true,
            logs: logs.clone(),
            gas_used: gas.tx_gas_used(),
            reason: String::new(),
            revert_data: Bytes::new(),
        },
        ExecutionResult::Revert { gas, output, .. } => PreconfReceipt {
            tx_hash,
            block_height,
            status: false,
            // Revert path does emit logs in this revm version, but for the
            // preconf receipt we mirror EIP-658: failed receipts carry no
            // logs. (This matches op-geth's behavior where the receipt's
            // Status=Failed path drops logs.)
            logs: Vec::new(),
            gas_used: gas.tx_gas_used(),
            reason: "execution reverted".into(),
            revert_data: output.clone(),
        },
        ExecutionResult::Halt { reason, gas, .. } => PreconfReceipt {
            tx_hash,
            block_height,
            status: false,
            logs: Vec::new(),
            gas_used: gas.tx_gas_used(),
            reason: format!("{reason:?}"),
            revert_data: Bytes::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, Log, LogData, address, b256, hex, keccak256};
    use reth_revm::context::result::{HaltReason, OutOfGasError, Output, ResultGas, SuccessReason};

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn gas(used: u64) -> ResultGas {
        // total_gas_spent = used, no refund, no floor —
        // `tx_gas_used()` returns `used`.
        ResultGas::default().with_total_gas_spent(used)
    }

    fn sample_log() -> Log {
        Log {
            address: address!("00000000000000000000000000000000000000aa"),
            data: LogData::new_unchecked(
                vec![b256!("0000000000000000000000000000000000000000000000000000000000000001")],
                Bytes::from_static(b"hello"),
            ),
        }
    }

    #[test]
    fn build_receipt_success_populates_logs_and_status_true() {
        let log = sample_log();
        let result: ExecutionResult<HaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: gas(21_000),
            logs: vec![log.clone()],
            output: Output::Call(Bytes::new()),
        };
        let r = build_receipt(h(1), 42, &result);
        assert_eq!(r.tx_hash, h(1));
        assert_eq!(r.block_height, 42);
        assert!(r.status);
        assert_eq!(r.gas_used, 21_000);
        assert_eq!(r.logs, vec![log]);
        assert!(r.reason.is_empty());
        assert!(r.revert_data.is_empty());
    }

    #[test]
    fn build_receipt_revert_carries_revert_data_and_status_false() {
        let revert_payload = Bytes::from_static(&hex!("08c379a0"));
        let result: ExecutionResult<HaltReason> = ExecutionResult::Revert {
            gas: gas(30_000),
            logs: vec![],
            output: revert_payload.clone(),
        };
        let r = build_receipt(h(2), 100, &result);
        assert_eq!(r.tx_hash, h(2));
        assert_eq!(r.block_height, 100);
        assert!(!r.status);
        assert_eq!(r.gas_used, 30_000);
        assert!(r.logs.is_empty());
        assert_eq!(r.reason, "execution reverted");
        assert_eq!(r.revert_data, revert_payload);
    }

    #[test]
    fn build_receipt_halt_formats_reason_and_status_false() {
        let result: ExecutionResult<HaltReason> = ExecutionResult::Halt {
            reason: HaltReason::OutOfGas(OutOfGasError::Basic),
            gas: gas(50_000),
            logs: vec![],
        };
        let r = build_receipt(h(3), 200, &result);
        assert_eq!(r.tx_hash, h(3));
        assert_eq!(r.block_height, 200);
        assert!(!r.status);
        assert_eq!(r.gas_used, 50_000);
        assert!(r.logs.is_empty());
        // Halt reason rendered via Debug — exact format isn't fixed, but it
        // must be non-empty and mention the variant.
        assert!(!r.reason.is_empty());
        assert!(r.reason.contains("OutOfGas") || r.reason.contains("Basic"));
        assert!(r.revert_data.is_empty());
    }

    #[test]
    fn build_receipt_success_with_no_logs_yields_empty_log_vec() {
        let result: ExecutionResult<HaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: gas(21_000),
            logs: vec![],
            output: Output::Call(Bytes::new()),
        };
        let r = build_receipt(h(4), 1, &result);
        assert!(r.status);
        assert!(r.logs.is_empty());
    }

    #[test]
    fn build_receipt_preserves_tx_hash_and_block_height_bytewise() {
        // Make sure no field swap shenanigans — TxHash and block_height
        // must propagate verbatim.
        let want_hash = TxHash::from(keccak256(b"some-tx-bytes"));
        let want_height: u64 = 0xdead_beef_u64;
        let result: ExecutionResult<HaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: gas(1),
            logs: vec![],
            output: Output::Call(Bytes::new()),
        };
        let r = build_receipt(want_hash, want_height, &result);
        assert_eq!(r.tx_hash, want_hash);
        assert_eq!(r.block_height, want_height);
        let other_hash: B256 =
            b256!("0000000000000000000000000000000000000000000000000000000000000001");
        assert_ne!(r.tx_hash, other_hash);
    }
}
