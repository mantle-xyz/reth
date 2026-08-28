//! Archive of recently published slices, for subscribers that reconnect.
//!
//! Live delivery is fire-and-forget: a subscriber that is not connected when
//! a slice goes out has missed it. This keeps the last few slices around so a
//! subscriber that names where it left off can be caught up before it joins
//! the live stream.

use std::{collections::VecDeque, num::NonZeroUsize};

use tokio_tungstenite::tungstenite::Utf8Bytes;

/// Where a slice sits in the stream. Ordered by block first, then by index
/// within the block, which is the order slices are published in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlashblockPosition {
    /// L2 block the slice belongs to.
    pub block_number: u64,
    /// Slice index within that block.
    pub flashblock_index: u64,
}

/// Bounded archive of published slices, keyed by position.
///
/// Entries are held already serialized: publishing pays for the encoding once
/// and every replay is a refcount bump.
#[derive(Debug)]
pub struct FlashblockRingBuffer {
    entries: VecDeque<(FlashblockPosition, Utf8Bytes)>,
    capacity: usize,
}

impl FlashblockRingBuffer {
    /// Create an archive holding at most `capacity` slices.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self { entries: VecDeque::with_capacity(capacity.get()), capacity: capacity.get() }
    }

    /// Archive a slice, evicting the oldest once full.
    ///
    /// Rebuilding a block restarts its indices, so an arriving position may
    /// not follow the last one stored. Everything from that position onward
    /// is dropped: those slices belong to a payload that was abandoned, and
    /// replaying them would hand a subscriber transactions that will never
    /// land. Dropping them also keeps positions ascending, which the lookups
    /// below rely on.
    pub fn push(&mut self, position: FlashblockPosition, payload: Utf8Bytes) {
        while self.entries.back().is_some_and(|(stored, _)| *stored >= position) {
            self.entries.pop_back();
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((position, payload));
    }

    /// Slices published strictly after `cutoff`, oldest first.
    ///
    /// Strictly after, so a subscriber naming its last received slice is not
    /// sent that slice again.
    pub fn entries_after(
        &self,
        cutoff: &FlashblockPosition,
    ) -> Vec<(FlashblockPosition, Utf8Bytes)> {
        let start = self.entries.partition_point(|(stored, _)| stored <= cutoff);
        self.entries.iter().skip(start).cloned().collect()
    }

    /// Whether `cutoff` sits before everything retained, meaning the window
    /// has moved past it and replay cannot close the gap.
    pub fn precedes_window(&self, cutoff: &FlashblockPosition) -> bool {
        self.oldest().is_some_and(|oldest| *cutoff < oldest)
    }

    /// Position of the oldest slice still held.
    pub fn oldest(&self) -> Option<FlashblockPosition> {
        self.entries.front().map(|(position, _)| *position)
    }

    /// Position of the newest slice held.
    pub fn latest(&self) -> Option<FlashblockPosition> {
        self.entries.back().map(|(position, _)| *position)
    }

    /// Whether `cutoff` sits past everything held. Either the subscriber's
    /// block was rebuilt out from under it, or it is not talking to this
    /// producer — nothing published reaches that far either way.
    pub fn exceeds_window(&self, cutoff: &FlashblockPosition) -> bool {
        self.latest().is_some_and(|latest| *cutoff > latest)
    }

    /// Slices currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been archived yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use tokio_tungstenite::tungstenite::Utf8Bytes;

    use super::{FlashblockPosition, FlashblockRingBuffer};

    fn cap(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("capacity is non-zero")
    }

    fn pos(block_number: u64, flashblock_index: u64) -> FlashblockPosition {
        FlashblockPosition { block_number, flashblock_index }
    }

    /// Payload text carrying its own position, so assertions read as the
    /// sequence a subscriber would see.
    fn payload(position: FlashblockPosition) -> Utf8Bytes {
        Utf8Bytes::from(format!("{}-{}", position.block_number, position.flashblock_index))
    }

    fn filled(
        capacity: usize,
        positions: impl IntoIterator<Item = FlashblockPosition>,
    ) -> FlashblockRingBuffer {
        let mut ring = FlashblockRingBuffer::new(cap(capacity));
        for position in positions {
            ring.push(position, payload(position));
        }
        ring
    }

    fn texts(entries: Vec<(FlashblockPosition, Utf8Bytes)>) -> Vec<String> {
        entries.into_iter().map(|(_, text)| text.as_str().to_owned()).collect()
    }

    /// A subscriber names the last slice it already has, so replay must start
    /// after it rather than resend it.
    #[test]
    fn replay_starts_strictly_after_the_requested_position() {
        let ring = filled(8, [pos(10, 0), pos(10, 1), pos(10, 2)]);

        assert_eq!(texts(ring.entries_after(&pos(10, 0))), ["10-1", "10-2"]);
    }

    /// Asking from index 0 of a block skips that block's base slice — the one
    /// carrying the block-level fields. A subscriber that needs it must ask
    /// from the previous block instead.
    #[test]
    fn replaying_from_index_zero_skips_the_base_slice() {
        let ring = filled(8, [pos(10, 0), pos(10, 1)]);

        assert_eq!(texts(ring.entries_after(&pos(10, 0))), ["10-1"]);
    }

    #[test]
    fn the_oldest_entry_is_evicted_once_capacity_is_reached() {
        let ring = filled(3, (0..5).map(|index| pos(10, index)));

        assert_eq!(ring.len(), 3);
        assert_eq!(ring.oldest(), Some(pos(10, 2)));
        assert_eq!(texts(ring.entries_after(&pos(10, 1))), ["10-2", "10-3", "10-4"]);
    }

    #[test]
    fn positions_order_across_a_block_boundary() {
        let ring = filled(8, [pos(10, 9), pos(10, 10), pos(11, 0)]);

        assert_eq!(texts(ring.entries_after(&pos(10, 9))), ["10-10", "11-0"]);
    }

    /// A cutoff older than anything retained cannot be honoured: the window
    /// has moved past it. Everything held is still handed over, but the caller
    /// has to be able to tell that a gap precedes it.
    #[test]
    fn a_cutoff_older_than_the_window_is_reported_as_a_gap() {
        let ring = filled(3, (5..8).map(|index| pos(10, index)));

        assert!(ring.precedes_window(&pos(10, 1)));
        assert_eq!(texts(ring.entries_after(&pos(10, 1))), ["10-5", "10-6", "10-7"]);
    }

    #[test]
    fn a_cutoff_inside_the_window_is_not_a_gap() {
        let ring = filled(8, [pos(10, 0), pos(10, 1)]);

        assert!(!ring.precedes_window(&pos(10, 0)));
        assert!(!ring.precedes_window(&pos(10, 1)));
    }

    /// The two window checks answer opposite questions and must not be
    /// confused: one says the archive moved past the subscriber, the other
    /// says the subscriber is past the archive.
    #[test]
    fn a_cutoff_past_the_newest_entry_exceeds_the_window() {
        let ring = filled(8, [pos(10, 1), pos(10, 2)]);

        assert!(ring.exceeds_window(&pos(10, 3)));
        assert!(ring.exceeds_window(&pos(11, 0)));
        assert!(!ring.exceeds_window(&pos(10, 2)), "level with the newest is not past it");
        assert!(!ring.exceeds_window(&pos(10, 0)));
        assert!(!ring.precedes_window(&pos(10, 3)), "the two checks never both hold");
    }

    #[test]
    fn an_empty_ring_reports_no_gap_and_replays_nothing() {
        let ring = FlashblockRingBuffer::new(cap(4));

        assert!(!ring.precedes_window(&pos(10, 0)));
        assert!(
            !ring.exceeds_window(&pos(10, 0)),
            "with nothing held there is no window either way"
        );
        assert!(ring.entries_after(&pos(10, 0)).is_empty());
        assert_eq!(ring.oldest(), None);
        assert_eq!(ring.latest(), None);
    }

    /// Rebuilding a block restarts its slice indices, so a position can arrive
    /// that is not after the last one stored. The superseded slices must go:
    /// replaying them would hand a subscriber transactions from a payload that
    /// was abandoned.
    #[test]
    fn rebuilding_a_block_drops_the_slices_it_supersedes() {
        let mut ring = filled(8, [pos(10, 0), pos(10, 1), pos(10, 2)]);

        ring.push(pos(10, 1), Utf8Bytes::from("rebuilt-10-1"));

        assert_eq!(ring.len(), 2, "10-1 and 10-2 are superseded by the rebuild");
        assert_eq!(texts(ring.entries_after(&pos(10, 0))), ["rebuilt-10-1"]);
    }

    #[test]
    fn rebuilding_from_the_base_slice_drops_the_whole_block() {
        let mut ring = filled(8, [pos(9, 4), pos(10, 0), pos(10, 1)]);

        ring.push(pos(10, 0), Utf8Bytes::from("rebuilt-10-0"));

        assert_eq!(texts(ring.entries_after(&pos(9, 3))), ["9-4", "rebuilt-10-0"]);
    }
}
