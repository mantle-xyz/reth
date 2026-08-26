//! Mantle flashblock wire format.
//!
//! A flashblock is a partial block the sequencer publishes mid-build, every
//! few hundred milliseconds, so consumers can show transaction results long
//! before the block is sealed. This crate defines the types that travel on
//! that wire and nothing else — it is shared by the sequencer-side producer
//! and the RPC-side consumer, so it deliberately depends only on `op-alloy`
//! and `serde`.
//!
//! The payload is byte-compatible with `op-alloy`'s `OpFlashblockPayload`:
//! the same JSON decodes as either type. Only `metadata` differs, and it
//! differs by addition.
//!
//! ```
//! use mantle_reth_flashblocks_types::{FlashblockId, MantleFlashblockPayload};
//!
//! let slice = MantleFlashblockPayload { index: 3, ..Default::default() };
//! let bytes = serde_json::to_vec(&slice)?;
//!
//! let decoded = MantleFlashblockPayload::try_decode_message(&bytes)?;
//! assert_eq!(decoded.index, 3);
//! // A slice with no recorded predecessor reports the sentinel.
//! assert_eq!(decoded.metadata.prev_flashblock_id, FlashblockId::NO_PREV);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod convert;
pub mod decoding;
pub mod error;
pub mod metadata;
pub mod payload;

pub use decoding::MAX_DECOMPRESSED_FLASHBLOCK_BYTES;
pub use error::FlashblockDecodeError;
pub use metadata::{FlashblockId, MantleFlashblockMetadata};
pub use payload::MantleFlashblockPayload;
