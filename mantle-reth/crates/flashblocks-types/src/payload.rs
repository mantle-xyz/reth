//! The flashblock payload that travels on the wire.

use alloy_rpc_types_engine::PayloadId;
use op_alloy_rpc_types_engine::{OpFlashblockPayloadBase, OpFlashblockPayloadDelta};
use serde::{Deserialize, Serialize};

use crate::metadata::MantleFlashblockMetadata;

/// One flashblock: the slice of a block the sequencer has built so far.
///
/// Same shape as `op-alloy`'s `OpFlashblockPayload` — `base` and `diff` are
/// reused verbatim — so one JSON document decodes as either type. Only
/// `metadata` is Mantle's own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MantleFlashblockPayload {
    /// Payload id the execution engine assigned to the block being built.
    /// Rebuilding the same block yields a new id.
    pub payload_id: PayloadId,

    /// Slice index within the block; 0 is the base slice. The count of
    /// slices per block varies and has no fixed ceiling — consumers must
    /// not assume a total.
    pub index: u64,

    /// Block-level invariants. Carried by index 0 only; omitted entirely on
    /// later slices — sending `null` instead would break decoders that
    /// expect the key to be absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<OpFlashblockPayloadBase>,

    /// This slice's delta: the newly executed transactions, plus roots, gas
    /// and bloom accumulated across the block so far.
    pub diff: OpFlashblockPayloadDelta,

    /// Mantle's own metadata.
    #[serde(default)]
    pub metadata: MantleFlashblockMetadata,
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;

    use super::MantleFlashblockPayload;

    fn base() -> OpFlashblockPayloadBase {
        OpFlashblockPayloadBase { block_number: 100, ..Default::default() }
    }

    #[test]
    fn index_zero_carries_base_and_later_slices_omit_it() {
        let first = MantleFlashblockPayload {
            payload_id: PayloadId::new([1u8; 8]),
            index: 0,
            base: Some(base()),
            ..Default::default()
        };
        let later = MantleFlashblockPayload { index: 1, base: None, ..first.clone() };

        let first_json = serde_json::to_value(&first).unwrap();
        let later_json = serde_json::to_value(&later).unwrap();

        assert!(first_json.get("base").is_some());
        assert!(
            later_json.get("base").is_none(),
            "a slice without base must omit the key entirely, not send null: {later_json}"
        );
    }

    #[test]
    fn a_payload_round_trips_with_and_without_base() {
        for payload in [
            MantleFlashblockPayload { index: 0, base: Some(base()), ..Default::default() },
            MantleFlashblockPayload { index: 1, base: None, ..Default::default() },
        ] {
            let json = serde_json::to_string(&payload).unwrap();
            let decoded: MantleFlashblockPayload = serde_json::from_str(&json).unwrap();

            assert_eq!(decoded, payload);
        }
    }
}
