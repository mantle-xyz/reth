//! Mantle node CLI components.
//!
//! Provides [`MantleChainSpecParser`] for `--chain mantle` / `--chain mantle-sepolia` support,
//! and [`MantleNode`] as the Mantle-specific node implementation with Mantle txpool validation.

mod chainspec;
pub use chainspec::MantleChainSpecParser;

pub mod txpool;
pub use txpool::{MantleTransactionValidator, MetaTxDisabled, UnprotectedTxDisabled};

pub mod node;
pub use node::{MantleNode, MantleNodeComponentBuilder, MantlePoolBuilder, MantleTransactionPool};

pub use reth_optimism_node::OpNode;
