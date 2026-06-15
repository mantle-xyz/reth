//! Startup seeding for reth's `blockchain_tree` block-height gauges.
//!
//! Interim workaround coupled to reth's internal metric names; drop it once the
//! upstream fix (seeding inside `EngineApiTreeHandler::new`) lands in our pinned
//! reth rev.

use reth_provider::{BlockIdReader, BlockNumReader};
use tracing::{info, warn};

/// Seed the `blockchain_tree` block-height gauges from the restored in-memory
/// state at node startup, so they report correct heights immediately instead of
/// reading 0 until reth next updates them.
///
/// reth only writes these gauges when the tracked value *changes* (safe/finalized
/// only advance at epoch boundaries), so without seeding they stay at 0 for a
/// while after every restart.
///
/// NOTE: the metric names must match reth's `TreeMetrics` derive exactly — scope
/// `blockchain_tree` joined with the field by a **dot** (`metrics-derive`'s
/// `DEFAULT_SEPARATOR`); the global `reth` prefix is added by reth's recorder on
/// export. Using `_` instead of `.` registers a *separate* shadow series that
/// collides with the engine's on export and causes the gauge to flip between
/// values.
pub fn seed_blockchain_tree_metrics<P>(provider: &P)
where
    P: BlockNumReader + BlockIdReader,
{
    match provider.best_block_number() {
        Ok(canonical) => {
            metrics::gauge!("blockchain_tree.canonical_chain_height").set(canonical as f64);
            info!(target: "reth::cli", canonical, "Seeded blockchain tree canonical chain height metric");
        }
        Err(error) => {
            warn!(target: "reth::cli", %error, "Failed to seed blockchain tree canonical chain height metric");
        }
    }

    match provider.safe_block_number() {
        Ok(Some(safe)) => {
            metrics::gauge!("blockchain_tree.safe_block_height").set(safe as f64);
            info!(target: "reth::cli", safe, "Seeded blockchain tree safe block height metric");
        }
        Ok(None) => {}
        Err(error) => {
            warn!(target: "reth::cli", %error, "Failed to seed blockchain tree safe block height metric");
        }
    }

    match provider.finalized_block_number() {
        Ok(Some(finalized)) => {
            metrics::gauge!("blockchain_tree.finalized_block_height").set(finalized as f64);
            info!(target: "reth::cli", finalized, "Seeded blockchain tree finalized block height metric");
        }
        Ok(None) => {}
        Err(error) => {
            warn!(target: "reth::cli", %error, "Failed to seed blockchain tree finalized block height metric");
        }
    }
}
