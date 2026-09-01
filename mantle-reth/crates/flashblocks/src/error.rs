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
