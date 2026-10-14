//! Configuration for the flashblock consumer.

use std::time::Duration;

use url::Url;

use crate::DEFAULT_MAX_CACHE_AHEAD_BLOCKS;

/// Default interval between upstream websocket ping frames.
pub const DEFAULT_SUBSCRIBER_PING_INTERVAL: Duration = Duration::from_secs(30);

/// Default number of canonical blocks the pending overlay may trail behind
/// before it is rebuilt from canonical state.
pub const DEFAULT_MAX_TRAILING_DEPTH: u64 = 3;

/// Default number of blocks the pending overlay may lead canonical by before
/// the guard fires.
pub const DEFAULT_MAX_LEADING_DEPTH: u64 = 3;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// See [`DEFAULT_MAX_TRAILING_DEPTH`].
    pub max_trailing_depth: u64,
    /// See [`DEFAULT_MAX_LEADING_DEPTH`].
    pub max_leading_depth: u64,
    /// See [`DEFAULT_MAX_CACHE_AHEAD_BLOCKS`].
    pub max_cache_ahead_blocks: u64,
    /// See [`DEFAULT_SUBSCRIBER_PING_INTERVAL`].
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

    /// See [`Self::max_trailing_depth`].
    pub const fn with_max_trailing_depth(mut self, max_trailing_depth: u64) -> Self {
        self.max_trailing_depth = max_trailing_depth;
        self
    }

    /// See [`Self::max_leading_depth`].
    pub const fn with_max_leading_depth(mut self, max_leading_depth: u64) -> Self {
        self.max_leading_depth = max_leading_depth;
        self
    }

    /// See [`Self::max_cache_ahead_blocks`].
    pub const fn with_max_cache_ahead_blocks(mut self, max_cache_ahead_blocks: u64) -> Self {
        self.max_cache_ahead_blocks = max_cache_ahead_blocks;
        self
    }

    /// See [`Self::subscriber_ping_interval`]. Panics on a zero interval.
    pub const fn with_subscriber_ping_interval(
        mut self,
        subscriber_ping_interval: Duration,
    ) -> Self {
        assert!(!subscriber_ping_interval.is_zero(), "ping interval must be positive");
        self.subscriber_ping_interval = subscriber_ping_interval;
        self
    }
}
