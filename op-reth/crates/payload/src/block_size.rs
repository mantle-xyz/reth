//! Mantle Osaka block-size accounting shared by ordinary and preconf builders.

use alloy_consensus::{Header, constants::MAXIMUM_EXTRA_DATA_SIZE};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rlp::{Encodable, length_of_length};
use op_alloy_consensus::{POST_EXEC_PAYLOAD_VERSION, SDMGasEntry};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;

/// Space left for mandatory block contents when packing optional transactions, matching geth.
pub const BLOCK_SIZE_RESERVE: usize = 1_000_000;

/// Optional transactions must keep the conservative block estimate below this target.
pub const BLOCK_SIZE_PACKING_TARGET: usize = MAX_RLP_BLOCK_SIZE - BLOCK_SIZE_RESERVE;

/// Whether an optional transaction can enter the block's RLP budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawSizeAdmission {
    /// Fits the current block, including its possible SDM refund entry.
    Fits,
    /// Fits a fresh block, but the current block is full.
    BlockFull,
    /// Exceeds the packing target even in a fresh block.
    TooLarge,
}

/// A finalized Mantle Osaka block exceeded the consensus RLP limit.
#[derive(Debug, thiserror::Error)]
#[error("built block RLP size {rlp_length} exceeds maximum {max_rlp_length}")]
pub struct BlockSizeError {
    /// Actual full block RLP length.
    pub rlp_length: usize,
    /// Consensus maximum RLP length.
    pub max_rlp_length: usize,
}

/// Tracks real transaction RLP items and a conservative bound for the rest of a Mantle block.
///
/// Mantle's OP block body has empty ommers and withdrawals. The header bound includes every
/// optional field at its maximum encoded width and 32 bytes of extra data. All list prefixes
/// are sized from their current payload lengths. This estimate only controls optional packing:
/// mandatory attributes may exceed the packing target and are decided by the exact final check.
///
/// With SDM enabled, each included normal transaction reserves one refund entry with its real
/// transaction index and a maximum-width refund. This includes the post-exec payload, type byte,
/// RLP string wrapper and list prefixes, so the reserve exists before a preconf receipt is sent.
#[derive(Debug, Clone, Default)]
pub struct BlockSizeBudget {
    enabled: bool,
    header_rlp_length: usize,
    transactions_rlp_length: usize,
    transaction_count: u64,
    sdm_enabled: bool,
    sdm_entries_rlp_length: usize,
    block_number: u64,
}

impl BlockSizeBudget {
    /// A disabled budget preserves the pre-Osaka and non-Mantle behavior.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            header_rlp_length: 0,
            transactions_rlp_length: 0,
            transaction_count: 0,
            sdm_enabled: false,
            sdm_entries_rlp_length: 0,
            block_number: 0,
        }
    }

    /// Activate the limit only for Mantle blocks at or after Osaka.
    pub fn new(is_mantle: bool, osaka_active: bool, block_number: u64, sdm_enabled: bool) -> Self {
        if !is_mantle || !osaka_active {
            return Self::disabled();
        }
        Self {
            enabled: true,
            header_rlp_length: maximum_header_rlp_length(),
            block_number,
            sdm_enabled,
            ..Self::disabled()
        }
    }

    /// Classify a real RLP transaction item before execution. `sdm_refund_possible` is false
    /// for deposits and post-exec transactions, which do not create refund entries.
    pub fn admission(&self, tx_rlp_length: usize, sdm_refund_possible: bool) -> RawSizeAdmission {
        if !self.enabled {
            return RawSizeAdmission::Fits;
        }

        let empty_entries = self.entry_reserve(0, sdm_refund_possible);
        if self.block_rlp_length(tx_rlp_length, empty_entries) >= BLOCK_SIZE_PACKING_TARGET {
            return RawSizeAdmission::TooLarge;
        }

        let txs = self.transactions_rlp_length.saturating_add(tx_rlp_length);
        let entries = self
            .sdm_entries_rlp_length
            .saturating_add(self.entry_reserve(self.transaction_count, sdm_refund_possible));
        if self.block_rlp_length(txs, entries) >= BLOCK_SIZE_PACKING_TARGET {
            RawSizeAdmission::BlockFull
        } else {
            RawSizeAdmission::Fits
        }
    }

    /// Record an included transaction only after the executor returns `Ok`, including EVM
    /// reverts and halts. A validation rejection contributes no bytes or refund reservation.
    pub fn record_transaction(&mut self, tx_rlp_length: usize, sdm_refund_possible: bool) {
        if !self.enabled {
            return;
        }
        self.transactions_rlp_length = self.transactions_rlp_length.saturating_add(tx_rlp_length);
        self.sdm_entries_rlp_length = self
            .sdm_entries_rlp_length
            .saturating_add(self.entry_reserve(self.transaction_count, sdm_refund_possible));
        self.transaction_count = self.transaction_count.saturating_add(1);
    }

    /// Replace the virtual post-exec reserve with its real RLP item after successful inclusion.
    pub fn record_post_exec(&mut self, tx_rlp_length: usize) {
        self.sdm_entries_rlp_length = 0;
        self.record_transaction(tx_rlp_length, false);
    }

    /// Conservative length of the current block, including the possible post-exec transaction.
    pub fn estimated_block_rlp_length(&self) -> usize {
        self.block_rlp_length(self.transactions_rlp_length, self.sdm_entries_rlp_length)
    }

    /// Check the actual completed block before it can be published as an executed payload.
    pub fn validate_final_rlp_length(&self, rlp_length: usize) -> Result<(), BlockSizeError> {
        if self.enabled && rlp_length > MAX_RLP_BLOCK_SIZE {
            return Err(BlockSizeError { rlp_length, max_rlp_length: MAX_RLP_BLOCK_SIZE });
        }
        Ok(())
    }

    fn entry_reserve(&self, index: u64, refund_possible: bool) -> usize {
        if self.sdm_enabled && refund_possible {
            SDMGasEntry { index, gas_refund: u64::MAX }.length()
        } else {
            0
        }
    }

    fn block_rlp_length(&self, transactions_length: usize, entries_length: usize) -> usize {
        let txs = transactions_length.saturating_add(self.post_exec_reserve(entries_length));
        // Empty ommers and withdrawals each encode as one-byte empty RLP lists.
        list_length(self.header_rlp_length.saturating_add(list_length(txs)).saturating_add(2))
    }

    fn post_exec_reserve(&self, entries_length: usize) -> usize {
        if entries_length == 0 {
            return 0;
        }
        let payload_fields = POST_EXEC_PAYLOAD_VERSION
            .length()
            .saturating_add(self.block_number.length())
            .saturating_add(list_length(entries_length));
        // Payload list, then EIP-2718 type byte, then the typed transaction's RLP string wrapper.
        let typed_length = list_length(payload_fields).saturating_add(1);
        typed_length.saturating_add(length_of_length(typed_length))
    }
}

fn list_length(payload_length: usize) -> usize {
    payload_length.saturating_add(length_of_length(payload_length))
}

fn maximum_header_rlp_length() -> usize {
    Header {
        difficulty: U256::MAX,
        number: u64::MAX,
        gas_limit: u64::MAX,
        gas_used: u64::MAX,
        timestamp: u64::MAX,
        extra_data: Bytes::from(vec![0; MAXIMUM_EXTRA_DATA_SIZE]),
        base_fee_per_gas: Some(u64::MAX),
        withdrawals_root: Some(B256::ZERO),
        blob_gas_used: Some(u64::MAX),
        excess_blob_gas: Some(u64::MAX),
        parent_beacon_block_root: Some(B256::ZERO),
        requests_hash: Some(B256::ZERO),
        block_access_list_hash: Some(B256::ZERO),
        slot_number: Some(u64::MAX),
        ..Default::default()
    }
    .length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Block, BlockBody, Signed, TxEip1559, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Sealable, Signature};
    use op_alloy_consensus::{OpTxEnvelope, TxDeposit, build_post_exec_tx};

    fn budget(sdm: bool) -> BlockSizeBudget {
        BlockSizeBudget::new(true, true, 42, sdm)
    }

    fn typed(input_length: usize) -> OpTxEnvelope {
        Signed::new_unhashed(
            TxEip1559 { input: vec![0; input_length].into(), ..Default::default() },
            Signature::test_signature(),
        )
        .into()
    }

    #[test]
    fn only_mantle_osaka_enables_the_cap() {
        for (mantle, osaka) in [(false, false), (false, true), (true, false)] {
            let budget = BlockSizeBudget::new(mantle, osaka, 42, true);
            assert_eq!(budget.admission(usize::MAX, true), RawSizeAdmission::Fits);
            assert!(budget.validate_final_rlp_length(usize::MAX).is_ok());
        }
        assert_eq!(budget(false).admission(usize::MAX, true), RawSizeAdmission::TooLarge);
    }

    #[test]
    fn exact_protocol_cap_is_legal_but_one_more_byte_is_not() {
        assert!(budget(false).validate_final_rlp_length(MAX_RLP_BLOCK_SIZE).is_ok());
        let error = budget(false).validate_final_rlp_length(MAX_RLP_BLOCK_SIZE + 1).unwrap_err();
        assert_eq!(error.rlp_length, MAX_RLP_BLOCK_SIZE + 1);
        assert_eq!(error.max_rlp_length, MAX_RLP_BLOCK_SIZE);
    }

    #[test]
    fn optional_packing_uses_strict_target_and_distinguishes_a_full_block() {
        let mut budget = budget(false);
        let framing =
            budget.block_rlp_length(BLOCK_SIZE_PACKING_TARGET, 0) - BLOCK_SIZE_PACKING_TARGET;
        let exact_target = BLOCK_SIZE_PACKING_TARGET - framing;
        assert_eq!(budget.admission(exact_target - 1, true), RawSizeAdmission::Fits);
        assert_eq!(budget.admission(exact_target, true), RawSizeAdmission::TooLarge);
        budget.record_transaction(exact_target - 100, true);
        assert_eq!(budget.admission(99, true), RawSizeAdmission::Fits);
        assert_eq!(budget.admission(100, true), RawSizeAdmission::BlockFull);
    }

    #[test]
    fn mandatory_contents_may_exceed_the_optional_target() {
        let mut budget = budget(false);
        budget.record_transaction(BLOCK_SIZE_PACKING_TARGET, false);
        assert_eq!(budget.admission(1, true), RawSizeAdmission::BlockFull);
        assert!(budget.validate_final_rlp_length(BLOCK_SIZE_PACKING_TARGET + 1).is_ok());
    }

    #[test]
    fn typed_items_include_the_outer_string_wrapper_while_legacy_does_not() {
        let typed = typed(128);
        assert_eq!(
            typed.length(),
            typed.encode_2718_len() + length_of_length(typed.encode_2718_len())
        );
        let legacy: OpTxEnvelope =
            Signed::new_unhashed(TxLegacy::default(), Signature::test_signature()).into();
        assert_eq!(legacy.length(), legacy.encode_2718_len());
        let mut budget = budget(false);
        budget.record_transaction(typed.length(), true);
        budget.record_transaction(legacy.length(), true);
        assert_eq!(budget.transactions_rlp_length, typed.length() + legacy.length());
    }

    #[test]
    fn conservative_framing_covers_full_block_rlp() {
        let transactions = vec![typed(128), typed(65_536)];
        let block = Block {
            header: Header::default(),
            body: BlockBody { transactions, ommers: vec![], withdrawals: Some(Default::default()) },
        };
        let mut budget = budget(false);
        for tx in &block.body.transactions {
            budget.record_transaction(tx.length(), true);
        }
        assert!(budget.estimated_block_rlp_length() >= block.length());
    }

    #[test]
    fn sdm_reserve_includes_all_wrappers_and_variable_width_indices() {
        for count in [1, 5, 127, 128, 129, 255, 256, 257, 65_537] {
            let mut budget = budget(true);
            let mut entries = Vec::new();
            for index in 0..count {
                budget.record_transaction(1, true);
                entries.push(SDMGasEntry { index, gas_refund: u64::MAX });
            }
            let post_exec: OpTxEnvelope = build_post_exec_tx(42, entries).seal_slow().into();
            assert_eq!(
                budget.post_exec_reserve(budget.sdm_entries_rlp_length),
                post_exec.length(),
                "count={count}"
            );
        }
    }

    #[test]
    fn deposits_advance_sdm_indices_without_creating_entries() {
        let deposit: OpTxEnvelope = TxDeposit::default().seal_slow().into();
        let normal = typed(0);
        let mut budget = budget(true);
        for _ in 0..128 {
            budget.record_transaction(deposit.length(), false);
        }
        budget.record_transaction(normal.length(), true);
        let post_exec: OpTxEnvelope =
            build_post_exec_tx(42, vec![SDMGasEntry { index: 128, gas_refund: u64::MAX }])
                .seal_slow()
                .into();
        assert_eq!(budget.post_exec_reserve(budget.sdm_entries_rlp_length), post_exec.length());
        assert_eq!(budget.transaction_count, 129);
    }

    #[test]
    fn sdm_capacity_is_reserved_before_admission() {
        let plain = budget(false);
        let framing =
            plain.block_rlp_length(BLOCK_SIZE_PACKING_TARGET, 0) - BLOCK_SIZE_PACKING_TARGET;
        let candidate = BLOCK_SIZE_PACKING_TARGET - framing - 1;
        assert_eq!(plain.admission(candidate, true), RawSizeAdmission::Fits);
        assert_eq!(budget(true).admission(candidate, true), RawSizeAdmission::TooLarge);
    }

    #[test]
    fn actual_post_exec_replaces_the_reserve() {
        let mut budget = budget(true);
        budget.record_transaction(200, true);
        budget.record_transaction(100, true);
        let before = budget.estimated_block_rlp_length();
        let post_exec: OpTxEnvelope =
            build_post_exec_tx(42, vec![SDMGasEntry { index: 1, gas_refund: 1 }])
                .seal_slow()
                .into();
        budget.record_post_exec(post_exec.length());
        assert_eq!(budget.sdm_entries_rlp_length, 0);
        assert_eq!(budget.transactions_rlp_length, 300 + post_exec.length());
        assert!(budget.estimated_block_rlp_length() < before);
    }
}
