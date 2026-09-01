//! Mantle flashblock consumer.
//!
//! Subscribes to the sequencer's flashblock stream, replays each slice locally
//! and serves the `pending` block tag from the resulting overlay.

mod block_assembler;
pub use block_assembler::{AssembledBlock, BlockAssembler};

mod cache;
pub use cache::{DEFAULT_MAX_CACHE_AHEAD_BLOCKS, FlashblockCache};

mod config;
pub use config::{
    DEFAULT_MAX_LEADING_DEPTH, DEFAULT_MAX_TRAILING_DEPTH, DEFAULT_SUBSCRIBER_PING_INTERVAL,
    FlashblocksConfig,
};

mod error;
pub use error::{
    BuildError, ExecutionError, ProtocolError, ProviderError, Result, StateProcessorError,
};

mod metrics;
pub use metrics::Metrics;

mod processor;
pub use processor::{StateProcessor, StateUpdate};

mod pending_blocks;
pub use pending_blocks::{PendingBlocks, PendingBlocksBuilder};

mod receipt_builder;
pub use receipt_builder::{ReceiptBuildError, UnifiedReceiptBuilder};

pub mod rpc;
pub use rpc::{
    BlockNumberOrTagExt, EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer,
    ExtendedSubscriptionKind, FlashblocksSubscriptionKind, PendingStateOverrides,
    TransactionWithLogs,
};

mod state;
pub use state::FlashblocksState;

mod state_builder;
pub use state_builder::{ExecutedPendingTransaction, PendingStateBuilder};

mod subscription;
pub use subscription::FlashblocksSubscriber;
mod traits;
pub use traits::{FlashblocksAPI, FlashblocksReceiver, PendingBlocksAPI};

mod validation;
pub use validation::{
    CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
    ReorgDetectionResult, ReorgDetector, SequenceValidationResult,
};
