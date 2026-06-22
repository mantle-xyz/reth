//! Mantle preconfirmation core types for op-reth.
//!
//! This crate provides the core types and traits for the preconf subsystem:
//!
//! - [`config::PreconfConfig`] — runtime configuration & whitelist checks
//! - [`types`] — common enums and error types
//! - [`preconf_tx_set::PreconfTxSet`] — the commitment truth source
//! - [`apply`] — builder apply path interface
//! - [`builder`] — payload-builder helpers (cross-iteration dedup, ...)

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod apply;
pub mod builder;
pub mod canon_handler;
pub mod config;
pub mod journal;
pub mod pool_ext;
pub mod preconf_tx_set;
pub mod rpc;
pub mod service_builder;
pub mod types;

pub use builder::{
    BuilderEvent, BuilderTxTracker, CarriedState, JobCancel, PreconfPayloadJob,
    PreconfPayloadJobGenerator,
};
pub use canon_handler::PreconfCanonHandler;
pub use config::PreconfConfig;
pub use journal::{
    EventPublisher, JournalEntry, JournalError, PreconfJournal, RestorePool, RestoredEnvelope,
    RestoredSet, RotateStats, restore_preconf_state, spawn_rejournal_loop,
};
pub use pool_ext::{
    PreconfAwareValidator, PreconfGasLimitExceeded, PreconfPoolListener, ReplaceActivePreconf,
};
pub use preconf_tx_set::{PreconfTxSet, TxEntry};
pub use rpc::PreconfRpcHandler;
pub use service_builder::{PreconfServiceBuilder, PreconfServiceError};
pub use types::{
    AttachError, MarkError, PreconfError, PreconfReceipt, PreconfStatus, PushResult, RecoverError,
};
