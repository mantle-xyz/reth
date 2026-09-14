//! Integration tests for the Mantle flashblocks subsystem.
//!
//! Aggregated into a single `[[test]]` binary via this `mod.rs`, so the
//! crate pays the compile-link cost once.

#![allow(missing_docs)]

// Mounted as `crate::helpers` because the node-launch macros resolve their
// chain spec and config builder through that path. Shared with the `preconf`
// target rather than duplicated: launching a node with slicing on is launching
// a preconf node, plus one bind.
#[path = "../preconf/helpers.rs"]
pub mod helpers;

// The consumer-side suites feed hand-built slices to a `FlashblocksState`
// instead of driving a producer, so their fixtures are a separate set from the
// preconf ones above and keep their own name.
pub mod consumer_helpers;
mod cache_replay;
mod consumer_pipeline;
mod da_footprint_header;
mod producer_e2e;
mod reconciliation;
mod reverted_tx;
mod rpc_pending;
mod rpc_pubsub;
mod slice_journal;
mod slice_replay;
mod subscriber_resume;
mod wire_compat;
mod ws_proxy_bridge;
mod ws_proxy_server;
