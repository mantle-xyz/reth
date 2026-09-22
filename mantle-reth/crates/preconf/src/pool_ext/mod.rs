//! Pool-integration extensions for the preconf subsystem.
//!
//! - [`pool_adapter::RestoreDirect`] — turns journal bytes back into envelopes so
//!   `restore_preconf_state` can put commitments straight back in the queue at startup.

pub mod pool_adapter;

pub use pool_adapter::{ProviderChainView, RestoreDirect};
