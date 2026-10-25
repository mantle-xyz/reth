//! Configuration for the flashblock consumer.

use std::time::Duration;

use url::Url;

use crate::DEFAULT_MAX_CACHE_AHEAD_BLOCKS;

/// Default interval between upstream websocket ping frames — 2s.
///
/// Half of a budget the producer sets: it archives `ring_capacity` slices for a
/// subscriber to resume from, roughly six seconds at the default capacity and
/// slice interval, after which a reconnect comes back with a gap and the
/// consumer loses the rest of that block. Detection has to fit inside that,
/// with room left for the handshake and the replay.
///
/// A dead connection is noticed after two ticks — one to send a ping, one to
/// find it unanswered — so this bounds detection at 4s. Only a connection that
/// goes quiet without ending needs it: a stream that ends is reported by the
/// socket and reconnects at once (`subscriber_resume.rs` pins that). Those are
/// rarer, which makes the keepalive cheap; it is one frame per interval on one
/// connection, against a reconnect that resumes from the archive at no cost
/// when it is wrong.
pub const DEFAULT_SUBSCRIBER_PING_INTERVAL: Duration = Duration::from_secs(2);

/// Longest ping interval the CLI accepts — five minutes.
pub const MAX_SUBSCRIBER_PING_INTERVAL: Duration = Duration::from_secs(300);

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
    /// See [`DEFAULT_MAX_CACHE_AHEAD_BLOCKS`]. The CLI caps it at
    /// [`MAX_CACHE_AHEAD_BLOCKS`](crate::MAX_CACHE_AHEAD_BLOCKS).
    pub max_cache_ahead_blocks: u64,
    /// See [`DEFAULT_SUBSCRIBER_PING_INTERVAL`]. Capped at
    /// [`MAX_SUBSCRIBER_PING_INTERVAL`].
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
