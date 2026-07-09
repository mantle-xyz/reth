//! Pool-integration extensions for the preconf subsystem.
//!
//! - [`validator::PreconfAwareValidator`] — pool-validator decorator that
//!   adds replacement guard + per-tx gas ceiling on top of any inner
//!   `TransactionValidator`.
//! - [`preconf_pool_listener::PreconfPoolListener`] — long-running async task
//!   that subscribes to the pool, filters preconf-eligible txs by whitelist,
//!   and pushes them into `PreconfTxSet`.
//! - [`pool_adapter::RestorePoolAdapter`] — bridges a live `TransactionPool`
//!   to the [`crate::RestorePool`] trait so `restore_preconf_state` can
//!   re-admit journal-persisted commitments at startup.

pub mod pool_adapter;
pub mod preconf_pool_listener;
pub mod validator;

pub use pool_adapter::RestorePoolAdapter;
pub use preconf_pool_listener::PreconfPoolListener;
pub use validator::{PreconfAwareValidator, PreconfGasLimitExceeded, ReplaceActivePreconf};
