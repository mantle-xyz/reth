//! Mantle node CLI components.
//!
//! Provides [`MantleChainSpecParser`] for `--chain mantle` / `--chain mantle-sepolia` support,
//! and [`MantleNode`] as the Mantle-specific node implementation with Mantle txpool validation.

mod chainspec;
pub use chainspec::MantleChainSpecParser;

pub mod args;
pub use args::{MantleArgs, PreconfArgs};

pub mod txpool;
pub use txpool::{MantleTransactionValidator, MetaTxDisabled, UnprotectedTxDisabled};

pub mod node;
pub use node::{MantleNode, MantleNodeComponentBuilder, MantlePoolBuilder, MantleTransactionPool};

pub mod version;

pub mod metrics_seed;
pub use metrics_seed::seed_blockchain_tree_metrics;

pub mod proofs_history;
pub use proofs_history::{spawn_proofs_db_metrics, with_proofs_history};

pub use reth_optimism_node::OpNode;
