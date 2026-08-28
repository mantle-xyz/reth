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

/// Default margin by which the budgeted slice grid finishes ahead of the
/// slot deadline — 50ms.
///
/// It shifts the whole tick grid rather than changing how many slices a
/// block gets: at the default interval the slice count is the same for any
/// leeway below one interval. Lower values push the last budgeted slice
/// closer to the deadline, which leaves the tail more gas headroom but
/// narrows the window between publishing that slice and sealing.
pub const DEFAULT_FLASHBLOCK_LEEWAY: Duration = Duration::from_millis(50);

/// Default address the publisher binds — loopback.
pub const DEFAULT_FLASHBLOCK_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Default port the publisher binds.
pub const DEFAULT_FLASHBLOCK_PORT: u16 = 1111;

/// Default number of published slices kept for resuming subscribers — 32.
///
/// At eleven slices per block that is a little under three blocks, so a
/// subscriber has roughly six seconds to reconnect before it has to fall
/// back to canonical sync.
pub const DEFAULT_RING_CAPACITY: usize = 32;

/// Default capacity of the live broadcast channel — 32.
pub const DEFAULT_BROADCAST_CAPACITY: usize = 32;

/// Runtime configuration for publishing flashblocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashblockProducerConfig {
    /// Interval between slices.
    pub block_time: Duration,
    /// How far ahead of the slot deadline building stops.
    pub leeway_time: Duration,
    /// Address the publisher binds.
    pub addr: IpAddr,
    /// Port the publisher binds.
    pub port: u16,
    /// Published slices kept for subscribers that reconnect.
    pub ring_capacity: usize,
    /// Capacity of the live broadcast channel.
    pub broadcast_capacity: usize,
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
        }
    }
}

/// Errors surfaced by [`FlashblockProducerConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlashblockProducerConfigError {
    /// `block_time == 0`.
    #[error("flashblocks block_time must be > 0")]
    InvalidBlockTime,
    /// `broadcast_capacity == 0` — tokio broadcast requires capacity > 0.
    #[error("flashblocks broadcast_capacity must be > 0")]
    InvalidBroadcastCapacity,
    /// A single slice would span more than the whole slot, which leaves the
    /// per-slice budget meaningless.
    #[error("flashblocks block_time ({block_time:?}) must be <= slot_duration ({slot:?})")]
    BlockTimeExceedsSlot {
        /// Configured slice interval.
        block_time: Duration,
        /// Slot duration the interval was compared against.
        slot: Duration,
    },
    /// Leeway at or past a whole slot leaves no time to build in.
    #[error("flashblocks leeway_time ({leeway:?}) must be < slot_duration ({slot:?})")]
    LeewayReachesSlot {
        /// Configured leeway.
        leeway: Duration,
        /// Slot duration the leeway was compared against.
        slot: Duration,
    },
    /// The archive is smaller than the live channel, so a subscriber could
    /// lag past what the ring can replay and never be able to resume.
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
}

impl FlashblockProducerConfig {
    /// Validates config invariants against the preconf config it runs
    /// alongside. `preconf` is `None` when preconf is disabled.
    ///
    /// Returns the original config on success for ergonomic chaining.
    pub fn validate(
        self,
        preconf: Option<&PreconfConfig>,
    ) -> Result<Self, FlashblockProducerConfigError> {
        if self.block_time.is_zero() {
            return Err(FlashblockProducerConfigError::InvalidBlockTime);
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

        Ok(self)
    }
}

/// Interval the build loop's ticker fires at.
///
/// Without flashblocks the cadence stays exactly what preconf used before,
/// which keeps turning the feature off a true rollback.
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

    use super::{FlashblockProducerConfig, FlashblockProducerConfigError, tick_interval};
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
        assert_eq!(cfg.leeway_time, Duration::from_millis(50));
        assert_eq!(cfg.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.port, 1111);
        assert_eq!(cfg.ring_capacity, 32);
        assert_eq!(cfg.broadcast_capacity, 32);
    }

    #[test]
    fn defaults_validate_clean() {
        let preconf = preconf_with_slot(Duration::from_secs(2));

        assert!(cfg().validate(Some(&preconf)).is_ok());
    }

    #[test]
    fn validate_rejects_zero_block_time() {
        let cfg = FlashblockProducerConfig { block_time: Duration::ZERO, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2)))),
            Err(FlashblockProducerConfigError::InvalidBlockTime)
        ));
    }

    #[test]
    fn validate_rejects_block_time_larger_than_slot() {
        let cfg = FlashblockProducerConfig { block_time: Duration::from_secs(3), ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2)))),
            Err(FlashblockProducerConfigError::BlockTimeExceedsSlot { .. })
        ));
    }

    /// Leeway is subtracted from the slot deadline. At or past a whole slot
    /// there is never any time left to build in.
    #[test]
    fn validate_rejects_leeway_reaching_the_whole_slot() {
        let cfg = FlashblockProducerConfig { leeway_time: Duration::from_secs(2), ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2)))),
            Err(FlashblockProducerConfigError::LeewayReachesSlot { .. })
        ));
    }

    /// A subscriber resuming from the ring must find every slice the channel
    /// could have dropped, so the archive cannot be the smaller of the two.
    #[test]
    fn validate_rejects_ring_smaller_than_broadcast() {
        let cfg = FlashblockProducerConfig { ring_capacity: 8, broadcast_capacity: 16, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2)))),
            Err(FlashblockProducerConfigError::RingSmallerThanBroadcast { ring: 8, broadcast: 16 })
        ));
    }

    #[test]
    fn validate_rejects_zero_broadcast_capacity() {
        let cfg = FlashblockProducerConfig { broadcast_capacity: 0, ..cfg() };

        assert!(matches!(
            cfg.validate(Some(&preconf_with_slot(Duration::from_secs(2)))),
            Err(FlashblockProducerConfigError::InvalidBroadcastCapacity)
        ));
    }

    /// The slicing logic lives inside the preconf payload builder, and with
    /// preconf off the node builds blocks with the upstream builder instead.
    /// Enabling flashblocks alone would silently do nothing.
    #[test]
    fn validate_rejects_flashblocks_without_preconf() {
        assert!(matches!(
            cfg().validate(None),
            Err(FlashblockProducerConfigError::RequiresPreconfEnabled)
        ));
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
