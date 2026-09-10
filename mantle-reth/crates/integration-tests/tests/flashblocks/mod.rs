//! Integration tests for the Mantle flashblocks subsystem.
//!
//! Aggregated into a single `[[test]]` binary via this `mod.rs`, so the
//! crate pays the compile-link cost once.

#![allow(missing_docs)]

pub mod helpers;

mod cache_replay;
mod consumer_pipeline;
mod reconciliation;
mod rpc_pending;
mod rpc_pubsub;
mod subscriber_resume;
mod wire_compat;
mod ws_proxy_bridge;
mod ws_proxy_server;
