//! Errors raised while decoding a flashblock off the wire.

use thiserror::Error;

/// Why a flashblock could not be decoded.
///
/// Named to match the equivalent type in the Base implementation, minus two
/// variants that cannot occur here: this crate decodes typed metadata
/// straight from the input slice, so there is no separate metadata parse and
/// no intermediate `String` whose UTF-8 could fail — both surface as
/// [`PayloadParse`](Self::PayloadParse).
#[derive(Debug, Error)]
pub enum FlashblockDecodeError {
    /// The bytes were not a valid flashblock document.
    #[error("failed to parse flashblock payload JSON: {0}")]
    PayloadParse(#[source] serde_json::Error),

    /// The message looked brotli-compressed but did not decompress.
    #[error("failed to decompress brotli payload: {0}")]
    Decompress(#[source] std::io::Error),

    /// The message exceeded the size ceiling, either as it arrived or after
    /// decompression.
    #[error("flashblock payload too large: {given} bytes, max {max}")]
    PayloadTooLarge {
        /// Bytes seen before giving up. For a compressed message this is the
        /// decompressed count, which stops one byte past the ceiling.
        given: usize,
        /// The ceiling that was exceeded.
        max: usize,
    },
}
