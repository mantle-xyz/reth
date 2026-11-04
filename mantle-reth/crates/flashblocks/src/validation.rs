//! Flashblock sequence validation and reorganization detection.

use alloy_primitives::B256;
use mantle_reth_flashblocks_types::FlashblockId;

/// Result of validating a flashblock's position in the sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceValidationResult {
    /// Next consecutive flashblock within the current block (same block, index + 1).
    NextInSequence,
    /// First flashblock (index 0) of the next block (block + 1).
    FirstOfNextBlock,
    /// Duplicate flashblock (same block and index) - should be ignored.
    Duplicate,
    /// First slice of a *different* build of the block already being tracked.
    ///
    /// A second payload job for one height starts its slice index at zero
    /// again and re-sends the base, so the index moving backwards alongside a
    /// base payload is the sequencer saying it began the block again. The
    /// slice replaces the overlay rather than extending it.
    RebuiltBlock,
    /// A slice of the tracked block from a build other than the one the overlay
    /// holds, which cannot replace it either.
    ///
    /// Indices restart per build, so such a slice names as its predecessor the
    /// very position the overlay sits at: position and link both read as sound
    /// and only the payload id separates them. Extending would execute one
    /// build's transactions on another's state — a pending block no build ever
    /// produced. The overlay is dropped instead, which costs the rest of this
    /// block and nothing after it.
    ForeignBuild,
    /// Non-sequential index within the same block - indicates missed flashblocks.
    NonSequentialGap {
        /// Expected flashblock index.
        expected: u64,
        /// Actual incoming flashblock index.
        actual: u64,
    },
    /// Incoming flashblock cannot be linked to the tracked block: either the
    /// next block opened with a non-zero index, or the block number is neither
    /// the tracked one nor its successor — in which case `index` may well be 0.
    InvalidNewBlockIndex {
        /// Block number of the incoming flashblock.
        block_number: u64,
        /// Index received alongside that block number; not necessarily non-zero.
        index: u64,
    },
    /// Incoming flashblock does not link back to the currently tracked latest flashblock.
    NonSequentialPredecessor {
        /// Expected predecessor flashblock id.
        expected: FlashblockId,
        /// Actual predecessor flashblock id reported by the incoming flashblock.
        actual: FlashblockId,
    },
}

/// How an incoming slice's build relates to the one the overlay is tracking.
///
/// An enum rather than two booleans: "carries a base" only matters for a slice
/// from another build, and the pair `(same build, carries a base)` has no
/// meaning the validator acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingBuild {
    /// Same payload id — a continuation of what the overlay holds.
    Same,
    /// Another payload id, carrying a base payload: it can replace the overlay.
    OtherWithBase,
    /// Another payload id and no base payload: it can neither extend the
    /// overlay nor open one in its place.
    OtherWithoutBase,
}

/// Stateless validator for flashblock sequence ordering.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlashblockSequenceValidator;

impl FlashblockSequenceValidator {
    /// Validates whether an incoming flashblock links to the current latest flashblock.
    ///
    /// `incoming_build` says how the slice's payload id relates to the one the
    /// overlay is tracking; see [`IncomingBuild`].
    pub fn validate(
        latest_block_number: u64,
        latest_flashblock_index: u64,
        incoming_block_number: u64,
        incoming_index: u64,
        incoming_prev_flashblock_id: FlashblockId,
        incoming_build: IncomingBuild,
    ) -> SequenceValidationResult {
        let latest_flashblock_id =
            FlashblockId { block_number: latest_block_number, index: latest_flashblock_index };

        // Ahead of the duplicate check, which this would otherwise fall into
        // whenever the overlay has not yet moved past its own base slice: both
        // attempts put a base at index 0, so position alone cannot separate a
        // rebuild from a retransmission.
        if incoming_block_number == latest_block_number &&
            incoming_index == 0 &&
            incoming_build == IncomingBuild::OtherWithBase
        {
            return SequenceValidationResult::RebuiltBlock;
        }

        // Every other slice of this height from another build. Scoped to the
        // tracked block number on purpose: the *next* block's base is a new
        // build by definition and, once it has linked to the overlay's last
        // slice, belongs on top of the overlay, not in place of it. Ahead of the link check below,
        // which such a slice passes — its predecessor is the position the overlay sits at.
        if incoming_block_number == latest_block_number && incoming_build != IncomingBuild::Same {
            return SequenceValidationResult::ForeignBuild;
        }

        if incoming_block_number == latest_block_number && incoming_index == latest_flashblock_index
        {
            return SequenceValidationResult::Duplicate;
        }

        // `NO_PREV` names nothing to compare here. A base slice whose parent
        // is in the overlay is judged by `base_links_to_overlay` instead,
        // where `NO_PREV` does not link.
        if !incoming_prev_flashblock_id.is_no_prev() &&
            incoming_prev_flashblock_id != latest_flashblock_id
        {
            return SequenceValidationResult::NonSequentialPredecessor {
                expected: latest_flashblock_id,
                actual: incoming_prev_flashblock_id,
            };
        }

        let next_flashblock_index = latest_flashblock_index.saturating_add(1);
        if incoming_block_number == latest_block_number && incoming_index == next_flashblock_index {
            return SequenceValidationResult::NextInSequence;
        }

        if incoming_block_number == latest_block_number + 1 && incoming_index == 0 {
            return SequenceValidationResult::FirstOfNextBlock;
        }

        if incoming_block_number == latest_block_number {
            return SequenceValidationResult::NonSequentialGap {
                expected: next_flashblock_index,
                actual: incoming_index,
            };
        }

        SequenceValidationResult::InvalidNewBlockIndex {
            block_number: incoming_block_number,
            index: incoming_index,
        }
    }
}

/// Result of a reorganization detection check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReorgDetectionResult {
    /// Transaction sequences match exactly.
    NoReorg,
    /// Transaction sequences differ (counts included for diagnostics).
    ReorgDetected {
        /// Number of transactions in the tracked (pending) sequence.
        tracked_count: usize,
        /// Number of transactions in the canonical chain sequence.
        canonical_count: usize,
    },
}

impl ReorgDetectionResult {
    /// Returns `true` if a reorganization was detected.
    #[inline]
    pub const fn is_reorg(&self) -> bool {
        matches!(self, Self::ReorgDetected { .. })
    }

    /// Returns `true` if no reorganization was detected.
    #[inline]
    pub const fn is_no_reorg(&self) -> bool {
        matches!(self, Self::NoReorg)
    }
}

/// Detects chain reorganizations by comparing transaction hash sequences.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReorgDetector;

impl ReorgDetector {
    /// Compares tracked vs canonical transaction hashes to detect reorgs.
    ///
    /// Returns `ReorgDetected` if counts differ, hashes differ, or order differs.
    ///
    /// An empty tracked set means the overlay held no flashblocks for that canonical
    /// block, so there is nothing to compare and no reorg to report. Every L2 block
    /// carries an L1-attributes deposit, so a block the overlay does track is never
    /// legitimately empty — only an untracked one reaches here empty.
    pub fn detect(
        tracked_tx_hashes: &[B256],
        canonical_tx_hashes: &[B256],
    ) -> ReorgDetectionResult {
        if tracked_tx_hashes.is_empty() {
            return ReorgDetectionResult::NoReorg;
        }

        if tracked_tx_hashes == canonical_tx_hashes {
            ReorgDetectionResult::NoReorg
        } else {
            ReorgDetectionResult::ReorgDetected {
                tracked_count: tracked_tx_hashes.len(),
                canonical_count: canonical_tx_hashes.len(),
            }
        }
    }
}

/// Strategy for reconciling pending state against an arriving canonical block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationStrategy {
    /// No pending state exists; nothing to reconcile.
    NoPendingState,
    /// Canonical caught up to or passed the latest pending block; clear pending state.
    CatchUp,
    /// Transaction sequences diverged; rebuild pending state from canonical.
    HandleReorg,
    /// Canonical has moved too far past the overlay's earliest block; rebuild
    /// from canonical.
    DepthLimitExceeded {
        /// How far the overlay's earliest block trails behind canonical,
        /// `canonical_block_number - earliest` — not the overlay's own span.
        depth: u64,
        /// Configured maximum trailing depth.
        max_depth: u64,
    },
    /// Canonical is behind the latest pending block; keep extending pending state.
    Continue,
}

/// Selects a [`ReconciliationStrategy`] from pending and canonical block numbers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalBlockReconciler;

impl CanonicalBlockReconciler {
    /// Returns the appropriate [`ReconciliationStrategy`] based on pending vs canonical state.
    ///
    /// `canonical_block_number` must be the node's effective canonical height, not the
    /// height of the notification being handled. A caller that passes the notification
    /// evaluates every guard below in whatever frame its queue has fallen behind to, so
    /// neither `CatchUp` nor `DepthLimitExceeded` fires while the overlay drifts away
    /// from the chain.
    ///
    /// Priority: `NoPendingState` → `CatchUp` → `HandleReorg` → `DepthLimitExceeded` → `Continue`
    pub const fn reconcile(
        pending_earliest_block: Option<u64>,
        pending_latest_block: Option<u64>,
        canonical_block_number: u64,
        max_depth: u64,
        reorg_detected: bool,
    ) -> ReconciliationStrategy {
        let (earliest, latest) = match (pending_earliest_block, pending_latest_block) {
            (Some(e), Some(l)) => (e, l),
            _ => return ReconciliationStrategy::NoPendingState,
        };

        if latest <= canonical_block_number {
            return ReconciliationStrategy::CatchUp;
        }

        if reorg_detected {
            return ReconciliationStrategy::HandleReorg;
        }

        let depth = canonical_block_number.saturating_sub(earliest);
        if depth > max_depth {
            return ReconciliationStrategy::DepthLimitExceeded { depth, max_depth };
        }

        ReconciliationStrategy::Continue
    }
}

/// Whether a base slice for block N may extend the overlay's block N-1.
/// `parent_last` is the overlay's last slice of N-1 as (position, `diff.block_hash`).
pub(crate) fn base_links_to_overlay(
    parent_last: (FlashblockId, B256),
    prev_id: FlashblockId,
    prev_hash: B256,
) -> bool {
    let (position, content) = parent_last;
    !prev_id.is_no_prev() && !content.is_zero() && prev_id == position && prev_hash == content
}

/// Whether a slice at index >= 1 follows the overlay's latest slice by content.
/// A zero `prev_hash` (a producer that predates the field) is not checked.
pub(crate) fn slice_follows_by_content(latest_hash: B256, prev_hash: B256) -> bool {
    prev_hash.is_zero() || prev_hash == latest_hash
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(100, 5, 100, 6, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::Same, SequenceValidationResult::NextInSequence)]
    #[case(
        100,
        5,
        100,
        6,
        FlashblockId::NO_PREV,
        IncomingBuild::Same,
        SequenceValidationResult::NextInSequence
    )]
    #[case(100, 5, 101, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::Same, SequenceValidationResult::FirstOfNextBlock)]
    #[case(
        100,
        5,
        101,
        0,
        FlashblockId::NO_PREV,
        IncomingBuild::Same,
        SequenceValidationResult::FirstOfNextBlock
    )]
    #[case(
        100,
        5,
        100,
        5,
        FlashblockId::NO_PREV,
        IncomingBuild::Same,
        SequenceValidationResult::Duplicate
    )]
    // The next block's base slice is a new build by definition; the rebuild
    // branch must not claim it, because it belongs on top of the overlay
    // rather than in place of it.
    #[case(100, 5, 101, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::OtherWithBase, SequenceValidationResult::FirstOfNextBlock)]
    // A rebuild, with the overlay partway through the block it replaces.
    #[case(100, 5, 100, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::OtherWithBase, SequenceValidationResult::RebuiltBlock)]
    // The same rebuild, arriving while the overlay still holds only the base
    // slice: same position as what it replaces, so only the build it names
    // separates it from a retransmission.
    #[case(100, 0, 100, 0, FlashblockId { block_number: 100, index: 0 }, IncomingBuild::OtherWithBase, SequenceValidationResult::RebuiltBlock)]
    #[case(100, 0, 100, 0, FlashblockId { block_number: 100, index: 0 }, IncomingBuild::Same, SequenceValidationResult::Duplicate)]
    // Index 0 without a base payload is not a build starting over, whatever
    // else it is.
    #[case(100, 5, 100, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::Same, SequenceValidationResult::NonSequentialGap { expected: 6, actual: 0 })]
    // A replacement's continuation, reaching a consumer that never saw its
    // base. Its predecessor link names the position the overlay sits at, so
    // every check below this one passes and it reads as the next slice.
    #[case(100, 2, 100, 3, FlashblockId { block_number: 100, index: 2 }, IncomingBuild::OtherWithoutBase, SequenceValidationResult::ForeignBuild)]
    // Same, with the replacement's slice carrying a base it cannot use at a
    // non-zero index.
    #[case(100, 2, 100, 3, FlashblockId { block_number: 100, index: 2 }, IncomingBuild::OtherWithBase, SequenceValidationResult::ForeignBuild)]
    // Index 0 from another build, but with no base to restart from: it can
    // neither replace the overlay nor extend it.
    #[case(100, 5, 100, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::OtherWithoutBase, SequenceValidationResult::ForeignBuild)]
    // The next block's base is a different build by definition, so the guard
    // above must not reach it.
    #[case(100, 5, 101, 0, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::OtherWithoutBase, SequenceValidationResult::FirstOfNextBlock)]
    // An index that moves backwards without reaching 0 is a stream fault
    // either way.
    #[case(100, 5, 100, 3, FlashblockId { block_number: 100, index: 5 }, IncomingBuild::Same, SequenceValidationResult::NonSequentialGap { expected: 6, actual: 3 })]
    #[case(
        100,
        5,
        100,
        7,
        FlashblockId { block_number: 100, index: 5 },
        IncomingBuild::Same,
        SequenceValidationResult::NonSequentialGap { expected: 6, actual: 7 }
    )]
    #[case(
        100,
        5,
        101,
        3,
        FlashblockId { block_number: 100, index: 5 },
        IncomingBuild::Same,
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 101, index: 3 }
    )]
    #[case(
        100,
        5,
        105,
        0,
        FlashblockId { block_number: 100, index: 5 },
        IncomingBuild::Same,
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 105, index: 0 }
    )]
    #[case(
        100,
        5,
        101,
        0,
        FlashblockId { block_number: 100, index: 4 },
        IncomingBuild::Same,
        SequenceValidationResult::NonSequentialPredecessor {
            expected: FlashblockId { block_number: 100, index: 5 },
            actual: FlashblockId { block_number: 100, index: 4 },
        }
    )]
    #[case(
        100,
        5,
        99,
        0,
        FlashblockId { block_number: 100, index: 5 },
        IncomingBuild::Same,
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 0 }
    )]
    fn test_sequence_validator(
        #[case] latest_block: u64,
        #[case] latest_idx: u64,
        #[case] incoming_block: u64,
        #[case] incoming_idx: u64,
        #[case] incoming_prev_flashblock_id: FlashblockId,
        #[case] incoming_build: IncomingBuild,
        #[case] expected: SequenceValidationResult,
    ) {
        let result = FlashblockSequenceValidator::validate(
            latest_block,
            latest_idx,
            incoming_block,
            incoming_idx,
            incoming_prev_flashblock_id,
            incoming_build,
        );
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case(&[], &[], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01], &[0x01], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02, 0x03], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x01, 0x02], &[0x01, 0x01, 0x02], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x02, 0x03], &[0x03, 0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 3 })]
    #[case(&[0x01, 0x02], &[0x02, 0x01], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 })]
    #[case(&[0x01], &[0x01, 0x02, 0x03], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 3 })]
    // An untracked canonical block is not a reorg; see `ReorgDetector::detect`.
    #[case(&[], &[0x01], ReorgDetectionResult::NoReorg)]
    #[case(&[], &[0x01, 0x02, 0x03], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01], &[], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 0 })]
    #[case(&[0x01, 0x01, 0x02], &[0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 })]
    #[case(&[0x01, 0x02], &[0x03, 0x04], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    #[case(&[0x01, 0x02], &[0x01, 0x03], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    #[case(&[0x42], &[0x43], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 1 })]
    fn test_reorg_detector(
        #[case] tracked_bytes: &[u8],
        #[case] canonical_bytes: &[u8],
        #[case] expected: ReorgDetectionResult,
    ) {
        let tracked: Vec<B256> = tracked_bytes.iter().map(|b| B256::repeat_byte(*b)).collect();
        let canonical: Vec<B256> = canonical_bytes.iter().map(|b| B256::repeat_byte(*b)).collect();
        let result = ReorgDetector::detect(&tracked, &canonical);
        assert_eq!(result, expected);
        assert_eq!(
            result.is_reorg(),
            matches!(expected, ReorgDetectionResult::ReorgDetected { .. })
        );
    }

    #[rstest]
    #[case(None, None, 100, 10, false, ReconciliationStrategy::NoPendingState)]
    #[case(Some(100), None, 100, 10, false, ReconciliationStrategy::NoPendingState)]
    #[case(None, Some(100), 100, 10, false, ReconciliationStrategy::NoPendingState)]
    #[case(Some(100), Some(105), 105, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(105), 110, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(100), 100, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(105), 105, 10, true, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(110), 102, 10, true, ReconciliationStrategy::HandleReorg)]
    #[case(Some(100), Some(130), 120, 10, true, ReconciliationStrategy::HandleReorg)]
    #[case(Some(100), Some(120), 115, 10, false, ReconciliationStrategy::DepthLimitExceeded { depth: 15, max_depth: 10 })]
    #[case(Some(100), Some(105), 101, 0, false, ReconciliationStrategy::DepthLimitExceeded { depth: 1, max_depth: 0 })]
    #[case(Some(100), Some(110), 105, 10, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(120), 110, 10, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(105), 100, 10, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(105), 100, 0, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(100), 99, 10, false, ReconciliationStrategy::Continue)]
    fn test_reconciler(
        #[case] earliest: Option<u64>,
        #[case] latest: Option<u64>,
        #[case] canonical: u64,
        #[case] max_depth: u64,
        #[case] reorg: bool,
        #[case] expected: ReconciliationStrategy,
    ) {
        let result =
            CanonicalBlockReconciler::reconcile(earliest, latest, canonical, max_depth, reorg);
        assert_eq!(result, expected);
    }

    #[test]
    fn a_base_links_only_with_matching_position_and_content() {
        let at = FlashblockId { block_number: 9, index: 3 };
        let h = B256::repeat_byte(7);
        assert!(base_links_to_overlay((at, h), at, h));
        assert!(!base_links_to_overlay((at, h), at, B256::repeat_byte(8)), "content differs");
        assert!(
            !base_links_to_overlay((at, h), FlashblockId { block_number: 9, index: 2 }, h),
            "position differs"
        );
        assert!(
            !base_links_to_overlay((at, h), FlashblockId::NO_PREV, B256::ZERO),
            "no predecessor named"
        );
        assert!(
            !base_links_to_overlay((at, h), at, B256::ZERO),
            "position without content is not enough"
        );
        assert!(!base_links_to_overlay((at, B256::ZERO), at, B256::ZERO), "zero never links");
    }

    #[test]
    fn a_later_slice_is_checked_by_content_only_when_named() {
        let h = B256::repeat_byte(7);
        assert!(slice_follows_by_content(h, h));
        assert!(!slice_follows_by_content(h, B256::repeat_byte(8)));
        assert!(slice_follows_by_content(h, B256::ZERO), "older producer: position check alone");
    }
}
