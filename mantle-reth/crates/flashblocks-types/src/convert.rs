//! Conversions to and from the `op-alloy` flashblock types.
//!
//! The two payloads differ only in metadata, and only by the predecessor
//! pointer: converting upstream drops it, converting back yields the
//! "no predecessor" sentinel.

use op_alloy_rpc_types_engine::{OpFlashblockPayload, OpFlashblockPayloadMetadata};

use crate::{
    metadata::{FlashblockId, MantleFlashblockMetadata},
    payload::MantleFlashblockPayload,
};

impl From<MantleFlashblockMetadata> for OpFlashblockPayloadMetadata {
    fn from(value: MantleFlashblockMetadata) -> Self {
        Self {
            block_number: value.block_number,
            new_account_balances: value.new_account_balances,
            receipts: value.receipts,
        }
    }
}

impl From<OpFlashblockPayloadMetadata> for MantleFlashblockMetadata {
    fn from(value: OpFlashblockPayloadMetadata) -> Self {
        Self {
            block_number: value.block_number,
            prev_flashblock_id: FlashblockId::NO_PREV,
            new_account_balances: value.new_account_balances,
            receipts: value.receipts,
        }
    }
}

impl From<MantleFlashblockPayload> for OpFlashblockPayload {
    fn from(value: MantleFlashblockPayload) -> Self {
        Self {
            payload_id: value.payload_id,
            index: value.index,
            base: value.base,
            diff: value.diff,
            metadata: value.metadata.into(),
        }
    }
}

impl From<OpFlashblockPayload> for MantleFlashblockPayload {
    fn from(value: OpFlashblockPayload) -> Self {
        Self {
            payload_id: value.payload_id,
            index: value.index,
            base: value.base,
            diff: value.diff,
            metadata: value.metadata.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address};
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_rpc_types_engine::{
        OpFlashblockPayload, OpFlashblockPayloadBase, OpFlashblockPayloadDelta,
    };

    use crate::{metadata::MantleFlashblockMetadata, payload::MantleFlashblockPayload};

    fn mantle_payload() -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: PayloadId::new([7u8; 8]),
            index: 2,
            base: Some(OpFlashblockPayloadBase {
                parent_hash: B256::repeat_byte(0xab),
                fee_recipient: address!("0000000000000000000000000000000000000001"),
                block_number: 100,
                gas_limit: 30_000_000,
                timestamp: 1_234_567_890,
                base_fee_per_gas: U256::from(1_000_000_000u64),
                ..Default::default()
            }),
            diff: OpFlashblockPayloadDelta {
                gas_used: 21_000,
                transactions: vec![Bytes::from_static(&[1, 2, 3])],
                ..Default::default()
            },
            metadata: MantleFlashblockMetadata {
                block_number: 100,
                prev_flashblock_id: crate::metadata::FlashblockId { block_number: 100, index: 1 },
                ..Default::default()
            },
        }
    }

    #[test]
    fn mantle_payload_decodes_into_op_alloy_payload() {
        let mantle = mantle_payload();

        let json = serde_json::to_string(&mantle).unwrap();
        let op: OpFlashblockPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(op.payload_id, mantle.payload_id);
        assert_eq!(op.index, mantle.index);
        assert_eq!(op.base, mantle.base);
        assert_eq!(op.diff, mantle.diff);
        assert_eq!(op.metadata.block_number, mantle.metadata.block_number);
    }

    #[test]
    fn op_alloy_payload_decodes_into_mantle_payload() {
        let op: OpFlashblockPayload =
            serde_json::from_str(&serde_json::to_string(&mantle_payload()).unwrap()).unwrap();

        let json = serde_json::to_string(&op).unwrap();
        let mantle: MantleFlashblockPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(mantle.payload_id, op.payload_id);
        assert_eq!(mantle.index, op.index);
        assert_eq!(mantle.base, op.base);
        assert_eq!(mantle.diff, op.diff);
        assert_eq!(mantle.metadata.block_number, op.metadata.block_number);
        // The upstream metadata has no predecessor field, so decoding it
        // yields the "no predecessor" sentinel rather than an error.
        assert!(mantle.metadata.prev_flashblock_id.is_no_prev());
    }

    /// The upstream metadata fields are required on decode. Empty maps must
    /// therefore still be written out — adding `skip_serializing_if` to them
    /// would silently break every upstream consumer.
    #[test]
    fn empty_metadata_maps_are_still_written_so_upstream_can_decode() {
        let mantle = MantleFlashblockPayload::default();
        assert!(mantle.metadata.new_account_balances.is_empty());
        assert!(mantle.metadata.receipts.is_empty());

        let json = serde_json::to_value(&mantle).unwrap();
        let metadata = json.get("metadata").unwrap();

        assert!(metadata.get("new_account_balances").is_some());
        assert!(metadata.get("receipts").is_some());
        serde_json::from_value::<OpFlashblockPayload>(json).unwrap();
    }

    #[test]
    fn converting_to_upstream_and_back_preserves_everything_but_the_predecessor() {
        let mantle = mantle_payload();

        let op = OpFlashblockPayload::from(mantle.clone());
        let round_tripped = MantleFlashblockPayload::from(op);

        assert_eq!(
            round_tripped,
            MantleFlashblockPayload {
                metadata: MantleFlashblockMetadata {
                    prev_flashblock_id: crate::metadata::FlashblockId::NO_PREV,
                    ..mantle.metadata.clone()
                },
                ..mantle
            }
        );
    }
}
