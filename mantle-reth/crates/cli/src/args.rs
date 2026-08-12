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

use alloy_primitives::Address;
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
    /// The journal is always on when preconf is enabled. Omit to use the
    /// datadir-relative default (`<datadir>/mantle-preconf/journal.jsonl`).
    /// The file is opened in append mode; existing contents are preserved
    /// (restart-replay is a separate step).
    #[arg(long = "preconf.journal-path")]
    pub journal_path: Option<PathBuf>,

    /// Treat all transactions as preconf-eligible (bypasses the from/to
    /// allowlist). Aligns with op-geth's `--txpool.allpreconfs`.
    #[arg(long = "preconf.all")]
    pub all: bool,

    /// Address of the L2 `PreconfWhitelist` contract, the single source of
    /// truth for the sender/recipient allowlists.
    ///
    /// Required when `--preconf.enable` is set without `--preconf.all`
    /// (enforced by `PreconfConfig::validate`). The node reads both lists out
    /// of this contract's storage at startup and refreshes them whenever it
    /// emits `WhitelistUpdated`, so the lists are governed on-chain rather than
    /// configured here. There are deliberately no `--preconf.from` /
    /// `--preconf.to` flags: a second, local source of truth would silently
    /// lose to the contract on the first refresh.
    #[arg(long = "preconf.whitelist-contract", value_name = "ADDRESS")]
    pub whitelist_contract: Option<Address>,

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

    /// How many consecutive replay applies may fail before a commitment whose
    /// receipt has already been returned is given up on. Default 3.
    ///
    /// One attempt per payload job, so the default spans roughly
    /// `3 × slot-duration`. Raising it keeps retrying an inapplicable tx (and
    /// keeps its nonce pinned) for longer; lowering it declares commitments
    /// broken sooner. Must be `>= 1`.
    #[arg(long = "preconf.max-apply-attempts")]
    pub max_apply_attempts: Option<u8>,

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
            whitelist_contract: self.whitelist_contract,
            preconf_timeout: self
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_PRECONF_TIMEOUT),
            safety_margin: mantle_reth_preconf::DEFAULT_SAFETY_MARGIN,
            preconf_max_apply_attempts: self
                .max_apply_attempts
                .unwrap_or(mantle_reth_preconf::DEFAULT_MAX_APPLY_ATTEMPTS),
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
        // Minimal opt-in: --preconf.enable alone parses. No whitelist
        // address was given, so this IS a config that `validate()` rejects with
        // `MissingWhitelistContract` — but that's a semantic check enforced at
        // PreconfServiceBuilder::new time, not CLI parse time. Two-layer
        // defense: clap catches syntax, validate() catches semantics.
        let a = parse(&["--preconf.enable"]);
        assert!(a.enable);
        let cfg = a.into_config().expect("enabled");
        assert!(cfg.enabled);
        assert!(!cfg.all_preconfs);
        assert_eq!(cfg.whitelist_contract, None);
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
    fn whitelist_contract_flag_lands_on_config() {
        // `--preconf.whitelist-contract` maps to
        // `PreconfConfig::whitelist_contract`. This is the only allowlist input
        // the CLI accepts — the lists themselves come from that contract.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.whitelist-contract",
            "0x1111111111111111111111111111111111111111",
        ]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.whitelist_contract, Some(Address::from([0x11; 20])));
        // The address is all the CLI carries — the lists themselves live on the
        // classifier and are only populated once the node can read L2 state.
        // ...and this shape passes validation, unlike the address-less one.
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn all_preconfs_needs_no_whitelist_contract() {
        // `--preconf.all` bypasses the contract entirely, so the address stays
        // optional in that mode.
        let a = parse(&["--preconf.enable", "--preconf.all"]);
        let cfg = a.into_config().expect("enabled");
        assert!(cfg.all_preconfs);
        assert_eq!(cfg.whitelist_contract, None);
        assert!(cfg.validate().is_ok());
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
