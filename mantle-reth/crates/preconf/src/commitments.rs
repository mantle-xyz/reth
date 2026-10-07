//! The record of every claim this node has made about a transaction, and the
//! `(sender, nonce)` slots those claims hold.
//!
//! A record exists for a hash whose receipt has gone out to a client, and
//! outlives that transaction's fifo entry: the `(sender, nonce)` claim that
//! refuses a replacement, the retention state behind [`SEAL_DEPTH`], and the
//! journal's eviction question ([`Commitments::is_tracked`]).
//!
//! **The journal is written elsewhere**, from the apply and from each slice, so
//! "has a journal line" is the wider set. A line with no record here is
//! slot-scoped — it survives a restart inside the slot and goes at the next
//! rotation — which is right for the ordinary transactions a slice carries.
//! It also catches a commitment whose client disconnected before its event, and
//! that one is not in the pool to be recovered from, so a crash after a rotation
//! and before the block is canonical loses it. Narrow, and stated here rather
//! than closed: closing it means establishing the record where the journal line
//! is written, which is a different place from where a client is answered.
//!
//! ## Why this is owned by `PreconfTxSet` and not by the allowlists
//!
//! It used to live on `PreconfClassifier`, which also owns the allowlists. The
//! two never shared a lock, never shared a consumer, and never appeared in the
//! same method — the only thing they shared was a constructor. Every consumer of
//! this half (the RPC handler, the canonical handler, journal restore, the
//! payload builder) already holds the fifo, so moving it there costs no new
//! handle and lets the fifo release a record directly from `drop_hash` instead
//! of through a registered callback.
//!
//! ## Locking
//!
//! Read from the builder's apply hook and from the pool best-tx step, both sync
//! `fn`s that never receive the fifo's `tokio::sync::Mutex` — hence
//! `parking_lot` here. A guard is never held across an `.await`; every accessor
//! returns an owned value and drops its guard before returning.

use alloy_primitives::{
    Address, B256, TxHash,
    map::{
        Entry,
        foldhash::{HashMap, HashSet},
    },
};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::warn;

use crate::config::PreconfConfig;

/// Safety bound on the commitment cache — 100k entries.
///
/// Each entry costs its hash key plus a `Commitment`, and a commitment record
/// additionally owns a `by_slot` entry; the bound assumes every entry is
/// preconf. Tens of MB at the ceiling, which only a stuck sweep can reach.
///
/// Not a limit that is enforced by deleting: see
/// [`Commitments::sweep`] for why exceeding it only warns.
pub const DEFAULT_COMMITMENT_CACHE_CAP: usize = 100_000;

/// How many **persisted** blocks must be stacked on top of a commitment's block
/// before its tracking (commitment record + `(sender, nonce)` slot) may be released
/// — 32.
///
/// This is the one ruler shared by the retention period here and the journal's
/// discard gate: a commitment is forgotten only once
/// `committed_height + SEAL_DEPTH <= last_block_number()`.
///
/// Two properties of that predicate matter more than the number itself:
///
/// * it is measured against `last_block_number()` (**on disk**), not `best_block_number()` (which
///   includes in-memory canonical blocks) — an un-persisted block is lost on a non-graceful exit,
///   so counting it would let a commitment be forgotten before it is durable;
/// * it is a block *depth*, not a duration — a duration says nothing about how deep a reorg can
///   reach, which is the only thing the retention period defends against.
///
/// **Why 32 and not less.** The cost of holding a slot longer is close to zero:
/// the nonce has been consumed on chain, so any *other* transaction for it is
/// rejected by the inner validator with nonce-too-low regardless; the only thing
/// the slot still blocks is exactly what it must block — a same-nonce
/// replacement of a commitment that a reorg could bring back. So the depth is
/// chosen for reorg tolerance, and the residual risk (a reorg deeper than this
/// forgets a commitment that then loses its nonce) shrinks with it.
///
/// **Why not finality.** Waiting for a finalized marker would stall on a chain
/// whose derivation pipeline has not started, pinning every slot indefinitely.
pub const SEAL_DEPTH: u64 = 32;

/// One commitment this node has made. A record exists exactly for a hash whose
/// receipt has gone out, so its **presence is the answer** to "do we still owe
/// this one"; [`Commitments::mark_promised`] is the only thing that
/// creates one.
///
/// A record exists exactly for a transaction the preconf arm owns; there is no
/// "not preconf" record, so the record's **presence is the answer**.
///
/// `slot` is the reverse link into [`CommitmentStore::by_slot`], so every
/// removal path can release the claim without scanning the index.
///
/// It is `Option` because the claim may not be *ours*, not because it happens
/// later: both writers insert the record and then call `CommitmentStore::claim`
/// under the same lock, which back-fills the field. A claim that loses to an
/// incumbent returns `Err(owner)` while the record itself stays (see
/// `mark_promised_does_not_displace_an_existing_owner`).
///
/// The invariant is exact: **`slot` is `Some(key)` iff this hash may own
/// `by_slot[key]`.**
///
/// `promised` / `committed_height` carry commitment-tracking state as fields
/// rather than one value, because the three have different lifetimes:
/// `record` is written once and never rewritten, `promised` is set once when a
/// receipt goes out, and `committed_height` is the only reversible one —
/// `uncommit` clears it on a reorg while the promise stands. See
/// the `promised` flag for why a record and a promise are not the same thing.
#[derive(Debug, Clone, Copy)]
struct Commitment {
    /// How strong the claim is — see [`ClaimKind`].
    ///
    /// Only ever moves `Announced` → `Promised`, by [`Commitments::mark_promised`].
    /// There is no way back: a receipt that reached a client cannot be unsent.
    kind: ClaimKind,
    /// The `(sender, nonce)` slot this record claimed, if it claimed one.
    slot: Option<(Address, u64)>,
    /// Height of the canonical block this commitment was observed in, if it has
    /// been observed at all.
    ///
    /// The only reversible field: [`Commitments::uncommit`] clears it when
    /// a reorg takes that block back, while the promise stands.
    committed_height: Option<u64>,
    /// Height the commitment was promised for — what bounds a promise that never
    /// lands. Past `promised_height + SEAL_DEPTH` no reorg can still land it
    /// there; a replaying one holds a fifo entry and is covered by `live`.
    promised_height: u64,
}

impl Commitment {
    /// A freshly recorded claim at `promised_height`, not yet seen on chain.
    const fn new(kind: ClaimKind, promised_height: u64) -> Self {
        Self { kind, slot: None, committed_height: None, promised_height }
    }
}

/// How strong a claim this node made about a transaction.
///
/// Both are public statements and both have to be kept, which is why they share
/// a record, a slot and the must-land machinery. They differ in **who cleans up
/// when the claim turns out false**, and that is what the per-kind behaviour
/// below follows from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// Executed, and announced to whoever is subscribed to the slice stream —
    /// but no client has been handed a receipt for it.
    ///
    /// A flashblock consumer serves this as `pending`, so the statement is
    /// public and has real readers. It is the weaker of the two because those
    /// readers reconcile on their own: the consumer has reorg detection and
    /// corrects itself at the next canonical block.
    Announced,
    /// A receipt went back to a client over
    /// `eth_sendRawTransactionWithPreconf`.
    ///
    /// Nobody downstream can repair this one. If the transaction does not land,
    /// the node lied to a caller that was synchronously waiting — which is what
    /// `preconf.tx.commitment_broken_total` counts, and why only this kind
    /// increments it.
    Promised,
}

/// The commitment records, plus the `(sender, nonce)` → hash index that makes
/// the replacement guard race-free.
///
/// ## Why both indexes share one lock
///
/// They are two views of one fact ("this transaction is a commitment"), and every
/// mutation touches both. Splitting them would mean either a documented lock
/// order (this type currently has none to get wrong — see the module docs) or a
/// window in which a record exists without its slot claim. Under one lock,
/// freezing a record and claiming its slot happen in a single critical section,
/// which is precisely what closes the race between the two.
#[derive(Debug, Default)]
struct CommitmentStore {
    /// One record per commitment, keyed by transaction hash.
    by_hash: HashMap<TxHash, Commitment>,
    /// Which transaction currently owns each `(sender, nonce)` slot.
    by_slot: HashMap<(Address, u64), TxHash>,
}

impl CommitmentStore {
    /// Removes `hash` and, if it owned its `(sender, nonce)` slot, releases it.
    ///
    /// Guarded by an equality check because the slot may already have been
    /// claimed by a different transaction; dropping it unconditionally would
    /// evict the new owner's claim.
    fn remove(&mut self, hash: &TxHash) {
        if let Some(cached) = self.by_hash.remove(hash) {
            self.release_slot_of(hash, &cached);
        }
    }

    /// Claims `key` for `hash` when the slot is free, and records the reverse
    /// link so every removal path can release it again.
    ///
    /// Only [`Commitments::mark_promised`] reaches here: a slot exists for
    /// a commitment whose receipt has gone out, and nothing else creates one.
    /// An occupant is reported rather than displaced — who owns the nonce is
    /// settled a layer up, by the queue.
    fn claim(&mut self, key: (Address, u64), hash: TxHash) -> SlotClaim {
        let claim = match self.by_slot.entry(key) {
            // Same hash re-entering (retry / re-validation): idempotent.
            Entry::Occupied(slot) if *slot.get() == hash => Ok(()),
            Entry::Occupied(slot) => Err(*slot.get()),
            Entry::Vacant(slot) => {
                slot.insert(hash);
                Ok(())
            }
        };

        // Record the reverse link exactly when the claim is ours, so
        // `release_slot_of` can find it later. Both callers insert their record
        // with `slot: None` and rely on this back-fill; neither ever writes the
        // field itself.
        if claim.is_ok() &&
            let Some(cached) = self.by_hash.get_mut(&hash)
        {
            cached.slot = Some(key);
        }

        claim
    }

    /// Releases `cached`'s slot iff it claimed one and `hash` is still the
    /// recorded owner.
    fn release_slot_of(&mut self, hash: &TxHash, cached: &Commitment) {
        let Some(key) = cached.slot else { return };
        if self.by_slot.get(&key) == Some(hash) {
            self.by_slot.remove(&key);
        }
    }
}

/// Outcome of trying to claim a `(sender, nonce)` slot at admission.
///
/// `Err` carries the hash that already owns the slot so the caller can ask the
/// fifo what state that transaction is in — the claim itself deliberately knows
/// nothing about fifo status.
pub type SlotClaim = Result<(), TxHash>;

/// The claims this node has made, and the nonces they hold.
///
/// Held by [`PreconfTxSet`](crate::preconf_tx_set::PreconfTxSet) and shared
/// through it. The writers are the RPC handler (the instant a client is handed
/// an event) and journal restore; the readers are the canonical handler, the
/// payload builder and rotation.
#[derive(Debug)]
pub struct Commitments {
    /// Mirrors `PreconfConfig::enabled`. When false this node runs no preconf
    /// machinery at all, so nothing is recorded.
    ///
    /// The leak this originally guarded against is gone — a node that has not
    /// opted in no longer builds the RPC handler, so nothing reaches here to
    /// record anything. Refusing anyway keeps the invariant this type's own,
    /// rather than a consequence of how it is wired.
    enabled: bool,
    /// The commitment records and the `(sender, nonce)` slot index.
    /// `parking_lot` ⇒ synchronous reads, usable from the builder's sync apply
    /// hook. Both indexes share this one lock — see [`CommitmentStore`].
    store: RwLock<CommitmentStore>,

    /// Warning threshold for the commitment cache. Never enforced by deleting.
    capacity: usize,

    /// Whether we are currently above [`Self::capacity`]. Tracked so the
    /// warning fires on the transition instead of once per admitted
    /// transaction, and so it still fires when the chain has stalled and
    /// [`Self::sweep`] is no longer being called.
    over_capacity: AtomicBool,

    /// The block this node sealed last, so the next build can recognise it as
    /// its parent and release what it executed without reading the chain.
    ///
    /// `parking_lot::Mutex` rather than the store's lock: it is written once per
    /// build and read once per build, and it has nothing to do with the records.
    sealed_payload: parking_lot::Mutex<Option<B256>>,

    /// Last known **persisted** block height — the reading of the ruler
    /// described on [`SEAL_DEPTH`]. Fed by the canonical-state handler once per
    /// notification via [`Self::observe_persisted`].
    ///
    /// Starts at 0, which is the safe direction: until the first notification
    /// arrives nothing is deep enough, so no commitment is released early. A
    /// stalled chain therefore pins slots rather than dropping them — the same
    /// bias every other decision here takes.
    persisted_height: AtomicU64,
}

impl Commitments {
    /// A recording registry, warning past `capacity`.
    ///
    /// Deliberately not a `new(bool, usize)`: the type this replaced took
    /// `all_preconfs` in that position, so a positional bool here is one
    /// mechanical edit away from silently turning recording off.
    pub fn enabled(capacity: usize) -> Self {
        Self::build(true, capacity)
    }

    /// The shape a node that has not opted in gets: records nothing.
    pub fn disabled() -> Self {
        Self::build(false, DEFAULT_COMMITMENT_CACHE_CAP)
    }

    fn build(enabled: bool, capacity: usize) -> Self {
        Self {
            enabled,
            store: RwLock::new(CommitmentStore::default()),
            capacity,
            over_capacity: AtomicBool::new(false),
            sealed_payload: parking_lot::Mutex::new(None),
            persisted_height: AtomicU64::new(0),
        }
    }

    /// From validated config, with the default cache bound.
    pub fn from_config(cfg: &PreconfConfig) -> Self {
        Self::build(cfg.enabled, DEFAULT_COMMITMENT_CACHE_CAP)
    }

    /// **The one same-nonce refusal the queue cannot make for itself.**
    /// `Err(owner)` when a commitment that is still owed holds
    /// `(sender, nonce)`.
    ///
    /// Read-only: admission records nothing. A conflict between two
    /// transactions that both have a queue entry is settled by the queue's own
    /// `by_sender` index — the incumbent is there to be judged, and it always
    /// wins. What is left for this is the case with no incumbent to judge: the
    /// entry is gone and the commitment is still owed, which is the retention
    /// window.
    ///
    /// Called with the fifo's lock held, so the answer cannot go stale before
    /// the enqueue that follows it.
    pub fn slot_conflict(&self, hash: TxHash, sender: &Address, nonce: u64) -> SlotClaim {
        if !self.enabled {
            return Ok(());
        }
        match self.store.read().by_slot.get(&(*sender, nonce)) {
            // Free, or ours already (a resubmit) — neither is another
            // commitment's claim.
            None => Ok(()),
            Some(owner) if *owner == hash => Ok(()),
            Some(owner) => Err(*owner),
        }
    }

    /// **Where tracking begins.** Records that `hash` executed in the block
    /// being built, and claims the `(sender, nonce)` it ran on.
    ///
    /// Called from the apply, not from the receipt — the claim is made by the
    /// transaction executing and going out in a slice, not by a client hearing
    /// about it. Two consequences follow, and both are the point:
    ///
    /// * a commitment whose client disconnected before its receipt still holds its nonce, where
    ///   before it held nothing and a same-nonce pool transaction could take it;
    /// * every executed transaction has a record, so `slot_owner` is a complete answer rather than
    ///   one that happens to cover the cases a client was waiting on.
    ///
    /// Idempotent, and **never downgrades**: a hash already `Promised` stays
    /// `Promised`, which is what a replay of an acknowledged commitment hits.
    pub fn record_announced(
        &self,
        hash: TxHash,
        sender: &Address,
        nonce: u64,
        height: u64,
    ) -> SlotClaim {
        if !self.enabled {
            return Ok(());
        }
        let key = (*sender, nonce);
        let mut store = self.store.write();

        let cached =
            store.by_hash.entry(hash).or_insert(Commitment::new(ClaimKind::Announced, height));
        cached.promised_height = height;

        let claim = store.claim(key, hash);
        let len = store.by_hash.len();
        drop(store);

        self.observe_len(len);
        claim
    }

    /// **Where commitment tracking is established.** Records that a `Success`
    /// receipt for `hash` has gone out to a client, and claims the
    /// `(sender, nonce)` it was issued against.
    ///
    /// Two callers: the RPC handler, the instant it hands a client an event,
    /// and journal restore's pre-pass, where the event went out in a previous
    /// process.
    ///
    /// **Not in lockstep with the journal.** A record is what keeps a journal
    /// line alive — rotation's only rule is [`Self::is_tracked`] — but the two
    /// are written in different places: the journal from the apply
    /// (`builder::dispatch`) and from each slice. A line whose hash never
    /// reaches here is therefore slot-scoped: it survives a restart inside the
    /// slot and is dropped at the next rotation. That is what the slice's
    /// ordinary transactions want, and it is also what a commitment whose
    /// client disconnected before its event gets, which is the narrower case —
    /// see the module docs on what that leaves exposed.
    ///
    /// **Why here and not at canonical time.** A canonical notification hands us
    /// bare transaction hashes for the whole block. To pick out our commitments
    /// it needs a record that already exists — and the commitment record cannot
    /// serve, because `forward → release_unless_committed` races the notification
    /// and may already have dropped it. A receipt, by contrast, necessarily
    /// precedes the block, so a record written here is in place before either
    /// event can happen. See [`Self::mark_committed`].
    ///
    /// There used to be a case here for a transaction whose record said it
    /// was not preconf: the claim was skipped, because such a transaction had
    /// no arm to defend the nonce with. It is unreachable now. The only
    /// writer of a record is this method, so every record is a commitment whose
    /// receipt has gone out — a transaction the door refused, or one still in
    /// flight, leaves nothing here at all.
    ///
    /// The slot claim **never displaces an existing owner**. If the slot is taken,
    /// the honest answer is that the incumbent owns the nonce; who wins is
    /// decided one layer down by `push_if_absent`. Seizing it here for a
    /// commitment that is about to lose it would make the guard refuse later
    /// replacements on behalf of a transaction that will never be applied.
    ///
    /// Returns the outcome of that claim so restore can log a commitment that
    /// arrives to find its nonce already spoken for.
    pub fn mark_promised(
        &self,
        hash: TxHash,
        sender: &Address,
        nonce: u64,
        promised_height: u64,
    ) -> SlotClaim {
        if !self.enabled {
            return Ok(());
        }
        let key = (*sender, nonce);
        let mut store = self.store.write();

        // Get-or-insert, then upgrade the kind. The insert case is journal
        // restore (this process has never seen the hash); the update case is the
        // ordinary one — the apply recorded it as `Announced` a moment earlier,
        // and this is the receipt reaching its client.
        let cached = store
            .by_hash
            .entry(hash)
            .or_insert(Commitment::new(ClaimKind::Promised, promised_height));
        cached.kind = ClaimKind::Promised;
        cached.promised_height = promised_height;

        // The record is there either way — inserted just above, or already
        // present from admission — so the slot is claimable.
        let claim = store.claim(key, hash);
        let len = store.by_hash.len();
        drop(store);

        self.observe_len(len);
        claim
    }

    /// Records that a promised transaction was observed in the canonical block
    /// at `height`. Idempotent; the newest observation wins.
    ///
    /// **No-op unless the hash already has a promise record**, and that
    /// condition is load-bearing rather than an optimisation: the caller feeds
    /// every transaction hash in the block, the overwhelming majority of which
    /// are ordinary user transactions. Acting on those would pin their nonces
    /// against replacement — a serious bug, and the reason the authority for
    /// "this is one of ours" has to be established earlier (see
    /// [`Self::mark_promised`]).
    ///
    /// Returns whether a record was updated, so the caller can count how many of
    /// a block's transactions were commitments.
    pub fn mark_committed(&self, hash: &TxHash, height: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let mut store = self.store.write();
        match store.by_hash.get_mut(hash) {
            Some(cached) => {
                cached.committed_height = Some(height);
                true
            }
            None => false,
        }
    }

    /// Withdraws the "seen on chain" observation after a reorg took the block
    /// back, **keeping the promise record and the slot**.
    ///
    /// That asymmetry is the point of the whole scheme: the commitment is live
    /// again and still owns its nonce, so the same-nonce replacement that a reorg
    /// invites is refused by the guard exactly as it was before the block. What
    /// is withdrawn is only the retention clock, which was counting toward
    /// forgetting it.
    ///
    /// Returns whether this hash had been observed on chain — which is precisely
    /// the `reorg_drift` predicate the canonical handler wants (a reverted
    /// transaction that we had recorded as committed is drift; one we never saw
    /// is not).
    pub fn uncommit(&self, hash: &TxHash) -> bool {
        if !self.enabled {
            return false;
        }
        let mut store = self.store.write();
        store.by_hash.get_mut(hash).and_then(|cached| cached.committed_height.take()).is_some()
    }

    /// Whether this hash still has a record here at all — the journal's only
    /// eviction question.
    ///
    /// The journal records commitments; it does not decide when to forget them.
    /// A record disappears from this map exactly when the commitment stops being
    /// owed: [`Self::sweep`] releases it once it has landed and been buried
    /// [`SEAL_DEPTH`] deep, or once a promise can no longer reach the block it
    /// was made for, and [`Self::release_unless_committed`] releases it when a
    /// build gives the commitment up. Keying rotation on this makes the two
    /// halves of commitment tracking unable to disagree.
    ///
    /// This is also *the* partition predicate the pool arm reads: a record
    /// exists exactly for a transaction the preconf arm owns, so no record
    /// means the pool arm takes it. Synchronous, because that arm cannot wait.
    pub fn is_tracked(&self, hash: &TxHash) -> bool {
        self.store.read().by_hash.contains_key(hash)
    }

    /// Whether a receipt for `hash` reached a client.
    ///
    /// `false` for a hash with no record at all and for one that only ever got
    /// as far as a slice — see [`ClaimKind`] for why the two are not the same
    /// failure.
    pub fn was_promised(&self, hash: &TxHash) -> bool {
        self.store.read().by_hash.get(hash).is_some_and(|c| c.kind == ClaimKind::Promised)
    }

    /// Which transaction currently owns `(sender, nonce)`, if any.
    ///
    /// Only commitment records ever own a slot, so `None` means "no in-flight
    /// preconf claim on this nonce".
    pub fn slot_owner(&self, sender: &Address, nonce: u64) -> Option<TxHash> {
        self.store.read().by_slot.get(&(*sender, nonce)).copied()
    }

    /// Record the block this build sealed.
    ///
    /// Paired with [`Self::parent_is_ours`]: together they answer "did what I
    /// executed last time land?" with one hash comparison and no chain reads,
    /// which is the healthy path and the reason this is worth keeping at all.
    pub fn note_sealed(&self, block: B256) {
        *self.sealed_payload.lock() = Some(block);
    }

    /// Whether `parent_hash` is the block the last build sealed.
    ///
    /// True means the chain built on what this node produced, so everything
    /// that build executed is on chain.
    ///
    /// False is the conservative direction and is what a superseded payload
    /// gives: the last `note_sealed` wins, so if the consensus layer took an
    /// earlier one this reads false and the caller falls back to asking the
    /// chain. Sweeping twice costs a few state reads; not sweeping loses
    /// transactions.
    pub fn parent_is_ours(&self, parent_hash: B256) -> bool {
        *self.sealed_payload.lock() == Some(parent_hash)
    }

    /// Publishes the current **persisted** block height — the reading of the
    /// ruler on [`SEAL_DEPTH`]. Called by the canonical-state handler once per
    /// notification with `BlockNumReader::last_block_number()`.
    ///
    /// Monotonic by construction: a reorg rewrites in-memory canonical blocks,
    /// but the on-disk tip only moves forward as the persistence task commits,
    /// so this takes the max rather than trusting the caller. Reorgs are handled
    /// by [`Self::uncommit`], not here.
    pub fn observe_persisted(&self, height: u64) {
        self.persisted_height.fetch_max(height, Ordering::Relaxed);
        metrics::gauge!("preconf.classifier.persisted_height").set(height as f64);
    }

    /// The retention predicate: has `committed_height` been buried under
    /// [`SEAL_DEPTH`] persisted blocks?
    ///
    /// `false` until the first [`Self::observe_persisted`] arrives, and `false`
    /// on a stalled chain — both mean "keep tracking", the safe direction.
    fn is_deep_enough(&self, committed_height: u64) -> bool {
        committed_height.saturating_add(SEAL_DEPTH) <= self.persisted_height.load(Ordering::Relaxed)
    }

    /// Drops one record and releases the slot it owned — **unless the
    /// transaction has been observed on chain and is not yet buried
    /// [`SEAL_DEPTH`] deep**, in which case the record is kept.
    ///
    /// Driven by the fifo's `drop_hash`, the single convergence point of every
    /// fifo removal path, and by the validator when admission is rejected.
    ///
    /// The exemption exists because one of those removal paths — `forward()` —
    /// fires on "this sender's nonce moved past the entry", which is both
    /// ambiguous about *which* transaction advanced it and, even when it was
    /// ours, revocable: releasing there would let a reorg strand a commitment
    /// without its nonce. Every other path removes a transaction that was never
    /// on chain, so it has no `committed_height` and is released.
    ///
    /// Note the condition is **"observed on chain"**, not "has a promise
    /// record". A commitment whose receipt went out but which never landed must
    /// still be released promptly when its fifo entry goes away — otherwise a
    /// sender whose transaction timed out would be stuck behind its own nonce
    /// until the retention period expired.
    ///
    /// Returns whether the record was actually released.
    pub fn release_unless_committed(&self, hash: &TxHash) -> bool {
        let mut store = self.store.write();
        let retained = store
            .by_hash
            .get(hash)
            .and_then(|cached| cached.committed_height)
            .is_some_and(|height| !self.is_deep_enough(height));
        if retained {
            return false;
        }
        store.remove(hash);
        true
    }

    /// Drops records that are **absent from `live`** and whose commitment can
    /// no longer be affected by a reorg. Returns how many were dropped.
    ///
    /// Two criteria, and both are block *depth* rather than age — see
    /// [`SEAL_DEPTH`]:
    ///
    /// * a commitment **seen on chain** is held until [`SEAL_DEPTH`] persisted blocks sit on top of
    ///   its `committed_height`, because until then a reorg can hand it back its nonce;
    /// * a commitment **promised but never seen** is held until the same depth past its
    ///   `promised_height` — past that no reorg can still land it where it was promised. Restore's
    ///   "nonce taken" and "cannot tell" arms deliberately leave such a record without a fifo
    ///   entry, so dropping it any earlier would strand its journal line with nothing tracking it.
    ///
    /// `live` is [`PreconfTxSet::snapshot`](crate::PreconfTxSet::snapshot), the
    /// fifo's `order` deque, which is only ever mutated alongside `entries`
    /// under one lock — so it cannot miss a transaction that has an entry.
    ///
    /// Called once per canonical notification (≈ one block), nowhere near the
    /// admission hot path.
    pub fn sweep(&self, live: &HashSet<TxHash>) -> usize {
        let mut store = self.store.write();
        let before = store.by_hash.len();

        // Collect first, then release: `retain`'s closure cannot borrow
        // `by_slot` mutably while `by_hash` is being iterated. Both live under
        // the same lock, so the two steps are still one critical section — no
        // one can observe a dropped record whose slot is still claimed.
        let persisted = self.persisted_height.load(Ordering::Relaxed);
        let mut released: Vec<(TxHash, Commitment)> = Vec::new();
        store.by_hash.retain(|hash, cached| {
            // Held by block depth, not by the time-based grace above — see the
            // third criterion in this method's docs.
            // Whichever height still has reorg reach over this commitment: the
            // block it landed in, or the one it was promised for.
            let reachable = cached.committed_height.unwrap_or(cached.promised_height);
            let keep = reachable.saturating_add(SEAL_DEPTH) > persisted || live.contains(hash);
            if !keep {
                released.push((*hash, *cached));
            }
            keep
        });
        for (hash, cached) in &released {
            store.release_slot_of(hash, cached);
        }

        let len = store.by_hash.len();
        let slots = store.by_slot.len();
        drop(store);

        metrics::gauge!("preconf.classifier.records").set(len as f64);
        // Published because a leaked slot is harder to diagnose from outside than
        // a leaked record — it looks like "that account's transaction is
        // mysteriously rejected".
        //
        // Read it as a **lower bound** on in-flight preconf transactions, not as
        // a count of them: a restored commitment admitted under the promised
        // exemption can hold a fifo entry without owning a slot.
        metrics::gauge!("preconf.classifier.slots").set(slots as f64);
        self.observe_len(len);
        before - len
    }

    #[cfg(test)]
    fn by_hash_len(&self) -> usize {
        self.store.read().by_hash.len()
    }

    #[cfg(test)]
    fn commitment_count(&self) -> usize {
        self.store.read().by_hash.len()
    }

    /// Number of claimed `(sender, nonce)` slots — for assertions. Always
    /// `<= record_count()`, since only commitment records claim a slot.
    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.store.read().by_slot.len()
    }

    /// Tracks whether the cache is over [`Self::capacity`], warning on the upward
    /// crossing. Never deletes.
    ///
    /// Deleting under pressure would break commitments, and would not even be the
    /// useful thing to do: a record is a hash, an enum, an `Instant` and an
    /// optional slot key, while a fifo entry holds a whole `Arc<TxEnvelope>` and
    /// the fifo has no bound of its own — its removal is entirely canon-driven, so
    /// a stalled chain grows it without limit. If memory is the problem, the fifo
    /// is the problem; crossing this threshold is a symptom to alert on, not a
    /// condition to enforce.
    fn observe_len(&self, len: usize) {
        let over = len > self.capacity;
        if over == self.over_capacity.swap(over, Ordering::Relaxed) {
            return;
        }
        if over {
            warn!(
                target: "mantle::preconf::classifier",
                len,
                capacity = self.capacity,
                "commitment cache above capacity — chain likely stalled (fifo eviction is canon-driven)"
            );
        }
        // The `preconf.classifier.*` series keep their names after the move:
        // they are a published interface, and renaming them would silently
        // break every dashboard and alert built on them.
        metrics::gauge!("preconf.classifier.over_capacity").set(f64::from(u8::from(over)));
    }
}

#[cfg(test)]
mod tests {
    // Tests mutate `PreconfConfig::default()` to exercise a single field;
    // struct-literal init would be noisy.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use crate::classifier::PreconfClassifier;
    use alloy_primitives::map::foldhash::HashSet;

    /// Both halves, because a commitment is only ever created through the door
    /// a client would use: the allowlist decides whether the request may be
    /// admitted, the registry records what came through. `Deref` keeps the
    /// registry's own methods reachable unqualified, which is what the tests
    /// below are actually about.
    struct Fx {
        claims: Commitments,
        wl: PreconfClassifier,
    }

    impl std::ops::Deref for Fx {
        type Target = Commitments;
        fn deref(&self) -> &Commitments {
            &self.claims
        }
    }

    impl Fx {
        /// Admits `hash` the way a **preconf RPC** submission does: claim the
        /// record at the RPC boundary, then claim the nonce.
        ///
        /// Two calls, because that is what production does and the order is
        /// the whole point. Nothing else classifies a transaction any more: a
        /// plain `eth_sendRawTransaction` leaves no record here at all, so a
        /// test that wants an eligible transaction has to say so through the
        /// same door a client would.
        fn admit_via_preconf_rpc(
            &self,
            hash: TxHash,
            from: &Address,
            nonce: u64,
        ) -> (bool, SlotClaim) {
            self.admit_via_preconf_rpc_to(hash, from, Some(&addr(2)), nonce)
        }

        /// [`Self::admit_via_preconf_rpc`] with an explicit recipient, for the
        /// tests that vary it.
        fn admit_via_preconf_rpc_to(
            &self,
            hash: TxHash,
            from: &Address,
            to: Option<&Address>,
            nonce: u64,
        ) -> (bool, SlotClaim) {
            if !self.wl.preview_eligibility(from, to) {
                return (false, Ok(()));
            }
            let claim = self.mark_promised(hash, from, nonce, 0);
            (self.is_tracked(&hash), claim)
        }

        /// [`Self::admit_via_preconf_rpc`] reduced to "did it leave a record".
        fn registered_via_preconf_rpc(&self, hash: TxHash, from: &Address) -> bool {
            self.admit_via_preconf_rpc(hash, from, u64::from(hash.0[0])).0
        }

        /// [`Self::registered_via_preconf_rpc`] with an explicit recipient.
        fn registered_via_preconf_rpc_to(
            &self,
            hash: TxHash,
            from: &Address,
            to: Option<&Address>,
        ) -> bool {
            self.admit_via_preconf_rpc_to(hash, from, to, u64::from(hash.0[0])).0
        }
    }

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn hashes(items: &[TxHash]) -> HashSet<TxHash> {
        let mut s = HashSet::default();
        s.extend(items.iter().copied());
        s
    }

    /// A set of exact `(from, to)` rules.
    fn pair_set(entries: &[(Address, Address)]) -> HashSet<(Address, Address)> {
        entries.iter().copied().collect()
    }

    /// A classifier that allows `addr(1)` → `addr(2)` and nothing else. One
    /// exact rule, no wildcards — so a test that wants "not eligible" only has
    /// to change one half of the pair.
    fn classifier() -> Fx {
        let wl = PreconfClassifier::new(false);
        wl.update_whitelist(
            pair_set(&[(addr(1), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );
        Fx { claims: Commitments::enabled(DEFAULT_COMMITMENT_CACHE_CAP), wl }
    }

    // ===== Retention: a commitment that has been observed on chain keeps its
    // ===== record and its nonce until the block is buried `SEAL_DEPTH` deep.

    /// Height used by the retention tests. Arbitrary, but non-zero so that
    /// "buried enough" is not accidentally true at watermark 0.
    const AT: u64 = 100;

    /// Walk a commitment to the state the retention period is about: admitted,
    /// receipt returned, observed in the canonical block at [`AT`].
    fn committed_commitment(c: &Fx) {
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));
        assert!(c.mark_committed(&hash(1), AT));
    }

    // ===== the allowlist rule: a three-way OR =====

    /// **The core of the scheme.** `forward()` removes the fifo entry as soon as
    /// the sender's nonce advances, and that fires `release_unless_committed` —
    /// which must *not* release a commitment whose block could still be reorged
    /// away, or a same-nonce replacement could take the nonce and earn a second
    /// receipt.
    #[test]
    fn a_committed_record_survives_the_fifo_forward() {
        let c = classifier();
        committed_commitment(&c);

        // Shallow: one block on top is nowhere near `SEAL_DEPTH`.
        c.observe_persisted(AT + 1);
        assert!(!c.release_unless_committed(&hash(1)), "must refuse to release");

        assert!(c.is_tracked(&hash(1)), "record kept");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and so is the nonce");
    }

    /// The other side of the same predicate: once the block is buried, the
    /// commitment is irrevocable and tracking it costs a pinned nonce for
    /// nothing.
    #[test]
    fn a_committed_record_is_released_once_it_is_deep_enough() {
        let c = classifier();
        committed_commitment(&c);

        c.observe_persisted(AT + SEAL_DEPTH);
        assert!(c.release_unless_committed(&hash(1)));

        assert!(!c.is_tracked(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), None);
    }

    /// Exactly one block short of the depth must still be held. Pins the
    /// boundary against an off-by-one in either direction (paired with the test
    /// above, which sits exactly on it).
    #[test]
    fn one_block_short_of_the_depth_is_still_held() {
        let c = classifier();
        committed_commitment(&c);

        c.observe_persisted(AT + SEAL_DEPTH - 1);
        assert!(!c.release_unless_committed(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
    }

    /// A commitment that was never observed on chain has no retention claim:
    /// its removal path is a timeout or a rejection, and holding its nonce would
    /// block the sender behind a transaction that is not coming.
    ///
    /// This is the case `forward()` also hits when a *different* transaction
    /// advanced the nonce — the ambiguity that makes `forward` unable to decide
    /// this itself.
    #[test]
    fn an_uncommitted_record_is_released_immediately() {
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));
        c.observe_persisted(AT + SEAL_DEPTH);

        assert!(c.release_unless_committed(&hash(1)), "promised but never seen on chain");
        assert_eq!(c.slot_owner(&addr(1), 7), None);
    }

    /// The filter that keeps `mark_committed` from pinning every nonce in a
    /// block. The canonical handler feeds it every hash in the block, and the
    /// overwhelming majority are ordinary user transactions.
    #[test]
    fn mark_committed_is_a_noop_for_a_hash_with_no_record() {
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);

        assert!(!c.mark_committed(&hash(9), AT), "an unknown hash is nothing");
        assert!(c.mark_committed(&hash(1), AT), "a commitment of ours is counted");

        // Only the latter earns a retention period.
        c.observe_persisted(AT + 1);
        assert!(!c.release_unless_committed(&hash(1)), "held by its depth");
    }

    /// Both orders end in the same state. `forward → release_unless_committed`
    /// and the canonical notification run on different tasks with nothing
    /// ordering them, so the scheme cannot depend on which lands first — which is
    /// why the promise record is established at the receipt, before either can
    /// happen.
    #[test]
    fn the_release_and_the_canonical_observation_commute() {
        // Order 1: canonical first, then the fifo removal.
        let c = classifier();
        committed_commitment(&c);
        c.observe_persisted(AT + 1);
        assert!(!c.release_unless_committed(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        // Order 2: the fifo removal arrives before the canonical notification.
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));
        c.observe_persisted(AT + 1);
        // Not yet observed on chain, so this one *does* release …
        assert!(c.release_unless_committed(&hash(1)));
        // … and the late notification must then not resurrect anything: the
        // record is gone, so there is nothing to mark.
        assert!(!c.mark_committed(&hash(1), AT), "no record ⇒ nothing to commit");
        assert_eq!(c.slot_count(), 0);
    }

    /// A reorg withdraws the observation but **keeps** the promise and the nonce
    /// — the commitment is live again and must still refuse a same-nonce
    /// replacement. The return value is the `reorg_drift` predicate.
    #[test]
    fn uncommit_stops_the_clock_reports_drift_and_keeps_the_slot() {
        let c = classifier();
        committed_commitment(&c);

        assert!(c.uncommit(&hash(1)), "we had observed it on chain — that is drift");
        assert!(!c.uncommit(&hash(1)), "idempotent: the second call reports nothing");

        assert!(c.is_tracked(&hash(1)), "still an owed commitment");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and it keeps its nonce");

        // With the observation withdrawn, depth no longer holds the record: it is
        // back to being an ordinary in-flight commitment, so the release
        // `forward` attempts is no longer refused.
        c.observe_persisted(AT + SEAL_DEPTH);
        assert!(c.release_unless_committed(&hash(1)), "no observation ⇒ nothing holds it");
    }

    /// A transaction the node never promised is not drift, however deep the
    /// reorg. Guards the metric against counting every reverted transaction.
    #[test]
    fn uncommit_reports_nothing_for_a_transaction_we_never_committed() {
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert!(!c.uncommit(&hash(1)));
        assert!(!c.uncommit(&hash(9)));
    }

    /// The sweep runs on the same cadence and would otherwise undo the scheme:
    /// a committed commitment has already lost its fifo entry, so depth is the
    /// only thing left holding it — see [`PreconfClassifier::sweep`].
    #[test]
    fn sweep_holds_a_committed_commitment_until_it_is_buried() {
        let c = classifier();
        committed_commitment(&c);
        c.observe_persisted(AT + 1);

        assert_eq!(c.sweep(&HashSet::default()), 0, "not in the fifo, not yet buried");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        c.observe_persisted(AT + SEAL_DEPTH);
        assert_eq!(c.sweep(&HashSet::default()), 1, "and released once buried");
        assert_eq!(c.slot_count(), 0);
    }

    /// A promise that has **not** landed is held by depth as well.
    ///
    /// Restore's "nonce taken" and "cannot tell" arms leave exactly this state —
    /// promised, no fifo entry, never observed on chain. Were age allowed to
    /// decide it, the record would vanish while the
    /// commitment was still owed, stranding its journal line with nothing
    /// tracking it. That divergence is what the journal used to need its own
    /// wall-clock rule to paper over.
    #[test]
    fn sweep_holds_an_unlanded_promise_until_its_block_is_out_of_reach() {
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, AT), Ok(()));

        c.observe_persisted(AT + SEAL_DEPTH - 1);
        assert_eq!(c.sweep(&HashSet::default()), 0, "not in the fifo, not yet buried");
        assert!(c.is_tracked(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        // One more block and no reorg can put the transaction in the block it was
        // promised for. A replay into a *later* block would hold a fifo entry and
        // be protected by `live` instead.
        c.observe_persisted(AT + SEAL_DEPTH);
        assert_eq!(c.sweep(&HashSet::default()), 1, "unreachable ⇒ released");
        assert!(!c.is_tracked(&hash(1)));
        assert_eq!(c.slot_count(), 0);
    }

    /// The same promise, still being replayed: a fifo entry makes it `live`, and
    /// `live` outranks the depth rule however far the chain has moved on.
    #[test]
    fn sweep_keeps_an_unreachable_promise_that_is_still_being_replayed() {
        let c = classifier();
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, AT), Ok(()));

        c.observe_persisted(AT + SEAL_DEPTH * 10);
        let live: HashSet<TxHash> = [hash(1)].into_iter().collect();
        assert_eq!(c.sweep(&live), 0, "a commitment being replayed is never swept");
        assert!(c.is_tracked(&hash(1)));
    }

    /// The watermark starts at 0 and only moves forward. A provider that reports
    /// a lower height (or never reports at all) must not shorten a retention
    /// period — pinning a nonce too long is recoverable, releasing it too early
    /// is not.
    #[test]
    fn the_persisted_watermark_never_moves_backwards() {
        let c = classifier();
        committed_commitment(&c);

        assert!(!c.release_unless_committed(&hash(1)), "nothing is deep enough at watermark 0");

        c.observe_persisted(AT + SEAL_DEPTH);
        c.observe_persisted(AT); // a stale or regressed reading
        assert!(c.release_unless_committed(&hash(1)), "the high-water mark stands");
    }

    /// **Preconf is a sequencer-only mechanism**, and a node that has not opted
    /// in must carry none of its state — see the `enabled` field.
    #[test]
    fn disabled_node_classifies_without_caching() {
        let cfg = PreconfConfig::default();
        assert!(!cfg.enabled, "the default config is what a non-sequencer node gets");
        let c =
            Fx { claims: Commitments::from_config(&cfg), wl: PreconfClassifier::from_config(&cfg) };

        for i in 1..=50u8 {
            assert!(
                !c.registered_via_preconf_rpc(hash(i), &addr(1)),
                "nothing is preconf-eligible on a node with preconf off",
            );
        }

        assert_eq!(c.commitment_count(), 0, "a disabled node must not retain any record");
        assert!(!c.is_tracked(&hash(1)), "and must report no record downstream");
    }

    /// Same rule for the non-authoritative preview the RPC layer uses: an
    /// allowlist seeded on a disabled node must not make anything eligible.
    #[test]
    fn disabled_node_previews_everything_as_ineligible() {
        let mut cfg = PreconfConfig::default();
        cfg.all_preconfs = true; // even the "everything is eligible" switch
        let c =
            Fx { claims: Commitments::from_config(&cfg), wl: PreconfClassifier::from_config(&cfg) };
        c.wl.update_whitelist(
            pair_set(&[(addr(1), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );

        assert!(!c.wl.preview_eligibility(&addr(1), Some(&addr(2))));
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn unknown_hash_has_no_record() {
        let c = classifier();
        assert!(!c.is_tracked(&hash(9)));
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn a_preconf_request_is_matched_against_the_allowlist() {
        let c = classifier();
        assert!(c.registered_via_preconf_rpc_to(hash(1), &addr(1), Some(&addr(2))));
        // Sender not allowlisted.
        assert!(!c.registered_via_preconf_rpc_to(hash(2), &addr(3), Some(&addr(2))));
        // Recipient not allowlisted.
        assert!(!c.registered_via_preconf_rpc_to(hash(3), &addr(1), Some(&addr(9))));
    }

    /// **Nothing classifies a transaction except the preconf RPC.** A
    /// transaction that reaches the node any other way — plain
    /// `eth_sendRawTransaction`, p2p, the pool's own reorg reinject — leaves
    /// no record here at all, and consumers read "no record" as not preconf.
    ///
    /// There used to be a second door: the pool's validator recorded every
    /// transaction it saw. With it gone there is nothing to record, which is
    /// why this asserts absence.
    #[test]
    fn a_transaction_that_never_asked_for_preconf_has_no_record() {
        let c = classifier();
        // `addr(1) -> addr(2)` is exactly the allowlisted pair, so this is not
        // about eligibility — it is about never having been asked.
        assert!(!c.is_tracked(&hash(1)));
        assert_eq!(c.commitment_count(), 0);
        assert_eq!(c.slot_count(), 0);
    }

    #[test]
    fn contract_creation_is_not_eligible_without_a_sender_wildcard() {
        let c = classifier();
        // The fixture allowlists the pair `addr(1) -> addr(2)` and no wildcards,
        // and a creation has no recipient for the pair to match against.
        assert!(!c.registered_via_preconf_rpc_to(hash(1), &addr(1), None));
    }

    #[test]
    fn all_preconfs_ignores_allowlists_including_contract_creation() {
        let c = Fx {
            claims: Commitments::enabled(DEFAULT_COMMITMENT_CACHE_CAP),
            wl: PreconfClassifier::new(true),
        };
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(7)));
        assert!(c.registered_via_preconf_rpc(hash(2), &addr(7)));
    }

    /// Case A: eligible at admission, then the sender is removed from the
    /// allowlist. The record must not flip, or the commitment breaks.
    #[test]
    fn record_is_frozen_when_allowlist_shrinks() {
        let c = classifier();
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(1)));

        c.wl.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        assert!(c.is_tracked(&hash(1)), "the commitment is untouched");
        assert!(
            !c.wl.preview_eligibility(&addr(1), Some(&addr(2))),
            "while the door now refuses it"
        );
    }

    /// Case B: refused at the door, then the sender is added to the allowlist.
    ///
    /// There is nothing frozen to protect here, and that is the point: a
    /// refused submission leaves no record at all, so the allowlist growing
    /// cannot flip anything. The transaction simply has to be sent again, and
    /// then it is eligible.
    ///
    /// Only the shrinking direction has something to freeze — see
    /// `record_is_frozen_when_allowlist_shrinks`.
    #[test]
    fn a_refusal_leaves_nothing_for_a_wider_allowlist_to_flip() {
        let c = classifier();
        assert!(!c.registered_via_preconf_rpc(hash(1), &addr(3)));
        assert!(!c.is_tracked(&hash(1)), "refused at the door, so nothing was recorded");

        c.wl.update_whitelist(
            pair_set(&[(addr(1), addr(2)), (addr(3), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );

        // The same bytes, sent again, are now eligible.
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(3)));
    }

    #[test]
    fn promised_survives_later_classification() {
        let c = classifier();
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        // Restore pushes the envelope through the validator with an allowlist
        // that no longer contains the sender.
        assert!(
            !c.wl.preview_eligibility(&addr(9), Some(&addr(2))),
            "the door refuses that sender"
        );
        assert!(c.is_tracked(&hash(1)), "the record survives it");
        assert!(c.is_tracked(&hash(1)), "and it is still a promise");
    }

    /// The journal-restore path in one call: record the promise **and** claim the
    /// nonce it was acknowledged for.
    ///
    /// The release at the end is the real assertion. A claim recorded in
    /// `by_slot` without its reverse link in `Commitment::slot` is
    /// unreleasable, so the nonce would stay blocked until the next sweep — a
    /// leak that no "is the slot claimed?" assertion would catch.
    #[test]
    fn mark_promised_claims_the_slot_and_records_the_reverse_link() {
        let c = classifier();
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
        assert!(c.is_tracked(&hash(1)));
        assert!(c.is_tracked(&hash(1)), "an unseen hash restores with a record of its own");

        c.release_unless_committed(&hash(1));
        assert_eq!(c.slot_owner(&addr(1), 7), None, "the claim must be releasable");
        assert_eq!(c.slot_count(), 0);
    }

    /// Deliberately **not** a seize. If the nonce is already owned, the incumbent
    /// is the one in flight and the restored commitment is the one that will lose
    /// it — taking the slot would make the guard refuse later replacements on
    /// behalf of a transaction that never gets applied.
    #[test]
    fn mark_promised_does_not_displace_an_existing_owner() {
        let c = classifier();
        // A live admission takes (addr(1), 7) first.
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(1), 7), (true, Ok(())));

        assert_eq!(
            c.mark_promised(hash(1), &addr(1), 7, 0),
            Err(hash(2)),
            "reports the incumbent rather than evicting it"
        );
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));
        assert!(c.is_tracked(&hash(1)), "the promise is still recorded — only the claim lost");
    }

    /// **`mark_promised` must not overwrite an existing record with
    /// `Promised`**, and this pins that. Overwriting would not change which
    /// transactions are exempt (that keys on the `promised` flag — see
    /// promised); it would only make a value the rest of the code
    /// treats as frozen start changing mid-life.
    ///
    /// The state this test constructs is reachable only from the RPC path, where
    /// an earlier receipt for the same hash wrote it: restore runs before
    /// anything in the process can answer a client, so its record is always a
    /// fresh insert.
    #[test]
    fn mark_promised_records_the_promise_without_rewriting_the_record() {
        let c = classifier();
        assert!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).0);

        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        assert!(c.is_tracked(&hash(1)), "record untouched");
        assert!(c.is_tracked(&hash(1)), "but the promise is recorded");
    }

    #[test]
    fn forget_drops_only_the_named_record() {
        let c = classifier();
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        c.registered_via_preconf_rpc(hash(2), &addr(1));

        c.release_unless_committed(&hash(1));

        assert!(!c.is_tracked(&hash(1)));
        assert!(c.is_tracked(&hash(2)));
    }

    #[test]
    fn sweep_drops_records_absent_from_live_and_out_of_reorg_reach() {
        let c = classifier();
        // Both allowlisted, so both leave a record to sweep. A refused
        // submission would leave none.
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        c.registered_via_preconf_rpc(hash(2), &addr(1));
        c.observe_persisted(SEAL_DEPTH + 1);

        assert_eq!(c.sweep(&HashSet::default()), 2);
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn sweep_keeps_a_promise_still_in_reorg_reach() {
        // The window between the record being frozen and the entry existing
        // entry: absent from `live`, but too young to drop.
        let c = classifier();
        c.registered_via_preconf_rpc(hash(1), &addr(1));

        assert_eq!(c.sweep(&HashSet::default()), 0);
        assert!(c.is_tracked(&hash(1)));
    }

    #[test]
    fn sweep_keeps_live_entries_regardless_of_age() {
        let c = classifier();
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        assert_eq!(c.mark_promised(hash(2), &addr(2), 0, 0), Ok(()));

        assert_eq!(c.sweep(&hashes(&[hash(1), hash(2)])), 0);
        assert!(c.is_tracked(&hash(1)));
        assert!(c.is_tracked(&hash(2)));
    }

    #[test]
    fn sweep_keeps_live_and_drops_the_rest() {
        let c = classifier();
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        c.registered_via_preconf_rpc(hash(2), &addr(1));
        c.registered_via_preconf_rpc(hash(3), &addr(1));
        c.observe_persisted(SEAL_DEPTH + 1);

        assert_eq!(c.sweep(&hashes(&[hash(2)])), 2);
        assert!(c.is_tracked(&hash(2)));
        assert_eq!(c.commitment_count(), 1);
    }

    #[test]
    fn over_capacity_flags_but_never_deletes() {
        let c = Fx { claims: Commitments::enabled(2), wl: PreconfClassifier::new(true) };
        for i in 1..=4 {
            c.registered_via_preconf_rpc(hash(i), &addr(1));
        }

        assert_eq!(c.commitment_count(), 4, "entries above capacity must be kept");
        assert!(c.claims.over_capacity.load(Ordering::Relaxed));

        // Falling back under the threshold clears the flag.
        c.release_unless_committed(&hash(1));
        c.release_unless_committed(&hash(2));
        assert_eq!(c.sweep(&hashes(&[hash(3), hash(4)])), 0);
        assert!(!c.claims.over_capacity.load(Ordering::Relaxed));
    }

    #[test]
    fn preview_eligibility_does_not_cache() {
        let c = classifier();

        assert!(c.wl.preview_eligibility(&addr(1), Some(&addr(2))));
        assert!(!c.wl.preview_eligibility(&addr(3), Some(&addr(2))));
        assert!(!c.wl.preview_eligibility(&addr(1), None));

        assert_eq!(c.commitment_count(), 0, "preview must not freeze anything");
    }

    /// `from_config`'s defaults: nothing recorded, and the shared cache bound.
    #[test]
    fn from_config_starts_empty() {
        let c = Commitments::from_config(&PreconfConfig::default());

        assert_eq!(c.capacity, DEFAULT_COMMITMENT_CACHE_CAP);
        assert_eq!(c.by_hash_len(), 0);
    }

    /// `--preconf.all` only ever reaches a config alongside `--preconf.enable`
    /// (`PreconfArgs::into_config` returns `None` otherwise), so a registry built
    /// from that pair records.
    #[test]
    fn from_config_carries_enabled() {
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.all_preconfs = true;
        let c =
            Fx { claims: Commitments::from_config(&cfg), wl: PreconfClassifier::from_config(&cfg) };

        assert!(c.registered_via_preconf_rpc(hash(1), &addr(200)));
    }

    // ========================= claim kinds =========================

    /// The apply records a claim; the receipt upgrades it. Both steps claim the
    /// same slot, and the second must not lose what the first established.
    #[test]
    fn the_receipt_upgrades_the_claim_the_apply_recorded() {
        let c = classifier();
        assert_eq!(c.record_announced(hash(1), &addr(1), 7, 100), Ok(()));
        assert!(c.is_tracked(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
        assert!(!c.was_promised(&hash(1)), "nothing has reached a client yet");

        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 100), Ok(()));
        assert!(c.was_promised(&hash(1)), "the receipt makes it a promise");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and the slot is unchanged");
    }

    /// A replay re-executes an already-acknowledged commitment, so the apply
    /// runs again on a record that is already `Promised`. Downgrading it would
    /// make a real breach stop counting as one.
    #[test]
    fn re_executing_a_promised_commitment_does_not_downgrade_it() {
        let c = classifier();
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 100), Ok(()));
        assert!(c.was_promised(&hash(1)));

        assert_eq!(c.record_announced(hash(1), &addr(1), 7, 101), Ok(()));

        assert!(c.was_promised(&hash(1)), "a re-apply must not unsend the receipt");
    }

    /// The slot rule is the registry's, not the caller's: an announced claim
    /// cannot take a nonce another claim already holds.
    #[test]
    fn an_announced_claim_cannot_take_a_held_nonce() {
        let c = classifier();
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 100), Ok(()));

        assert_eq!(
            c.record_announced(hash(2), &addr(1), 7, 100),
            Err(hash(1)),
            "the incumbent is reported, not displaced",
        );
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
    }

    /// A node with preconf off records nothing, by either door.
    #[test]
    fn a_disabled_registry_records_no_announcement() {
        let c = Commitments::disabled();
        assert_eq!(c.record_announced(hash(1), &addr(1), 7, 100), Ok(()));
        assert!(!c.is_tracked(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), None);
    }

    // ===================== (sender, nonce) slot index =====================
    //
    // The slot index exists because the queue cannot answer "is this nonce
    // spoken for?" on its own: a commitment whose receipt has gone out keeps its
    // nonce through the retention window with no entry left behind. Hence these
    // tests never touch a fifo.

    /// The whole point: a second hash on the same `(sender, nonce)` is told who
    /// holds the slot, **with no fifo involved**. This is the case the
    /// fifo-membership guard misses.
    #[test]
    fn second_tx_on_same_sender_nonce_is_refused_the_slot() {
        let c = classifier();

        let (v1, claim1) = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert!(v1);
        assert_eq!(claim1, Ok(()), "first eligible tx must own the slot");

        let (v2, claim2) = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);
        assert!(v2, "a newcomer is still recorded normally");
        assert_eq!(claim2, Err(hash(1)), "and the claim names the incumbent");
    }

    /// Re-validating the same hash must not look like a collision with itself —
    /// the pool re-runs validation on several paths (same-hash resubmit,
    /// reorg re-inject).
    #[test]
    fn reclaiming_the_slot_with_the_same_hash_is_idempotent() {
        let c = classifier();

        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).1, Ok(()));
        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).1, Ok(()));
        assert_eq!(c.slot_count(), 1);
    }

    /// **The slot is checked whatever the newcomer's own record is.**
    ///
    /// Reached by an allowlist update landing between two same-`(sender, nonce)`
    /// submissions: the first is recorded and owns the slot, the second is
    /// refused because the sender was removed in between. Gating
    /// the check on the newcomer's record would let it through, and the pool
    /// would then hold one transaction on each arm for the same nonce —
    /// whichever executes first silently kills the other.
    #[test]
    fn de_whitelisted_replacement_is_still_refused_the_slot() {
        let c = classifier();

        let (v1, claim1) = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert!(v1);
        assert_eq!(claim1, Ok(()));

        // Governance revokes the rule; the incumbent's record stays frozen.
        c.wl.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        assert_eq!(
            c.slot_conflict(hash(2), &addr(1), 7),
            Err(hash(1)),
            "but it must still be told the slot is taken — otherwise both arms end up \
             holding a transaction for the same nonce",
        );
        assert!(c.is_tracked(&hash(1)), "incumbent unaffected");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
    }

    /// The classifier reports a taken slot **truthfully even for `Promised`**;
    /// the decision to let a restored commitment through anyway belongs to the
    /// guard, not here.
    ///
    /// Keeping the report honest matters: if this returned `Ok(())` the index
    /// would silently disagree with reality, and the caller could no longer
    /// tell "nobody holds this nonce" from "a commitment is being restored over
    /// someone else's claim".
    #[test]
    fn promised_is_told_the_truth_about_a_taken_slot() {
        let c = classifier();

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        // A journal entry for a *different* hash on the same (sender, nonce):
        // its claim loses, so it ends up Promised with no slot of its own.
        assert_eq!(c.mark_promised(hash(2), &addr(1), 7, 0), Err(hash(1)));
        let (registered, claim) = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);

        assert!(registered, "the losing commitment keeps its record");
        assert!(c.is_tracked(&hash(2)), "and it is still a promise");
        assert_eq!(claim, Err(hash(1)), "the incumbent is reported, not silently overwritten");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and it keeps the slot");
    }

    /// A transaction the door refused must not occupy a slot: it would reject
    /// replacements the preconf arm has no stake in.
    ///
    /// It leaves no record either, which is a stronger position than the one
    /// this used to assert — back when the pool's validator recorded every
    /// transaction, there was one to keep out of the slot index.
    #[test]
    fn a_refused_tx_claims_no_slot_and_leaves_no_record() {
        let c = classifier();

        let (registered, claim) = c.admit_via_preconf_rpc(hash(1), &addr(9), 7);
        assert!(!registered, "the door refused it, so no record");
        assert_eq!(claim, Ok(()), "and it claims nothing");
        assert_eq!(c.commitment_count(), 0, "nor is anything recorded");
        assert_eq!(c.slot_count(), 0);

        // …so an eligible tx on the same (sender, nonce) is unobstructed.
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(9), 7).1, Ok(()));
    }

    /// Different nonces from one sender are independent slots.
    #[test]
    fn slots_are_keyed_by_nonce_not_just_sender() {
        let c = classifier();

        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).1, Ok(()));
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(1), 8).1, Ok(()));
        assert_eq!(c.slot_count(), 2);
    }

    /// `forget` is the fifo's removal callback and the validator's
    /// rejection path; it must release the slot or the nonce stays blocked
    /// until the next sweep.
    #[test]
    fn forget_releases_the_slot() {
        let c = classifier();

        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).1, Ok(()));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        c.release_unless_committed(&hash(1));
        assert_eq!(c.slot_owner(&addr(1), 7), None);
        assert_eq!(c.slot_count(), 0);
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(1), 7).1, Ok(()));
    }

    /// Sweeping a record must release its slot in the same critical section —
    /// a leaked slot blocks that nonce for as long as the process lives.
    #[test]
    fn sweep_releases_slots_of_dropped_records() {
        let c = classifier();

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.slot_count(), 1);
        c.observe_persisted(SEAL_DEPTH + 1);

        assert_eq!(c.sweep(&hashes(&[])), 1);
        assert_eq!(c.commitment_count(), 0);
        assert_eq!(c.slot_count(), 0, "slot released with the record");
    }

    /// The mirror of the above: a record the sweep keeps must keep its slot.
    #[test]
    fn sweep_keeps_slots_of_surviving_records() {
        let c = classifier();

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        // In `live` ⇒ unsweepable regardless of age.
        assert_eq!(c.sweep(&hashes(&[hash(1)])), 0);
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
    }

    /// Marking a promise on a hash that was already classified must carry that
    /// entry's existing slot claim over rather than orphaning it. Same hash
    /// throughout — nothing is being taken from anyone (for that, see
    /// `mark_promised_does_not_displace_an_existing_owner`).
    ///
    /// This is the live-RPC shape: the transaction was classified on the way into
    /// the pool, and the receipt goes out a moment later.
    #[test]
    fn a_promise_on_a_classified_hash_keeps_its_slot() {
        let c = classifier();

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(
            c.mark_promised(hash(1), &addr(1), 7, 0),
            Ok(()),
            "same hash re-claiming is idempotent"
        );

        assert!(c.is_tracked(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "claim preserved");
        c.release_unless_committed(&hash(1));
        assert_eq!(c.slot_count(), 0, "and is still releasable");
    }

    /// A disabled node must carry no slot state either — same reasoning as
    /// `disabled_node_classifies_without_caching`.
    #[test]
    fn disabled_node_claims_no_slots() {
        let disabled = Fx { claims: Commitments::disabled(), wl: PreconfClassifier::new(false) };

        for i in 0..20u8 {
            assert_eq!(disabled.admit_via_preconf_rpc(hash(i), &addr(1), 7).1, Ok(()));
        }
        assert_eq!(disabled.commitment_count(), 0);
        assert_eq!(disabled.slot_count(), 0);
    }
}
