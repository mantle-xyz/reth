//! Error types for the flashblocks state processor.

use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::{Address, B256};
use thiserror::Error;

/// Errors related to flashblock protocol sequencing and ordering.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProtocolError {
    /// Invalid flashblock sequence or ordering.
    #[error("invalid flashblock sequence: flashblocks must be processed in order")]
    InvalidSequence,

    /// First flashblock in a sequence must contain a base payload.
    #[error("missing base: first flashblock in sequence must contain a base payload")]
    MissingBase,

    /// Cannot build from an empty flashblocks collection.
    #[error("empty flashblocks: cannot build state from zero flashblocks")]
    EmptyFlashblocks,
}

/// Errors related to state provider and infrastructure operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProviderError {
    /// Missing canonical header for a given block number.
    #[error(
        "missing canonical header for block {block_number}. This can be ignored if the node has recently restarted, restored from a snapshot or is still syncing."
    )]
    MissingCanonicalHeader {
        /// The block number for which the header is missing.
        block_number: u64,
    },

    /// State provider error with context.
    #[error("state provider error: {0}")]
    StateProvider(String),
}

/// Errors related to transaction execution and processing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ExecutionError {
    /// Transaction execution failed.
    #[error("transaction execution failed for tx {tx_hash} from sender {sender}: {reason}")]
    TransactionFailed {
        /// The hash of the failed transaction.
        tx_hash: B256,
        /// The sender address of the failed transaction.
        sender: Address,
        /// The reason for the execution failure.
        reason: String,
    },

    /// ECDSA signature recovery failed.
    #[error("sender recovery failed: {0}")]
    SenderRecovery(String),

    /// Deposit transaction paired with a non-deposit receipt.
    #[error("deposit receipt mismatch: deposit transaction must have a deposit receipt")]
    DepositReceiptMismatch,

    /// Cumulative gas used overflow.
    #[error("gas overflow: cumulative gas used exceeded u64::MAX")]
    GasOverflow,

    /// EVM environment setup error.
    #[error("EVM environment error: {0}")]
    EvmEnv(String),

    /// L1 block info extraction error.
    #[error("L1 block info extraction error: {0}")]
    L1BlockInfo(String),

    /// Payload to block conversion error.
    #[error("block conversion error: {0}")]
    BlockConversion(String),

    /// Failed to load cache account for depositor.
    #[error("failed to load cache account for deposit transaction sender")]
    DepositAccountLoad,

    /// Failed to build RPC receipt.
    #[error("failed to build RPC receipt: {0}")]
    RpcReceiptBuild(String),
}

impl From<RecoveryError> for ExecutionError {
    fn from(err: RecoveryError) -> Self {
        Self::SenderRecovery(err.to_string())
    }
}

impl From<crate::ReceiptBuildError> for ExecutionError {
    fn from(err: crate::ReceiptBuildError) -> Self {
        Self::RpcReceiptBuild(err.to_string())
    }
}

/// Errors related to pending blocks construction.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum BuildError {
    /// Cannot build pending blocks without headers.
    #[error("missing headers: cannot build pending blocks without header information")]
    MissingHeaders,

    /// Cannot build pending blocks with no flashblocks.
    #[error("no flashblocks: cannot build pending blocks from empty flashblock collection")]
    NoFlashblocks,

    /// Cannot build pending blocks when a transaction is missing its receipt.
    #[error(
        "missing receipt: cannot build pending blocks when transaction {tx_hash} has no receipt"
    )]
    MissingReceipt {
        /// The hash of the transaction missing a receipt.
        tx_hash: B256,
    },

    /// Cannot build pending blocks when the same transaction is added twice.
    #[error("duplicate transaction: transaction {tx_hash} was added more than once to the builder")]
    DuplicateTransaction {
        /// The hash of the duplicated transaction.
        tx_hash: B256,
    },
}

/// Errors that can occur during flashblock state processing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum StateProcessorError {
    /// Protocol-level errors (sequencing, ordering).
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// Provider/infrastructure errors.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Transaction execution errors.
    #[error(transparent)]
    Execution(#[from] ExecutionError),

    /// Pending blocks build errors.
    #[error(transparent)]
    Build(#[from] BuildError),

    /// Missing first flashblock, so this one can't be processed.
    #[error("missing first flashblock: cannot build pending blocks without first flashblock")]
    MissingFirstFlashblock,

    /// A flashblock for a block that does not continue the tracked sequence.
    ///
    /// Distinct from [`Self::MissingFirstFlashblock`]: the tracked snapshot is
    /// intact and untouched, and an `index == 0` slice is a valid restart point.
    #[error(
        "non-consecutive block: flashblock {block_number}-{index} does not continue block {tracked_block_number}"
    )]
    NonConsecutiveBlock {
        /// Block number of the incoming flashblock.
        block_number: u64,
        /// Index of the incoming flashblock.
        index: u64,
        /// Block number the overlay currently tracks.
        tracked_block_number: u64,
    },
}

impl From<RecoveryError> for StateProcessorError {
    fn from(err: RecoveryError) -> Self {
        Self::Execution(ExecutionError::from(err))
    }
}

impl From<crate::ReceiptBuildError> for StateProcessorError {
    fn from(err: crate::ReceiptBuildError) -> Self {
        Self::Execution(ExecutionError::from(err))
    }
}

/// A type alias for `Result<T, StateProcessorError>`.
pub type Result<T> = std::result::Result<T, StateProcessorError>;

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn tx_hash() -> B256 {
        B256::with_last_byte(0xAA)
    }

    /// Messages reaching operators through logs; the interpolated fields are the
    /// part worth pinning, since a silent reordering makes triage reports wrong.
    #[rstest]
    #[case(ProtocolError::InvalidSequence.into(), "must be processed in order")]
    #[case(ProtocolError::MissingBase.into(), "first flashblock in sequence")]
    #[case(ProtocolError::EmptyFlashblocks.into(), "zero flashblocks")]
    #[case(
        ProviderError::MissingCanonicalHeader { block_number: 7 }.into(),
        "missing canonical header for block 7"
    )]
    #[case(ProviderError::StateProvider("cold".into()).into(), "state provider error: cold")]
    #[case(
        ExecutionError::TransactionFailed {
            tx_hash: tx_hash(),
            sender: Address::with_last_byte(0xBB),
            reason: "nonce too low".into(),
        }
        .into(),
        "nonce too low"
    )]
    #[case(ExecutionError::GasOverflow.into(), "cumulative gas used exceeded u64::MAX")]
    #[case(ExecutionError::EvmEnv("bad env".into()).into(), "EVM environment error: bad env")]
    #[case(BuildError::MissingReceipt { tx_hash: tx_hash() }.into(), "has no receipt")]
    #[case(BuildError::DuplicateTransaction { tx_hash: tx_hash() }.into(), "more than once")]
    #[case(StateProcessorError::MissingFirstFlashblock, "missing first flashblock")]
    #[case(
        StateProcessorError::NonConsecutiveBlock {
            block_number: 12,
            index: 3,
            tracked_block_number: 9,
        },
        "flashblock 12-3 does not continue block 9"
    )]
    fn display_contains(#[case] error: StateProcessorError, #[case] expected: &str) {
        assert!(error.to_string().contains(expected), "`{error}` should contain `{expected}`");
    }

    /// The four wrapping variants are `#[error(transparent)]`, so they must add no
    /// prefix of their own — a wrapped error reads the same at either level.
    #[rstest]
    #[case(ProtocolError::MissingBase.into(), ProtocolError::MissingBase.to_string())]
    #[case(
        ProviderError::StateProvider("io".into()).into(),
        ProviderError::StateProvider("io".into()).to_string()
    )]
    #[case(
        ExecutionError::DepositReceiptMismatch.into(),
        ExecutionError::DepositReceiptMismatch.to_string()
    )]
    #[case(BuildError::NoFlashblocks.into(), BuildError::NoFlashblocks.to_string())]
    fn wrapping_variants_are_transparent(
        #[case] wrapped: StateProcessorError,
        #[case] inner: String,
    ) {
        assert_eq!(wrapped.to_string(), inner);
    }

    /// `?` must reach `StateProcessorError` from a recovery failure in one step,
    /// landing in `Execution(SenderRecovery)` rather than a fresh variant.
    #[test]
    fn recovery_error_converts_at_both_levels() {
        let execution = ExecutionError::from(RecoveryError::new());
        assert!(matches!(execution, ExecutionError::SenderRecovery(_)));

        let processor = StateProcessorError::from(RecoveryError::new());
        assert_eq!(processor, StateProcessorError::Execution(execution));
    }

    #[test]
    fn receipt_build_error_converts_at_both_levels() {
        let execution = ExecutionError::from(crate::ReceiptBuildError::DepositAccountLoad);
        assert!(matches!(execution, ExecutionError::RpcReceiptBuild(_)));
        assert!(execution.to_string().contains("failed to load deposit account"));

        let processor = StateProcessorError::from(crate::ReceiptBuildError::DepositAccountLoad);
        assert_eq!(processor, StateProcessorError::Execution(execution));
    }

    /// The error crosses the processor task boundary, so it must stay `Send + Sync`.
    #[test]
    const fn error_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StateProcessorError>();
    }
}
