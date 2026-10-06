//! Startup restore adapters.
//!
//! - [`pool_adapter::RestoreDirect`] — decodes journal bytes back into envelopes so
//!   `restore_preconf_state` can put commitments straight back in the queue.
//! - [`pool_adapter::ProviderChainView`] — lets a node provider answer restore's chain questions.
//!
//! Neither integrates with the transaction pool. The module and file names are
//! left over from when restore went through it.

pub mod pool_adapter;

pub use pool_adapter::{ProviderChainView, RestoreDirect};
