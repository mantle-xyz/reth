//! Producer side of flashblocks: slicing a block as it is built and
//! publishing each slice to subscribers.

pub mod config;
pub mod slice_pacer;

pub use config::{
    DEFAULT_BROADCAST_CAPACITY, DEFAULT_FLASHBLOCK_ADDR, DEFAULT_FLASHBLOCK_BLOCK_TIME,
    DEFAULT_FLASHBLOCK_LEEWAY, DEFAULT_FLASHBLOCK_PORT, DEFAULT_RING_CAPACITY,
    FlashblockProducerConfig, FlashblockProducerConfigError,
};
pub use slice_pacer::{Reservation, SliceLimits, SlicePacer, SliceSchedule, derive_slice_schedule};
