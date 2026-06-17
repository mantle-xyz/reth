//! Pool-integration extensions for the preconf subsystem.
//!
//! - [`validator::PreconfAwareValidator`] — pool-validator decorator that
//!   adds replacement guard + per-tx gas ceiling on top of any inner
//!   `TransactionValidator`.
//! - [`preconf_pool_listener::PreconfPoolListener`] — long-running async task
//!   that subscribes to the pool, filters preconf-eligible txs by whitelist,
//!   and pushes them into `PreconfTxSet`.

pub mod preconf_pool_listener;
pub mod validator;

pub use preconf_pool_listener::PreconfPoolListener;
pub use validator::{PreconfAwareValidator, PreconfGasLimitExceeded, ReplaceActivePreconf};
