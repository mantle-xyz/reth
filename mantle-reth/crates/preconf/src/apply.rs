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
use alloy_sol_types::{Revert, SolError};
use core::any::Any;
use op_revm::OpHaltReason;
use reth_evm::execute::{BlockBuilder, ExecutorTx};
use reth_revm::context::result::{ExecutionResult, HaltReason, OutOfGasError};

/// Apply a single transaction via the supplied [`BlockBuilder`] and produce a
/// [`PreconfReceipt`] from the EVM execution result.
///
/// The supplied transaction is committed to the builder's running state on
/// success (`BlockBuilder::execute_transaction_with_result_closure` always
/// commits — see reth-evm `crates/evm/evm/src/execute.rs:334-344`).
///
/// Errors:
/// - [`PreconfError::BuilderRejected`] — the EVM rejected the transaction (signature / nonce /
///   balance / etc.). State is left unchanged.
/// - [`PreconfError::Internal`] — the closure was somehow never invoked. This should not happen
///   with a correctly-implemented `BlockBuilder`, but the trait signature does not guarantee
///   single-shot invocation, so we defensively surface this as `Internal` rather than `panic!`.
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
    let exec_result = builder.execute_transaction_with_result_closure(tx, |res| {
        let ras = res.result();
        captured = Some(build_receipt(tx_hash, block_height, &ras.result));
    });
    interpret_apply_result(exec_result.map(|_| ()).map_err(|e| e.to_string()), captured)
}

/// Pure interpretation of the `(execute_result, captured_receipt)` pair —
/// the untestable-directly `apply_preconf_tx` wrapper's decision matrix
/// extracted so all three outcomes get unit coverage without a full
/// `BlockBuilder` mock.
///
/// | execute result       | captured   | outcome                                        |
/// |----------------------|------------|------------------------------------------------|
/// | `Ok(())`             | `Some(r)`  | `Ok(r)` — happy path                           |
/// | `Ok(())`             | `None`     | `Err(Internal)` — upstream trait contract bug  |
/// | `Err(e)`             | anything   | `Err(BuilderRejected(e))` — EVM rejected tx    |
fn interpret_apply_result(
    exec_result: Result<(), String>,
    captured: Option<PreconfReceipt>,
) -> Result<PreconfReceipt, PreconfError> {
    match exec_result {
        Ok(()) => captured
            .ok_or_else(|| PreconfError::Internal("BlockBuilder closure not invoked".into())),
        Err(e) => Err(PreconfError::BuilderRejected(e)),
    }
}

/// Convert a revm [`ExecutionResult`] into a [`PreconfReceipt`].
///
/// Pure function — no side effects. Generic over the halt-reason type so
/// callers can pass either the stock revm `HaltReason` or the OP-stack
/// `OpHaltReason`; the halt reason is mapped to op-geth-compatible text via
/// [`GethHaltReason`] (e.g. out-of-gas → "out of gas") when the concrete type
/// is recognized, else rendered via `Debug`.
pub fn build_receipt<H: core::fmt::Debug + 'static>(
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
        ExecutionResult::Revert { gas, output, .. } => {
            // Decode `Error(string)` → "execution reverted: <msg>", matching
            // op-geth's `abi.UnpackRevert`. Panic/custom/raw reverts miss the
            // selector → `Err` → bare "execution reverted".
            let reason = match Revert::abi_decode(output.as_ref()) {
                Ok(r) => format!("execution reverted: {}", r.reason),
                Err(_) => "execution reverted".to_string(),
            };
            PreconfReceipt {
                tx_hash,
                block_height,
                status: false,
                // revm surfaces "logs emitted before revert" as an
                // observability field, but the EVM's state rollback erases
                // them from statedb. Op-geth reads receipts from statedb
                // (`state_processor.go:281`) so its Failed-status receipts
                // observe empty logs naturally. To keep our preconf receipt
                // byte-equal with the eventually-sealed receipt, we drop
                // revm's pre-revert log snapshot here.
                logs: Vec::new(),
                gas_used: gas.tx_gas_used(),
                reason,
                revert_data: output.clone(),
            }
        }
        ExecutionResult::Halt { reason, gas, .. } => {
            // Map to op-geth's vm-error text when the concrete halt type is
            // known (production: OpHaltReason). build_receipt is generic over
            // the EVM's abstract HaltReason, so we downcast rather than thread
            // a new trait bound through the (deliberately generic) builder
            // path; unknown halt types keep their opaque Debug form.
            let any = reason as &dyn Any;
            let reason = any
                .downcast_ref::<OpHaltReason>()
                .map(|h| h.geth_halt_reason())
                .or_else(|| any.downcast_ref::<HaltReason>().map(|h| h.geth_halt_reason()))
                .unwrap_or_else(|| format!("{reason:?}"));
            PreconfReceipt {
                tx_hash,
                block_height,
                status: false,
                logs: Vec::new(),
                gas_used: gas.tx_gas_used(),
                reason,
                revert_data: Bytes::new(),
            }
        }
    }
}

/// Renders a halt reason as an op-geth-compatible `reason` string.
///
/// Mirrors revm-inspectors' callTracer text (`tracing::utils::fmt_error_msg`),
/// which is how op-geth's `vm.Err*` messages read — so a preconf `reason` for
/// e.g. an out-of-gas halt is "out of gas", matching op-geth. Variants that
/// `fmt_error_msg` doesn't special-case fall back to `Debug`, exactly as the
/// callTracer does.
pub trait GethHaltReason {
    /// The op-geth-style reason string for this halt.
    fn geth_halt_reason(&self) -> String;
}

impl GethHaltReason for HaltReason {
    fn geth_halt_reason(&self) -> String {
        match self {
            Self::OutOfGas(OutOfGasError::Basic | OutOfGasError::Precompile) => "out of gas",
            Self::OutOfGas(OutOfGasError::Memory) => "out of gas: out of memory",
            Self::OutOfGas(OutOfGasError::MemoryLimit) => "out of gas: reach memory limit",
            Self::OutOfGas(OutOfGasError::InvalidOperand) => "out of gas: invalid operand",
            Self::OutOfGas(OutOfGasError::ReentrancySentry) => {
                "out of gas: not enough gas for reentrancy sentry"
            }
            Self::OpcodeNotFound => "invalid opcode",
            Self::InvalidFEOpcode => "invalid opcode: INVALID",
            Self::InvalidJump => "invalid jump destination",
            Self::StackOverflow => "Out of stack",
            Self::PrecompileError => "precompiled failed",
            Self::OutOfFunds => "insufficient balance for transfer",
            // Not special-cased by op-geth's callTracer either → opaque Debug.
            other => return format!("{other:?}"),
        }
        .to_string()
    }
}

impl GethHaltReason for OpHaltReason {
    fn geth_halt_reason(&self) -> String {
        match self {
            Self::Base(inner) => inner.geth_halt_reason(),
            // OP-specific; no op-geth vm-error equivalent — keep the Debug form.
            Self::FailedDeposit => format!("{self:?}"),
        }
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
        // Bare `Error(string)` selector with no body is NOT decodable →
        // reason falls back to the plain string (matches op-geth's behavior
        // for reverts it can't unpack).
        assert_eq!(r.reason, "execution reverted");
        assert_eq!(r.revert_data, revert_payload);
    }

    #[test]
    fn build_receipt_revert_decodes_error_string_reason() {
        // A well-formed `Error(string)` payload (as emitted by
        // `require(false, "…")`) must surface as `execution reverted: <msg>`,
        // byte-for-byte matching op-geth's `abi.UnpackRevert` output.
        let payload = Bytes::from(Revert { reason: "allowance insufficient".into() }.abi_encode());
        let result: ExecutionResult<HaltReason> =
            ExecutionResult::Revert { gas: gas(30_000), logs: vec![], output: payload.clone() };
        let r = build_receipt(h(4), 101, &result);
        assert!(!r.status);
        assert_eq!(r.reason, "execution reverted: allowance insufficient");
        // Raw ABI bytes are still preserved verbatim for downstream consumers.
        assert_eq!(r.revert_data, payload);
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
        // OOG halt reason is mapped to op-geth's text.
        assert_eq!(r.reason, "out of gas");
        assert!(r.revert_data.is_empty());
    }

    #[test]
    fn geth_halt_reason_maps_variants_like_op_geth() {
        assert_eq!(HaltReason::OutOfGas(OutOfGasError::Basic).geth_halt_reason(), "out of gas");
        assert_eq!(
            HaltReason::OutOfGas(OutOfGasError::Memory).geth_halt_reason(),
            "out of gas: out of memory"
        );
        assert_eq!(HaltReason::OpcodeNotFound.geth_halt_reason(), "invalid opcode");
        assert_eq!(HaltReason::InvalidJump.geth_halt_reason(), "invalid jump destination");
        assert_eq!(HaltReason::PrecompileError.geth_halt_reason(), "precompiled failed");
        // Not special-cased → opaque Debug, same as op-geth's callTracer.
        assert_eq!(HaltReason::StackUnderflow.geth_halt_reason(), "StackUnderflow");
        // OP wrapper: Base delegates; FailedDeposit keeps its Debug form.
        assert_eq!(
            OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Basic)).geth_halt_reason(),
            "out of gas"
        );
        assert_eq!(OpHaltReason::FailedDeposit.geth_halt_reason(), "FailedDeposit");
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

    /// EIP-658 semantics: a failed receipt (`status = false`) carries **no
    /// logs**, even if the EVM's `ExecutionResult::Revert` variant surfaces
    /// logs. `build_receipt` must strip them. Op-geth does the same at the
    /// `Status=Failed` path — dropping this would break receipt byte-equality
    /// with a normally-sealed receipt for the same tx.
    #[test]
    fn build_receipt_revert_strips_logs_even_when_present() {
        let log = sample_log();
        let result: ExecutionResult<HaltReason> = ExecutionResult::Revert {
            gas: gas(30_000),
            logs: vec![log],
            output: Bytes::from_static(&hex!("08c379a0")),
        };
        let r = build_receipt(h(2), 100, &result);
        assert!(r.logs.is_empty(), "EIP-658 requires failed receipts drop logs; got {:?}", r.logs);
        assert!(!r.status);
    }

    /// `build_receipt` is generic over the halt-reason type `H`. Stock revm
    /// callers use `HaltReason`; op-stack callers use `OpHaltReason` (which
    /// adds the `FailedDeposit` variant on top of `Base(HaltReason)`). Lock
    /// the op-stack instantiation so a future breaking change to
    /// `OpHaltReason` (renaming `FailedDeposit`, removing `Debug`, etc.)
    /// gets caught at build time instead of at devnet.
    #[test]
    fn build_receipt_generic_over_op_halt_reason() {
        use op_revm::OpHaltReason;
        let result: ExecutionResult<OpHaltReason> = ExecutionResult::Halt {
            reason: OpHaltReason::FailedDeposit,
            gas: gas(50_000),
            logs: vec![],
        };
        let r = build_receipt(h(9), 1, &result);
        assert!(!r.status);
        assert!(
            r.reason.contains("FailedDeposit"),
            "expected 'FailedDeposit' in reason, got {}",
            r.reason
        );
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

    fn sample_receipt() -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: h(7),
            block_height: 1,
            status: true,
            logs: vec![],
            gas_used: 21_000,
            reason: String::new(),
            revert_data: Bytes::new(),
        }
    }

    /// Happy path: `execute_transaction_with_result_closure` returned `Ok`
    /// AND the closure ran (captured is Some) → interpret returns the
    /// captured receipt verbatim.
    #[test]
    fn interpret_apply_result_ok_with_captured_returns_receipt() {
        let receipt = sample_receipt();
        let out = interpret_apply_result(Ok(()), Some(receipt.clone()));
        assert_eq!(out, Ok(receipt));
    }

    /// Upstream trait bug: reth returned `Ok` but never invoked the closure.
    /// interpret must surface this as `PreconfError::Internal` with an
    /// actionable message pointing to the upstream contract.
    #[test]
    fn interpret_apply_result_ok_without_captured_returns_internal() {
        let out = interpret_apply_result(Ok(()), None);
        match out {
            Err(PreconfError::Internal(msg)) => {
                assert_eq!(msg, "BlockBuilder closure not invoked");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// EVM rejected the tx (e.g. nonce mismatch, insufficient balance). The
    /// error string from reth is passed through as `BuilderRejected(text)`
    /// so client-facing error messages carry the concrete cause.
    #[test]
    fn interpret_apply_result_err_returns_builder_rejected_with_message() {
        let out = interpret_apply_result(Err("nonce mismatch".to_string()), None);
        match out {
            Err(PreconfError::BuilderRejected(msg)) => {
                assert_eq!(msg, "nonce mismatch");
            }
            other => panic!("expected BuilderRejected, got {other:?}"),
        }
    }

    /// The `Err` branch shadows any captured receipt — even if the closure
    /// happened to have written to `captured` before the executor errored
    /// out, we must NOT return that partial receipt as a success. Locks
    /// the precedence of the two branches in `interpret_apply_result`.
    #[test]
    fn interpret_apply_result_err_takes_precedence_over_captured() {
        let receipt = sample_receipt();
        let out = interpret_apply_result(Err("post-capture failure".to_string()), Some(receipt));
        assert!(
            matches!(out, Err(PreconfError::BuilderRejected(_))),
            "Err path must ignore captured receipt, got {out:?}"
        );
    }

    /// R7 D — revert_data's first 4 bytes are the ABI selector. The
    /// canonical `Error(string)` selector is `0x08c379a0` (see solc
    /// docs / EIP-838). SDKs downstream (op-geth compat) unpack via
    /// this selector; ensuring build_receipt preserves the selector
    /// prefix intact is a byte-level contract, not a doc.
    #[test]
    fn build_receipt_revert_data_preserves_error_selector() {
        // Full `Error("boom")` ABI-encoded payload:
        //   selector (4)   = 0x08c379a0
        //   offset  (32)   = 0x00..20
        //   length  (32)   = 4
        //   data    (32)   = "boom" padded
        let payload = Bytes::from_static(&hex!(
            "08c379a0\
             0000000000000000000000000000000000000000000000000000000000000020\
             0000000000000000000000000000000000000000000000000000000000000004\
             626f6f6d00000000000000000000000000000000000000000000000000000000"
        ));
        let result: ExecutionResult<HaltReason> =
            ExecutionResult::Revert { gas: gas(30_000), logs: vec![], output: payload.clone() };
        let r = build_receipt(h(2), 100, &result);

        // Full payload preserved (structural check, not just length).
        assert_eq!(r.revert_data, payload);

        // 4-byte selector at the head is exactly Error(string).
        let selector = &r.revert_data[..4];
        assert_eq!(
            selector,
            &[0x08, 0xc3, 0x79, 0xa0],
            "revert_data must preserve the `Error(string)` selector prefix"
        );
    }

    /// R7 D — build_receipt's `Halt` branch renders `HaltReason` via
    /// its `Debug` impl. Existing coverage only checks
    /// `OutOfGas::Basic`; a Debug-format shift on other variants would
    /// silently degrade log messages. Spot-check a few common variants
    /// so upstream Debug drift trips a test.
    #[test]
    fn build_receipt_halt_reason_covers_common_variants() {
        for reason in [
            HaltReason::OpcodeNotFound,
            HaltReason::InvalidJump,
            HaltReason::StackOverflow,
            HaltReason::StackUnderflow,
            HaltReason::PrecompileError,
        ] {
            let result: ExecutionResult<HaltReason> =
                ExecutionResult::Halt { reason: reason.clone(), gas: gas(50_000), logs: vec![] };
            let r = build_receipt(h(3), 200, &result);
            assert!(!r.status, "halt ⇒ status false");
            assert!(!r.reason.is_empty(), "halt reason string must be non-empty for {reason:?}");
            assert!(r.revert_data.is_empty(), "halt has no revert data");
            assert_eq!(r.gas_used, 50_000);
        }
    }
}
