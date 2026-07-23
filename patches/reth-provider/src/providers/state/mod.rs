//! [`StateProvider`](crate::StateProvider) implementations
pub(crate) mod historical;
pub(crate) mod latest;
pub(crate) mod overlay;

use crate::ProviderResult;
use reth_db_api::{tables, transaction::DbTx};
use reth_stages_types::StageId;

// ==================== [MANTLE PATCH] hashed-snapshot marker ====================
// Entire block below is a Mantle addition (absent in upstream reth-provider). It lets the
// execution-path reads in `latest.rs` recognize a DB built from a hashed-only state snapshot
// and fall back to the hashed tables for preimage-less accounts. See latest.rs for the rationale.

/// DB-key identifying a database that was initialized from a hashed-only state snapshot
/// (`init-state --without-evm` where some accounts lacked an address preimage). Stored in
/// [`tables::StageCheckpointProgresses`] under this custom [`StageId`]. Matches the key written by
/// the v1.9.3 init-state path so a DB built by either version is recognized. `[MANTLE PATCH]`
pub(crate) const HASHED_SNAPSHOT_MARKER: StageId = StageId::Other("__hashed_only_state_snapshot__");

/// Returns `true` if this database was built from a hashed-only state snapshot, i.e. it may contain
/// accounts/storage that exist only in the hashed tables (no plain-state / address preimage).
///
/// Execution-path reads ([`latest::LatestStateProviderRef`]) consult this to fall back to the
/// hashed tables when a plain lookup misses, so preimage-less accounts are read at their real value
/// instead of 0 (which would diverge the state root and fork the chain). `[MANTLE PATCH]`
pub(crate) fn has_hashed_snapshot_marker<TX: DbTx>(tx: &TX) -> ProviderResult<bool> {
    tx.get::<tables::StageCheckpointProgresses>(HASHED_SNAPSHOT_MARKER.to_string())
        .map(|marker| marker.is_some())
        .map_err(Into::into)
}

