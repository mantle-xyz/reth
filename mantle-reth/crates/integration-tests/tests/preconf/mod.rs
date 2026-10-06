//! Integration tests for the Mantle preconf subsystem.
//!
//! Each submodule targets a specific SLA facet (happy path, timeout,
//! gas budgets, ...). All submodules share `helpers` — chain-spec,
//! payload attributes, `PreconfCfgBuilder`, plus a jsonrpsee
//! free function for `eth_sendRawTransactionWithPreconf` (which
//! reth's `RpcTestContext::inject_tx` does not cover).
//!
//! Aggregated into a single `[[test]]` binary via this `mod.rs`; every
//! individual test still runs in its own tokio runtime, but the crate
//! only pays the compile-link cost once.

#![recursion_limit = "1024"]
#![allow(missing_docs)]

pub mod helpers;

mod account_view;
mod basefee_threshold;
mod canon_cleanup;
mod chain_id_pair;
mod classifier_freeze;
mod da_footprint;
mod delegated_sender;
mod dual_channel_nonce;
mod dual_channel_same_hash;
mod gas_budgets;
mod happy_path;
mod insufficient_funds;
mod journal_rotation;
mod journal_size_rotation;
mod journal_write;
mod no_tx_pool;
mod predeploy_genesis;
mod queue_gas_ceiling;
mod race_pool_arm;
mod reorg_dropped_payload;
mod reorg_multi_block_relands;
mod reorg_no_duplicate;
mod reorg_ordering;
mod reorg_shallow_relands;
mod replacement;
mod replay_da;
mod replay_nonce_consumed;
mod replay_transient_defer;
mod restart_replay;
mod single_payload;
mod timeout;
mod tx_type_whitelist;
mod validation_reject;
mod validator_error_mapping;
mod whitelist_onchain;
