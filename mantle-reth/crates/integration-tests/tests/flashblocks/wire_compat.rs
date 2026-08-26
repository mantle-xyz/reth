//! Wire-compatibility contract between Mantle's flashblock payload and the
//! upstream `op-alloy` one.
//!
//! Mantle publishes flashblocks that non-Mantle consumers are expected to
//! read. That only holds if one JSON document decodes as either type, so
//! these tests exercise the contract against the real upstream types rather
//! than against Mantle's own round-trip.

use alloy_primitives::{B256, Bytes, U256, address};
use alloy_rpc_types_engine::PayloadId;
use mantle_reth_flashblocks_types::{
    metadata::{FlashblockId, MantleFlashblockMetadata},
    payload::MantleFlashblockPayload,
};
use op_alloy_rpc_types_engine::{
    OpFlashblockPayload, OpFlashblockPayloadBase, OpFlashblockPayloadDelta,
};

const BLOCK_NUMBER: u64 = 4_242;

fn base() -> OpFlashblockPayloadBase {
    OpFlashblockPayloadBase {
        parent_hash: B256::repeat_byte(0xab),
        fee_recipient: address!("4200000000000000000000000000000000000011"),
        block_number: BLOCK_NUMBER,
        gas_limit: 30_000_000,
        timestamp: 1_800_000_000,
        base_fee_per_gas: U256::from(1_000_000_000u64),
        ..Default::default()
    }
}

/// A block's worth of slices: index 0 carries `base`, later ones do not,
/// and each carries only the transactions it added.
fn sequence() -> Vec<MantleFlashblockPayload> {
    let payload_id = PayloadId::new([3u8; 8]);

    (0..3u64)
        .map(|index| MantleFlashblockPayload {
            payload_id,
            index,
            base: (index == 0).then(base),
            diff: OpFlashblockPayloadDelta {
                gas_used: 21_000 * (index + 1),
                transactions: vec![Bytes::from(vec![index as u8; 4])],
                ..Default::default()
            },
            metadata: MantleFlashblockMetadata {
                block_number: BLOCK_NUMBER,
                prev_flashblock_id: match index {
                    0 => FlashblockId::NO_PREV,
                    _ => FlashblockId { block_number: BLOCK_NUMBER, index: index - 1 },
                },
                ..Default::default()
            },
        })
        .collect()
}

#[test]
fn an_upstream_consumer_can_read_a_whole_slice_sequence() {
    for slice in sequence() {
        let json = serde_json::to_string(&slice).unwrap();

        let upstream: OpFlashblockPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(upstream.block_number(), BLOCK_NUMBER);
        assert_eq!(upstream.raw_transactions(), slice.diff.transactions);
        assert_eq!(
            upstream.parent_hash(),
            (slice.index == 0).then(|| base().parent_hash),
            "parent hash is reachable only through the base slice"
        );
    }
}

#[test]
fn a_mantle_consumer_can_read_an_upstream_slice() {
    let upstream: OpFlashblockPayload = serde_json::from_str(
        &serde_json::to_string(&sequence().into_iter().next().unwrap()).unwrap(),
    )
    .unwrap();

    let json = serde_json::to_string(&upstream).unwrap();
    let mantle: MantleFlashblockPayload = serde_json::from_str(&json).unwrap();

    assert_eq!(mantle.base, upstream.base);
    assert_eq!(mantle.diff, upstream.diff);
    assert!(
        mantle.metadata.prev_flashblock_id.is_no_prev(),
        "upstream carries no predecessor, so decoding must fall back to the sentinel"
    );
}

/// The upstream metadata fields have no serde defaults, so dropping the
/// always-empty maps from what we publish would break every upstream
/// consumer. This pins that: without them, upstream decoding fails.
#[test]
fn dropping_the_empty_metadata_maps_would_break_upstream_decoding() {
    let mut json = serde_json::to_value(&sequence()[0]).unwrap();
    serde_json::from_value::<OpFlashblockPayload>(json.clone())
        .expect("what we actually publish must decode upstream");

    let metadata = json.get_mut("metadata").unwrap().as_object_mut().unwrap();
    metadata.remove("new_account_balances");
    metadata.remove("receipts");

    assert!(serde_json::from_value::<OpFlashblockPayload>(json).is_err());
}
