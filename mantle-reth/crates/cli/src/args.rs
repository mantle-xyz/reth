//! Mantle CLI arguments — flattens upstream `RollupArgs` with three
//! mantle-specific groups: [`PreconfArgs`], [`FlashblocksArgs`] (the
//! sequencer-side publisher) and [`FlashblocksConsumerArgs`].
//!
//! Composition strategy (see `main.rs`):
//!
//! ```text
//! Cli::<MantleChainSpecParser, MantleArgs>::parse().run(|builder, args| {
//!     let mut node = MantleNode::new(args.rollup.clone());
//!     let MantleConfigs { preconf, publisher, consumer } = args.into_configs()?;
//!     // `publisher` is bound onto the preconf service, which `with_preconf` takes
//!     ...
//! })
//! ```
//!
//! [`MantleArgs::into_configs`] is the single resolution point, and the place
//! combinations that cannot work are rejected before the node starts.
//!
//! No flag in these three groups has an env-var fallback: every knob lives on
//! the command line and is discoverable via `--help`.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use alloy_primitives::Address;
use clap::Args;
use mantle_reth_flashblocks::{
    DEFAULT_MAX_CACHE_AHEAD_BLOCKS, DEFAULT_MAX_LEADING_DEPTH, DEFAULT_MAX_TRAILING_DEPTH,
    DEFAULT_SUBSCRIBER_PING_INTERVAL, FlashblocksConfig, MAX_CACHE_AHEAD_BLOCKS,
    MAX_SUBSCRIBER_PING_INTERVAL,
};
use mantle_reth_preconf::{
    FlashblockProducerConfig, FlashblockProducerConfigError, PreconfConfig,
    config::{
        DEFAULT_BROADCAST_CAP, DEFAULT_JOURNAL_MAX_SIZE, DEFAULT_PRECONF_MAX_GAS_PER_BLOCK,
        DEFAULT_PRECONF_MAX_GAS_PER_TX, DEFAULT_PRECONF_QUEUE_GAS_BLOCKS, DEFAULT_PRECONF_TIMEOUT,
        DEFAULT_REJOURNAL_INTERVAL, DEFAULT_SLOT_DURATION, DEFAULT_SWEEP_INTERVAL,
        PreconfConfigError,
    },
    flashblocks::{
        DEFAULT_BEST_TX_LEAD_SLICES, DEFAULT_FLASHBLOCK_ADDR, DEFAULT_FLASHBLOCK_BLOCK_TIME,
        DEFAULT_FLASHBLOCK_LEEWAY, DEFAULT_FLASHBLOCK_PORT,
    },
};
use reth_optimism_node::args::RollupArgs;
use url::Url;

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

    /// Mantle flashblocks publisher arguments (`--flashblocks.*`).
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,

    /// Mantle flashblock consumer arguments (`--flashblocks.*`).
    ///
    /// Kept in its own group rather than merged with [`FlashblocksArgs`]: the
    /// two serve opposite ends of the same stream and validate against
    /// different preconditions. Their flag names are disjoint, so both groups
    /// coexist under the one `--flashblocks.` prefix.
    #[command(flatten)]
    pub flashblocks_consumer: FlashblocksConsumerArgs,
}

impl MantleArgs {
    /// Resolve the three Mantle subsystem configs, rejecting combinations that
    /// cannot work before the node starts.
    ///
    /// Each config is `None` when its subsystem is off. The publisher and the
    /// consumer are resolved independently: running both on one node is
    /// permitted but unusual, and `main` warns about it.
    ///
    /// Preconf is validated first: the publisher's checks read its slot duration.
    ///
    /// # Errors
    /// See [`MantleArgsError`].
    pub fn into_configs(self) -> Result<MantleConfigs, MantleArgsError> {
        let sdm_enabled = self.rollup.operator_sdm_opt_in;
        let preconf = self.preconf.into_config().map(PreconfConfig::validate).transpose()?;
        if preconf.is_some() && self.rollup.sequencer.is_some() {
            return Err(MantleArgsError::PreconfOnForwardingNode);
        }
        if self.flashblocks.enable && self.flashblocks.port == Some(0) {
            return Err(MantleArgsError::ZeroFlashblocksPort);
        }
        let publisher = self
            .flashblocks
            .into_config()
            .map(|cfg| cfg.validate(preconf.as_ref(), sdm_enabled))
            .transpose()?;
        let consumer =
            self.flashblocks_consumer.into_config(self.rollup.flashblocks_url.as_ref())?;

        Ok(MantleConfigs { preconf, publisher, consumer })
    }
}

/// The subsystem configs resolved from [`MantleArgs`].
///
/// Not `PartialEq`: neither [`PreconfConfig`] nor [`FlashblocksConfig`] is.
#[derive(Debug)]
pub struct MantleConfigs {
    /// Preconfirmation service; `None` when `--preconf.enable` was absent.
    pub preconf: Option<PreconfConfig>,
    /// Flashblock publisher; `None` when `--flashblocks.enable` was absent.
    pub publisher: Option<FlashblockProducerConfig>,
    /// Flashblock consumer; `None` when `--flashblocks.websocket-url` was absent.
    pub consumer: Option<FlashblocksConfig>,
}

/// Errors produced while resolving [`MantleArgs`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MantleArgsError {
    /// The preconf config is invalid.
    #[error(transparent)]
    Preconf(#[from] PreconfConfigError),

    /// Preconf was enabled on a node that forwards transactions to a sequencer.
    #[error(
        "--preconf.enable and --rollup.sequencer are mutually exclusive: preconf requests are \
         then answered here instead of being forwarded, and a node that does not build blocks \
         times every one of them out"
    )]
    PreconfOnForwardingNode,

    /// The publisher was asked to bind port 0.
    #[error(
        "--flashblocks.port must not be 0: the OS would pick a port, and nothing downstream could \
         know which"
    )]
    ZeroFlashblocksPort,

    /// The publisher was enabled in a combination that cannot work.
    #[error(transparent)]
    Publisher(#[from] FlashblockProducerConfigError),

    /// The consumer was enabled in a combination that cannot work.
    #[error(transparent)]
    Consumer(#[from] FlashblocksConsumerArgsError),
}

/// Flashblock consumer CLI flags.
///
/// Distinct from upstream's `--flashblocks-url`, which drives op-reth's own
/// consumer. The two implementations are independent and must not both run.
///
/// Separate from [`FlashblocksArgs`] despite sharing the `--flashblocks.`
/// prefix: that group configures the publisher on a sequencer, this one the
/// consumer on an RPC node. The flag names are disjoint.
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct FlashblocksConsumerArgs {
    /// Websocket endpoint streaming flashblocks to the Mantle consumer.
    ///
    /// Named to match the `flashblocks-rpc` deployments already in operation.
    /// Absent (default) leaves the consumer off and the `pending` tag on its
    /// standard local-mempool semantics.
    ///
    /// Every flag in this group also answers to a `--subblocks.` prefix, the
    /// name Optimism's documentation now uses for the same thing.
    #[arg(long = "flashblocks.websocket-url", alias = "subblocks.websocket-url")]
    pub websocket_url: Option<Url>,

    /// Interval between upstream websocket ping frames, in seconds. Default 2,
    /// at most 300.
    #[arg(
        long = "flashblocks.ping-interval-secs",
        alias = "subblocks.ping-interval-secs",
        requires = "websocket_url"
    )]
    pub ping_interval_secs: Option<u64>,

    /// Canonical blocks the pending overlay may trail behind before a rebuild.
    /// Default 3.
    #[arg(
        long = "flashblocks.max-trailing-depth",
        alias = "subblocks.max-trailing-depth",
        requires = "websocket_url"
    )]
    pub max_trailing_depth: Option<u64>,

    /// Blocks the pending overlay may lead canonical by before slices are dropped.
    /// Default 3.
    #[arg(
        long = "flashblocks.max-leading-depth",
        alias = "subblocks.max-leading-depth",
        requires = "websocket_url"
    )]
    pub max_leading_depth: Option<u64>,

    /// Blocks ahead of canonical for which early-arriving slices are cached.
    /// Default 5, at most 256.
    #[arg(
        long = "flashblocks.max-cache-ahead-blocks",
        alias = "subblocks.max-cache-ahead-blocks",
        requires = "websocket_url"
    )]
    pub max_cache_ahead_blocks: Option<u64>,
}

impl FlashblocksConsumerArgs {
    /// Converts CLI args into a [`FlashblocksConfig`] when a consumer URL was
    /// given; otherwise returns `None` (the consumer stays off).
    ///
    /// # Errors
    /// Returns an error when upstream's `--flashblocks-url` is also set, or a
    /// consumer interval or depth is zero.
    pub fn into_config(
        self,
        upstream_flashblocks_url: Option<&Url>,
    ) -> Result<Option<FlashblocksConfig>, FlashblocksConsumerArgsError> {
        let Some(websocket_url) = self.websocket_url else {
            return Ok(None);
        };
        if upstream_flashblocks_url.is_some() {
            return Err(FlashblocksConsumerArgsError::ConflictingConsumers);
        }

        // Checked here rather than through `with_subscriber_ping_interval`,
        // which asserts: a refused flag should name itself to the operator,
        // not abort the process.
        let subscriber_ping_interval = match self.ping_interval_secs {
            Some(0) => return Err(FlashblocksConsumerArgsError::ZeroPingInterval),
            Some(secs) if Duration::from_secs(secs) > MAX_SUBSCRIBER_PING_INTERVAL => {
                return Err(FlashblocksConsumerArgsError::PingIntervalTooLong { secs });
            }
            Some(secs) => Duration::from_secs(secs),
            None => DEFAULT_SUBSCRIBER_PING_INTERVAL,
        };
        if self.max_leading_depth == Some(0) {
            return Err(FlashblocksConsumerArgsError::ZeroLeadingDepth);
        }
        if self.max_trailing_depth == Some(0) {
            return Err(FlashblocksConsumerArgsError::ZeroTrailingDepth);
        }
        match self.max_cache_ahead_blocks {
            Some(0) => return Err(FlashblocksConsumerArgsError::ZeroCacheAheadBlocks),
            Some(blocks) if blocks > MAX_CACHE_AHEAD_BLOCKS => {
                return Err(FlashblocksConsumerArgsError::CacheAheadTooLarge { blocks });
            }
            _ => {}
        }

        Ok(Some(FlashblocksConfig {
            websocket_url,
            max_trailing_depth: self.max_trailing_depth.unwrap_or(DEFAULT_MAX_TRAILING_DEPTH),
            max_leading_depth: self.max_leading_depth.unwrap_or(DEFAULT_MAX_LEADING_DEPTH),
            max_cache_ahead_blocks: self
                .max_cache_ahead_blocks
                .unwrap_or(DEFAULT_MAX_CACHE_AHEAD_BLOCKS),
            subscriber_ping_interval,
        }))
    }
}

/// Errors produced while resolving [`FlashblocksConsumerArgs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FlashblocksConsumerArgsError {
    /// Both the Mantle and the upstream op-reth consumer were requested.
    #[error(
        "--flashblocks.websocket-url and --flashblocks-url are mutually exclusive: the Mantle and \
         op-reth flashblock consumers are independent implementations and must not both run"
    )]
    ConflictingConsumers,
    /// `--flashblocks.ping-interval-secs 0`.
    #[error(
        "--flashblocks.ping-interval-secs must be positive: the subscriber builds a \
         `tokio::time::interval` from it once the node is running, and a zero period panics \
         that task rather than the startup"
    )]
    ZeroPingInterval,
    /// The consumer would drop every slice ahead of canonical.
    #[error("--flashblocks.max-leading-depth must be positive")]
    ZeroLeadingDepth,
    /// The consumer would rebuild on every canonical advance.
    #[error("--flashblocks.max-trailing-depth must be positive")]
    ZeroTrailingDepth,
    /// Early-arriving slices could never be cached.
    #[error("--flashblocks.max-cache-ahead-blocks must be positive")]
    ZeroCacheAheadBlocks,
    /// Longer than [`MAX_SUBSCRIBER_PING_INTERVAL`].
    #[error("--flashblocks.ping-interval-secs ({secs}) must be <= {}", MAX_SUBSCRIBER_PING_INTERVAL.as_secs())]
    PingIntervalTooLong {
        /// Configured interval, in seconds.
        secs: u64,
    },
    /// Wider than [`MAX_CACHE_AHEAD_BLOCKS`].
    #[error("--flashblocks.max-cache-ahead-blocks ({blocks}) must be <= {MAX_CACHE_AHEAD_BLOCKS}")]
    CacheAheadTooLarge {
        /// Configured window, in blocks.
        blocks: u64,
    },
}

/// Flashblocks publisher CLI flags.
///
/// Sequencer-side only. The other flags here are refused without
/// `--flashblocks.enable`. `--flashblocks.enable` requires `--preconf.enable`;
/// the check lives in [`FlashblockProducerConfig::validate`] rather than in
/// clap, because the two flags come from different flattened groups.
///
/// Every flag sets an explicit `id`: clap derives the id from the field
/// name, and `enable` alone would collide with the preconf group.
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct FlashblocksArgs {
    /// Slice each block as it is built and publish the slices.
    ///
    /// Off by default: leaving it off keeps the payload builder's
    /// pre-flashblock behaviour byte for byte, which is the rollback path.
    ///
    /// Every flag in this group also answers to a `--subblocks.` prefix, the
    /// name Optimism's documentation now uses for the same thing.
    #[arg(id = "flashblocks.enable", long = "flashblocks.enable", alias = "subblocks.enable")]
    pub enable: bool,

    /// Address the flashblocks publisher binds. Default `127.0.0.1`.
    #[arg(
        id = "flashblocks.addr",
        long = "flashblocks.addr",
        alias = "subblocks.addr",
        requires = "flashblocks.enable"
    )]
    pub addr: Option<IpAddr>,

    /// Port the flashblocks publisher binds. Default `1111`; 0 is refused.
    #[arg(
        id = "flashblocks.port",
        long = "flashblocks.port",
        alias = "subblocks.port",
        requires = "flashblocks.enable"
    )]
    pub port: Option<u16>,

    /// Interval between slices, in milliseconds. Default 200ms, at least 100ms.
    ///
    /// When flashblocks are enabled this also becomes the payload builder's
    /// ticker cadence, superseding `--preconf.sweep-interval-ms`.
    #[arg(
        id = "flashblocks.block-time",
        long = "flashblocks.block-time",
        alias = "subblocks.block-time",
        requires = "flashblocks.enable"
    )]
    pub block_time_ms: Option<u64>,

    /// How far ahead of the slot deadline the budgeted slice grid finishes,
    /// in milliseconds. Default 0 — the loop runs until `getPayload`, so
    /// unlike the reference implementation there is no self-imposed seal to
    /// reserve time for. A non-zero value only shifts the tick grid earlier.
    #[arg(
        id = "flashblocks.leeway-time",
        long = "flashblocks.leeway-time",
        alias = "subblocks.leeway-time",
        requires = "flashblocks.enable"
    )]
    pub leeway_time_ms: Option<u64>,

    /// Slice boundaries a refreshed pool snapshot waits before the builder
    /// reads from it. Default 1.
    ///
    /// Each slice of lead delays mid-block pool transactions by that much,
    /// widening the head start preconf submissions have; preconf latency is
    /// unchanged. 0 disables it. Must stay below the slices a slot holds.
    #[arg(
        id = "flashblocks.best-tx-lead-slices",
        long = "flashblocks.best-tx-lead-slices",
        alias = "subblocks.best-tx-lead-slices",
        requires = "flashblocks.enable"
    )]
    pub best_tx_lead_slices: Option<u64>,
}

impl FlashblocksArgs {
    /// Convert CLI args into a [`FlashblockProducerConfig`] if
    /// `--flashblocks.enable` was given; otherwise return `None`.
    ///
    /// Ring and broadcast capacities are not exposed on the CLI; they take
    /// their defaults.
    pub fn into_config(self) -> Option<FlashblockProducerConfig> {
        if !self.enable {
            return None;
        }
        Some(FlashblockProducerConfig {
            addr: self.addr.unwrap_or(DEFAULT_FLASHBLOCK_ADDR),
            port: self.port.unwrap_or(DEFAULT_FLASHBLOCK_PORT),
            block_time: self
                .block_time_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_FLASHBLOCK_BLOCK_TIME),
            leeway_time: self
                .leeway_time_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_FLASHBLOCK_LEEWAY),
            best_tx_lead_slices: self.best_tx_lead_slices.unwrap_or(DEFAULT_BEST_TX_LEAD_SLICES),
            ..Default::default()
        })
    }
}

/// Preconfirmation subsystem CLI flags.
///
/// Leave everything at defaults and preconf stays disabled (matches
/// `MantleNode::new` behavior without `.with_preconf()`). The other flags here
/// are refused without `--preconf.enable`, as is `--preconf.enable` together
/// with `--rollup.sequencer`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Default)]
pub struct PreconfArgs {
    /// Enable the mantle preconfirmation subsystem.
    ///
    /// When absent (default), the node behaves exactly like upstream
    /// `op-reth` — no admission, canon handler, or RPC method registration.
    #[arg(long = "preconf.enable")]
    pub enable: bool,

    /// Path to the preconf commitment journal for restart-safety.
    ///
    /// The journal is always on when preconf is enabled. Omit to use the
    /// datadir-relative default (`<datadir>/mantle-preconf/journal.jsonl`).
    /// The file is opened in append mode; existing contents are preserved
    /// (restart-replay is a separate step).
    #[arg(long = "preconf.journal-path", requires = "enable")]
    pub journal_path: Option<PathBuf>,

    /// Treat all transactions as preconf-eligible (bypasses the from/to
    /// allowlist). Aligns with op-geth's `--txpool.allpreconfs`.
    #[arg(long = "preconf.all", requires = "enable")]
    pub all: bool,

    /// Address of the L2 `PreconfWhitelist` contract, the single source of
    /// truth for the sender/recipient allowlists.
    ///
    /// Required when `--preconf.enable` is set without `--preconf.all`
    /// (enforced by `PreconfConfig::validate`). The node reads both lists out
    /// of this contract's storage at startup and refreshes them whenever it
    /// emits `WhitelistUpdated`, so the lists are governed on-chain — this
    /// address is the only allowlist input the CLI accepts.
    #[arg(long = "preconf.whitelist-contract", value_name = "ADDRESS", requires = "enable")]
    pub whitelist_contract: Option<Address>,

    /// Client-visible RPC oneshot timeout, in milliseconds. Default matches
    /// [`mantle_reth_preconf::config::DEFAULT_PRECONF_TIMEOUT`] (1s). Must
    /// exceed the 40ms dispatch safety margin, or no request would be tried.
    #[arg(long = "preconf.timeout-ms", requires = "enable")]
    pub timeout_ms: Option<u64>,

    /// Payload-builder sweep-ticker interval, in milliseconds — cadence at
    /// which the builder admits a fresh batch of pool best-txs. Each tick
    /// admits pool txs up to the time-proportional cumulative gas quota
    /// (see `--preconf.slot-duration-ms`). Default 200ms.
    #[arg(long = "preconf.sweep-interval-ms", requires = "enable")]
    pub sweep_interval_ms: Option<u64>,

    /// Slot duration in milliseconds — the ceiling on the remaining-time input
    /// to the pool best-tx gas schedule. `derive_pool_quota_schedule` clamps
    /// the time left to the slot deadline at this value, then divides
    /// `block_gas_limit` evenly across the ticks that fit in it. The tick is
    /// `--flashblocks.block-time` when the publisher is enabled and
    /// `--preconf.sweep-interval-ms` otherwise. Default 2000ms (OP-stack block
    /// time), at most 60000ms. Change only on chains with non-standard cadence.
    #[arg(long = "preconf.slot-duration-ms", requires = "enable")]
    pub slot_duration_ms: Option<u64>,

    /// Per-tx gas cap for preconf-eligible txs. Default `2_000_000`.
    #[arg(long = "preconf.max-gas-per-tx", requires = "enable")]
    pub max_gas_per_tx: Option<u64>,

    /// Cumulative preconf gas budget per block. Default `6_000_000`. Must be
    /// `>=` `preconf.max-gas-per-tx` (checked by `PreconfConfig::validate`).
    #[arg(long = "preconf.max-gas-per-block", requires = "enable")]
    pub max_gas_per_block: Option<u64>,

    /// How much backlog the preconf queue will hold, in blocks' worth of
    /// `preconf.max-gas-per-block`. Default `8` — a backstop against a flood,
    /// not a restatement of the per-block budget: overflowing a single block is
    /// ordinary and recoverable, and refusing that at admission would remove a
    /// recovery path that exists on purpose. See
    /// `DEFAULT_PRECONF_QUEUE_GAS_BLOCKS`.
    #[arg(long = "preconf.queue-gas-blocks", requires = "enable")]
    pub queue_gas_blocks: Option<u64>,

    /// Journal rotation interval, in seconds. Default 60s, at most 86400 (one
    /// day). Always in effect
    /// when preconf is enabled: the journal has no disabled mode, and an unset
    /// `--preconf.journal-path` only selects the datadir default.
    #[arg(long = "preconf.rejournal-interval-secs", requires = "enable")]
    pub rejournal_interval_secs: Option<u64>,

    /// Journal file size ceiling, in bytes. Default 1 GiB (`1_073_741_824`).
    /// Above this, rotation renames the current file and starts a new one.
    /// Always in effect when preconf is enabled, like
    /// `--preconf.rejournal-interval-secs`.
    #[arg(long = "preconf.journal-max-size", requires = "enable")]
    pub journal_max_size: Option<u64>,

    /// Broadcast channel capacity. Default 65536, at most 1048576. Advanced
    /// tuning knob — sizes
    /// the fifo notifier broadcast channel; consumers see `Lagged(n)` and fall
    /// back to snapshot reconcile when full.
    #[arg(long = "preconf.broadcast-cap", requires = "enable")]
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
            preconf_queue_gas_blocks: self
                .queue_gas_blocks
                .unwrap_or(DEFAULT_PRECONF_QUEUE_GAS_BLOCKS),
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
        // `MissingWhitelistContract` — a semantic check, which
        // `MantleArgs::into_configs` runs, not clap.
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
        // `PreconfConfig::whitelist_contract`.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.whitelist-contract",
            "0x1111111111111111111111111111111111111111",
        ]);
        let cfg = a.into_config().expect("enabled");
        assert_eq!(cfg.whitelist_contract, Some(Address::from([0x11; 20])));
        // And unlike the address-less shape above, this one passes validation.
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
        // Covers all 9 numeric tuning knobs — regression against silent
        // wiring drift when new flags are added.
        let a = parse(&[
            "--preconf.enable",
            "--preconf.all",
            "--preconf.timeout-ms",
            "500",
            "--preconf.sweep-interval-ms",
            "25",
            "--preconf.slot-duration-ms",
            "3000",
            "--preconf.queue-gas-blocks",
            "4",
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
        assert_eq!(cfg.slot_duration, Duration::from_millis(3000));
        assert_eq!(cfg.preconf_queue_gas_blocks, 4);
        assert_eq!(cfg.preconf_max_gas_per_tx, 3_000_000);
        assert_eq!(cfg.preconf_max_gas_per_block, 9_000_000);
        assert_eq!(cfg.rejournal_interval, Duration::from_secs(30));
        assert_eq!(cfg.journal_max_size, 2_147_483_648);
        assert_eq!(cfg.broadcast_cap, 8192);
    }

    /// Test helper mirroring [`parse`] for the flashblocks group.
    #[derive(Parser, Debug)]
    struct TestFlashblocksCli {
        #[command(flatten)]
        args: FlashblocksArgs,
    }

    fn parse_flashblocks(argv: &[&str]) -> FlashblocksArgs {
        TestFlashblocksCli::parse_from(std::iter::once(&"reth").chain(argv.iter())).args
    }

    #[test]
    fn flashblocks_default_parse_yields_disabled() {
        // No flags → into_config() returns None → flashblocks stay off,
        // matching how the preconf group signals the same thing.
        assert!(parse_flashblocks(&[]).into_config().is_none());
    }

    #[test]
    fn flashblocks_numeric_overrides_replace_defaults() {
        let cfg = parse_flashblocks(&[
            "--flashblocks.enable",
            "--flashblocks.addr",
            "0.0.0.0",
            "--flashblocks.port",
            "2222",
            "--flashblocks.block-time",
            "250",
            "--flashblocks.leeway-time",
            "0",
            "--flashblocks.best-tx-lead-slices",
            "1",
        ])
        .into_config()
        .expect("enabled");

        assert_eq!(cfg.addr, IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        assert_eq!(cfg.port, 2222);
        assert_eq!(cfg.block_time, Duration::from_millis(250));
        assert_eq!(cfg.leeway_time, Duration::ZERO);
        assert_eq!(cfg.best_tx_lead_slices, 1);
    }

    #[test]
    fn flashblocks_omitted_flags_keep_defaults() {
        let cfg = parse_flashblocks(&["--flashblocks.enable"]).into_config().expect("enabled");

        assert_eq!(cfg.addr, DEFAULT_FLASHBLOCK_ADDR);
        assert_eq!(cfg.port, DEFAULT_FLASHBLOCK_PORT);
        assert_eq!(cfg.block_time, DEFAULT_FLASHBLOCK_BLOCK_TIME);
        assert_eq!(cfg.leeway_time, DEFAULT_FLASHBLOCK_LEEWAY);
        assert_eq!(
            cfg.best_tx_lead_slices, 1,
            "the default holds a refreshed snapshot back by one slice"
        );
    }

    /// Optimism renamed flashblocks to subblocks in its public documentation
    /// while keeping `flashblocks` as the canonical flag name. We mirror that:
    /// every publisher flag also answers to `--subblocks.`.
    #[test]
    fn subblocks_aliases_reach_the_publisher_flags() {
        let cfg = parse_flashblocks(&[
            "--subblocks.enable",
            "--subblocks.addr",
            "0.0.0.0",
            "--subblocks.port",
            "2222",
            "--subblocks.block-time",
            "250",
            "--subblocks.leeway-time",
            "0",
            "--subblocks.best-tx-lead-slices",
            "2",
        ])
        .into_config()
        .expect("enabled");

        assert_eq!(cfg.addr, IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        assert_eq!(cfg.port, 2222);
        assert_eq!(cfg.block_time, Duration::from_millis(250));
        assert_eq!(cfg.leeway_time, Duration::ZERO);
        assert_eq!(cfg.best_tx_lead_slices, 2);
    }

    /// The two groups are validated against each other, and enabling
    /// flashblocks without preconf is rejected before the node starts.
    #[test]
    fn flashblocks_without_preconf_is_rejected_at_startup() {
        let flashblocks =
            parse_flashblocks(&["--flashblocks.enable"]).into_config().expect("enabled");
        let preconf = parse(&["--preconf.enable", "--preconf.all"]).into_config();

        assert!(flashblocks.clone().validate(preconf.as_ref(), false).is_ok());
        assert!(flashblocks.validate(None, false).is_err());
    }

    /// Test helper covering the whole arg surface, so the startup gate is
    /// exercised the way `main` reaches it.
    #[derive(Parser, Debug)]
    struct TestMantleCli {
        #[command(flatten)]
        args: MantleArgs,
    }

    fn parse_mantle(argv: &[&str]) -> MantleArgs {
        TestMantleCli::parse_from(std::iter::once(&"reth").chain(argv.iter())).args
    }

    #[test]
    fn enabling_flashblocks_alone_fails_the_startup_gate() {
        let err = parse_mantle(&["--flashblocks.enable"]).into_configs().unwrap_err();

        assert_eq!(
            err,
            MantleArgsError::Publisher(FlashblockProducerConfigError::RequiresPreconfEnabled)
        );
    }

    #[test]
    fn enabling_both_passes_the_startup_gate() {
        let cfgs = parse_mantle(&["--preconf.enable", "--preconf.all", "--flashblocks.enable"])
            .into_configs()
            .expect("both enabled is a valid combination");

        assert!(cfgs.preconf.is_some());
        assert!(cfgs.publisher.is_some());
        assert!(cfgs.consumer.is_none());
    }

    /// Boot-time gate only: this proves `--rollup.operator-sdm-opt-in` is
    /// wired into the publisher's startup check, not that the combination is
    /// refused everywhere — `admin_setOperatorSdmOptIn` still toggles SDM on
    /// at runtime, after this check has already passed.
    #[test]
    fn enabling_flashblocks_with_sdm_fails_the_startup_gate() {
        let err = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--flashblocks.enable",
            "--rollup.operator-sdm-opt-in=true",
        ])
        .into_configs()
        .unwrap_err();

        assert_eq!(
            err,
            MantleArgsError::Publisher(
                FlashblockProducerConfigError::SdmNotSupportedWithFlashblocks
            )
        );
    }

    /// Preconf on, no flashblocks flag: the publisher config stays absent, so
    /// nothing downstream has to tolerate a half-configured producer.
    #[test]
    fn enabling_preconf_alone_leaves_flashblocks_absent() {
        let cfgs = parse_mantle(&["--preconf.enable", "--preconf.all"])
            .into_configs()
            .expect("preconf alone is a valid combination");

        assert!(cfgs.preconf.is_some());
        assert!(cfgs.publisher.is_none());
    }

    #[test]
    fn enabling_neither_passes_the_startup_gate() {
        let cfgs = parse_mantle(&[]).into_configs().expect("defaults are valid");

        assert!(cfgs.preconf.is_none());
        assert!(cfgs.publisher.is_none());
        assert!(cfgs.consumer.is_none());
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

    /// Wraps the full `MantleArgs` so the flashblock flags are parsed alongside
    /// `RollupArgs`: upstream registers `--flashblocks-url` there, and a clash
    /// with our own long names would make clap panic on startup.
    #[derive(Parser, Debug)]
    struct FlashblocksConsumerTestCli {
        #[command(flatten)]
        args: MantleArgs,
    }

    fn parse_consumer(argv: &[&str]) -> MantleArgs {
        FlashblocksConsumerTestCli::parse_from(std::iter::once(&"reth").chain(argv.iter())).args
    }

    #[test]
    fn no_url_leaves_the_consumer_off() {
        let args = parse_consumer(&[]);
        assert!(
            args.flashblocks_consumer
                .into_config(args.rollup.flashblocks_url.as_ref())
                .expect("no conflict")
                .is_none()
        );
    }

    #[test]
    fn websocket_url_enables_the_consumer_with_defaults() {
        // The flag name matches the `flashblocks-rpc` deployments in operation.
        let args = parse_consumer(&["--flashblocks.websocket-url", "ws://sequencer:1111"]);
        let cfg = args
            .flashblocks_consumer
            .into_config(args.rollup.flashblocks_url.as_ref())
            .expect("no conflict")
            .expect("enabled");
        assert_eq!(cfg.websocket_url.as_str(), "ws://sequencer:1111/");
        assert_eq!(cfg.max_trailing_depth, DEFAULT_MAX_TRAILING_DEPTH);
        assert_eq!(cfg.max_leading_depth, DEFAULT_MAX_LEADING_DEPTH);
        assert_eq!(cfg.max_cache_ahead_blocks, DEFAULT_MAX_CACHE_AHEAD_BLOCKS);
        assert_eq!(cfg.subscriber_ping_interval, DEFAULT_SUBSCRIBER_PING_INTERVAL);
    }

    /// `tokio::time::interval` panics on a zero period, and the subscriber
    /// builds one from this value only once the node is already running. The
    /// refusal belongs here, where it still reaches an operator.
    #[test]
    fn a_zero_ping_interval_is_refused() {
        let args = parse_consumer(&[
            "--flashblocks.websocket-url",
            "ws://sequencer:1111",
            "--flashblocks.ping-interval-secs",
            "0",
        ]);
        assert!(matches!(
            args.flashblocks_consumer.into_config(args.rollup.flashblocks_url.as_ref()),
            Err(FlashblocksConsumerArgsError::ZeroPingInterval)
        ));
    }

    #[test]
    fn zero_consumer_depths_are_refused_with_the_flag_name() {
        for prefix in ["flashblocks", "subblocks"] {
            for name in ["max-leading-depth", "max-trailing-depth", "max-cache-ahead-blocks"] {
                let flag = format!("--{prefix}.{name}");
                let args = parse_consumer(&[
                    "--flashblocks.websocket-url",
                    "ws://sequencer:1111",
                    &flag,
                    "0",
                ]);
                let err = args
                    .flashblocks_consumer
                    .into_config(args.rollup.flashblocks_url.as_ref())
                    .expect_err("zero depth must refuse startup");
                assert!(err.to_string().contains(&format!("--flashblocks.{name}")));
            }
        }
    }

    #[test]
    fn minimum_positive_consumer_depths_are_accepted() {
        let args = parse_consumer(&[
            "--flashblocks.websocket-url",
            "ws://sequencer:1111",
            "--flashblocks.max-leading-depth",
            "1",
            "--flashblocks.max-trailing-depth",
            "1",
            "--flashblocks.max-cache-ahead-blocks",
            "1",
        ]);
        let cfg = args.flashblocks_consumer.into_config(None).unwrap().unwrap();
        assert_eq!(
            (cfg.max_leading_depth, cfg.max_trailing_depth, cfg.max_cache_ahead_blocks),
            (1, 1, 1)
        );
    }

    #[test]
    fn tuning_flags_override_defaults() {
        let args = parse_consumer(&[
            "--flashblocks.websocket-url",
            "ws://sequencer:1111",
            "--flashblocks.ping-interval-secs",
            "5",
            "--flashblocks.max-trailing-depth",
            "7",
            "--flashblocks.max-leading-depth",
            "9",
            "--flashblocks.max-cache-ahead-blocks",
            "11",
        ]);
        let cfg = args
            .flashblocks_consumer
            .into_config(args.rollup.flashblocks_url.as_ref())
            .expect("no conflict")
            .expect("enabled");
        assert_eq!(cfg.subscriber_ping_interval, Duration::from_secs(5));
        assert_eq!(cfg.max_trailing_depth, 7);
        assert_eq!(cfg.max_leading_depth, 9);
        assert_eq!(cfg.max_cache_ahead_blocks, 11);
    }

    /// Same mirror of Optimism's naming on the consumer side. The alias must
    /// not disturb upstream's own `--flashblocks-url`, which stays distinct.
    #[test]
    fn subblocks_aliases_reach_the_consumer_flags() {
        let args = parse_consumer(&[
            "--subblocks.websocket-url",
            "ws://sequencer:1111",
            "--subblocks.ping-interval-secs",
            "5",
            "--subblocks.max-trailing-depth",
            "7",
            "--subblocks.max-leading-depth",
            "9",
            "--subblocks.max-cache-ahead-blocks",
            "11",
        ]);
        let cfg = args
            .flashblocks_consumer
            .into_config(args.rollup.flashblocks_url.as_ref())
            .expect("no conflict")
            .expect("enabled");
        assert_eq!(cfg.websocket_url.as_str(), "ws://sequencer:1111/");
        assert_eq!(cfg.subscriber_ping_interval, Duration::from_secs(5));
        assert_eq!(cfg.max_trailing_depth, 7);
        assert_eq!(cfg.max_leading_depth, 9);
        assert_eq!(cfg.max_cache_ahead_blocks, 11);
    }

    #[test]
    fn both_consumers_is_rejected() {
        // Two independent implementations would each subscribe and each build
        // pending state; the node must refuse to start rather than pick one.
        let args = parse_consumer(&[
            "--flashblocks.websocket-url",
            "ws://sequencer:1111",
            "--flashblocks-url",
            "ws://other:2222",
        ]);
        assert_eq!(
            args.flashblocks_consumer
                .into_config(args.rollup.flashblocks_url.as_ref())
                .unwrap_err(),
            FlashblocksConsumerArgsError::ConflictingConsumers
        );
    }

    /// The publisher and the consumer groups share the `--flashblocks.` prefix
    /// but no flag name, so clap accepts both on one command line and each
    /// resolves independently. Running both is unusual, not an error; `main`
    /// warns.
    #[test]
    fn the_publisher_and_the_consumer_can_be_enabled_together() {
        let cfgs = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--flashblocks.enable",
            "--flashblocks.websocket-url",
            "ws://sequencer:1111",
        ])
        .into_configs()
        .expect("both ends on one node is permitted");

        assert!(cfgs.publisher.is_some());
        assert!(cfgs.consumer.is_some());
    }

    /// Preconf is validated before the publisher reads it: an invalid preconf
    /// config is refused even where the publisher's own checks would pass.
    #[test]
    fn an_invalid_preconf_config_fails_the_startup_gate() {
        let err = parse_mantle(&["--preconf.enable"]).into_configs().unwrap_err();
        assert_eq!(err, MantleArgsError::Preconf(PreconfConfigError::MissingWhitelistContract));

        let err = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--preconf.slot-duration-ms",
            "60001",
            "--flashblocks.enable",
        ])
        .into_configs()
        .unwrap_err();
        assert!(
            matches!(err, MantleArgsError::Preconf(PreconfConfigError::SlotDurationTooLong { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn a_preconf_timeout_within_the_safety_margin_fails_the_startup_gate() {
        let err =
            parse_mantle(&["--preconf.enable", "--preconf.all", "--preconf.timeout-ms", "40"])
                .into_configs()
                .unwrap_err();
        assert!(
            matches!(
                err,
                MantleArgsError::Preconf(PreconfConfigError::TimeoutWithinSafetyMargin { .. })
            ),
            "{err:?}"
        );
    }

    /// A node with `--rollup.sequencer` forwards transactions instead of
    /// building blocks; preconf there would answer requests locally and time
    /// every one of them out.
    #[test]
    fn preconf_on_a_forwarding_node_fails_the_startup_gate() {
        let err = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--rollup.sequencer",
            "http://sequencer:8545",
        ])
        .into_configs()
        .unwrap_err();
        assert_eq!(err, MantleArgsError::PreconfOnForwardingNode);

        assert!(
            parse_mantle(&["--rollup.sequencer", "http://sequencer:8545"]).into_configs().is_ok(),
            "forwarding without preconf is the ordinary follower"
        );
    }

    #[test]
    fn a_flashblocks_block_time_below_the_minimum_fails_the_startup_gate() {
        let err = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--flashblocks.enable",
            "--flashblocks.block-time",
            "99",
        ])
        .into_configs()
        .unwrap_err();
        assert!(
            matches!(
                err,
                MantleArgsError::Publisher(FlashblockProducerConfigError::BlockTimeTooShort { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn flashblocks_port_zero_fails_the_startup_gate() {
        let err = parse_mantle(&[
            "--preconf.enable",
            "--preconf.all",
            "--flashblocks.enable",
            "--flashblocks.port",
            "0",
        ])
        .into_configs()
        .unwrap_err();
        assert_eq!(err, MantleArgsError::ZeroFlashblocksPort);
    }

    /// Every tuning flag of a group is refused at parse time without the
    /// group's switch, and accepted with it.
    #[test]
    fn tuning_flags_require_their_switch() {
        let preconf = [
            ("--preconf.journal-path", Some("/tmp/journal")),
            ("--preconf.all", None),
            ("--preconf.whitelist-contract", Some("0x1111111111111111111111111111111111111111")),
            ("--preconf.timeout-ms", Some("500")),
            ("--preconf.sweep-interval-ms", Some("100")),
            ("--preconf.slot-duration-ms", Some("2000")),
            ("--preconf.max-gas-per-tx", Some("1")),
            ("--preconf.max-gas-per-block", Some("1")),
            ("--preconf.queue-gas-blocks", Some("1")),
            ("--preconf.rejournal-interval-secs", Some("1")),
            ("--preconf.journal-max-size", Some("1")),
            ("--preconf.broadcast-cap", Some("1")),
        ];
        let publisher = [
            ("--flashblocks.addr", Some("0.0.0.0")),
            ("--flashblocks.port", Some("2222")),
            ("--flashblocks.block-time", Some("200")),
            ("--flashblocks.leeway-time", Some("0")),
            ("--flashblocks.best-tx-lead-slices", Some("1")),
        ];
        let consumer = [
            ("--flashblocks.ping-interval-secs", Some("2")),
            ("--flashblocks.max-trailing-depth", Some("3")),
            ("--flashblocks.max-leading-depth", Some("3")),
            ("--flashblocks.max-cache-ahead-blocks", Some("5")),
        ];
        let try_parse = |argv: &[&str]| {
            TestMantleCli::try_parse_from(std::iter::once(&"reth").chain(argv.iter()))
        };
        for (switch, flags) in [
            (&["--preconf.enable"][..], &preconf[..]),
            (&["--flashblocks.enable"][..], &publisher[..]),
            (&["--flashblocks.websocket-url", "ws://sequencer:1111"][..], &consumer[..]),
        ] {
            for &(name, value) in flags {
                let flag: Vec<&str> = std::iter::once(name).chain(value).collect();

                let err = try_parse(&flag).expect_err(name);
                assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument, "{name}");

                let with_switch: Vec<&str> = switch.iter().copied().chain(flag).collect();
                assert!(try_parse(&with_switch).is_ok(), "{name} with {switch:?}");
            }
        }
    }

    #[test]
    fn consumer_intervals_above_their_maximum_fail() {
        let consumer = |argv: &[&str]| {
            let mut full = vec!["--flashblocks.websocket-url", "ws://sequencer:1111"];
            full.extend_from_slice(argv);
            parse_mantle(&full).flashblocks_consumer.into_config(None)
        };

        assert_eq!(
            consumer(&["--flashblocks.ping-interval-secs", "301"]).unwrap_err(),
            FlashblocksConsumerArgsError::PingIntervalTooLong { secs: 301 },
        );
        assert!(consumer(&["--flashblocks.ping-interval-secs", "300"]).is_ok());
        assert_eq!(
            consumer(&["--flashblocks.max-cache-ahead-blocks", "257"]).unwrap_err(),
            FlashblocksConsumerArgsError::CacheAheadTooLarge { blocks: 257 },
        );
        assert!(consumer(&["--flashblocks.max-cache-ahead-blocks", "256"]).is_ok());
    }
}
