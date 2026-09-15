//! Turning what has been executed since the last slice into a payload.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{Encodable2718, eip4895::Withdrawal};
use alloy_primitives::{B256, Bloom};
use alloy_rpc_types_engine::PayloadId;
use mantle_reth_flashblocks_types::{
    FlashblockId, MantleFlashblockMetadata, MantleFlashblockPayload,
};
use op_alloy_rpc_types_engine::{OpFlashblockPayloadBase, OpFlashblockPayloadDelta};

use reth_primitives_traits::SignedTransaction;

/// Block-level fields that hold for every slice of one block.
///
/// Established once the block's pre-execution steps are done, then reused
/// unchanged: the first slice carries `base` for a consumer to build a header
/// from, and every slice repeats the withdrawals in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInvariants {
    /// Header fields a consumer needs to reconstruct the block.
    pub base: OpFlashblockPayloadBase,
    /// Withdrawals for the block, repeated on every slice.
    pub withdrawals: Vec<Withdrawal>,
    /// Root of those withdrawals.
    pub withdrawals_root: B256,
}

/// The header fields a slice publishes, read off the block as it stands.
///
/// Kept as an input rather than derived here: computing them needs the chain
/// spec and the executed state, while everything else about assembling a slice
/// is bookkeeping over what has already been recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    /// Gas the block has burnt so far.
    pub gas_used: u64,
    /// Receipts root over the block's receipts so far.
    pub receipts_root: B256,
    /// Bloom over the block's logs so far.
    pub logs_bloom: Bloom,
    /// Hash of the block as it stands.
    pub block_hash: B256,
    /// Blob gas used, when the fork in force has it.
    pub blob_gas_used: Option<u64>,
}

/// Assemble the slice carrying `pending`.
///
/// Marking those transactions published is the caller's to do, and only once
/// the slice has actually gone out: a slice that is assembled and then dropped
/// has to be carried by the next one, or its transactions reach no subscriber
/// while the index chain still looks unbroken.
pub fn build_flashblock<T: SignedTransaction>(
    invariants: &BlockInvariants,
    header: SliceHeader,
    pending: &[Recovered<T>],
    payload_id: PayloadId,
    index: u64,
    prev_flashblock_id: FlashblockId,
) -> MantleFlashblockPayload {
    let transactions = pending.iter().map(|tx| tx.encoded_2718().into()).collect();

    MantleFlashblockPayload {
        payload_id,
        index,
        // Only the first slice carries them; later ones would be repeating
        // what the consumer already has.
        base: (index == 0).then(|| invariants.base.clone()),
        diff: OpFlashblockPayloadDelta {
            // Intermediate slices do not pay for a state root. Zero is the
            // agreed stand-in for "not computed".
            state_root: B256::ZERO,
            receipts_root: header.receipts_root,
            logs_bloom: header.logs_bloom,
            block_hash: header.block_hash,
            blob_gas_used: header.blob_gas_used,
            // Accumulating totals, so a consumer reads the header field it is
            // reconstructing rather than having to sum slices itself.
            gas_used: header.gas_used,
            transactions,
            withdrawals: invariants.withdrawals.clone(),
            withdrawals_root: invariants.withdrawals_root,
        },
        metadata: MantleFlashblockMetadata {
            // Slices past the first carry no `base`, so this is the only
            // reliable block number a consumer has.
            block_number: invariants.base.block_number,
            prev_flashblock_id,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_eips::{Encodable2718, eip4895::Withdrawal};
    use alloy_primitives::{Address, B256, Bloom, Bytes, Signature, TxKind, U256};
    use alloy_rpc_types_engine::PayloadId;
    use mantle_reth_flashblocks_types::{FlashblockId, MantleFlashblockPayload};
    use op_alloy_consensus::OpTxEnvelope;
    use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;

    use super::{BlockInvariants, SliceHeader, build_flashblock};
    use crate::builder::ExecutionInfo;

    type Info = ExecutionInfo<OpTxEnvelope>;

    /// A legacy transaction is enough: assembly reads only the encoding.
    fn signed(byte: u8) -> Recovered<OpTxEnvelope> {
        let inner = TxLegacy {
            chain_id: Some(1),
            nonce: u64::from(byte),
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        Recovered::new_unchecked(
            OpTxEnvelope::Legacy(Signed::new_unhashed(inner, signature)),
            Address::repeat_byte(byte),
        )
    }

    const BLOCK_NUMBER: u64 = 4_242;

    fn invariants() -> BlockInvariants {
        BlockInvariants {
            base: OpFlashblockPayloadBase {
                parent_hash: B256::repeat_byte(0xab),
                fee_recipient: Address::repeat_byte(0x11),
                block_number: BLOCK_NUMBER,
                gas_limit: 30_000_000,
                timestamp: 1_800_000_000,
                base_fee_per_gas: U256::from(1_000_000_000u64),
                ..Default::default()
            },
            withdrawals: vec![Withdrawal { index: 7, ..Default::default() }],
            withdrawals_root: B256::repeat_byte(0xcd),
        }
    }

    fn header_with_gas(gas_used: u64) -> SliceHeader {
        SliceHeader {
            gas_used,
            receipts_root: B256::repeat_byte(0x01),
            logs_bloom: Bloom::repeat_byte(0x02),
            block_hash: B256::repeat_byte(0x03),
            blob_gas_used: Some(0),
        }
    }

    fn header() -> SliceHeader {
        header_with_gas(21_000)
    }

    fn record(info: &mut Info, byte: u8) {
        info.record(signed(byte));
    }

    /// Assemble a slice and mark it published, standing in for a publish that
    /// succeeded. Production only advances the cursor once the slice is out.
    fn publish_slice(info: &mut Info, index: u64) -> MantleFlashblockPayload {
        let payload = build_flashblock(
            &invariants(),
            header(),
            info.pending_slice(),
            PayloadId::new([7u8; 8]),
            index,
            FlashblockId::NO_PREV,
        );
        info.advance_publish_cursor();
        payload
    }

    /// The block-level fields ride on the first slice only; a subscriber that
    /// missed it cannot reconstruct the block and has to resume earlier.
    #[test]
    fn the_first_slice_carries_the_block_level_fields() {
        let mut info = Info::default();
        record(&mut info, 1);

        let payload = publish_slice(&mut info, 0);

        assert_eq!(payload.base, Some(invariants().base));
    }

    #[test]
    fn later_slices_leave_the_block_level_fields_out() {
        let mut info = Info::default();
        record(&mut info, 1);

        assert_eq!(publish_slice(&mut info, 1).base, None);
    }

    /// A slice carries the transactions added since the last one, not the
    /// block so far — that is what makes it a delta.
    #[test]
    fn a_slice_carries_only_the_transactions_added_since_the_last_one() {
        let mut info = Info::default();
        record(&mut info, 1);
        record(&mut info, 2);
        let _first = publish_slice(&mut info, 0);
        record(&mut info, 3);

        let second = publish_slice(&mut info, 1);

        assert_eq!(second.diff.transactions, vec![Bytes::from(signed(3).encoded_2718())]);
    }

    /// Publishing twice with nothing executed in between must not republish:
    /// the cursor moves exactly once per slice that goes out.
    #[test]
    fn republishing_without_new_work_carries_no_transactions() {
        let mut info = Info::default();
        record(&mut info, 1);
        let _first = publish_slice(&mut info, 0);

        let second = publish_slice(&mut info, 1);

        assert!(second.diff.transactions.is_empty());
    }

    /// Assembling on its own settles nothing. A slice that is built and then
    /// dropped — the payload resolved, the encoding failed — leaves its
    /// transactions for the next slice to carry, so what subscribers have seen
    /// stays a prefix of the block.
    #[test]
    fn assembling_a_slice_leaves_its_transactions_pending() {
        let mut info = Info::default();
        record(&mut info, 1);

        let dropped = build_flashblock(
            &invariants(),
            header(),
            info.pending_slice(),
            PayloadId::new([7u8; 8]),
            0,
            FlashblockId::NO_PREV,
        );

        assert_eq!(dropped.diff.transactions.len(), 1);
        assert_eq!(info.pending_slice().len(), 1, "still pending until the slice has gone out");
    }

    /// Gas is the block running total rather than the slice's own, so a
    /// consumer reads the header field it is reconstructing.
    #[test]
    fn gas_used_is_the_block_total_not_the_slice_total() {
        let payload = build_flashblock(
            &invariants(),
            header_with_gas(63_000),
            &[signed(2)],
            PayloadId::new([7u8; 8]),
            1,
            FlashblockId::NO_PREV,
        );

        assert_eq!(payload.diff.gas_used, 63_000);
    }

    /// Intermediate slices do not pay for a state root. Zero is the agreed
    /// stand-in for "not computed"; a consumer must not read it as a claim.
    #[test]
    fn a_slice_leaves_the_state_root_unset() {
        let mut info = Info::default();

        assert_eq!(publish_slice(&mut info, 0).diff.state_root, B256::ZERO);
    }

    /// Withdrawals are block-level, so every slice repeats them in full
    /// rather than sending a delta a consumer would have to accumulate.
    #[test]
    fn every_slice_repeats_the_withdrawals_in_full() {
        let mut info = Info::default();
        let first = publish_slice(&mut info, 0);
        let second = publish_slice(&mut info, 1);

        assert_eq!(first.diff.withdrawals, invariants().withdrawals);
        assert_eq!(second.diff.withdrawals, invariants().withdrawals);
        assert_eq!(second.diff.withdrawals_root, invariants().withdrawals_root);
    }

    /// Slices past the first carry no `base`, so the block number in the
    /// metadata is the only reliable one a consumer has.
    #[test]
    fn the_metadata_names_the_block_on_every_slice() {
        let mut info = Info::default();
        let later = publish_slice(&mut info, 3);

        assert_eq!(later.metadata.block_number, BLOCK_NUMBER);
    }

    #[test]
    fn the_metadata_fields_reserved_for_upstream_stay_empty() {
        let mut info = Info::default();
        record(&mut info, 1);

        let payload = publish_slice(&mut info, 0);

        assert!(payload.metadata.new_account_balances.is_empty());
        assert!(payload.metadata.receipts.is_empty());
    }

    #[test]
    fn the_predecessor_pointer_is_carried_through() {
        let previous = FlashblockId { block_number: BLOCK_NUMBER, index: 2 };
        let nothing: [Recovered<OpTxEnvelope>; 0] = [];

        let payload = build_flashblock(
            &invariants(),
            header(),
            &nothing,
            PayloadId::new([7u8; 8]),
            3,
            previous,
        );

        assert_eq!(payload.metadata.prev_flashblock_id, previous);
    }

    /// After a restart there is no predecessor to point at, and the sentinel
    /// says so rather than naming a slice that was never published.
    #[test]
    fn a_first_slice_after_a_restart_points_at_no_predecessor() {
        let mut info = Info::default();

        assert!(publish_slice(&mut info, 0).metadata.prev_flashblock_id.is_no_prev());
    }

    #[test]
    fn the_computed_header_fields_are_carried_onto_the_slice() {
        let mut info = Info::default();

        let payload = publish_slice(&mut info, 0);

        assert_eq!(payload.diff.receipts_root, header().receipts_root);
        assert_eq!(payload.diff.logs_bloom, header().logs_bloom);
        assert_eq!(payload.diff.block_hash, header().block_hash);
        assert_eq!(payload.diff.blob_gas_used, header().blob_gas_used);
    }
}
