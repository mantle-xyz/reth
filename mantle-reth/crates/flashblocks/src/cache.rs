//! Cache for flashblocks that cannot be applied yet — because their canonical
//! block has not landed, or because their predecessor slice is missing.

use std::collections::HashMap;

use alloy_primitives::BlockNumber;
use mantle_reth_flashblocks_types::MantleFlashblockPayload;

/// Default number of blocks past the next block to be produced for which
/// flashblocks may be cached.
///
/// Five, on top of the next block itself — see
/// [`FlashblockCache::is_cacheable`] for the window that spells out.
pub const DEFAULT_MAX_CACHE_AHEAD_BLOCKS: u64 = 5;

/// Largest `max_cache_ahead_blocks` the CLI accepts. Already far wider than
/// anything the overlay can use.
pub const MAX_CACHE_AHEAD_BLOCKS: u64 = 256;

/// Buffers flashblocks that cannot be applied to the overlay yet. Every entry
/// goes in through [`FlashblockCache::insert_resumable`], from three paths in
/// `StateProcessor::apply_flashblock`: slices whose canonical block has not
/// landed ([`ProviderError::MissingCanonicalHeader`]), slices that cannot extend
/// the tracked overlay ([`StateProcessorError::MissingFirstFlashblock`],
/// [`StateProcessorError::NonConsecutiveBlock`]), and slices past the overlay's
/// leading depth. Once the gap closes, the caller drains the corresponding
/// entries and feeds them through normal execution.
///
/// [`ProviderError::MissingCanonicalHeader`]: crate::ProviderError::MissingCanonicalHeader
/// [`StateProcessorError::MissingFirstFlashblock`]: crate::StateProcessorError::MissingFirstFlashblock
/// [`StateProcessorError::NonConsecutiveBlock`]: crate::StateProcessorError::NonConsecutiveBlock
#[derive(Debug)]
pub struct FlashblockCache {
    entries: HashMap<BlockNumber, HashMap<u64, MantleFlashblockPayload>>,
    latest_canonical: Option<BlockNumber>,
    max_ahead_blocks: u64,
}

impl FlashblockCache {
    /// Creates a new cache initialized with the given canonical block number.
    pub fn new(latest_canonical: BlockNumber, max_ahead_blocks: u64) -> Self {
        Self { entries: HashMap::new(), latest_canonical: Some(latest_canonical), max_ahead_blocks }
    }

    /// Returns `true` when the flashblock is cached.
    ///
    /// Used to decide whether a slice with `index > 0` is a usable restart
    /// point: it only is when its predecessor is already buffered here.
    pub fn has_flashblock(&self, block_number: BlockNumber, index: u64) -> bool {
        self.entries.get(&block_number).and_then(|by_index| by_index.get(&index)).is_some()
    }

    /// Returns `true` when the flashblock's block number is no higher than
    /// `latest_canonical + max_ahead_blocks + 1` — the next block to be
    /// produced, plus `max_ahead_blocks` beyond it.
    pub const fn is_cacheable(&self, block_number: BlockNumber) -> bool {
        match self.latest_canonical {
            // Saturating: a configured `u64::MAX` would overflow.
            Some(canonical) => {
                block_number <= canonical.saturating_add(self.max_ahead_blocks).saturating_add(1)
            }
            None => false,
        }
    }

    /// Inserts a flashblock into the cache. The processor uses
    /// [`Self::insert_resumable`] instead, which adds the resumability check.
    ///
    /// Slices already held for this height under a different payload id are
    /// dropped first: they belong to a build that was replaced, and draining
    /// them alongside this one would hand the consumer two builds spliced
    /// together. Same rule the live path applies as
    /// [`SequenceValidationResult::RebuiltBlock`](crate::SequenceValidationResult::RebuiltBlock).
    ///
    /// Returns `true` if the flashblock was cached, `false` if it was rejected
    /// because its block number exceeds the cache-ahead limit.
    pub fn insert(&mut self, flashblock: MantleFlashblockPayload) -> bool {
        let block_number = flashblock.metadata.block_number;
        if !self.is_cacheable(block_number) {
            return false;
        }
        let min_block_number_to_retain = block_number.saturating_sub(self.max_ahead_blocks);
        self.entries.retain(|&bn, _| bn > min_block_number_to_retain);
        // Not gated on `base.is_some()`: a later index arriving under a new
        // payload id means the rest of this height is stale too, and keeping it
        // would splice. Dropping leaves the replacement unable to open a block
        // on its own, which the consumer already handles.
        if self
            .entries
            .get(&block_number)
            .and_then(|held| held.values().next())
            .is_some_and(|held| held.payload_id != flashblock.payload_id)
        {
            self.entries.remove(&block_number);
        }
        self.entries.entry(block_number).or_default().insert(flashblock.index, flashblock);
        true
    }

    /// Caches the flashblock only if the consumer could resume from it: index 0,
    /// or its predecessor is already held. A slice without its predecessor could
    /// never be replayed, so keeping it would only waste a slot.
    ///
    /// Returns what [`Self::insert`] returns, or `false` if not resumable.
    pub fn insert_resumable(&mut self, flashblock: MantleFlashblockPayload) -> bool {
        let resumable = flashblock.index == 0 ||
            self.has_flashblock(flashblock.metadata.block_number, flashblock.index - 1);
        resumable && self.insert(flashblock)
    }

    /// Drains all cached flashblocks for the given block number, returning them
    /// sorted by index. Returns an empty `Vec` when nothing is cached.
    pub fn drain(&mut self, block_number: BlockNumber) -> Vec<MantleFlashblockPayload> {
        let Some(by_index) = self.entries.remove(&block_number) else {
            return Vec::new();
        };
        let mut flashblocks: Vec<MantleFlashblockPayload> = by_index.into_values().collect();
        flashblocks.sort_by_key(|fb| fb.index);
        flashblocks
    }

    /// Updates the latest canonical block number and evicts any cached entries
    /// at or below it (they can no longer be useful).
    pub fn update_canonical(&mut self, block_number: BlockNumber) {
        self.latest_canonical = Some(block_number);
        self.entries.retain(|&bn, _| bn > block_number);
    }

    /// Drops every cached slice. The canonical anchor, and so the cacheable
    /// window, is kept.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns the number of distinct block numbers currently cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the total number of individual flashblocks cached across all
    /// block numbers.
    pub fn total_flashblocks(&self) -> usize {
        self.entries.values().map(|by_index| by_index.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use mantle_reth_flashblocks_types::MantleFlashblockMetadata;

    use super::*;

    fn cache_at(latest_canonical: BlockNumber) -> FlashblockCache {
        FlashblockCache::new(latest_canonical, DEFAULT_MAX_CACHE_AHEAD_BLOCKS)
    }

    fn make_flashblock(block_number: u64, index: u64) -> MantleFlashblockPayload {
        MantleFlashblockPayload {
            index,
            metadata: MantleFlashblockMetadata {
                block_number,
                ..MantleFlashblockMetadata::default()
            },
            ..MantleFlashblockPayload::default()
        }
    }

    #[test]
    fn insert_and_drain() {
        let mut cache = cache_at(10);

        assert!(cache.insert(make_flashblock(11, 1)));
        assert!(cache.insert(make_flashblock(11, 0)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_flashblocks(), 2);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn drain_empty() {
        let mut cache = cache_at(0);
        let drained = cache.drain(42);
        assert!(drained.is_empty());
    }

    #[test]
    fn rejects_beyond_cache_limit() {
        let mut cache = cache_at(10);

        assert!(cache.insert(make_flashblock(16, 0)));
        assert!(!cache.insert(make_flashblock(17, 0)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn update_canonical_evicts_old_entries() {
        let mut cache = cache_at(10);

        assert!(cache.insert(make_flashblock(11, 0)));
        assert!(cache.insert(make_flashblock(12, 0)));
        assert!(cache.insert(make_flashblock(13, 0)));
        assert_eq!(cache.len(), 3);

        cache.update_canonical(12);
        assert_eq!(cache.len(), 1);
        assert!(cache.drain(11).is_empty());
        assert!(cache.drain(12).is_empty());
        assert_eq!(cache.drain(13).len(), 1);
    }

    /// Inserting at the ahead limit evicts the whole block at the retention
    /// boundary while preserving the next block. This is independent of
    /// canonical progress: no `update_canonical` call drives the eviction.
    #[test]
    fn inserting_at_the_ahead_limit_evicts_only_blocks_at_or_below_the_retention_boundary() {
        for (max_ahead_blocks, newest) in [(5, 106), (2, 103)] {
            let mut cache = FlashblockCache::new(100, max_ahead_blocks);
            assert!(cache.insert(make_flashblock(101, 0)));
            assert!(cache.insert(make_flashblock(101, 1)));
            assert!(cache.insert(make_flashblock(102, 0)));
            assert_eq!(cache.total_flashblocks(), 3);

            assert!(cache.insert(make_flashblock(newest, 0)));

            assert!(cache.drain(101).is_empty(), "all slices at the boundary must be evicted");
            assert_eq!(cache.len(), 2);
            assert_eq!(cache.total_flashblocks(), 2);
            let retained = cache.drain(102);
            assert_eq!(retained.len(), 1, "the first block above the boundary must survive");
            assert_eq!(retained[0].metadata.block_number, 102);
            assert_eq!(retained[0].index, 0);
            let inserted = cache.drain(newest);
            assert_eq!(inserted.len(), 1);
            assert_eq!(inserted[0].metadata.block_number, newest);
            assert!(cache.is_empty());
        }
    }

    #[test]
    fn an_unbounded_window_does_not_overflow() {
        let cache = FlashblockCache {
            entries: HashMap::new(),
            latest_canonical: Some(10),
            max_ahead_blocks: u64::MAX,
        };
        assert!(cache.is_cacheable(u64::MAX));
    }

    #[test]
    fn not_cacheable_without_canonical() {
        let mut cache = FlashblockCache {
            entries: HashMap::new(),
            latest_canonical: None,
            max_ahead_blocks: DEFAULT_MAX_CACHE_AHEAD_BLOCKS,
        };
        assert!(!cache.is_cacheable(1));
        assert!(!cache.insert(make_flashblock(1, 0)));
    }

    #[test]
    fn duplicate_index_keeps_latest() {
        let mut cache = cache_at(10);

        let mut fb_old = make_flashblock(11, 0);
        fb_old.diff.state_root = alloy_primitives::B256::ZERO;
        let mut fb_new = make_flashblock(11, 0);
        fb_new.diff.state_root = alloy_primitives::B256::with_last_byte(1);

        assert!(cache.insert(fb_old));
        assert!(cache.insert(fb_new.clone()));
        assert_eq!(cache.total_flashblocks(), 1);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].diff.state_root, fb_new.diff.state_root);
    }

    /// A new build may arrive while its canonical parent is still missing.
    /// Replaying it must not append a higher index from the abandoned build.
    #[test]
    fn rebuilt_base_discards_cached_slices_from_previous_payload() {
        use alloy_rpc_types_engine::PayloadId;

        let mut cache = cache_at(10);
        let mut old_base = make_flashblock(11, 0);
        old_base.payload_id = PayloadId::new([1; 8]);
        let mut old_following = make_flashblock(11, 1);
        old_following.payload_id = old_base.payload_id;
        let mut new_base = make_flashblock(11, 0);
        new_base.payload_id = PayloadId::new([2; 8]);

        assert!(cache.insert(old_base));
        assert!(cache.insert(old_following));
        assert!(cache.insert(new_base.clone()));

        let replay = cache.drain(11);
        assert_eq!(replay.len(), 1, "old index 1 must not follow the replacement base");
        assert_eq!(replay[0].payload_id, new_base.payload_id);
    }

    #[test]
    fn multiple_flashblocks_same_block() {
        let mut cache = cache_at(10);

        assert!(cache.insert(make_flashblock(11, 0)));
        assert!(cache.insert(make_flashblock(11, 1)));
        assert!(cache.insert(make_flashblock(11, 2)));
        assert_eq!(cache.total_flashblocks(), 3);
        assert_eq!(cache.len(), 1);

        let drained = cache.drain(11);
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].index, 0);
        assert_eq!(drained[1].index, 1);
        assert_eq!(drained[2].index, 2);
    }

    #[test]
    fn a_continuation_without_its_predecessor_is_not_cached() {
        let mut cache = cache_at(10);
        assert!(!cache.insert_resumable(make_flashblock(11, 2)));
        assert!(!cache.has_flashblock(11, 2));
    }

    #[test]
    fn a_continuation_after_its_predecessor_is_cached() {
        let mut cache = cache_at(10);
        assert!(cache.insert_resumable(make_flashblock(11, 0)));
        assert!(cache.insert_resumable(make_flashblock(11, 1)));
        assert!(cache.insert_resumable(make_flashblock(11, 2)));
    }

    #[test]
    fn clear_drops_every_slice_but_keeps_the_window() {
        let mut cache = cache_at(10);
        assert!(cache.insert(make_flashblock(11, 0)));
        assert!(cache.insert(make_flashblock(12, 0)));

        cache.clear();

        assert!(!cache.has_flashblock(11, 0));
        assert!(!cache.has_flashblock(12, 0));
        assert!(cache.is_empty());
        assert!(cache.is_cacheable(16), "the window is still anchored at canonical 10");
        assert!(!cache.is_cacheable(17));
    }

    #[test]
    fn a_base_slice_is_always_resumable() {
        let mut cache = cache_at(10);
        assert!(cache.insert_resumable(make_flashblock(11, 0)));
    }
}
