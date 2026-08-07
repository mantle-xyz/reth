//! Mantle CLI arguments — flattens upstream `RollupArgs` with mantle-specific
//! extensions (currently: preconf configuration).
//!
//! Composition strategy (see `main.rs`):
//!
//! ```text
//! Cli::<MantleChainSpecParser, MantleArgs>::parse().run(|builder, args| {
//!     let node = MantleNode::new(args.rollup);
//!     let node = if let Some(cfg) = args.preconf.into_config() {
//!         node.with_preconf(PreconfServiceBuilder::from_config(cfg).await?)
//!     } else { node };
//!     ...
//! })
//! ```
//!
//! All `--preconf.*` flags are CLI-only (no env-var fallback), matching the
//! upstream `RollupArgs` convention — every configuration knob lives on the
//! command line and is discoverable via `--help`.

use std::{path::PathBuf, time::Duration};

use alloy_primitives::{Address, map::foldhash::HashSet};
use clap::Args;
use mantle_reth_preconf::{
    PreconfConfig,
    config::{
        DEFAULT_BROADCAST_CAP, DEFAULT_JOURNAL_MAX_SIZE, DEFAULT_PRECONF_MAX_GAS_PER_BLOCK,
        DEFAULT_PRECONF_MAX_GAS_PER_TX, DEFAULT_PRECONF_TIMEOUT, DEFAULT_REJOURNAL_INTERVAL,
        DEFAULT_SLOT_DURATION, DEFAULT_SWEEP_INTERVAL,
    },
};
use reth_optimism_node::args::RollupArgs;

/// Top-level mantle CLI args — flattens upstream `RollupArgs` and mantle
/// preconf options into a single `clap::Args`-derived struct.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct MantleArgs {
    /// Upstream OP-reth rollup arguments (`--rollup.*`).
    #[command(flatten)]
    pub rollup: RollupArgs,

    /// Mantle preconf subsystem arguments (`--preconf.*`).
    #[command(flatten)]
    pub preconf: PreconfArgs,
}

/// Preconfirmation subsystem CLI flags.
///
/// Leave everything at defaults and preconf stays disabled (matches
/// `MantleNode::new` behavior without `.with_preconf()`).
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct PreconfArgs {
    /// Enable the mantle preconfirmation subsystem.
    ///
    /// When absent (default), the node behaves exactly like upstream
    /// `op-reth` — no preconf validator, listener, canon handler, or RPC
    /// method registration.
    #[arg(long = "preconf.enable")]
    pub enable: bool,

    /// Path to the preconf commitment journal for restart-safety.
    ///
    /// Omit to disable persistence — promised but unsealed commitments are
    /// lost on crash. When set, the journal file is opened in append mode;
    /// existing contents are preserved (restart-replay is a separate step).
    #[arg(long = "preconf.journal-path")]
    pub journal_path: Option<PathBuf>,

    /// Treat all transactions as preconf-eligible (bypasses the from/to
    /// allowlist). Aligns with op-geth's `--txpool.allpreconfs`.
    #[arg(long = "preconf.all")]
    pub all: bool,

    /// Allowlisted sender addresses. Accepts a comma-separated list
    /// (`--preconf.from 0x1,0x2,0x3`) matching op-geth's
    /// `--txpool.frompreconfs="0x1,0x2,0x3"`, or repeated flags
    /// (`--preconf.from 0x1 --preconf.from 0x2`) — both accumulate.
    /// Ignored when `--preconf.all` is set.
    #[arg(long = "preconf.from", value_name = "ADDRESSES", value_delimiter = ',')]
    pub from: Vec<Address>,

    /// Allowlisted recipient addresses. Same semantics as `--preconf.from`
    /// (comma-separated or repeated). Aligns with op-geth's
    /// `--txpool.topreconfs`. Contract-creation txs (`to == None`) are only
    /// eligible when `--preconf.all` is on.
    #[arg(long = "preconf.to", value_name = "ADDRESSES", value_delimiter = ',')]
    pub to: Vec<Address>,

    /// Client-visible RPC oneshot timeout, in milliseconds. Default matches
    /// [`mantle_reth_preconf::config::DEFAULT_PRECONF_TIMEOUT`] (1s).
    #[arg(long = "preconf.timeout-ms")]
    pub timeout_ms: Option<u64>,

    /// Payload-builder sweep-ticker interval, in milliseconds — cadence at
    /// which the builder admits a fresh batch of pool best-txs. Each tick
    /// admits pool txs up to the time-proportional cumulative gas quota
    /// (see `--preconf.slot-duration-ms`). Default 200ms.
    #[arg(long = "preconf.sweep-interval-ms")]
    pub sweep_interval_ms: Option<u64>,

    /// Slot duration in milliseconds — denominator of the pool best-tx
    /// gas quota schedule: `pool_cumulative_quota(t) = min(t /
    /// slot_duration, 1.0) × block_gas_limit`. Default 2000ms (OP-stack
    /// block time). Change only on chains with non-standard cadence.
    #[arg(long = "preconf.slot-duration-ms")]
    pub slot_duration_ms: Option<u64>,

    /// Per-tx gas cap for preconf-eligible txs. Default `2_000_000`.
    #[arg(long = "preconf.max-gas-per-tx")]
    pub max_gas_per_tx: Option<u64>,

    /// Cumulative preconf gas budget per block. Default `6_000_000`. Must be
    /// `>=` `preconf.max-gas-per-tx` (checked by `PreconfConfig::validate`).
    #[arg(long = "preconf.max-gas-per-block")]
    pub max_gas_per_block: Option<u64>,

    /// Journal rotation interval, in seconds. Default 60s. Only meaningful
    /// when `--preconf.journal-path` is set.
    #[arg(long = "preconf.rejournal-interval-secs")]
    pub rejournal_interval_secs: Option<u64>,

    /// Journal file size ceiling, in bytes. Default 1 GiB (`1_073_741_824`).
    /// Above this, rotation renames the current file and starts a new one.
    /// Only meaningful when `--preconf.journal-path` is set.
    #[arg(long = "preconf.journal-max-size")]
    pub journal_max_size: Option<u64>,

    /// Broadcast channel capacity. Default 65536. Advanced tuning knob — sizes
    /// the fifo notifier broadcast channel; consumers see `Lagged(n)` and fall
    /// back to snapshot reconcile when full.
    #[arg(long = "preconf.broadcast-cap")]
    pub broadcast_cap: Option<usize>,
}

impl PreconfArgs {
    /// Convert CLI args into a `PreconfConfig` if `--preconf.enable` was
    /// given; otherwise return `None` (preconf stays off).
    ///
    /// Every numeric tuning flag is `Option`-typed and falls back to its
    /// `DEFAULT_*` constant when unset — the CLI surface only exposes
    /// deltas from [`PreconfConfig::default`] for numeric fields. Allowlist
    /// / boolean / path fields default to false / empty / None as expected
    /// for `bool` / `Vec` / `Option<PathBuf>` clap parsing.
    pub fn into_config(self) -> Option<PreconfConfig> {
        if !self.enable {
            return None;
        }
        Some(PreconfConfig {
            enabled: true,
            all_preconfs: self.all,
            from_preconfs: {
                let mut s = HashSet::default();
                s.extend(self.from);
                s
            },
            to_preconfs: {
                let mut s = HashSet::default();
                s.extend(self.to);
                s
            },
            preconf_timeout: self
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_PRECONF_TIMEOUT),
            safety_margin: mantle_reth_preconf::DEFAULT_SAFETY_MARGIN,
            sweep_interval: self
                .sweep_interval_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_SWEEP_INTERVAL),
            slot_duration: self
                .slot_duration_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_SLOT_DURATION),
            preconf_max_gas_per_tx: self.max_gas_per_tx.unwrap_or(DEFAULT_PRECONF_MAX_GAS_PER_TX),
            preconf_max_gas_per_block: self
                .max_gas_per_block
                .unwrap_or(DEFAULT_PRECONF_MAX_GAS_PER_BLOCK),
            journal_path: self.journal_path,
            rejournal_interval: self
                .rejournal_interval_secs
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_REJOURNAL_INTERVAL),
            journal_max_size: self.journal_max_size.unwrap_or(DEFAULT_JOURNAL_MAX_SIZE),
            broadcast_cap: self.broadcast_cap.unwrap_or(DEFAULT_BROADCAST_CAP),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Test helper — wraps `PreconfArgs` alone (not the full `MantleArgs`) so
    /// each test can invoke `parse_from` without needing `RollupArgs` context.
    #[derive(Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        args: PreconfArgs,
    }

    fn parse(argv: &[&str]) -> PreconfArgs {
        TestCli::parse_from(std::iter::once(&"reth").chain(argv.iter())).args
    }

    #[test]
    fn default_parse_yields_disabled() {
        // No flags → into_config() returns None → preconf stays off. This is
        // the wire that keeps the "opt-in" contract of the CLI.
        let a = parse(&[]);
        assert!(!a.enable);
        assert!(a.into_config().is_none());
    }

    #[test]
    fn enable_flag_produces_enabled_config() {
        // Minimal opt-in: --preconf.enable alone is enough. Whitelists empty
        // and --preconf.all not set → `enabled: true, all_preconfs: false,
        // {from,to}_preconfs: empty`. This IS a config that `validate()`
        // rejects with `RequiresEligibilityRules` — but that's a semantic
        // check enforced at PreconfServiceBuilder::new time, not CLI parse
        // time. Two-layer defense: clap catches syntax, validate() catches
        // semantics.
        let a = parse(&["--preconf.enable"]);
        assert!(a.enable);
        let cfg = a.into_config().expect("enabled");
        assert!(cfg.enabled);
        assert!(!cfg.all_preconfs);
        assert!(cfg.from_preconfs.is_empty());
        assert!(cfg.to_preconfs.is_empty());
    }

    #[test]
    fn journal_path_flag_lands_on_config() {
        // `--preconf.journal-path` maps to `PreconfConfig::journal_path`
        // as a `PathBuf`, gated behind `--preconf.enable`. Regression
        // guard against silent wiring drift.
        let a = parse(&["--preconf.enable", "--preconf.journal-path", "/tmp/mantle-preconf.jsonl"]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(
            cfg.journal_path.as_deref(),
            Some(std::path::Path::new("/tmp/mantle-preconf.jsonl")),
        );
    }

    #[test]
    fn repeatable_from_to_flags_accumulate_into_sets() {
        // Two --preconf.from + two --preconf.to → each set has 2 members.
        // The Vec<Address> parses successfully; into_config dedups via
        // HashSet.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.from",
            "0x1111111111111111111111111111111111111111",
            "--preconf.from",
            "0x2222222222222222222222222222222222222222",
            "--preconf.to",
            "0x3333333333333333333333333333333333333333",
            "--preconf.to",
            "0x4444444444444444444444444444444444444444",
        ]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.from_preconfs.len(), 2);
        assert_eq!(cfg.to_preconfs.len(), 2);
    }

    #[test]
    fn comma_separated_from_to_matches_op_geth_format() {
        // op-geth accepts `--txpool.frompreconfs="0x1,0x2,0x3"`.
        // The CLI must parse the same shape when a single flag carries
        // a comma-separated address list.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.from",
            "0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222,0x3333333333333333333333333333333333333333",
            "--preconf.to",
            "0x4444444444444444444444444444444444444444,0x5555555555555555555555555555555555555555",
        ]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.from_preconfs.len(), 3);
        assert_eq!(cfg.to_preconfs.len(), 2);
    }

    #[test]
    fn numeric_overrides_replace_defaults() {
        // Every optional numeric flag must land on the corresponding
        // PreconfConfig field with the right unit conversion (ms/secs → Duration).
        // Covers all 7 numeric tuning knobs — regression against silent
        // wiring drift when new flags are added.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.all",
            "--preconf.timeout-ms",
            "500",
            "--preconf.sweep-interval-ms",
            "25",
            "--preconf.max-gas-per-tx",
            "3000000",
            "--preconf.max-gas-per-block",
            "9000000",
            "--preconf.rejournal-interval-secs",
            "30",
            "--preconf.journal-max-size",
            "2147483648",
            "--preconf.broadcast-cap",
            "8192",
        ]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.preconf_timeout, Duration::from_millis(500));
        assert_eq!(cfg.sweep_interval, Duration::from_millis(25));
        assert_eq!(cfg.preconf_max_gas_per_tx, 3_000_000);
        assert_eq!(cfg.preconf_max_gas_per_block, 9_000_000);
        assert_eq!(cfg.rejournal_interval, Duration::from_secs(30));
        assert_eq!(cfg.journal_max_size, 2_147_483_648);
        assert_eq!(cfg.broadcast_cap, 8192);
    }

    #[test]
    fn omitted_numeric_flags_keep_defaults() {
        // If the user only opts in without touching tuning knobs, every
        // numeric field must match the module-level DEFAULT_* constant.
        // Regression guard against silent default drift.
        let a = parse(&["--preconf.enable", "--preconf.all"]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.preconf_timeout, DEFAULT_PRECONF_TIMEOUT);
        assert_eq!(cfg.sweep_interval, DEFAULT_SWEEP_INTERVAL);
        assert_eq!(cfg.preconf_max_gas_per_tx, DEFAULT_PRECONF_MAX_GAS_PER_TX);
        assert_eq!(cfg.preconf_max_gas_per_block, DEFAULT_PRECONF_MAX_GAS_PER_BLOCK);
        assert_eq!(cfg.rejournal_interval, DEFAULT_REJOURNAL_INTERVAL);
        assert_eq!(cfg.journal_max_size, DEFAULT_JOURNAL_MAX_SIZE);
        assert_eq!(cfg.broadcast_cap, DEFAULT_BROADCAST_CAP);
    }
}
