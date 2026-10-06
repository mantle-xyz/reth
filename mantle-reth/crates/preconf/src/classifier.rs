//! The preconf allowlists, and the record of every commitment this node owes.
//!
//! ## Why eligibility is decided only once
//!
//! Once the allowlists became on-chain governed and refreshable at runtime (see
//! [`crate::whitelist`]), "is this transaction preconf-eligible?" became a
//! function of *when you ask*. Asking it twice, in two places, is how a
//! transaction ends up applied by both build arms or by neither.
//!
//! So it is asked once, by [`PreconfClassifier::preview_eligibility`] at
//! admission, and what that answer produces — a fifo entry — is the only thing
//! either arm consults afterwards. Both skip a hash iff the fifo holds it
//! (`builder::payload_builder::apply_one_best_tx` for the pool arm), so a later
//! allowlist update cannot move a transaction between them.
//!
//! ## What the records carry
//!
//! A record exists for a hash whose event has gone out to a client, and
//! outlives that transaction's fifo entry: the `(sender, nonce)` claim that
//! refuses a replacement, the retention state behind [`SEAL_DEPTH`], and the
//! journal's eviction question ([`PreconfClassifier::is_tracked`]).
//!
//! ## Why the allowlists live here and not on `PreconfConfig`
//!
//! The lists are private to [`PreconfClassifier`], and there is deliberately no
//! public `is_preconf_tx` — no way to hand in a transaction and get back an
//! answer derived from whatever the lists happen to say at that instant.
//!
//! That is **not** the same as "eligibility cannot be re-derived anywhere else".
//! It can: [`PreconfClassifier::whitelist_snapshot`] hands out an
//! `Arc<Whitelist>` and [`Whitelist::is_eligible`] evaluates the predicate
//! against it, which is exactly what the payload builder does once per block to
//! judge commitments against the allowlist in force at build time.
//!
//! What the shape buys is that re-deriving forces the caller to **name which
//! allowlist it means**. A snapshot answers "who would be eligible under these
//! lists"; it cannot answer the question this module owns — "what was this
//! *already-admitted* transaction classified as" — because that answer is not a
//! function of any list. It lives in the commitment cache, and every consumer that
//! needs the partition to hold reads it from there.
//!
//! ## Locking
//!
//! The commitment store is read from the builder's apply hook, a sync `fn` that
//! never receives the fifo, so it has to be **synchronously readable** — hence
//! `parking_lot` here, where every `PreconfTxSet` lookup is `async` behind a
//! `tokio::sync::Mutex`.
//!
//! Two independent locks, deliberately: the allowlists are read-often /
//! written-almost-never, while the commitment cache takes one write per admitted
//! transaction. They are never held at the same time: no method reads the
//! allowlists and the records together, so no lock order exists to get wrong. As everywhere else in
//! this crate, a guard is never held across an `.await`; every accessor here returns an owned value
//! and drops its guard before returning, so callers cannot accidentally hold one.

use alloy_primitives::{
    Address, TxHash,
    map::{
        Entry,
        foldhash::{HashMap, HashSet},
    },
};
use parking_lot::RwLock;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tracing::warn;

use crate::config::PreconfConfig;

/// The preconf allowlists, mirrored from the on-chain `PreconfWhitelist`
/// contract (see [`crate::whitelist`]).
///
/// Lives here rather than on [`PreconfConfig`] because eligibility is decided
/// exactly once, by [`PreconfClassifier`] — see the module docs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Whitelist {
    /// Exact `(from, to)` rules.
    pub pairs: HashSet<(Address, Address)>,
    /// Senders whose every transaction is eligible, whatever the recipient —
    /// including a contract creation, which has no recipient at all.
    pub from_wildcards: HashSet<Address>,
    /// Recipients that make any transaction to them eligible, whatever the
    /// sender.
    pub to_wildcards: HashSet<Address>,
}

impl Whitelist {
    /// The three-way OR that decides eligibility, on **this** set of lists.
    ///
    /// A method rather than inline code in `PreconfClassifier::evaluate_whitelist`
    /// because the payload builder evaluates the same question against a
    /// different `Whitelist` — the build-scoped snapshot that carries a
    /// governance update landing in the block being built. Two copies of the
    /// predicate would be two places for the wildcard rules to drift apart.
    ///
    /// Says nothing about `enabled` / `all_preconfs`; those are classifier
    /// state, and the caller applies them.
    pub fn is_eligible(&self, from: &Address, to: Option<&Address>) -> bool {
        match to {
            None => self.from_wildcards.contains(from),
            Some(to) => {
                self.pairs.contains(&(*from, *to)) ||
                    self.from_wildcards.contains(from) ||
                    self.to_wildcards.contains(to)
            }
        }
    }
}

/// Safety bound on the commitment cache — 100k entries.
///
/// Each entry costs its hash key plus a `Commitment`, and a commitment record
/// additionally owns a `by_slot` entry; the bound assumes every entry is
/// preconf. Tens of MB at the ceiling, which only a stuck sweep can reach.
///
/// Not a limit that is enforced by deleting: see
/// [`PreconfClassifier::sweep`] for why exceeding it only warns.
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
/// * it is a block *depth*, not a duration. Time-based grace (what [`PreconfClassifier::sweep`]
///   uses for un-committed records) says nothing about how deep a reorg can reach.
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

/// One commitment this node has made, plus the instant it was recorded — the
/// latter for the grace period in [`PreconfClassifier::sweep`].
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
    at: Instant,
    /// The `(sender, nonce)` slot this record claimed, if it claimed one.
    slot: Option<(Address, u64)>,
    /// A `Success` receipt for this hash has been returned to a client.
    ///
    /// Set by [`PreconfClassifier::mark_promised`], from the two places that
    /// write the journal: the RPC handler at receipt time, and journal restore
    /// (where the receipt went out in a previous process). It is therefore the
    /// in-memory half of "is there a journal record for this hash", and the
    /// filter [`PreconfClassifier::mark_committed`] needs — a canonical block
    /// hands us bare hashes, and without this we could not tell our commitments
    /// from every other transaction in the block.
    promised: bool,
    /// Height of the canonical block this commitment was observed in, if it has
    /// been observed at all.
    ///
    /// `Some` is what earns the retention period: only a commitment that has
    /// actually been seen on chain is held past its fifo entry's removal, and it
    /// is released once [`SEAL_DEPTH`] persisted blocks sit on top. Cleared by
    /// [`PreconfClassifier::uncommit`] when a reorg takes that block back.
    committed_height: Option<u64>,
    /// Height the commitment was promised for — the bound on a promise that never lands, since a
    /// receipt already out must not be swept on age. Past `promised_height + SEAL_DEPTH` no reorg
    /// can still land it there; a replaying one holds a fifo entry and is covered by `live`.
    promised_height: Option<u64>,
}

impl Commitment {
    /// A freshly recorded commitment: nothing promised, nothing committed yet.
    fn new(slot: Option<(Address, u64)>) -> Self {
        Self {
            at: Instant::now(),
            slot,
            promised: false,
            committed_height: None,
            promised_height: None,
        }
    }
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
    /// Only [`PreconfClassifier::mark_promised`] reaches here: a slot exists for
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

/// Decides preconf eligibility once per transaction and remembers the answer.
///
/// Held as `Arc<PreconfClassifier>` and shared by admission (the only writer of
/// new records), the payload builder and the canon handler.
#[derive(Debug)]
pub struct PreconfClassifier {
    /// Mirrors `PreconfConfig::enabled`. When false this node runs no preconf
    /// machinery at all, so nothing is classified and **nothing is cached**.
    ///
    /// The leak this originally guarded against is gone — a node that has not
    /// opted in no longer builds the RPC handler, so nothing reaches the
    /// classifier to cache anything. Refusing here anyway keeps the invariant
    /// the classifier's own, rather than a consequence of how it is wired.
    enabled: bool,

    /// Mirrors `PreconfConfig::all_preconfs`: bypass the allowlists entirely.
    /// Copied rather than referenced because it is immutable after config
    /// validation.
    all_preconfs: bool,

    /// The allowlists, mirrored from the on-chain contract. **Private** — this
    /// is the point of the module. All three sets share one lock so a refresh
    /// swaps them together; with separate locks a reader could pair a new `from`
    /// against a stale `to`.
    ///
    /// Behind an `Arc` so [`Self::whitelist_snapshot`] can pin the lists without
    /// copying them — see there for why that has to be a refcount bump.
    whitelist: RwLock<Arc<Whitelist>>,

    /// The commitment records and the `(sender, nonce)` slot index.
    /// `parking_lot` ⇒ synchronous reads, usable from the builder's sync apply
    /// hook. Both indexes share this one lock — see [`CommitmentStore`].
    commitments: RwLock<CommitmentStore>,

    /// Minimum age before a record may be swept. Protects the window between
    /// the record being frozen and the entry existing.
    grace: Duration,

    /// Warning threshold for the commitment cache. Never enforced by deleting.
    capacity: usize,

    /// Whether we are currently above [`Self::capacity`]. Tracked so the
    /// warning fires on the transition instead of once per admitted
    /// transaction, and so it still fires when the chain has stalled and
    /// [`Self::sweep`] is no longer being called.
    over_capacity: AtomicBool,

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

impl PreconfClassifier {
    /// Builds an **enabled** classifier with explicit parameters.
    ///
    /// `grace` is injected rather than derived so tests can pin both sides of
    /// the sweep predicate without sleeping. The disabled shape is only
    /// reachable through [`Self::from_config`], which is also the only way
    /// production builds one.
    pub fn new(all_preconfs: bool, grace: Duration, capacity: usize) -> Self {
        Self {
            enabled: true,
            all_preconfs,
            whitelist: RwLock::new(Arc::new(Whitelist::default())),
            commitments: RwLock::new(CommitmentStore::default()),
            grace,
            capacity,
            over_capacity: AtomicBool::new(false),
            persisted_height: AtomicU64::new(0),
        }
    }

    /// Builds a classifier from validated config.
    ///
    /// The allowlists start **empty** — they are filled by `bootstrap_whitelist`
    /// before anything that can classify a transaction comes up.
    ///
    /// `grace` is `max(2 × slot_duration, preconf_timeout)`. It has to cover two
    /// different windows, and the larger of the two wins:
    ///
    /// * **`2 × slot_duration`** — the gap between the record being frozen and the entry existing.
    ///   Admission does both, so this is now sub-millisecond; the headroom is generous.
    /// * **`preconf_timeout`** — the whole period in which a client may still be waiting on its
    ///   responder. Sweeping inside it turns a transaction that was about to be preconfirmed into a
    ///   spurious `Timeout`.
    ///
    /// Taking the max makes that invariant hold **by construction**. Deriving it
    /// from `slot_duration` alone does not: both knobs are independently
    /// operator-settable (`--preconf.slot-duration-ms` / `--preconf.timeout-ms`)
    /// and `PreconfConfig::validate` relates neither to the other, so
    /// `preconf_timeout = 10s` with `slot_duration = 2s` — a config it accepts —
    /// would leave a 6s window in which a waiting client's record is sweepable.
    ///
    /// Note this bounds *spurious timeouts*, not broken commitments: an actual
    /// commitment implies a fifo entry, and [`Self::sweep`] provably never drops
    /// a record whose hash is in the fifo (see its docs).
    pub fn from_config(cfg: &PreconfConfig) -> Self {
        let grace = (cfg.slot_duration * 2).max(cfg.preconf_timeout);
        Self {
            enabled: cfg.enabled,
            ..Self::new(cfg.all_preconfs, grace, DEFAULT_COMMITMENT_CACHE_CAP)
        }
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
        match self.commitments.read().by_slot.get(&(*sender, nonce)) {
            // Free, or ours already (a resubmit) — neither is another
            // commitment's claim.
            None => Ok(()),
            Some(owner) if *owner == hash => Ok(()),
            Some(owner) => Err(*owner),
        }
    }

    /// **Where commitment tracking is established.** Records that a `Success`
    /// receipt for `hash` has gone out to a client, and claims the
    /// `(sender, nonce)` it was issued against.
    ///
    /// Exactly two callers, and they are the two places that write the journal:
    ///
    /// * the RPC handler, next to `append_promised`, the instant the receipt is returned;
    /// * journal restore's pre-pass, rebuilding the same state after a restart — the receipt there
    ///   went out in a *previous* process, so nothing else in this one would know.
    ///
    /// Calling it from both is what makes the classifier's promised set and the
    /// journal's contents agree by construction rather than by two independent
    /// judgements.
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
        let mut store = self.commitments.write();

        // Get-or-insert, then set `promised`. The insert case is journal restore
        // (this process has never seen the hash); the update case is the RPC
        // handler, where an earlier receipt for the same hash may already have
        // written it. Either way the record itself is left alone.
        let cached = store.by_hash.entry(hash).or_insert_with(|| Commitment::new(None));
        cached.promised = true;
        cached.promised_height = Some(promised_height);

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
        let mut store = self.commitments.write();
        match store.by_hash.get_mut(hash) {
            Some(cached) if cached.promised => {
                cached.committed_height = Some(height);
                true
            }
            _ => false,
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
        let mut store = self.commitments.write();
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
        self.commitments.read().by_hash.contains_key(hash)
    }

    /// Which transaction currently owns `(sender, nonce)`, if any.
    ///
    /// Only commitment records ever own a slot, so `None` means "no in-flight
    /// preconf claim on this nonce".
    pub fn slot_owner(&self, sender: &Address, nonce: u64) -> Option<TxHash> {
        self.commitments.read().by_slot.get(&(*sender, nonce)).copied()
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
        let mut store = self.commitments.write();
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

    /// Drops records that are **absent from `live`** and **older than the grace
    /// period**. Returns how many were dropped.
    ///
    /// ## What it can never drop
    ///
    /// `live` is [`PreconfTxSet::snapshot`](crate::PreconfTxSet::snapshot), i.e.
    /// the fifo's `order` deque, and `order` is only ever mutated alongside
    /// `entries` under a single lock — `push_if_absent` inserts into both,
    /// `drop_hash` removes from both. So `live` cannot miss a transaction that
    /// has a fifo entry, and a record backing an **actual commitment** is
    /// therefore unsweepable regardless of age: a commitment implies a fifo
    /// entry, and a fifo entry implies membership in `live`.
    ///
    /// The grace period covers the other direction — a record that does *not*
    /// yet have a fifo entry. See [`Self::from_config`] for why it is
    /// `max(2 × slot_duration, preconf_timeout)` rather than the slot term alone.
    ///
    /// A **committed** commitment has neither protection — `forward()` dropped
    /// its fifo entry as soon as its nonce advanced, and `grace` expires long
    /// before a reorg window closes — so it is held by a third criterion that
    /// overrides both: [`SEAL_DEPTH`] persisted blocks on top of its
    /// `committed_height`.
    ///
    /// One residual race, stated rather than closed: `live` is captured by the
    /// caller before this call, so a record older than `grace` whose entry is
    /// inserted between the snapshot and the `retain` below loses that record
    /// while holding a fifo entry.
    ///
    /// The gap it needs is between [`Self::mark_promised`], which writes the
    /// record, and the entry it belongs to — and since a record is only written
    /// for a commitment whose event has gone out, that entry is already there.
    /// It self-heals regardless: `mark_promised` get-or-inserts, so the next
    /// receipt writes the record back. Closing it properly would mean taking the fifo's
    /// async lock from this sync path, which is the dependency the whole
    /// callback/sweep split exists to avoid.
    ///
    /// ## What would otherwise leak
    ///
    /// Records outlive entries on purpose in two places, and neither reaches
    /// `drop_hash` again: a promise whose commitment never landed (the receipt
    /// is out, so `release_unless_committed` keeps the record when the entry
    /// goes), and the restore arms that deliberately leave a record with no
    /// entry at all. Depth holds both first — `promise_recoverable` below — and
    /// once that lapses this sweep is the only thing that can reclaim them.
    ///
    /// Called from the canonical-state handler, next to the fifo cleanup — once
    /// per canonical notification (≈ one block), nowhere near the admission hot
    /// path, and at that cadence the cache stays small enough to need no
    /// capacity-triggered eviction.
    pub fn sweep(&self, live: &HashSet<TxHash>) -> usize {
        let now = Instant::now();

        let mut store = self.commitments.write();
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
            let retained_for_reorg = cached
                .committed_height
                .is_some_and(|height| height.saturating_add(SEAL_DEPTH) > persisted);
            // A promise that has not landed is held by depth too. `grace` must
            // not decide it: the receipt is out, and restore's "nonce taken" and
            // "cannot tell" arms deliberately leave such a record without a fifo
            // entry, so age alone would drop a commitment we still owe and strand
            // its journal line with nothing tracking it.
            let promise_recoverable = cached.committed_height.is_none() &&
                cached
                    .promised_height
                    .is_some_and(|height| height.saturating_add(SEAL_DEPTH) > persisted);
            let keep = retained_for_reorg ||
                promise_recoverable ||
                live.contains(hash) ||
                now.saturating_duration_since(cached.at) < self.grace;
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

    /// Replaces the whole allowlist in one write. Called by the whitelist
    /// watcher.
    ///
    /// One write for all three sets, not three: they are three parts of a
    /// single policy and a reader must never see a mix of old and new. The
    /// watcher reads them from one state view for the same reason.
    ///
    /// Affects **only transactions admitted after this point** — every record
    /// already frozen stays as it is. That is the whole guarantee.
    pub fn update_whitelist(
        &self,
        pairs: HashSet<(Address, Address)>,
        from_wildcards: HashSet<Address>,
        to_wildcards: HashSet<Address>,
    ) {
        *self.whitelist.write() = Arc::new(Whitelist { pairs, from_wildcards, to_wildcards });
    }

    /// Current allowlist sizes as `(pairs, from_wildcards, to_wildcards)` — for
    /// logging and assertions.
    pub fn whitelist_counts(&self) -> (usize, usize, usize) {
        let wl = self.whitelist.read();
        (wl.pairs.len(), wl.from_wildcards.len(), wl.to_wildcards.len())
    }

    /// Pins the current allowlist so a reader can hold one fixed view of it.
    ///
    /// A refcount bump, not a copy — which is what makes it affordable for the
    /// payload builder to take one per block. [`Self::update_whitelist`] swaps
    /// the `Arc` wholesale, so a snapshot taken before a refresh keeps the lists
    /// it was taken with, and every transaction in one block is judged against
    /// the same policy even if governance lands mid-build.
    pub fn whitelist_snapshot(&self) -> Arc<Whitelist> {
        self.whitelist.read().clone()
    }

    /// Number of commitment records — for logging, metrics and assertions.
    #[cfg(test)]
    fn commitment_count(&self) -> usize {
        self.commitments.read().by_hash.len()
    }

    /// Number of claimed `(sender, nonce)` slots — for assertions. Always
    /// `<= record_count()`, since only commitment records claim a slot.
    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.commitments.read().by_slot.len()
    }

    /// **Non-authoritative** eligibility preview, for the RPC handler's early
    /// rejection only. Does not write the cache, so it cannot pre-empt the
    /// record the validator will freeze a moment later.
    pub fn preview_eligibility(&self, from: &Address, to: Option<&Address>) -> bool {
        self.evaluate_whitelist(from, to)
    }

    /// The allowlist rule itself. Private, and the only reader of
    /// [`Self::whitelist`].
    ///
    /// A plain three-way OR, with no precedence and no deny list:
    ///
    /// ```text
    /// eligible(from, to) <=> pairs.contains((from, to))
    ///                     || from_wildcards.contains(from)
    ///                     || to_wildcards.contains(to)
    /// ```
    ///
    /// One consequence is worth stating because governance will meet it:
    /// revoking an exact rule does **not** revoke traffic that a wildcard also
    /// covers. `(A, X)` can be removed from `pairs` and `A -> X` stays eligible
    /// while `A` is a from wildcard.
    ///
    /// # Contract creations
    ///
    /// A creation has no recipient — `TxKind::Create`, not `Call(0x0)` — so it
    /// reaches here as `None` and can match neither `pairs` nor `to_wildcards`,
    /// both of which need a `to`. A from wildcard is the only rule that can
    /// authorize it, which is exactly what "every transaction from this sender"
    /// says.
    ///
    /// This is the crate's one recorded **divergence from op-geth**, whose
    /// `IsPreconfTx` returns false whenever `to == nil`
    /// (`preconf/tx_pool_config.go`); it also still cross-products two
    /// independent lists rather than holding explicit rules. op-geth is the
    /// reference implementation, not consensus — preconf runs on a single
    /// sequencer — so divergence is allowed, but only deliberately.
    ///
    /// Note also that a transfer *to* `address(0)` is a normal transaction here,
    /// distinct from a creation. It simply can never match on the `to` side: the
    /// contract refuses to store the zero address, which it reserves as the
    /// calldata marker that routes a rule to a wildcard set.
    fn evaluate_whitelist(&self, from: &Address, to: Option<&Address>) -> bool {
        self.enabled && (self.all_preconfs || self.whitelist.read().is_eligible(from, to))
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
        metrics::gauge!("preconf.classifier.over_capacity").set(f64::from(u8::from(over)));
    }
}

#[cfg(test)]
mod tests {
    // Tests mutate `PreconfConfig::default()` to exercise a single field;
    // struct-literal init would be noisy.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    impl PreconfClassifier {
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
            if !self.preview_eligibility(from, to) {
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

    fn set(addrs: &[Address]) -> HashSet<Address> {
        let mut s = HashSet::default();
        s.extend(addrs.iter().copied());
        s
    }

    fn hashes(items: &[TxHash]) -> HashSet<TxHash> {
        let mut s = HashSet::default();
        s.extend(items.iter().copied());
        s
    }

    /// Grace long enough that nothing is ever sweepable during a test.
    const LONG_GRACE: Duration = Duration::from_secs(3600);

    /// A set of exact `(from, to)` rules.
    fn pair_set(entries: &[(Address, Address)]) -> HashSet<(Address, Address)> {
        entries.iter().copied().collect()
    }

    /// A classifier that allows `addr(1)` → `addr(2)` and nothing else. One
    /// exact rule, no wildcards — so a test that wants "not eligible" only has
    /// to change one half of the pair.
    fn classifier(grace: Duration) -> PreconfClassifier {
        let c = PreconfClassifier::new(false, grace, DEFAULT_COMMITMENT_CACHE_CAP);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), HashSet::default(), HashSet::default());
        c
    }

    // ===== Retention: a commitment that has been observed on chain keeps its
    // ===== record and its nonce until the block is buried `SEAL_DEPTH` deep.

    /// Height used by the retention tests. Arbitrary, but non-zero so that
    /// "buried enough" is not accidentally true at watermark 0.
    const AT: u64 = 100;

    /// Walk a commitment to the state the retention period is about: admitted,
    /// receipt returned, observed in the canonical block at [`AT`].
    fn committed_commitment(c: &PreconfClassifier) {
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));
        assert!(c.mark_committed(&hash(1), AT));
    }

    /// **The core of the scheme.** `forward()` removes the fifo entry as soon as
    /// the sender's nonce advances, and that fires `release_unless_committed` —
    /// which must *not* release a commitment whose block could still be reorged
    /// away, or a same-nonce replacement could take the nonce and earn a second
    /// receipt.
    #[test]
    fn a_committed_record_survives_the_fifo_forward() {
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
        committed_commitment(&c);
        c.observe_persisted(AT + 1);
        assert!(!c.release_unless_committed(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        // Order 2: the fifo removal arrives before the canonical notification.
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert!(!c.uncommit(&hash(1)));
        assert!(!c.uncommit(&hash(9)));
    }

    /// The sweep runs on the same cadence and would otherwise undo the scheme:
    /// both of its other criteria (fifo membership, `grace`) have already expired
    /// for a committed commitment — see [`PreconfClassifier::sweep`].
    #[test]
    fn sweep_holds_a_committed_commitment_past_its_grace() {
        let c = classifier(Duration::ZERO);
        committed_commitment(&c);
        c.observe_persisted(AT + 1);

        assert_eq!(c.sweep(&HashSet::default()), 0, "not in the fifo, past grace, still held");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        c.observe_persisted(AT + SEAL_DEPTH);
        assert_eq!(c.sweep(&HashSet::default()), 1, "and released once buried");
        assert_eq!(c.slot_count(), 0);
    }

    /// A promise that has **not** landed is held by depth too, not by `grace`.
    ///
    /// Restore's "nonce taken" and "cannot tell" arms leave exactly this state —
    /// promised, no fifo entry, never observed on chain — and `grace` is seconds.
    /// Were age allowed to decide it, the record would vanish while the
    /// commitment was still owed, stranding its journal line with nothing
    /// tracking it. That divergence is what the journal used to need its own
    /// wall-clock rule to paper over.
    #[test]
    fn sweep_holds_an_unlanded_promise_until_its_block_is_out_of_reach() {
        let c = classifier(Duration::ZERO);
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, AT), Ok(()));

        c.observe_persisted(AT + SEAL_DEPTH - 1);
        assert_eq!(c.sweep(&HashSet::default()), 0, "not in the fifo, past grace, still held");
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
        let c = classifier(Duration::ZERO);
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
        let c = classifier(LONG_GRACE);
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
        let c = PreconfClassifier::from_config(&cfg);

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
        let c = PreconfClassifier::from_config(&cfg);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), HashSet::default(), HashSet::default());

        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn unknown_hash_has_no_record() {
        let c = classifier(LONG_GRACE);
        assert!(!c.is_tracked(&hash(9)));
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn a_preconf_request_is_matched_against_the_allowlist() {
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
        // `addr(1) -> addr(2)` is exactly the allowlisted pair, so this is not
        // about eligibility — it is about never having been asked.
        assert!(!c.is_tracked(&hash(1)));
        assert_eq!(c.commitment_count(), 0);
        assert_eq!(c.slot_count(), 0);
    }

    #[test]
    fn contract_creation_is_not_eligible_without_a_sender_wildcard() {
        let c = classifier(LONG_GRACE);
        // The fixture allowlists the pair `addr(1) -> addr(2)` and no wildcards,
        // and a creation has no recipient for the pair to match against.
        assert!(!c.registered_via_preconf_rpc_to(hash(1), &addr(1), None));
    }

    #[test]
    fn all_preconfs_ignores_allowlists_including_contract_creation() {
        let c = PreconfClassifier::new(true, LONG_GRACE, DEFAULT_COMMITMENT_CACHE_CAP);
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(7)));
        assert!(c.registered_via_preconf_rpc(hash(2), &addr(7)));
    }

    /// Case A: eligible at admission, then the sender is removed from the
    /// allowlist. The record must not flip, or the commitment breaks.
    #[test]
    fn record_is_frozen_when_allowlist_shrinks() {
        let c = classifier(LONG_GRACE);
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(1)));

        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        assert!(c.is_tracked(&hash(1)), "the commitment is untouched");
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))), "while the door now refuses it");
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
        let c = classifier(LONG_GRACE);
        assert!(!c.registered_via_preconf_rpc(hash(1), &addr(3)));
        assert!(!c.is_tracked(&hash(1)), "refused at the door, so nothing was recorded");

        c.update_whitelist(
            pair_set(&[(addr(1), addr(2)), (addr(3), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );

        // The same bytes, sent again, are now eligible.
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(3)));
    }

    #[test]
    fn promised_survives_later_classification() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        // Restore pushes the envelope through the validator with an allowlist
        // that no longer contains the sender.
        assert!(!c.preview_eligibility(&addr(9), Some(&addr(2))), "the door refuses that sender");
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
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
        let c = classifier(LONG_GRACE);
        assert!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).0);

        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        assert!(c.is_tracked(&hash(1)), "record untouched");
        assert!(c.is_tracked(&hash(1)), "but the promise is recorded");
    }

    #[test]
    fn forget_drops_only_the_named_record() {
        let c = classifier(LONG_GRACE);
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        c.registered_via_preconf_rpc(hash(2), &addr(1));

        c.release_unless_committed(&hash(1));

        assert!(!c.is_tracked(&hash(1)));
        assert!(c.is_tracked(&hash(2)));
    }

    #[test]
    fn sweep_drops_entries_absent_from_live_and_past_grace() {
        let c = classifier(Duration::ZERO);
        // Both allowlisted, so both leave a record to sweep. A refused
        // submission would leave none.
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        c.registered_via_preconf_rpc(hash(2), &addr(1));
        // Past the depth that holds an unlanded promise, so `grace` decides.
        c.observe_persisted(SEAL_DEPTH + 1);

        assert_eq!(c.sweep(&HashSet::default()), 2);
        assert_eq!(c.commitment_count(), 0);
    }

    #[test]
    fn sweep_keeps_entries_within_grace() {
        // The window between the record being frozen and the entry existing
        // entry: absent from `live`, but too young to drop.
        let c = classifier(LONG_GRACE);
        c.registered_via_preconf_rpc(hash(1), &addr(1));

        assert_eq!(c.sweep(&HashSet::default()), 0);
        assert!(c.is_tracked(&hash(1)));
    }

    #[test]
    fn sweep_keeps_live_entries_regardless_of_age() {
        let c = classifier(Duration::ZERO);
        c.registered_via_preconf_rpc(hash(1), &addr(1));
        assert_eq!(c.mark_promised(hash(2), &addr(2), 0, 0), Ok(()));

        assert_eq!(c.sweep(&hashes(&[hash(1), hash(2)])), 0);
        assert!(c.is_tracked(&hash(1)));
        assert!(c.is_tracked(&hash(2)));
    }

    #[test]
    fn sweep_keeps_live_and_drops_the_rest() {
        let c = classifier(Duration::ZERO);
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
        let c = PreconfClassifier::new(true, LONG_GRACE, 2);
        for i in 1..=4 {
            c.registered_via_preconf_rpc(hash(i), &addr(1));
        }

        assert_eq!(c.commitment_count(), 4, "entries above capacity must be kept");
        assert!(c.over_capacity.load(Ordering::Relaxed));

        // Falling back under the threshold clears the flag.
        c.release_unless_committed(&hash(1));
        c.release_unless_committed(&hash(2));
        assert_eq!(c.sweep(&hashes(&[hash(3), hash(4)])), 0);
        assert!(!c.over_capacity.load(Ordering::Relaxed));
    }

    // ===== the allowlist rule: a three-way OR =====

    /// A classifier holding one of each rule form:
    /// `(1 -> 2)` exact, `3` a from wildcard, `4` a to wildcard.
    fn or_classifier() -> PreconfClassifier {
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_COMMITMENT_CACHE_CAP);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(3)]), set(&[addr(4)]));
        c
    }

    /// **The predicate, exhaustively.** Each of the three rules must be
    /// sufficient on its own, and their absence must be sufficient to refuse —
    /// the table covers every combination of the three sub-predicates, so
    /// turning the OR into an AND, or dropping any one arm, kills a row.
    #[test]
    fn eligibility_is_the_or_of_three_rules() {
        let c = or_classifier();
        // (from, to, pair?, from-wc?, to-wc?, expected)
        let cases = [
            (addr(1), addr(2), true, false, false, true),
            (addr(3), addr(9), false, true, false, true),
            (addr(9), addr(4), false, false, true, true),
            (addr(3), addr(4), false, true, true, true),
            (addr(1), addr(4), false, false, true, true),
            (addr(3), addr(2), false, true, false, true),
            (addr(9), addr(9), false, false, false, false),
            // The exact rule is directional: the reverse is none of the three.
            (addr(2), addr(1), false, false, false, false),
        ];
        for (from, to, pair, from_wc, to_wc, want) in cases {
            assert_eq!(
                c.preview_eligibility(&from, Some(&to)),
                want,
                "from={from:?} to={to:?} (pair={pair} from_wc={from_wc} to_wc={to_wc})",
            );
        }
    }

    /// The consequence governance will actually meet: revoking an exact rule
    /// does **not** revoke traffic a wildcard also covers. Stated in
    /// `evaluate_whitelist`'s docs, pinned here so it cannot quietly become a
    /// precedence rule.
    #[test]
    fn a_wildcard_still_covers_traffic_whose_exact_rule_was_revoked() {
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_COMMITMENT_CACHE_CAP);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(1)]), HashSet::default());
        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));

        // Governance drops the exact rule but leaves the sender wildcard.
        c.update_whitelist(HashSet::default(), set(&[addr(1)]), HashSet::default());
        assert!(
            c.preview_eligibility(&addr(1), Some(&addr(2))),
            "the wildcard still authorizes it — revoking needs both",
        );

        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    /// **Contract creations have no recipient**, so only a from wildcard can
    /// authorize them: `pairs` and `to_wildcards` both need a `to` to match
    /// against. A deliberate divergence from op-geth — see `evaluate_whitelist`.
    #[test]
    fn a_contract_creation_is_eligible_only_through_a_from_wildcard() {
        let c = or_classifier();

        assert!(c.preview_eligibility(&addr(3), None), "from wildcard covers a creation");
        assert!(
            !c.preview_eligibility(&addr(1), None),
            "an exact rule cannot: a creation has no `to` to match its other half",
        );
        assert!(
            !c.preview_eligibility(&addr(9), None),
            "and a to wildcard cannot cover a transaction with no recipient at all",
        );
    }

    /// A transfer **to** the zero address is an ordinary transaction, distinct
    /// from a contract creation, and is judged by the ordinary `Some(to)` arm.
    /// Pinned because flattening `TxKind::Create` into `Some(Address::ZERO)`
    /// anywhere upstream would collapse two cases the rule treats differently:
    /// this one can be authorized by a to wildcard or an exact pair, a creation
    /// cannot.
    ///
    /// That the zero address can never be on the `to` side of a *rule* is a
    /// separate guarantee, owned and tested one layer up — see
    /// `whitelist::report_zero_entries`. Asserting it here would mean
    /// hand-building an allowlist the production path cannot produce.
    #[test]
    fn a_transfer_to_the_zero_address_is_judged_like_any_other() {
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_COMMITMENT_CACHE_CAP);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), set(&[addr(3)]), HashSet::default());

        assert!(
            c.preview_eligibility(&addr(3), Some(&Address::ZERO)),
            "the sender's wildcard covers it, exactly as it would any other recipient",
        );
        assert!(
            !c.preview_eligibility(&addr(1), Some(&Address::ZERO)),
            "and an exact rule for a different recipient does not",
        );
    }

    #[test]
    fn preview_eligibility_does_not_cache() {
        let c = classifier(LONG_GRACE);

        assert!(c.preview_eligibility(&addr(1), Some(&addr(2))));
        assert!(!c.preview_eligibility(&addr(3), Some(&addr(2))));
        assert!(!c.preview_eligibility(&addr(1), None));

        assert_eq!(c.commitment_count(), 0, "preview must not freeze anything");
    }

    #[test]
    fn update_whitelist_replaces_wholesale_and_accessors_follow() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.whitelist_counts(), (1, 0, 0));

        c.update_whitelist(
            pair_set(&[(addr(3), addr(4))]),
            set(&[addr(5)]),
            set(&[addr(6), addr(7)]),
        );

        assert_eq!(c.whitelist_counts(), (1, 1, 2));
        let snapshot = c.whitelist_snapshot();
        assert!(snapshot.pairs.contains(&(addr(3), addr(4))));
        assert!(snapshot.from_wildcards.contains(&addr(5)));
        assert!(snapshot.to_wildcards.contains(&addr(7)));
        // Replacement, not a union: the previous generation is gone.
        assert!(!snapshot.pairs.contains(&(addr(1), addr(2))));
        assert!(!c.preview_eligibility(&addr(1), Some(&addr(2))));
    }

    /// `from_config`'s defaults, and the **slot-derived side** of `grace`'s
    /// `max`. Paired with [`grace_never_undercuts_the_client_deadline`], which
    /// covers the deadline side — neither test alone pins the rule, and this one
    /// on its own passes whether or not the `max` is there.
    #[test]
    fn from_config_defaults_take_the_slot_derived_grace() {
        let cfg = PreconfConfig::default();
        let c = PreconfClassifier::from_config(&cfg);

        assert!(
            cfg.slot_duration * 2 > cfg.preconf_timeout,
            "precondition: the default config is the side where the slot term wins",
        );
        assert_eq!(c.grace, cfg.slot_duration * 2);
        assert_eq!(c.capacity, DEFAULT_COMMITMENT_CACHE_CAP);
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
        assert_eq!(c.commitment_count(), 0);
        assert!(!c.all_preconfs);
    }

    /// The **deadline side** of `grace`'s `max`: it must never be shorter than
    /// the client deadline. Paired with
    /// [`from_config_defaults_take_the_slot_derived_grace`]; see
    /// [`PreconfClassifier::from_config`] for what the shortfall would cost.
    ///
    /// The pairing below is one `PreconfConfig::validate` accepts without
    /// relating the two knobs, which is why the invariant cannot be delegated to
    /// it.
    #[test]
    fn grace_never_undercuts_the_client_deadline() {
        let mut cfg = PreconfConfig::default();
        cfg.slot_duration = Duration::from_secs(2);
        cfg.preconf_timeout = Duration::from_secs(10);
        // Precondition of the test: validate() genuinely accepts this pairing,
        // so the invariant cannot be delegated to config validation.
        cfg.enabled = true;
        cfg.whitelist_contract = Some(addr(7));
        assert!(cfg.clone().validate().is_ok(), "validate() does not relate these two knobs");

        let c = PreconfClassifier::from_config(&cfg);
        assert_eq!(c.grace, Duration::from_secs(10), "the deadline term must win here");
        assert!(c.grace >= cfg.preconf_timeout);
    }

    #[test]
    fn from_config_carries_all_preconfs() {
        // `--preconf.all` only ever reaches a config alongside `--preconf.enable`
        // (`PreconfArgs::into_config` returns `None` otherwise), and a disabled
        // node classifies nothing regardless — so both flags belong here.
        let mut cfg = PreconfConfig::default();
        cfg.enabled = true;
        cfg.all_preconfs = true;
        let c = PreconfClassifier::from_config(&cfg);

        assert!(c.enabled);
        assert!(c.all_preconfs);
        assert!(c.registered_via_preconf_rpc(hash(1), &addr(200)));
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
        let c = classifier(LONG_GRACE);

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
        let c = classifier(LONG_GRACE);

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
        let c = classifier(LONG_GRACE);

        let (v1, claim1) = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert!(v1);
        assert_eq!(claim1, Ok(()));

        // Governance revokes the rule; the incumbent's record stays frozen.
        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

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
        let c = classifier(LONG_GRACE);

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
        let c = classifier(LONG_GRACE);

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
        let c = classifier(LONG_GRACE);

        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).1, Ok(()));
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(1), 8).1, Ok(()));
        assert_eq!(c.slot_count(), 2);
    }

    /// `forget` is the fifo's removal callback and the validator's
    /// rejection path; it must release the slot or the nonce stays blocked
    /// until the next sweep.
    #[test]
    fn forget_releases_the_slot() {
        let c = classifier(LONG_GRACE);

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
        let c = classifier(Duration::ZERO);

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
        let c = classifier(Duration::ZERO);

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
        let c = classifier(LONG_GRACE);

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
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_COMMITMENT_CACHE_CAP);
        let disabled = PreconfClassifier { enabled: false, ..c };

        for i in 0..20u8 {
            assert_eq!(disabled.admit_via_preconf_rpc(hash(i), &addr(1), 7).1, Ok(()));
        }
        assert_eq!(disabled.commitment_count(), 0);
        assert_eq!(disabled.slot_count(), 0);
    }
}
