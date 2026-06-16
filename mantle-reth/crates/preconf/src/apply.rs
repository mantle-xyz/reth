//! Builder apply interface.
//!
//! This module declares the function signatures used by the payload builder
//! to apply preconf-eligible transactions one at a time against the running
//! state cache.

use crate::types::{PreconfError, PreconfReceipt};
use alloy_consensus::TxEnvelope;

/// Apply a single preconf-eligible transaction against the builder's cache.
///
/// Returns a [`PreconfReceipt`] on success (used to feed both the RPC
/// responder and the per-tx broadcast event), or [`PreconfError`] on
/// builder-side failure (which the builder converts to `PreconfStatus::Failed`
/// at the fifo layer).
pub fn apply_preconf_tx(_tx: &TxEnvelope) -> Result<PreconfReceipt, PreconfError> {
    unimplemented!("apply_preconf_tx")
}
