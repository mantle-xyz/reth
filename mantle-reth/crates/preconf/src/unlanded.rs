//! What this node announced in a flashblock and has not yet seen on chain.
//!
//! Slicing prunes each slice's transactions from the pool the moment the slice
//! goes out, half a slot before the block could be canonical, and tells the pool
//! their senders have moved on. Both are true only if the block lands. This
//! index is what makes the other case recoverable: it remembers what was
//! announced so the next build can ask the chain whether it landed.
//!
//! It is a staging area, not a ledger. A survivor of the next build's sweep is
//! handed to the fifo as a [`PreconfSource::Replay`] entry and dropped from
//! here; from that point the fifo's must-land machinery owns it, which is why
//! this type can afford a single hash where a per-build map would otherwise be
//! needed — clearing the staging list early cannot lose a transaction the fifo
//! is already holding.
//!
//! [`PreconfSource::Replay`]: crate::types::PreconfSource::Replay
//!
//! Deliberately ignorant. It does not read the chain, does not touch the disk,
//! and holds no opinion about which entries are still owed — the caller reads
//! the chain and hands the verdict in. That keeps the judgement with the only
//! component that can make it and leaves this type exhaustively unit-testable.
//!
//! # What it cannot recover
//!
//! Everything here happens at the start of a build, so a node that has stopped
//! building never gets to it. Lose the sequencer role or stall on forkchoice
//! updates and both halves stay wrong: the pool still believes those senders
//! moved on, and the transactions are in no block and no pool.
//! `flashblock.unlanded_pending` stays at whatever it reached and stops moving —
//! a gauge stuck non-zero while no blocks are being produced is that state, and
//! it clears only when the node builds again or an operator resubmits.

use std::collections::{HashMap, HashSet, VecDeque};

use alloy_primitives::{Address, B256, Bytes, TxHash};
use parking_lot::Mutex;

/// How many announced-but-unseen transactions the index holds before the oldest
/// start falling out.
///
/// A full block carries on the order of 1400 pool transactions, so this is
/// roughly three blocks' worth — long enough to cover a run of blocks that do
/// not land, short enough that the whole index is about a megabyte. Past it the
/// oldest go first: they are the ones most likely to have landed already.
pub const UNLANDED_CAP: usize = 4096;

/// One announcement on its way into the index: hash, wire encoding, signer,
/// nonce.
///
/// A tuple rather than a struct because it is only ever a handover — the build
/// projects its executed transactions into this shape and the index turns them
/// into [`UnlandedTx`], which is the form anything stored or read has.
pub type Announced = (TxHash, Bytes, Address, u64);

/// One announced transaction, with everything the sweep and the handover need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlandedTx {
    /// Transaction hash — the dedup key.
    pub hash: TxHash,
    /// Wire encoding, for rebuilding the envelope the fifo takes. Produced for
    /// the journal record anyway, so keeping it costs nothing extra.
    pub rlp: Bytes,
    /// Recovered signer. The sweep needs it to know which accounts to read, and
    /// deriving it again would be an ec-recover per entry on the path that is
    /// already recovering a block.
    pub sender: Address,
    /// Sender nonce, compared against the chain by the sweep.
    pub nonce: u64,
    /// Block height the announcing build was targeting. Telemetry only.
    pub height: u64,
}

/// Announced-but-unseen transactions, plus the block the last build sealed.
#[derive(Debug)]
pub struct Unlanded {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Insertion order, for oldest-first eviction.
    entries: VecDeque<UnlandedTx>,
    /// Dedup index over `entries`.
    seen: HashSet<TxHash>,
    /// The block the most recent build sealed, or `None` before any build has.
    /// When the next build's parent is this block, everything staged here
    /// landed with it.
    payload_hash: Option<B256>,
}

impl Default for Unlanded {
    fn default() -> Self {
        Self::new()
    }
}

impl Unlanded {
    /// An empty index.
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner::default()) }
    }

    /// Record what a slice just announced. Repeats are ignored: a replayed
    /// transaction is announced again, and the index must not grow for it.
    pub fn note_announced(&self, height: u64, txs: &[Announced]) {
        let mut inner = self.inner.lock();
        for (hash, rlp, sender, nonce) in txs {
            if !inner.seen.insert(*hash) {
                continue;
            }
            inner.entries.push_back(UnlandedTx {
                hash: *hash,
                rlp: rlp.clone(),
                sender: *sender,
                nonce: *nonce,
                height,
            });
        }
        while inner.entries.len() > UNLANDED_CAP {
            if let Some(dropped) = inner.entries.pop_front() {
                inner.seen.remove(&dropped.hash);
                metrics::counter!("flashblock.unlanded_evicted_total").increment(1);
            }
        }
        let len = inner.entries.len();
        metrics::gauge!("flashblock.unlanded_pending").set(len as f64);
    }

    /// Record the block this build sealed, so the next build can recognise it as
    /// its parent and clear the staging list without reading the chain.
    pub fn note_sealed(&self, block: B256) {
        self.inner.lock().payload_hash = Some(block);
    }

    /// Whether `parent_hash` is the block the last build sealed.
    ///
    /// True means the chain built on what this node produced, so everything
    /// staged here went with it and [`Self::clear`] is the whole sweep.
    pub fn parent_is_ours(&self, parent_hash: B256) -> bool {
        self.inner.lock().payload_hash == Some(parent_hash)
    }

    /// Drop everything staged.
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.entries.clear();
        inner.seen.clear();
        metrics::gauge!("flashblock.unlanded_pending").set(0.0);
    }

    /// Every distinct sender still staged, for the caller to read nonces for.
    pub fn senders(&self) -> HashSet<Address> {
        self.inner.lock().entries.iter().map(|e| e.sender).collect()
    }

    /// Drop entries the chain has moved past and return what survives, emptying
    /// the staging list.
    ///
    /// Taking rather than reading: a survivor goes straight to the fifo, and two
    /// homes for one transaction is how it ends up dispatched twice. `heads` is
    /// the caller's reading of account state — this type never looks at the
    /// chain itself, and a sender missing from it keeps its entries, because
    /// "we could not read that account" must not be spent as "it landed".
    pub fn take_unlanded(&self, heads: &HashMap<Address, u64>) -> Vec<UnlandedTx> {
        let mut inner = self.inner.lock();
        let survivors: Vec<UnlandedTx> = inner
            .entries
            .drain(..)
            .filter(|e| heads.get(&e.sender).is_none_or(|chain| e.nonce >= *chain))
            .collect();
        inner.seen.clear();
        metrics::gauge!("flashblock.unlanded_pending").set(0.0);
        survivors
    }

    /// How many entries are staged.
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// Whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn tx(h: u8, sender: u8, nonce: u64) -> Announced {
        (hash(h), Bytes::from(vec![h]), addr(sender), nonce)
    }

    /// A replayed transaction is announced a second time. The index tracks what
    /// is outstanding, not how often it was mentioned.
    #[test]
    fn announcing_the_same_transaction_twice_adds_one_entry() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);
        assert_eq!(idx.len(), 1);
    }

    /// Over the cap the oldest go first — they are the ones most likely to have
    /// landed already, and dropping a newer one would discard the entry most
    /// likely to still be owed.
    #[test]
    fn over_the_cap_the_oldest_entry_is_dropped() {
        let idx = Unlanded::new();
        let batch: Vec<Announced> = (0..=UNLANDED_CAP as u64)
            .map(|i| {
                let mut raw = [0u8; 32];
                raw[24..].copy_from_slice(&i.to_be_bytes());
                (TxHash::from(raw), Bytes::from(vec![0u8]), addr(1), i)
            })
            .collect();
        idx.note_announced(7, &batch);

        assert_eq!(idx.len(), UNLANDED_CAP, "the cap holds");
        let survivors = idx.take_unlanded(&HashMap::new());
        assert_eq!(survivors[0].nonce, 1, "the entry with nonce 0 was oldest and went first");
    }

    /// The healthy path: the chain built on the block this node sealed, so
    /// everything staged went with it and no account needs reading.
    #[test]
    fn the_parent_being_our_own_block_is_recognised() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);
        idx.note_sealed(B256::from([9u8; 32]));

        assert!(idx.parent_is_ours(B256::from([9u8; 32])));
        idx.clear();
        assert!(idx.is_empty());
    }

    /// A build that never reached sealing records no hash, so no parent can
    /// match it and its announcements always take the nonce sweep. That is the
    /// safe direction: a missed fast path costs account reads, a wrong one
    /// costs transactions.
    #[test]
    fn a_build_that_never_sealed_matches_no_parent() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);
        assert!(!idx.parent_is_ours(B256::from([9u8; 32])));
    }

    /// The sweep keeps what the chain has not passed and drops the rest, and
    /// takes the list with it — a survivor's next home is the fifo.
    #[test]
    fn the_sweep_keeps_what_the_chain_has_not_passed_and_empties_the_list() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5), tx(2, 0xbb, 3)]);

        let heads = HashMap::from([(addr(0xaa), 6u64), (addr(0xbb), 3u64)]);
        let survivors = idx.take_unlanded(&heads);

        assert_eq!(
            survivors.iter().map(|e| e.hash).collect::<Vec<_>>(),
            vec![hash(2)],
            "nonce 3 is still what the chain expects; nonce 5 is behind nonce 6",
        );
        assert!(idx.is_empty(), "taking empties the staging list");
    }

    /// An unreadable account gives no nonce, and "we could not read it" must not
    /// be spent as "it landed".
    #[test]
    fn a_sender_absent_from_the_heads_keeps_its_entries() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);

        let survivors = idx.take_unlanded(&HashMap::new());

        assert_eq!(survivors.len(), 1);
    }

    /// The sweep has to know which accounts to read before it can read any.
    #[test]
    fn senders_reports_each_distinct_sender_once() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5), tx(2, 0xaa, 6), tx(3, 0xbb, 0)]);
        assert_eq!(idx.senders(), HashSet::from([addr(0xaa), addr(0xbb)]));
    }

    /// Taking clears the dedup index too, or a transaction announced again
    /// after being handed off would be silently dropped on the floor.
    #[test]
    fn a_transaction_can_be_staged_again_after_being_taken() {
        let idx = Unlanded::new();
        idx.note_announced(7, &[tx(1, 0xaa, 5)]);
        idx.take_unlanded(&HashMap::new());
        idx.note_announced(8, &[tx(1, 0xaa, 5)]);
        assert_eq!(idx.len(), 1);
    }
}
