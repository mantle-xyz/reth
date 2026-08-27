//! Mantle preconfirmation core types for op-reth.
//!
//! This crate provides the core types and traits for the preconf subsystem:
//!
//! - [`config::PreconfConfig`] — runtime configuration & whitelist checks
//! - [`classifier::PreconfClassifier`] — freezes each tx's preconf eligibility at admission
//! - [`types`] — common enums and error types
//! - [`preconf_tx_set::PreconfTxSet`] — the commitment truth source
//! - [`apply`] — builder apply path interface
//! - [`builder`] — forked OP payload-builder (async build loop + preconf dispatch)

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod apply;
pub mod builder;
pub mod canon_handler;
pub mod classifier;
pub mod config;
pub mod flashblocks;
pub mod journal;
pub mod metrics_seed;
pub mod payload_service_builder;
pub mod pool_ext;
pub mod preconf_tx_set;
pub mod rpc;
pub mod service_builder;
pub mod types;
pub mod whitelist;

pub use builder::{
    JobCancel, PreconfPayloadBuilder, PreconfPayloadJob, PreconfPayloadJobGenerator,
    ResolvePayloadFuture,
};
pub use canon_handler::PreconfCanonHandler;
pub use classifier::{DEFAULT_VERDICT_CACHE_CAP, PreconfClassifier, Verdict, Whitelist};
pub use config::{DEFAULT_SAFETY_MARGIN, PreconfConfig};
pub use flashblocks::{FlashblockProducerConfig, FlashblockProducerConfigError};
pub use journal::{
    CommitmentChainView, JournalEntry, JournalError, OnChain, PreconfJournal, RestorePool,
    RestoreSkip, RestoredEnvelope, RotateStats, restore_preconf_state, run_rejournal_loop,
    spawn_rejournal_loop,
};
pub use metrics_seed::seed_preconf_metrics;
pub use payload_service_builder::MantlePreconfServiceBuilder;
pub use pool_ext::{
    PreconfAwareValidator, PreconfGasLimitExceeded, PreconfPoolListener, ProviderChainView,
    ReplaceActivePreconf, RestorePoolAdapter,
};
pub use preconf_tx_set::{PreconfTxSet, TxEntry};
pub use rpc::PreconfRpcHandler;
pub use service_builder::{PreconfServiceBuilder, PreconfServiceError};
pub use types::{AttachError, MarkError, PreconfError, PreconfReceipt, PreconfStatus, PushResult};
pub use whitelist::{
    EXPECTED_LAYOUT_VERSION, FROM_WILDCARDS_SLOT, LAYOUT_VERSION_SLOT, PAIRS_SLOT,
    TO_WILDCARDS_SLOT, WHITELIST_UPDATED_TOPIC0, WhitelistError, bootstrap_whitelist,
    has_whitelist_event, reload_whitelist, run_whitelist_watcher, should_reload,
};
