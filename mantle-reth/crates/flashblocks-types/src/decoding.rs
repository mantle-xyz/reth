//! Decoding a flashblock off the wire: plain JSON or brotli-compressed JSON.

use std::io::Read;

use crate::{error::FlashblockDecodeError, payload::MantleFlashblockPayload};

/// Largest flashblock accepted, before and after decompression.
pub const MAX_DECOMPRESSED_FLASHBLOCK_BYTES: usize = 5 * 1024 * 1024;

impl MantleFlashblockPayload {
    /// Decode a flashblock from bytes that may be plain JSON or
    /// brotli-compressed JSON.
    ///
    /// The size ceiling applies to the decompressed bytes as well, so a small
    /// message that expands without bound is rejected rather than buffered.
    pub fn try_decode_message(bytes: &[u8]) -> Result<Self, FlashblockDecodeError> {
        if bytes.len() > MAX_DECOMPRESSED_FLASHBLOCK_BYTES {
            return Err(FlashblockDecodeError::PayloadTooLarge {
                given: bytes.len(),
                max: MAX_DECOMPRESSED_FLASHBLOCK_BYTES,
            });
        }

        let json = if looks_like_json(bytes) { None } else { Some(decompress(bytes)?) };

        serde_json::from_slice(json.as_deref().unwrap_or(bytes))
            .map_err(FlashblockDecodeError::PayloadParse)
    }
}

/// JSON documents start with `{`, optionally after whitespace. Brotli never
/// does, so this is enough to tell the two apart.
fn looks_like_json(bytes: &[u8]) -> bool {
    bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{')
}

fn decompress(bytes: &[u8]) -> Result<Vec<u8>, FlashblockDecodeError> {
    // Read one byte past the ceiling: enough to tell "exactly at the limit"
    // from "over it" without ever buffering more than that.
    let mut out = Vec::new();
    brotli::Decompressor::new(bytes, 4096)
        .take(MAX_DECOMPRESSED_FLASHBLOCK_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(FlashblockDecodeError::Decompress)?;

    if out.len() > MAX_DECOMPRESSED_FLASHBLOCK_BYTES {
        return Err(FlashblockDecodeError::PayloadTooLarge {
            given: out.len(),
            max: MAX_DECOMPRESSED_FLASHBLOCK_BYTES,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use alloy_rpc_types_engine::PayloadId;

    use super::MAX_DECOMPRESSED_FLASHBLOCK_BYTES;
    use crate::{error::FlashblockDecodeError, payload::MantleFlashblockPayload};

    fn sample() -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: PayloadId::new([9u8; 8]),
            index: 4,
            ..Default::default()
        }
    }

    fn brotli_compress(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
        writer.write_all(input).unwrap();
        drop(writer);
        out
    }

    #[test]
    fn plain_json_decodes() {
        let json = serde_json::to_vec(&sample()).unwrap();

        assert_eq!(MantleFlashblockPayload::try_decode_message(&json).unwrap(), sample());
    }

    #[test]
    fn brotli_compressed_decodes() {
        let compressed = brotli_compress(&serde_json::to_vec(&sample()).unwrap());

        assert_eq!(MantleFlashblockPayload::try_decode_message(&compressed).unwrap(), sample());
    }

    #[test]
    fn oversized_input_is_rejected() {
        let oversized = vec![b'{'; MAX_DECOMPRESSED_FLASHBLOCK_BYTES + 1];

        assert!(matches!(
            MantleFlashblockPayload::try_decode_message(&oversized),
            Err(FlashblockDecodeError::PayloadTooLarge { .. })
        ));
    }

    /// A few kilobytes of brotli can expand to gigabytes. The ceiling has to
    /// apply to the decompressed bytes, not just to what arrived.
    #[test]
    fn a_decompression_bomb_is_rejected() {
        let bomb = brotli_compress(&vec![b'a'; MAX_DECOMPRESSED_FLASHBLOCK_BYTES + 1024]);
        assert!(
            bomb.len() < MAX_DECOMPRESSED_FLASHBLOCK_BYTES,
            "the bomb itself must be under the ceiling"
        );

        assert!(matches!(
            MantleFlashblockPayload::try_decode_message(&bomb),
            Err(FlashblockDecodeError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn garbage_input_is_an_error_not_a_panic() {
        assert!(MantleFlashblockPayload::try_decode_message(&[0xff, 0xfe, 0xfd]).is_err());
        assert!(MantleFlashblockPayload::try_decode_message(b"").is_err());
        assert!(MantleFlashblockPayload::try_decode_message(b"{not json}").is_err());
    }
}
