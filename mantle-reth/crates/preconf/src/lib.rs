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
pub mod config;
pub mod preconf_tx_set;
pub mod types;

pub use builder::{BuilderTxTracker, CarriedState};
pub use config::PreconfConfig;
pub use preconf_tx_set::{PreconfTxSet, TxEntry};
pub use types::{
    AttachError, MarkError, PreconfError, PreconfReceipt, PreconfStatus, PushResult, RecoverError,
};
