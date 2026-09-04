//! Producer side of flashblocks: slicing a block as it is built and
//! publishing each slice to subscribers.

pub mod broadcast;
pub mod config;
pub mod publisher;
pub mod ring_buffer;
pub mod slice_pacer;

pub use broadcast::{ReplayOutcome, SubscriptionExit, parse_resume_position};
pub use config::{
    DEFAULT_BROADCAST_CAPACITY, DEFAULT_FLASHBLOCK_ADDR, DEFAULT_FLASHBLOCK_BLOCK_TIME,
    DEFAULT_FLASHBLOCK_LEEWAY, DEFAULT_FLASHBLOCK_PORT, DEFAULT_RING_CAPACITY,
    FlashblockProducerConfig, FlashblockProducerConfigError,
};
pub use publisher::{MantleFlashblocksPublisher, PublisherHandle};
pub use ring_buffer::{FlashblockPosition, FlashblockRingBuffer};
pub use slice_pacer::{Reservation, SliceLimits, SlicePacer, SliceSchedule, derive_slice_schedule};
