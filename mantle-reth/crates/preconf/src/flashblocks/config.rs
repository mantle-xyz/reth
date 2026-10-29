//! Producer-side flashblocks configuration.
//!
//! Deliberately separate from [`PreconfConfig`]: the node builds a default
//! `PreconfConfig` when preconf is off, which would wipe any flashblock
//! values that had been folded into it.
//!
//! "Off" is the absence of a config, not a field inside one — same shape as
//! preconf at the CLI boundary.

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use crate::PreconfConfig;

/// Default interval between slices — 200ms.
pub const DEFAULT_FLASHBLOCK_BLOCK_TIME: Duration = Duration::from_millis(200);

/// Shortest interval between slices accepted — 100ms. Below it the builder
/// spends the slot publishing slices rather than executing.
pub const MIN_FLASHBLOCK_BLOCK_TIME: Duration = Duration::from_millis(100);

/// Default margin by which the budgeted slice grid finishes ahead of the
/// slot deadline — none.
///
/// The reference implementation deducts a leeway because it seals the block
/// itself once the budgeted slices are out, and the margin is the time that
/// seal needs. This builder does not: the loop runs until `getPayload`
/// cancels it, so there is no self-imposed seal to reserve time for, and the
/// closing slice already covers whatever executed after the last tick.
///
/// What remains is the effect on the grid, which is why the knob is kept:
/// the leeway is subtracted from the drift, so it shifts every tick earlier.
/// Whether it also changes how many fire depends on where the subtraction
/// lands — `tick_count` is `ceil((drift - leeway) / interval)`, so a leeway
/// that carries the window past a multiple of the interval drops a tick, even
/// one far below an interval. From a full, interval-aligned slot it does not.
/// A non-zero value buys one thing — the last budgeted
/// slice publishes that much before the payload is asked for, instead of
/// racing it — and costs one extra slice assembly, which roots every
/// transaction in the block, at the point in the slot where the critical
/// path already is.
pub const DEFAULT_FLASHBLOCK_LEEWAY: Duration = Duration::ZERO;

/// Default address the publisher binds — loopback.
pub const DEFAULT_FLASHBLOCK_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Default port the publisher binds.
pub const DEFAULT_FLASHBLOCK_PORT: u16 = 1111;

/// Default number of published slices kept for resuming subscribers — 32.
///
/// At eleven slices per block that is a little under three blocks, so a
/// subscriber has roughly six seconds to reconnect before it has to fall
/// back to canonical sync.
///
/// That six seconds is a budget shared with the consumer's keepalive, which
/// is sized to notice a dead connection well inside it
/// (`DEFAULT_SUBSCRIBER_PING_INTERVAL` in `mantle-reth-flashblocks`). Shrink
/// this capacity and that interval has to follow, or a reconnect arrives after
/// the slices it needed were already dropped.
pub const DEFAULT_RING_CAPACITY: usize = 32;

/// Default capacity of the live broadcast channel — 32.
pub const DEFAULT_BROADCAST_CAPACITY: usize = 32;

/// Default boundaries a pool snapshot waits before it is used — 1.
///
/// A mid-block pool transaction is therefore offered to the builder a slice
/// later than it otherwise would be, which buys preconf submissions a further
/// slice of head start. `0` disables the lead.
pub const DEFAULT_BEST_TX_LEAD_SLICES: u64 = 1;

/// Runtime configuration for publishing flashblocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashblockProducerConfig {
    /// Interval between slices. At least [`MIN_FLASHBLOCK_BLOCK_TIME`].
    pub block_time: Duration,
    /// How far ahead of the slot deadline the budgeted slice grid finishes.
    /// Shifts the tick grid; does not end the build, which runs until
    /// `getPayload` cancels it.
    pub leeway_time: Duration,
    /// Address the publisher binds.
    pub addr: IpAddr,
    /// Port the publisher binds. `0` lets the OS pick (tests use it); the CLI
    /// refuses it.
    pub port: u16,
    /// Published slices kept for subscribers that reconnect.
    pub ring_capacity: usize,
    /// Capacity of the live broadcast channel.
    pub broadcast_capacity: usize,
    /// Slice boundaries a refreshed pool snapshot waits before it is used.
    pub best_tx_lead_slices: u64,
}

impl Default for FlashblockProducerConfig {
    fn default() -> Self {
        Self {
            block_time: DEFAULT_FLASHBLOCK_BLOCK_TIME,
            leeway_time: DEFAULT_FLASHBLOCK_LEEWAY,
            addr: DEFAULT_FLASHBLOCK_ADDR,
            port: DEFAULT_FLASHBLOCK_PORT,
            ring_capacity: DEFAULT_RING_CAPACITY,
            broadcast_capacity: DEFAULT_BROADCAST_CAPACITY,
            best_tx_lead_slices: DEFAULT_BEST_TX_LEAD_SLICES,
        }
    }
}

/// Errors surfaced by [`FlashblockProducerConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlashblockProducerConfigError {
    /// `block_time` below [`MIN_FLASHBLOCK_BLOCK_TIME`].
    #[error("--flashblocks.block-time ({block_time:?}) must be >= {min:?}")]
    BlockTimeTooShort {
        /// Configured slice interval.
        block_time: Duration,
        /// Shortest accepted interval.
        min: Duration,
    },
    /// `broadcast_capacity == 0` — tokio broadcast requires capacity > 0.
    #[error("flashblocks broadcast_capacity must be > 0")]
    InvalidBroadcastCapacity,
    /// A single slice would span more than the whole slot, which leaves the
    /// per-slice budget meaningless.
    #[error(
        "--flashblocks.block-time ({block_time:?}) must be <= --preconf.slot-duration-ms ({slot:?})"
    )]
    BlockTimeExceedsSlot {
        /// Configured slice interval.
        block_time: Duration,
        /// Slot duration the interval was compared against.
        slot: Duration,
    },
    /// Leeway at or past a whole slot leaves no time to build in.
    #[error(
        "--flashblocks.leeway-time ({leeway:?}) must be < --preconf.slot-duration-ms ({slot:?})"
    )]
    LeewayReachesSlot {
        /// Configured leeway.
        leeway: Duration,
        /// Slot duration the leeway was compared against.
        slot: Duration,
    },
    /// The archive is smaller than the live channel. A subscriber may lag by a
    /// whole channel before it is dropped, so it could fall behind by more
    /// than the ring can replay and never be able to resume.
    #[error("flashblocks ring_capacity ({ring}) must be >= broadcast_capacity ({broadcast})")]
    RingSmallerThanBroadcast {
        /// Configured ring capacity.
        ring: usize,
        /// Configured broadcast capacity.
        broadcast: usize,
    },
    /// Flashblocks were configured without preconf. Slicing lives in the
    /// preconf payload builder, and with preconf off the node builds blocks
    /// with the upstream builder, so the flag would have no effect.
    #[error("--flashblocks.enable requires --preconf.enable")]
    RequiresPreconfEnabled,
    /// SDM production and flashblocks publishing are both on. The producer
    /// appends the SDM post-exec transaction to the closing slice's
    /// `transactions`; the consumer short-circuits it to a state-free no-op
    /// rather than running it, the same way the canonical block executor
    /// does — the gas refunds the entries describe are applied by the
    /// surrounding post-exec machinery, not by executing this transaction.
    /// What the consumer skips are the two payload checks the canonical
    /// executor additionally runs against it, both in the `alloy-op-evm`
    /// dependency (`block/mod.rs`): `PostExecState::verify_post_exec_tx`, which
    /// matches the transaction's payload against the entries the executor
    /// accumulated, and `OpBlockExecutor::verifier_post_exec_refund_for_tx`,
    /// which rejects a payload entry that targets the post-exec transaction's
    /// own index. The consumer's replayed pending/preconf overlay does not
    /// reproduce that surrounding machinery, so it omits the refunds until the
    /// next canonical block resynchronises state — a transient pending-state
    /// inaccuracy, not a chain split. The 1.6.3 wire protocol carries the
    /// post-exec transaction out of band instead, in the slice's
    /// `post_exec_tx` field and with replace-not-append semantics; no producer
    /// fills it yet.
    ///
    /// This only gates the boot-time flag (`--rollup.operator-sdm-opt-in`):
    /// `admin_setOperatorSdmOptIn` can still flip SDM on at runtime after a
    /// node has started with flashblocks already publishing.
    #[error(
        "--flashblocks.enable refuses this startup configuration while \
         --rollup.operator-sdm-opt-in is also set, because the post-exec \
         transaction is not yet carried out of band; this does not prevent \
         enabling SDM at runtime via admin_setOperatorSdmOptIn"
    )]
    SdmNotSupportedWithFlashblocks,
    /// The lead spans the slot, so no refresh ever matures before the seal.
    #[error(
        "--flashblocks.best-tx-lead-slices ({lead}) must be less than the slices in a slot \
         ({slices}: --preconf.slot-duration-ms divided by --flashblocks.block-time); lower \
         --flashblocks.best-tx-lead-slices or --flashblocks.block-time"
    )]
    BestTxLeadSpansSlot {
        /// Configured lead, in slice boundaries.
        lead: u64,
        /// Slices a slot holds at the configured interval.
        slices: u64,
    },
}

impl FlashblockProducerConfig {
    /// Validates config invariants against the preconf config it runs
    /// alongside. `preconf` is `None` when preconf is disabled. `sdm_enabled`
    /// is the boot-time SDM opt-in (`--rollup.operator-sdm-opt-in`); it is
    /// checked unconditionally because no other field in this config can make
    /// that combination safe.
    ///
    /// Returns the original config on success for ergonomic chaining.
    pub fn validate(
        self,
        preconf: Option<&PreconfConfig>,
        sdm_enabled: bool,
    ) -> Result<Self, FlashblockProducerConfigError> {
        if sdm_enabled {
            return Err(FlashblockProducerConfigError::SdmNotSupportedWithFlashblocks);
        }
        if self.block_time < MIN_FLASHBLOCK_BLOCK_TIME {
            return Err(FlashblockProducerConfigError::BlockTimeTooShort {
                block_time: self.block_time,
                min: MIN_FLASHBLOCK_BLOCK_TIME,
            });
        }
        if self.broadcast_capacity == 0 {
            return Err(FlashblockProducerConfigError::InvalidBroadcastCapacity);
        }
        if self.ring_capacity < self.broadcast_capacity {
            return Err(FlashblockProducerConfigError::RingSmallerThanBroadcast {
                ring: self.ring_capacity,
                broadcast: self.broadcast_capacity,
            });
        }

        let Some(preconf) = preconf else {
            return Err(FlashblockProducerConfigError::RequiresPreconfEnabled);
        };

        if self.block_time > preconf.slot_duration {
            return Err(FlashblockProducerConfigError::BlockTimeExceedsSlot {
                block_time: self.block_time,
                slot: preconf.slot_duration,
            });
        }
        if self.leeway_time >= preconf.slot_duration {
            return Err(FlashblockProducerConfigError::LeewayReachesSlot {
                leeway: self.leeway_time,
                slot: preconf.slot_duration,
            });
        }
        // The checks above leave `block_time` non-zero and within the slot, so
        // this divides safely and yields at least 1.
        let slices = (preconf.slot_duration.as_nanos() / self.block_time.as_nanos()) as u64;
        if self.best_tx_lead_slices >= slices {
            return Err(FlashblockProducerConfigError::BestTxLeadSpansSlot {
                lead: self.best_tx_lead_slices,
                slices,
            });
        }

        Ok(self)
    }
}

/// Interval the build loop's ticker fires at.
///
/// Without flashblocks the cadence stays exactly what preconf used before.
pub fn tick_interval(
    flashblocks: Option<&FlashblockProducerConfig>,
    preconf_sweep: Duration,
) -> Duration {
    flashblocks.map_or(preconf_sweep, |cfg| cfg.block_time)
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };

    use super::{
        FlashblockProducerConfig, FlashblockProducerConfigError, MIN_FLASHBLOCK_BLOCK_TIME,
        tick_interval,
    };
    use crate::PreconfConfig;

    fn preconf_with_slot(slot: Duration) -> PreconfConfig {
        PreconfConfig { slot_duration: slot, ..Default::default() }
    }

    fn cfg() -> FlashblockProducerConfig {
        FlashblockProducerConfig::default()
    }

    /// Defaults are an operator-visible contract — `--help` documents them
    /// and deployments rely on them. Pin the concrete values so a change is
    /// deliberate rather than incidental.
    #[test]
    fn defaults_match_the_documented_values() {
        let cfg = FlashblockProducerConfig::default();

        assert_eq!(cfg.block_time, Duration::from_millis(200));
        assert_eq!(cfg.leeway_time, Duration::ZERO);
        assert_eq!(cfg.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.port, 1111);
        assert_eq!(cfg.ring_capacity, 32);
        assert_eq!(cfg.broadcast_capacity, 32);
        assert_eq!(cfg.best_tx_lead_slices, 1);
    }

    #[test]
    fn defaults_validate_clean() {
        let preconf = preconf_with_slot(Duration::from_secs(2));

        assert!(cfg().validate(Some(&preconf), false).is_ok());
    }

    #[test]
    fn validate_rejects_a_block_time_below_the_minimum() {
        let preconf = preconf_with_slot(Duration::from_secs(2));
        for block_time in [Duration::ZERO, MIN_FLASHBLOCK_BLOCK_TIME - Duration::from_millis(1)] {
            let cfg = FlashblockProducerConfig { block_time, ..cfg() };
            assert!(
                matches!(
                    cfg.validate(Some(&preconf), false),
                    Err(FlashblockProducerConfigError::BlockTimeTooShort { .. })
                ),
                "{block_time:?}"
            );
        }
        let at_minimum =
            FlashblockProducerConfig { block_time: MIN_FLASHBLOCK_BLOCK_TIME, ..cfg() };
        assert!(at_minimum.validate(Some(&preconf), false).is_ok());
    }

    #[test]
    fn validate_rejects_block_time_larger_than_slot() {
        let cfg = FlashblockProducerConfig { block_time: Duration::from_secs(3), ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false),
            Err(FlashblockProducerConfigError::BlockTimeExceedsSlot { .. })
        ));
    }

    /// Leeway is subtracted from the slot deadline. At or past a whole slot
    /// there is never any time left to build in.
    #[test]
    fn validate_rejects_leeway_reaching_the_whole_slot() {
        let cfg = FlashblockProducerConfig { leeway_time: Duration::from_secs(2), ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false),
            Err(FlashblockProducerConfigError::LeewayReachesSlot { .. })
        ));
    }

    /// The archive cannot be the smaller of the two; see
    /// [`FlashblockProducerConfigError::RingSmallerThanBroadcast`].
    #[test]
    fn validate_rejects_ring_smaller_than_broadcast() {
        let cfg = FlashblockProducerConfig { ring_capacity: 8, broadcast_capacity: 16, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false),
            Err(FlashblockProducerConfigError::RingSmallerThanBroadcast { ring: 8, broadcast: 16 })
        ));
    }

    #[test]
    fn validate_rejects_zero_broadcast_capacity() {
        let cfg = FlashblockProducerConfig { broadcast_capacity: 0, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false),
            Err(FlashblockProducerConfigError::InvalidBroadcastCapacity)
        ));
    }

    /// The slicing logic lives inside the preconf payload builder, and with
    /// preconf off the node builds blocks with the upstream builder instead.
    /// Enabling flashblocks alone would silently do nothing.
    #[test]
    fn validate_rejects_flashblocks_without_preconf() {
        assert!(matches!(
            cfg().validate(None, false),
            Err(FlashblockProducerConfigError::RequiresPreconfEnabled)
        ));
    }

    /// Enabling both at boot is refused, and the SDM flag is what decides it:
    /// no other field in this config can make the combination safe. See
    /// [`FlashblockProducerConfigError::SdmNotSupportedWithFlashblocks`].
    #[test]
    fn validate_rejects_sdm_together_with_flashblocks() {
        let preconf = preconf_with_slot(Duration::from_secs(2));

        assert!(matches!(
            cfg().validate(Some(&preconf), true),
            Err(FlashblockProducerConfigError::SdmNotSupportedWithFlashblocks)
        ));
    }

    /// The other side of `validate_rejects_sdm_together_with_flashblocks`:
    /// with SDM off, an explicitly tuned publishing config — not just the
    /// defaults, which the test above already covers — is accepted and handed
    /// back unchanged, so chaining `validate` cannot silently normalise an
    /// operator's values.
    #[test]
    fn validate_accepts_flashblocks_without_sdm() {
        let preconf = preconf_with_slot(Duration::from_secs(2));
        let configured = FlashblockProducerConfig {
            block_time: Duration::from_millis(250),
            leeway_time: Duration::from_millis(50),
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 9_999,
            ring_capacity: 64,
            broadcast_capacity: 16,
            best_tx_lead_slices: 2,
        };

        assert_eq!(configured.clone().validate(Some(&preconf), false), Ok(configured));
    }

    /// Every slice after the first would then be served by the build-start
    /// snapshot — a misconfiguration, not a stronger head start.
    #[test]
    fn validate_rejects_a_lead_that_spans_the_whole_slot() {
        // 2s slot at 200ms slices is ten boundaries.
        let cfg = FlashblockProducerConfig { best_tx_lead_slices: 10, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false),
            Err(FlashblockProducerConfigError::BestTxLeadSpansSlot { lead: 10, slices: 10 })
        ));
    }

    #[test]
    fn validate_accepts_a_lead_inside_the_slot() {
        let cfg = FlashblockProducerConfig { best_tx_lead_slices: 9, ..cfg() };

        assert!(cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2))), false).is_ok());
    }

    #[test]
    fn tick_interval_prefers_block_time_when_configured() {
        let cfg = FlashblockProducerConfig { block_time: Duration::from_millis(150), ..cfg() };

        assert_eq!(
            tick_interval(Some(&cfg), Duration::from_millis(999)),
            Duration::from_millis(150)
        );
    }

    #[test]
    fn tick_interval_falls_back_to_sweep_when_absent() {
        assert_eq!(tick_interval(None, Duration::from_millis(999)), Duration::from_millis(999));
    }
}
