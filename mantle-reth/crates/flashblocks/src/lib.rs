//! Mantle flashblock consumer.
//!
//! Subscribes to the sequencer's flashblock stream, replays each slice locally
//! and serves the `pending` block tag from the resulting overlay.
//!
//! # Dropping over splicing
//!
//! A reorg, a rebuilt block, a reconnection that missed what it needed —
//! anything leaving this node holding slices it cannot place resolves the same
//! way: the overlay is **dropped, never spliced**. `pending` falls back to
//! `latest` until the next base slice opens a fresh one, in practice the next
//! block.
//!
//! Losing the rest of a block is the accepted cost. Serving state no build
//! produced is not: what the sequencer promises is a prediction of one
//! particular build, and an overlay assembled from two predicts nothing that
//! ever ran — while looking, to every caller and every counter, like it did.
//!
//! The staleness and anchor checks in [`StateProcessor`], the build check in
//! [`FlashblockSequenceValidator`] and the eviction in [`FlashblockCache`] are
//! instances of this one rule rather than separate policies. A recovery that
//! would shorten the gap is worth adding; one that would narrow it by accepting
//! slices from a build this node did not follow is not.
//!
//! # A prefix cannot answer a question about a sealed block
//!
//! A slice is a prefix. Header fields whose meaning depends on the block being
//! finished therefore have no value to report, and the pending header says so
//! with `B256::ZERO` rather than with a figure that looks computed:
//!
//! - `state_root` — never computed for a slice; the producer sends zero.
//! - `withdrawals_root` — post-Isthmus the `L2ToL1MessagePasser` storage root. The producer sends
//!   zero too, and [`BlockAssembler`] writes zero whatever arrives: a figure here would be a
//!   well-formed root for a block that is not finished.
//!
//! Zero is safe to read as "not computed" because `EMPTY_ROOT_HASH` is
//! `0x56e81f17…` — no state produces a zero root, so the sentinel cannot be
//! confused with an answer. That is the whole point: a well-formed but wrong
//! root is indistinguishable from a right one, and a consumer has no way to
//! tell which it got.
//!
//! Computing the true per-slice value was considered and declined, on grounds
//! of use rather than cost: it is too early for an L1 withdrawal proof,
//! redundant for "did my withdrawal execute" (the slice carries the
//! transactions), and not to be believed without a local replay that would
//! compute it anyway.

mod block_assembler;
pub use block_assembler::{AssembledBlock, BlockAssembler};

mod canonical;
pub use canonical::{await_canonical_match, forward_canonical_state};

mod cache;
pub use cache::{DEFAULT_MAX_CACHE_AHEAD_BLOCKS, FlashblockCache, MAX_CACHE_AHEAD_BLOCKS};

mod config;
pub use config::{
    DEFAULT_MAX_LEADING_DEPTH, DEFAULT_MAX_TRAILING_DEPTH, DEFAULT_SUBSCRIBER_PING_INTERVAL,
    FlashblocksConfig, MAX_SUBSCRIBER_PING_INTERVAL,
};

mod error;
pub use error::{
    BuildError, ExecutionError, ProtocolError, ProviderError, Result, StateProcessorError,
};

mod metrics;
pub use metrics::Metrics;

mod processor;
pub use processor::{QueueConsumer, StateProcessor, StateUpdate};

mod pending_blocks;
pub use pending_blocks::{PendingBlocks, PendingBlocksBuilder};

mod receipt_builder;
pub use receipt_builder::{ReceiptBuildError, UnifiedReceiptBuilder};

pub mod rpc;
pub use rpc::{
    BlockNumberOrTagExt, EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer,
    ExtendedSubscriptionKind, FlashblocksSubscriptionKind, PendingEthApi, PendingExecutionContext,
    PendingStateOverrides, TransactionWithLogs,
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
    CanonicalBlockReconciler, FlashblockSequenceValidator, IncomingBuild, ReconciliationStrategy,
    ReorgDetectionResult, ReorgDetector, SequenceValidationResult,
};
