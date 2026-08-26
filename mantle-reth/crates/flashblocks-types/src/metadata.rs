//! Flashblock metadata and the identifier that gives flashblocks a total order.

use std::{borrow::Cow, collections::BTreeMap, fmt};

use alloy_primitives::{Address, B256, U256};
use op_alloy_consensus::OpReceipt;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Mantle's flashblock metadata.
///
/// A superset of the metadata `op-alloy` defines: same three fields, plus
/// the predecessor pointer. Every field is optional on decode, so metadata
/// produced elsewhere still parses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MantleFlashblockMetadata {
    /// L2 block this slice belongs to.
    ///
    /// Redundant with `base.block_number`, but slices at index >= 1 carry no
    /// `base`, which makes this the only reliable source of the block number.
    pub block_number: u64,

    /// The slice this one follows, for gap detection.
    #[serde(default)]
    pub prev_flashblock_id: FlashblockId,

    /// Always empty. Present so the payload stays shaped like the upstream
    /// one; consumers must not read it.
    #[serde(default)]
    pub new_account_balances: BTreeMap<Address, U256>,

    /// Always empty, for the same reason as
    /// [`new_account_balances`](Self::new_account_balances).
    #[serde(default)]
    pub receipts: BTreeMap<B256, OpReceipt>,
}

/// Totally-ordered identifier of a flashblock: its block number and its
/// index within that block.
///
/// Encoded on the wire as `"<block_number>-<index>"` rather than as an
/// object, so it stays readable in logs and cheap to consume from other
/// languages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlashblockId {
    /// L2 block this flashblock belongs to.
    pub block_number: u64,
    /// Slice index within the block. Index 0 is the base slice.
    pub index: u64,
}

impl FlashblockId {
    /// Sentinel for "no predecessor". A producer emits it on the first
    /// flashblock after a restart; consumers skip the predecessor check.
    pub const NO_PREV: Self = Self { block_number: 0, index: 0 };

    /// Whether this is the [`NO_PREV`](Self::NO_PREV) sentinel.
    pub const fn is_no_prev(&self) -> bool {
        self.block_number == 0 && self.index == 0
    }
}

impl fmt::Display for FlashblockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.block_number, self.index)
    }
}

impl Serialize for FlashblockId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FlashblockId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `Cow`, not `&str`: an encoder elsewhere may escape the separator,
        // and an escaped string cannot be borrowed out of the input.
        let raw = Cow::<'_, str>::deserialize(deserializer)?;
        let (block_number, index) = raw
            .split_once('-')
            .ok_or_else(|| de::Error::custom(format!("expected `<block>-<index>`, got `{raw}`")))?;

        Ok(Self {
            block_number: block_number.parse().map_err(de::Error::custom)?,
            index: index.parse().map_err(de::Error::custom)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::FlashblockId;

    #[test]
    fn flashblock_id_serializes_as_block_dash_index() {
        let id = FlashblockId { block_number: 12, index: 3 };

        assert_eq!(serde_json::to_string(&id).unwrap(), r#""12-3""#);
        assert_eq!(serde_json::from_str::<FlashblockId>(r#""12-3""#).unwrap(), id);
    }

    #[test]
    fn malformed_flashblock_id_is_an_error_not_a_panic() {
        for raw in [r#""12""#, r#""a-3""#, r#""12-""#, r#""12-3-4""#, "12"] {
            assert!(
                serde_json::from_str::<FlashblockId>(raw).is_err(),
                "expected `{raw}` to be rejected"
            );
        }
    }

    /// A JSON encoder elsewhere may escape the separator. Decoding must not
    /// depend on the string being borrowable straight out of the input.
    #[test]
    fn escaped_flashblock_id_string_still_decodes() {
        // The JSON below is `"12-3"` — same value, escaped separator.
        let decoded: FlashblockId = serde_json::from_str("\"12\\u002d3\"").unwrap();

        assert_eq!(decoded, FlashblockId { block_number: 12, index: 3 });
    }

    #[test]
    fn no_prev_round_trips_as_zero_dash_zero() {
        assert_eq!(serde_json::to_string(&FlashblockId::NO_PREV).unwrap(), r#""0-0""#);

        let decoded: FlashblockId = serde_json::from_str(r#""0-0""#).unwrap();
        assert_eq!(decoded, FlashblockId::NO_PREV);
        assert!(decoded.is_no_prev());
    }
}
