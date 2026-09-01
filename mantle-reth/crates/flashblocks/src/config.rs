//! Configuration for the flashblock consumer.

use std::time::Duration;

use url::Url;

use crate::DEFAULT_MAX_CACHE_AHEAD_BLOCKS;

/// Default interval between upstream websocket ping frames.
pub const DEFAULT_SUBSCRIBER_PING_INTERVAL: Duration = Duration::from_secs(30);

/// Default number of canonical blocks the pending overlay may trail behind.
pub const DEFAULT_MAX_TRAILING_DEPTH: u64 = 3;

/// Default number of blocks the pending overlay may lead canonical by.
pub const DEFAULT_MAX_LEADING_DEPTH: u64 = 3;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Canonical blocks the pending overlay may trail behind before a rebuild.
    pub max_trailing_depth: u64,
    /// Blocks the pending overlay may lead canonical by before the guard fires.
    pub max_leading_depth: u64,
    /// Blocks ahead of canonical for which early flashblocks are cached.
    pub max_cache_ahead_blocks: u64,
    /// Interval between upstream websocket ping frames.
    pub subscriber_ping_interval: Duration,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration with default tuning.
    pub const fn new(websocket_url: Url) -> Self {
        Self {
            websocket_url,
            max_trailing_depth: DEFAULT_MAX_TRAILING_DEPTH,
            max_leading_depth: DEFAULT_MAX_LEADING_DEPTH,
            max_cache_ahead_blocks: DEFAULT_MAX_CACHE_AHEAD_BLOCKS,
            subscriber_ping_interval: DEFAULT_SUBSCRIBER_PING_INTERVAL,
        }
    }

    /// Set the canonical blocks the pending overlay may trail behind.
    pub const fn with_max_trailing_depth(mut self, max_trailing_depth: u64) -> Self {
        self.max_trailing_depth = max_trailing_depth;
        self
    }

    /// Set the blocks the pending overlay may lead canonical by.
    pub const fn with_max_leading_depth(mut self, max_leading_depth: u64) -> Self {
        self.max_leading_depth = max_leading_depth;
        self
    }

    /// Set the blocks ahead of canonical for which early flashblocks are cached.
    pub const fn with_max_cache_ahead_blocks(mut self, max_cache_ahead_blocks: u64) -> Self {
        self.max_cache_ahead_blocks = max_cache_ahead_blocks;
        self
    }

    /// Set the interval between upstream websocket ping frames.
    pub const fn with_subscriber_ping_interval(
        mut self,
        subscriber_ping_interval: Duration,
    ) -> Self {
        assert!(!subscriber_ping_interval.is_zero(), "ping interval must be positive");
        self.subscriber_ping_interval = subscriber_ping_interval;
        self
    }
}
