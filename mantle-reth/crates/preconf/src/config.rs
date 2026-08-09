//! Runtime configuration for the preconf subsystem.
//!
//! Default values match op-geth's `--txpool.*` semantics where applicable;
//! intentional differences are noted inline.

use alloy_primitives::{Address, map::foldhash::HashSet};
use std::{path::PathBuf, time::Duration};

/// Default client-side RPC oneshot wait — 1s.
///
/// Matches op-geth `PreconfTimeout=1s` default.
pub const DEFAULT_PRECONF_TIMEOUT: Duration = Duration::from_secs(1);

/// Default sweep ticker interval — 200ms.
///
/// Sweep ticker cadence for the non-preconf (normal) tx path. At each tick
/// the pool best-tx branch is allowed to consume up to a time-proportional
/// share of the block gas limit — see `builder::payload_builder` Stage 3
/// select! loop. Lower values reduce normal-tx latency at CPU cost; the
/// preconf path is event-driven and not affected.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_millis(200);

/// Default slot duration — 2 seconds.
///
/// Governs the time-proportional pool gas quota:
/// `pool_cumulative_quota(t) = min(t / slot_duration, 1.0) × block_gas_limit`.
/// Matches the OP-stack default block time. Change only if operating on a
/// chain with non-standard slot cadence.
pub const DEFAULT_SLOT_DURATION: Duration = Duration::from_secs(2);

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

/// Default dispatch-time preemption margin — 40ms.
///
/// `apply_one_preconf` aborts a Waiting entry when
/// `entry.inserted_at.elapsed() + safety_margin >= preconf_timeout`.
/// Sized to slightly exceed measured p99 apply latency on the target
/// hardware so the abort only fires on genuine races rather than merely
/// slow-but-in-budget applies. Kept separate from `preconf_timeout` (the
/// client-facing SLA) so operator hardware tuning does not silently
/// widen the client contract.
pub const DEFAULT_SAFETY_MARGIN: Duration = Duration::from_millis(40);

/// Default journal rotation interval — 60s.
///
/// Matches op-geth `--txpool.rejournal` default.
pub const DEFAULT_REJOURNAL_INTERVAL: Duration = Duration::from_secs(60);

/// Default journal max disk size — 1 GiB.
///
/// Above this, rotation forces rename + new file.
pub const DEFAULT_JOURNAL_MAX_SIZE: u64 = 1_073_741_824;

/// Default broadcast channel capacity — 65536.
///
/// Sized at ~320x worst-case burst (assuming ~200 preconf-tx/block peak);
/// 65536 × ~40 bytes ≈ 2.5 MB memory — negligible against the multi-GB
/// sequencer working set.
///
/// Consumers receive `Lagged(n)` and fall back to snapshot reconcile when
/// the channel overflows. Bumped from 4096 → 65536 to give wide headroom
/// for prolonged builder stalls under prod-shape throughput without forcing
/// the (slower) snapshot-reconcile path.
pub const DEFAULT_BROADCAST_CAP: usize = 65536;

/// Runtime preconf configuration; distributed as `Arc<PreconfConfig>` after
/// construction.
///
/// Lifecycle: build → [`Self::validate`] → `Arc::new`. Once wrapped in `Arc`,
/// downstream readers (validator / pool listener / RPC handler / payload
/// builder) cannot mutate it.
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

    /// Client-side oneshot wait — default 1s (see [`DEFAULT_PRECONF_TIMEOUT`]).
    pub preconf_timeout: Duration,

    /// Dispatch-time preemption margin — see [`DEFAULT_SAFETY_MARGIN`].
    /// `apply_one_preconf` skips a tx when
    /// `elapsed_since_insertion + safety_margin >= preconf_timeout`, so
    /// the receipt never lands after the client has already given up.
    /// Tune per hardware; kept separate from `preconf_timeout` (the
    /// client SLA) so operator hardening does not implicitly change the
    /// client contract.
    pub safety_margin: Duration,

    /// Interval at which the payload builder ticks the pool best-tx
    /// sweep. Each tick admits pool txs up to the time-proportional
    /// cumulative gas quota (see [`slot_duration`](Self::slot_duration))
    /// — default 200ms.
    pub sweep_interval: Duration,

    /// Total slot duration used as the denominator of the pool gas
    /// quota schedule: `pool_cumulative_quota(t) = (t / slot_duration)
    /// × block_gas_limit`. Default 2s (OP-stack block time).
    pub slot_duration: Duration,

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
    /// Journal path. `None` ⇒ use the datadir-relative default
    /// (`<datadir>/mantle-preconf/journal.jsonl`), resolved at the CLI layer
    /// before the service builder opens it. The journal is always on when
    /// preconf is enabled — there is no "disabled" mode.
    pub journal_path: Option<PathBuf>,

    /// Journal rotation cadence — default 60s. Must be > 0.
    pub rejournal_interval: Duration,

    /// Journal file size ceiling — default 1 GiB. Must be > 0.
    pub journal_max_size: u64,

    // ===== Internal channel capacity =====
    /// Capacity of the fifo notifier broadcast channel. Consumers that fall
    /// behind see `Lagged(n)` and fall back to a snapshot reconcile.
    /// Default 65536 (see [`DEFAULT_BROADCAST_CAP`]).
    pub broadcast_cap: usize,
}

impl Default for PreconfConfig {
    /// Constructs a disabled-by-default config. Operator opts in by setting
    /// the `MANTLE_PRECONF_ENABLE` env var (consumed by `mantle-reth-cli`'s
    /// `main.rs`); a proper clap-derived `--preconf.enable` CLI flag is
    /// planned but not yet wired (see review followups).
    fn default() -> Self {
        Self {
            enabled: false,
            from_preconfs: HashSet::default(),
            to_preconfs: HashSet::default(),
            all_preconfs: false,
            preconf_timeout: DEFAULT_PRECONF_TIMEOUT,
            safety_margin: DEFAULT_SAFETY_MARGIN,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            slot_duration: DEFAULT_SLOT_DURATION,
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
    /// `rejournal_interval == 0`.
    #[error("rejournal_interval must be > 0")]
    InvalidRejournalInterval,
    /// `journal_max_size == 0`.
    #[error("journal_max_size must be > 0")]
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
    /// `slot_duration == 0`.
    #[error("slot_duration must be > 0")]
    InvalidSlotDuration,
    /// `sweep_interval > slot_duration` — a single tick would exceed the
    /// entire slot, making the time-proportional pool quota meaningless.
    #[error("sweep_interval ({sweep:?}) must be <= slot_duration ({slot:?})")]
    SweepIntervalExceedsSlot {
        /// Configured sweep interval.
        sweep: Duration,
        /// Configured slot duration.
        slot: Duration,
    },
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
    /// `enabled = true` but no eligibility rules configured — `all_preconfs`
    /// is false and `from_preconfs` is empty, meaning [`PreconfConfig::is_preconf_tx`]
    /// would return false for every tx. Spawning the pool listener / journal
    /// / canon handler in this state burns resources for no functional effect.
    #[error(
        "preconf enabled but no eligibility rules: set all_preconfs=true or populate from_preconfs"
    )]
    EnabledWithoutEligibility,
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
        if self.slot_duration.is_zero() {
            return Err(PreconfConfigError::InvalidSlotDuration);
        }
        if self.sweep_interval > self.slot_duration {
            return Err(PreconfConfigError::SweepIntervalExceedsSlot {
                sweep: self.sweep_interval,
                slot: self.slot_duration,
            });
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
        // The journal is always on when preconf is enabled (no disabled mode),
        // so these bounds are unconditional — `journal_path == None` just means
        // "use the datadir default", not "skip validation".
        if self.rejournal_interval.is_zero() {
            return Err(PreconfConfigError::InvalidRejournalInterval);
        }
        if self.journal_max_size == 0 {
            return Err(PreconfConfigError::InvalidJournalMaxSize);
        }
        // `enabled` is only meaningful if some eligibility rule lets at least
        // one tx through `is_preconf_tx`. Without `all_preconfs`, both whitelist
        // sets must be non-empty: `is_preconf_tx` requires
        // `from_preconfs.contains(from) && to_preconfs.contains(to)`, so an empty
        // set on either side makes every tx fail the check. `enabled=true` in
        // that state would spawn pool listener / journal / canon handler for
        // zero functional effect.
        if self.enabled &&
            !self.all_preconfs &&
            (self.from_preconfs.is_empty() || self.to_preconfs.is_empty())
        {
            return Err(PreconfConfigError::EnabledWithoutEligibility);
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
    fn validate_rejects_zero_rejournal_unconditionally() {
        // The journal is always on when preconf is enabled, so a zero rejournal
        // interval is invalid regardless of whether `journal_path` is set
        // (`None` just means "use the datadir default").
        let mut cfg = PreconfConfig::default();
        cfg.rejournal_interval = Duration::ZERO;
        assert!(matches!(cfg.clone().validate(), Err(PreconfConfigError::InvalidRejournalInterval)));
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
        // Default config (sane journal defaults, path resolved later) must validate.
        assert!(PreconfConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_passes_block_budget_equal_to_per_tx() {
        // Boundary: per_block == per_tx is the smallest valid block budget — a
        // single max-sized preconf-tx must fit. The check is `>=`, not `>`, so
        // equality must pass.
        let mut cfg = PreconfConfig::default();
        cfg.preconf_max_gas_per_tx = 2_000_000;
        cfg.preconf_max_gas_per_block = 2_000_000;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_sweep_interval() {
        // Companion to `validate_rejects_zero_timeout` — sweep_interval has its
        // own InvalidSweepInterval variant but no direct test until now.
        let mut cfg = PreconfConfig::default();
        cfg.sweep_interval = Duration::ZERO;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidSweepInterval)));
    }

    #[test]
    fn validate_rejects_zero_slot_duration() {
        let mut cfg = PreconfConfig::default();
        cfg.slot_duration = Duration::ZERO;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidSlotDuration)));
    }

    #[test]
    fn validate_rejects_sweep_interval_larger_than_slot_duration() {
        // A single tick larger than the slot itself makes the time-
        // proportional pool quota degenerate to "full block immediately".
        let mut cfg = PreconfConfig::default();
        cfg.slot_duration = Duration::from_millis(500);
        cfg.sweep_interval = Duration::from_millis(600);
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::SweepIntervalExceedsSlot { .. })));
    }

    #[test]
    fn validate_passes_sweep_interval_equal_to_slot_duration() {
        // Boundary: single tick spanning the whole slot is allowed —
        // it just means pool gets one shot at full quota at slot end,
        // preconf has priority for the entire slot up until then.
        let mut cfg = PreconfConfig::default();
        cfg.slot_duration = Duration::from_millis(500);
        cfg.sweep_interval = Duration::from_millis(500);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_config_slot_duration_matches_op_stack_block_time() {
        // Anchor the default so unrelated refactors don't silently break
        // production quota timing.
        let cfg = PreconfConfig::default();
        assert_eq!(cfg.slot_duration, DEFAULT_SLOT_DURATION);
        assert_eq!(cfg.slot_duration, Duration::from_secs(2));
    }

    #[test]
    fn validate_rejects_zero_journal_max_size_unconditionally() {
        // Companion to `validate_rejects_zero_rejournal_unconditionally` — the
        // other journal field has its own variant; verify it fires regardless of
        // whether `journal_path` is set.
        let mut cfg = PreconfConfig::default();
        cfg.journal_max_size = 0;
        assert!(matches!(cfg.clone().validate(), Err(PreconfConfigError::InvalidJournalMaxSize)));
        cfg.journal_path = Some(PathBuf::from("/tmp/preconf"));
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::InvalidJournalMaxSize)));
    }

    #[test]
    fn validate_rejects_enabled_without_eligibility_rules() {
        // enabled=true with no all_preconfs and both whitelists empty — would
        // spawn background tasks for zero functional effect.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::EnabledWithoutEligibility)));
    }

    #[test]
    fn validate_rejects_enabled_with_only_from_whitelist() {
        // from-only whitelist is a misconfig: is_preconf_tx requires both from
        // AND to to be in their respective sets (op-geth-aligned semantics),
        // so empty to_preconfs makes every tx fail eligibility.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.from_preconfs.insert(addr(1));
        // to_preconfs left empty.
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::EnabledWithoutEligibility)));
    }

    #[test]
    fn validate_rejects_enabled_with_only_to_whitelist() {
        // Symmetric to the from-only case.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.to_preconfs.insert(addr(2));
        assert!(matches!(cfg.validate(), Err(PreconfConfigError::EnabledWithoutEligibility)));
    }

    #[test]
    fn validate_passes_enabled_with_all_preconfs() {
        // enabled=true + all_preconfs=true is the smallest valid eligible config.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.all_preconfs = true;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_enabled_with_both_whitelists_populated() {
        // enabled=true + non-empty (from, to) whitelists is valid.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.from_preconfs.insert(addr(1));
        cfg.to_preconfs.insert(addr(2));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_disabled_with_empty_whitelists() {
        // enabled=false bypasses the eligibility check entirely — default
        // config (all empty) must remain valid. Regression guard for the
        // default-disabled wiring path used when MantleNode.preconf == None.
        let cfg = PreconfConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.validate().is_ok());
    }
}
