//! Runtime configuration for the preconf subsystem.
//!
//! Default values match op-geth's `--txpool.*` semantics where applicable;
//! intentional differences are noted inline.

use alloy_primitives::{Address, map::foldhash::HashSet};
use std::{path::PathBuf, time::Duration};

/// Default client-side RPC oneshot wait — 200ms.
///
/// **Intentional difference from op-geth** (`PreconfTimeout=1s`):
/// the v1 reth design prefers fast client failure over server-side slack.
pub const DEFAULT_PRECONF_TIMEOUT: Duration = Duration::from_millis(200);

/// Default sweep ticker interval — 50ms.
///
/// Sweep ticker cadence for the non-preconf (normal) tx path. Lower values
/// reduce normal-tx latency at CPU cost; the preconf path is event-driven
/// and not affected.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_millis(50);

/// Default per-tx gas limit for preconf-eligible transactions — `2_000_000`.
///
/// op-geth has no equivalent — this is operator hardening against
/// pathological large-gas spam (a single huge contract call blocking the
/// preconf path).
pub const DEFAULT_PRECONF_MAX_GAS_PER_TX: u64 = 2_000_000;

/// Default cumulative preconf gas budget per block — `6_000_000`.
///
/// Caps how much of a block's gas can be spent on the preconf fast-path,
/// leaving headroom for the regular tx path. Hardening against bursts of
/// preconf-eligible traffic that would otherwise crowd out non-preconf txs.
pub const DEFAULT_PRECONF_MAX_GAS_PER_BLOCK: u64 = 6_000_000;

/// Default journal rotation interval — 60s.
///
/// Matches op-geth `--txpool.rejournal` default.
pub const DEFAULT_REJOURNAL_INTERVAL: Duration = Duration::from_secs(60);

/// Default journal max disk size — 1 GiB.
///
/// Above this, rotation forces rename + new file.
pub const DEFAULT_JOURNAL_MAX_SIZE: u64 = 1_073_741_824;

/// Default broadcast channel capacity — 4096.
///
/// Sized at ~20x worst-case burst; 4096 × ~40 bytes ≈ 160 KB memory.
/// Consumers receive `Lagged(n)` and fall back to snapshot reconcile when
/// the channel overflows.
pub const DEFAULT_BROADCAST_CAP: usize = 4096;

/// Runtime preconf configuration; passed by `Arc` throughout the subsystem.
///
/// **All fields are immutable after [`PreconfConfig::validate`]** — runtime
/// reload requires a restart in v1.
#[derive(Debug, Clone)]
pub struct PreconfConfig {
    /// Master switch: false → entire preconf subsystem stays inactive
    /// (RPC handler returns `NotPreconfEligible` immediately, pool listener
    /// task does not spawn).
    pub enabled: bool,

    /// Whitelist of "from" addresses. A tx is preconf-eligible if its sender
    /// is in this set **and** its destination is in `to_preconfs`
    /// (see [`Self::is_preconf_tx`]) — or [`Self::all_preconfs`] is `true`.
    pub from_preconfs: HashSet<Address>,

    /// Whitelist of "to" addresses; see `from_preconfs`.
    pub to_preconfs: HashSet<Address>,

    /// If true, all transactions are treated as preconf-eligible regardless
    /// of `from_preconfs` / `to_preconfs`. Aligns with op-geth's
    /// `tx_pool_config::AllPreconfs`.
    pub all_preconfs: bool,

    /// Client-side oneshot wait — default 200ms (see [`DEFAULT_PRECONF_TIMEOUT`]).
    pub preconf_timeout: Duration,

    /// Phase 2 sweep ticker interval — default 50ms.
    pub sweep_interval: Duration,

    // ===== Operator hardening =====
    /// Per-tx gas limit for preconf-eligible transactions.
    /// Default `2_000_000` (see [`DEFAULT_PRECONF_MAX_GAS_PER_TX`]).
    pub preconf_max_gas_per_tx: u64,

    /// Cumulative gas budget the preconf fast-path may consume in a single
    /// block. Once exceeded, subsequent preconf-eligible txs in the same
    /// block are rejected with [`crate::types::PreconfError::BlockGasBudgetExceeded`].
    /// Default `6_000_000` (see [`DEFAULT_PRECONF_MAX_GAS_PER_BLOCK`]).
    pub preconf_max_gas_per_block: u64,

    // ===== Journal persistence =====
    /// Journal path. `None` disables the journal subsystem entirely;
    /// `rejournal_interval` and `journal_max_size` are ignored in that case.
    pub journal_path: Option<PathBuf>,

    /// Journal rotation cadence — default 60s.
    /// Must be > 0 if `journal_path` is `Some`.
    pub rejournal_interval: Duration,

    /// Journal file size ceiling — default 1 GiB.
    /// Must be > 0 if `journal_path` is `Some`.
    pub journal_max_size: u64,

    // ===== Internal channel capacity =====
    /// Broadcast channel capacity for both `event_broadcast`
    /// (newPreconfTransaction subscription) and the fifo notifier.
    /// Default 4096 (see [`DEFAULT_BROADCAST_CAP`]).
    pub broadcast_cap: usize,
}

impl Default for PreconfConfig {
    /// Constructs a disabled-by-default config — operator opts in via CLI
    /// `--preconf.enable`.
    fn default() -> Self {
        Self {
            enabled: false,
            from_preconfs: HashSet::default(),
            to_preconfs: HashSet::default(),
            all_preconfs: false,
            preconf_timeout: DEFAULT_PRECONF_TIMEOUT,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            preconf_max_gas_per_tx: DEFAULT_PRECONF_MAX_GAS_PER_TX,
            preconf_max_gas_per_block: DEFAULT_PRECONF_MAX_GAS_PER_BLOCK,
            journal_path: None,
            rejournal_interval: DEFAULT_REJOURNAL_INTERVAL,
            journal_max_size: DEFAULT_JOURNAL_MAX_SIZE,
            broadcast_cap: DEFAULT_BROADCAST_CAP,
        }
    }
}

/// Errors surfaced by [`PreconfConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreconfConfigError {
    /// `journal_path` set but `rejournal_interval == 0`.
    #[error("rejournal_interval must be > 0 when journal_path is set")]
    InvalidRejournalInterval,
    /// `journal_path` set but `journal_max_size == 0`.
    #[error("journal_max_size must be > 0 when journal_path is set")]
    InvalidJournalMaxSize,
    /// `broadcast_cap == 0` — tokio broadcast requires capacity > 0.
    #[error("broadcast_cap must be > 0")]
    InvalidBroadcastCap,
    /// `preconf_timeout == 0`.
    #[error("preconf_timeout must be > 0")]
    InvalidPreconfTimeout,
    /// `sweep_interval == 0`.
    #[error("sweep_interval must be > 0")]
    InvalidSweepInterval,
    /// `preconf_max_gas_per_tx == 0`.
    #[error("preconf_max_gas_per_tx must be > 0")]
    InvalidPreconfMaxGasPerTx,
    /// `preconf_max_gas_per_block == 0`.
    #[error("preconf_max_gas_per_block must be > 0")]
    InvalidPreconfMaxGasPerBlock,
    /// `preconf_max_gas_per_block < preconf_max_gas_per_tx` — a single tx
    /// could never fit the block budget.
    #[error("preconf_max_gas_per_block ({block}) must be >= preconf_max_gas_per_tx ({per_tx})")]
    BlockBudgetSmallerThanPerTx {
        /// Configured per-block budget.
        block: u64,
        /// Configured per-tx limit.
        per_tx: u64,
    },
}

impl PreconfConfig {
    /// Whitelist check for "from" only.
    ///
    /// Returns true when `all_preconfs` is set or `from` is in `from_preconfs`.
    /// Mirrors op-geth `tx_pool_config::IsPreconfTxFrom`.
    #[inline]
    pub fn is_preconf_from(&self, from: &Address) -> bool {
        self.all_preconfs || self.from_preconfs.contains(from)
    }

    /// Whitelist check for the full (from, to) pair.
    ///
    /// Returns true if `all_preconfs` is set OR (`from` ∈ `from_preconfs` AND
    /// `to` is `Some(addr)` with `addr` ∈ `to_preconfs`).
    ///
    /// Returns false for contract creations (`to == None`) when not in
    /// `all_preconfs` mode — matches op-geth `IsPreconfTx` behavior.
    #[inline]
    pub fn is_preconf_tx(&self, from: &Address, to: Option<&Address>) -> bool {
        if self.all_preconfs {
            return true;
        }
        let Some(to) = to else { return false };
        self.from_preconfs.contains(from) && self.to_preconfs.contains(to)
    }

    /// Validates config invariants. Returns the original config on success
    /// for ergonomic chaining (`config.validate()?`).
    pub fn validate(self) -> Result<Self, PreconfConfigError> {
        if self.broadcast_cap == 0 {
            return Err(PreconfConfigError::InvalidBroadcastCap);
        }
        if self.preconf_timeout.is_zero() {
            return Err(PreconfConfigError::InvalidPreconfTimeout);
        }
        if self.sweep_interval.is_zero() {
            return Err(PreconfConfigError::InvalidSweepInterval);
        }
        if self.preconf_max_gas_per_tx == 0 {
            return Err(PreconfConfigError::InvalidPreconfMaxGasPerTx);
        }
        if self.preconf_max_gas_per_block == 0 {
            return Err(PreconfConfigError::InvalidPreconfMaxGasPerBlock);
        }
        if self.preconf_max_gas_per_block < self.preconf_max_gas_per_tx {
            return Err(PreconfConfigError::BlockBudgetSmallerThanPerTx {
                block: self.preconf_max_gas_per_block,
                per_tx: self.preconf_max_gas_per_tx,
            });
        }
        if self.journal_path.is_some() {
            if self.rejournal_interval.is_zero() {
                return Err(PreconfConfigError::InvalidRejournalInterval);
            }
            if self.journal_max_size == 0 {
                return Err(PreconfConfigError::InvalidJournalMaxSize);
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    // Tests intentionally mutate `PreconfConfig::default()` to exercise
    // single-field validations; struct-literal init would be noisy.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn default_is_disabled() {
        let cfg = PreconfConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.all_preconfs);
        assert!(cfg.from_preconfs.is_empty());
        assert!(cfg.to_preconfs.is_empty());
    }

    #[test]
    fn is_preconf_from_with_whitelist() {
        let mut cfg = PreconfConfig::default();
        cfg.from_preconfs.insert(addr(1));
        assert!(cfg.is_preconf_from(&addr(1)));
        assert!(!cfg.is_preconf_from(&addr(2)));
    }

    #[test]
    fn is_preconf_from_with_all() {
        let mut cfg = PreconfConfig::default();
        cfg.all_preconfs = true;
        assert!(cfg.is_preconf_from(&addr(99)));
    }

    #[test]
    fn is_preconf_tx_requires_both_from_and_to() {
        let mut cfg = PreconfConfig::default();
        cfg.from_preconfs.insert(addr(1));
        cfg.to_preconfs.insert(addr(2));
        // Both match
        assert!(cfg.is_preconf_tx(&addr(1), Some(&addr(2))));
        // Only `from` matches
        assert!(!cfg.is_preconf_tx(&addr(1), Some(&addr(3))));
        // Only `to` matches
        assert!(!cfg.is_preconf_tx(&addr(5), Some(&addr(2))));
        // Contract creation — never eligible without all_preconfs
        assert!(!cfg.is_preconf_tx(&addr(1), None));
    }

    #[test]
    fn is_preconf_tx_all_mode_includes_contract_creation() {
        let mut cfg = PreconfConfig::default();
        cfg.all_preconfs = true;
        assert!(cfg.is_preconf_tx(&addr(99), None));
        assert!(cfg.is_preconf_tx(&addr(99), Some(&addr(77))));
    }

    #[test]
    fn validate_rejects_zero_broadcast_cap() {
        let mut cfg = PreconfConfig::default();
        cfg.broadcast_cap = 0;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidBroadcastCap)));
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let mut cfg = PreconfConfig::default();
        cfg.preconf_timeout = Duration::ZERO;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidPreconfTimeout)));
    }

    #[test]
    fn validate_rejects_zero_rejournal_only_when_journal_enabled() {
        let mut cfg = PreconfConfig::default();
        cfg.rejournal_interval = Duration::ZERO;
        // Journal disabled — zero rejournal is ignored.
        assert!(cfg.clone().validate().is_ok());
        // Journal enabled — zero rejournal must fail.
        cfg.journal_path = Some(PathBuf::from("/tmp/preconf"));
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidRejournalInterval)));
    }

    #[test]
    fn validate_rejects_zero_max_gas_per_tx() {
        let mut cfg = PreconfConfig::default();
        cfg.preconf_max_gas_per_tx = 0;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidPreconfMaxGasPerTx)));
    }

    #[test]
    fn validate_rejects_zero_max_gas_per_block() {
        let mut cfg = PreconfConfig::default();
        cfg.preconf_max_gas_per_block = 0;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidPreconfMaxGasPerBlock)));
    }

    #[test]
    fn validate_rejects_block_budget_smaller_than_per_tx() {
        let mut cfg = PreconfConfig::default();
        cfg.preconf_max_gas_per_tx = 5_000_000;
        cfg.preconf_max_gas_per_block = 1_000_000;
        assert!(matches!(
            cfg.validate(),
            Err(PreconfConfigError::BlockBudgetSmallerThanPerTx { .. })
        ));
    }

    #[test]
    fn validate_passes_default() {
        // Default config (journal disabled, sane defaults) must validate.
        assert!(PreconfConfig::default().validate().is_ok());
    }
}
