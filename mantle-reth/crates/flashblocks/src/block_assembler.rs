//! Block assembly from flashblocks.

use alloy_consensus::{Header, Sealed, proofs};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{B256, Bytes, bytes::BufMut};
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
    PraguePayloadFields,
};
use alloy_rpc_types_eth::Withdrawal;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;
use op_alloy_rpc_types_engine::{
    OpExecutionPayload, OpExecutionPayloadSidecar, OpExecutionPayloadV4, OpFlashblockPayloadBase,
};
use op_revm::L1BlockInfo;
use reth_optimism_primitives::OpBlock;

use crate::{ExecutionError, ProtocolError, Result};

/// Result of assembling a block from flashblocks.
#[derive(Debug, Clone)]
pub struct AssembledBlock {
    /// The reconstructed block.
    pub block: OpBlock,
    /// The base payload data from the first flashblock.
    pub base: OpFlashblockPayloadBase,
    /// The flashblocks used to assemble this block.
    pub flashblocks: Vec<MantleFlashblockPayload>,
    /// The sealed header for this block.
    pub header: Sealed<Header>,
}

impl AssembledBlock {
    /// Extracts L1 block info from the assembled block's body.
    pub fn l1_block_info(&self) -> Result<L1BlockInfo> {
        reth_optimism_evm::extract_l1_info(&self.block.body)
            .map_err(|e| ExecutionError::L1BlockInfo(e.to_string()).into())
    }
}

/// Assembles blocks from flashblocks.
#[derive(Debug, Default)]
pub struct BlockAssembler;

impl BlockAssembler {
    /// Creates a new block assembler.
    pub const fn new() -> Self {
        Self
    }

    /// Assembles a complete block from a slice of flashblocks for a single block number.
    ///
    /// # Errors
    /// Returns an error if the slice is empty, the first flashblock is missing its
    /// base payload, or block conversion fails.
    pub fn assemble(flashblocks: &[MantleFlashblockPayload]) -> Result<AssembledBlock> {
        let first = flashblocks.first().ok_or(ProtocolError::EmptyFlashblocks)?;
        let base = first.base.clone().ok_or(ProtocolError::MissingBase)?;
        let latest_flashblock = flashblocks.last().ok_or(ProtocolError::EmptyFlashblocks)?;

        let transactions: Vec<Bytes> = flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals: Vec<Withdrawal> =
            flashblocks.iter().flat_map(|flashblock| flashblock.diff.withdrawals.clone()).collect();

        // `OpExecutionPayloadV4` sets `withdrawals_root` directly instead of
        // computing it from the list.
        let execution_payload = OpExecutionPayloadV4 {
            payload_inner: ExecutionPayloadV3 {
                blob_gas_used: latest_flashblock.diff.blob_gas_used.unwrap_or_default(),
                excess_blob_gas: 0,
                payload_inner: ExecutionPayloadV2 {
                    withdrawals,
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: base.parent_hash,
                        fee_recipient: base.fee_recipient,
                        state_root: latest_flashblock.diff.state_root,
                        receipts_root: latest_flashblock.diff.receipts_root,
                        logs_bloom: latest_flashblock.diff.logs_bloom,
                        prev_randao: base.prev_randao,
                        block_number: base.block_number,
                        gas_limit: base.gas_limit,
                        gas_used: latest_flashblock.diff.gas_used,
                        timestamp: base.timestamp,
                        extra_data: base.extra_data.clone(),
                        base_fee_per_gas: base.base_fee_per_gas,
                        block_hash: latest_flashblock.diff.block_hash,
                        transactions,
                    },
                },
            },
            withdrawals_root: latest_flashblock.diff.withdrawals_root,
        };

        let sidecar = OpExecutionPayloadSidecar::v4(
            CancunPayloadFields {
                parent_beacon_block_root: base.parent_beacon_block_root,
                versioned_hashes: vec![],
            },
            PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
        );

        let block: OpBlock = OpExecutionPayload::V4(execution_payload)
            .try_into_block_with_sidecar(&sidecar)
            .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?;

        // The final hash is not known until the block is sealed by the sequencer.
        let sealed_header = block.header.clone().seal(B256::ZERO);

        Ok(AssembledBlock { block, base, flashblocks: flashblocks.to_vec(), header: sealed_header })
    }

    /// Refreshes a same-block pending header without rebuilding the full block body.
    pub fn refresh_same_block_header(
        previous_header: &Sealed<Header>,
        flashblocks: &[MantleFlashblockPayload],
    ) -> Result<Sealed<Header>> {
        let latest_flashblock = flashblocks.last().ok_or(ProtocolError::EmptyFlashblocks)?;
        let transactions_root = proofs::ordered_trie_root_with_encoder(
            &flashblocks
                .iter()
                .flat_map(|flashblock| flashblock.diff.transactions.iter())
                .collect::<Vec<_>>(),
            |transaction, buf| buf.put_slice(transaction.as_ref()),
        );

        let mut header = previous_header.inner().clone();
        header.transactions_root = transactions_root;
        header.state_root = latest_flashblock.diff.state_root;
        header.receipts_root = latest_flashblock.diff.receipts_root;
        header.logs_bloom = latest_flashblock.diff.logs_bloom;
        header.gas_used = latest_flashblock.diff.gas_used;
        if header.withdrawals_root.is_some() {
            header.withdrawals_root = Some(latest_flashblock.diff.withdrawals_root);
        }
        if previous_header.inner().blob_gas_used.is_some() {
            header.blob_gas_used = Some(latest_flashblock.diff.blob_gas_used.unwrap_or_default());
        }

        Ok(header.seal(B256::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Sealable;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bloom, TxKind, U256};
    use alloy_rpc_types_engine::PayloadId;
    use mantle_reth_flashblocks_types::MantleFlashblockMetadata;
    use op_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use op_alloy_rpc_types_engine::OpFlashblockPayloadDelta;

    use super::*;
    use crate::ProtocolError;

    fn create_test_flashblock(index: u64, with_base: bool) -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            payload_id: PayloadId::default(),
            index,
            base: if with_base {
                Some(OpFlashblockPayloadBase {
                    parent_beacon_block_root: B256::ZERO,
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    prev_randao: B256::ZERO,
                    block_number: 100,
                    gas_limit: 30_000_000,
                    timestamp: 1700000000,
                    extra_data: Bytes::default(),
                    base_fee_per_gas: U256::from(1000000000u64),
                })
            } else {
                None
            },
            diff: OpFlashblockPayloadDelta {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: MantleFlashblockMetadata {
                block_number: 100,
                ..MantleFlashblockMetadata::default()
            },
        }
    }

    // Mantle's `TxDeposit` carries two extra RLP fields (`eth_value`, `eth_tx_value`)
    // over the OP layout, so Base's hex fixtures do not decode here.
    fn deposit_tx() -> Bytes {
        let tx = TxDeposit {
            source_hash: B256::repeat_byte(0x11),
            from: Address::repeat_byte(0x22),
            to: TxKind::Call(Address::repeat_byte(0x33)),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: false,
            eth_value: 0,
            input: Bytes::default(),
            eth_tx_value: None,
        };
        OpTxEnvelope::Deposit(tx.seal_slow()).encoded_2718().into()
    }

    #[test]
    fn test_assemble_single_flashblock() {
        let flashblocks = vec![create_test_flashblock(0, true)];

        let assembled = BlockAssembler::assemble(&flashblocks).unwrap();
        assert_eq!(assembled.base.block_number, 100);
        assert_eq!(assembled.flashblocks.len(), 1);
    }

    #[test]
    fn test_assemble_multiple_flashblocks() {
        let flashblocks = vec![
            create_test_flashblock(0, true),
            create_test_flashblock(1, false),
            create_test_flashblock(2, false),
        ];

        let assembled = BlockAssembler::assemble(&flashblocks).unwrap();
        assert_eq!(assembled.flashblocks.len(), 3);
    }

    #[test]
    fn test_assemble_propagates_blob_gas_used_from_latest_flashblock() {
        let mut fb0 = create_test_flashblock(0, true);
        fb0.diff.blob_gas_used = Some(10);

        let mut fb1 = create_test_flashblock(1, false);
        fb1.diff.blob_gas_used = Some(42_000);

        let assembled = BlockAssembler::assemble(&[fb0, fb1]).unwrap();
        assert_eq!(assembled.block.header.blob_gas_used, Some(42_000));
    }

    #[test]
    fn test_refresh_same_block_header_matches_full_assembly() {
        let mut fb0 = create_test_flashblock(0, true);
        fb0.diff.transactions = vec![deposit_tx()];
        fb0.diff.blob_gas_used = Some(10);

        let mut fb1 = create_test_flashblock(1, false);
        fb1.diff.transactions = vec![deposit_tx()];
        fb1.diff.state_root = B256::from([0x11; 32]);
        fb1.diff.receipts_root = B256::from([0x22; 32]);
        fb1.diff.logs_bloom = Bloom::from([0x33; 256]);
        fb1.diff.gas_used = 42_000;
        fb1.diff.blob_gas_used = Some(42_000);

        let mut fb2 = create_test_flashblock(2, false);
        fb2.diff.transactions = vec![deposit_tx()];
        fb2.diff.state_root = B256::from([0x44; 32]);
        fb2.diff.receipts_root = B256::from([0x55; 32]);
        fb2.diff.logs_bloom = Bloom::from([0x66; 256]);
        fb2.diff.gas_used = 63_000;
        fb2.diff.blob_gas_used = None;

        let flashblocks = vec![fb0, fb1, fb2];
        let previous_header = BlockAssembler::assemble(&flashblocks[..2]).unwrap().header;
        let refreshed_header =
            BlockAssembler::refresh_same_block_header(&previous_header, &flashblocks).unwrap();
        let expected_header = BlockAssembler::assemble(&flashblocks).unwrap().header;

        assert_eq!(refreshed_header.inner(), expected_header.inner());
        assert_eq!(refreshed_header.hash(), B256::ZERO);
    }

    #[test]
    fn test_assemble_empty_flashblocks_fails() {
        let flashblocks: Vec<MantleFlashblockPayload> = vec![];
        let result = BlockAssembler::assemble(&flashblocks);
        assert!(matches!(
            result,
            Err(crate::StateProcessorError::Protocol(ProtocolError::EmptyFlashblocks))
        ));
    }

    #[test]
    fn test_assemble_missing_base_fails() {
        let flashblocks = vec![create_test_flashblock(0, false)];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(matches!(
            result,
            Err(crate::StateProcessorError::Protocol(ProtocolError::MissingBase))
        ));
    }
}
