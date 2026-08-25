//! Freezes each transaction's preconf eligibility at pool-admission time.
//!
//! ## Why this exists
//!
//! Once the allowlists became on-chain governed and refreshable at runtime (see
//! [`crate::whitelist`]), "is this transaction preconf-eligible?" stopped being a
//! pure function of the transaction and became a function of *when you ask*. That
//! is fatal, because the builder treats the answer as a **partition**: a
//! preconf-eligible transaction may only be applied by the preconf arm, and the
//! pool arm must skip it (`builder::payload_builder` Stage 3). The pool arm is
//! only safe to skip because the preconf arm is expected to pick the transaction
//! up — which requires a fifo entry, created by the pool listener using the
//! allowlists *as they were at admission*.
//!
//! If the listener and the builder read different allowlists, the partition
//! breaks in both directions:
//!
//! * **eligible → not eligible**: a fifo entry exists and the client already holds a
//!   preconfirmation receipt, but the pool arm no longer skips the transaction. It lands via the
//!   normal path, nobody applies the fifo entry, the responder never fires — the client times out
//!   on a transaction that is already on chain, and the commitment we handed out is broken.
//! * **not eligible → eligible**: no fifo entry was ever created, yet the pool arm now skips the
//!   transaction. Neither arm applies it, so it is silently excluded from block building until the
//!   allowlist flips back or it is evicted. A silent liveness failure, hitting a transaction that
//!   was never promised anything.
//!
//! So the guarantee we need is stronger than "commitments are irrevocable":
//!
//! > **Every component must agree on a transaction's classification for that
//! > transaction's whole lifetime.**
//!
//! This module provides it by deciding once, at admission, and caching the
//! result. Every downstream consumer reads the cache instead of re-deriving.
//!
//! ## Why the allowlists live here and not on `PreconfConfig`
//!
//! The allowlists are private to [`PreconfClassifier`] on purpose: with no
//! public `is_preconf_tx`, re-deriving eligibility somewhere new goes from
//! "something you should not do" to "something you cannot do".
//!
//! ## Locking
//!
//! The verdict store is read from the builder's apply hook, a sync `fn` that
//! never receives the fifo, so it has to be **synchronously readable** — hence
//! `parking_lot` here, where every `PreconfTxSet` lookup is `async` behind a
//! `tokio::sync::Mutex`.
//!
//! Two independent locks, deliberately: the allowlists are read-often /
//! written-almost-never, while the verdict cache takes one write per admitted
//! transaction. They are never held at the same time — [`PreconfClassifier::claim_preconf`],
//! the only caller that needs both, finishes with the allowlist before taking the
//! verdict lock — so no lock order exists to get wrong. As everywhere else in this crate, a guard
//! is never held across an `.await`; [`PreconfClassifier::verdict`] hands back a `Copy`
//! value, so callers cannot accidentally do so.

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

/// Safety bound on the verdict cache — 100k entries.
///
/// Each entry costs its hash key plus a `CachedVerdict`, and a preconf verdict
/// additionally owns a `by_slot` entry; the bound assumes every entry is
/// preconf. Tens of MB at the ceiling, which only a stuck sweep can reach.
///
/// Not a limit that is enforced by deleting: see
/// [`PreconfClassifier::sweep`] for why exceeding it only warns.
pub const DEFAULT_VERDICT_CACHE_CAP: usize = 100_000;

/// How many **persisted** blocks must be stacked on top of a commitment's block
/// before its tracking (frozen verdict + `(sender, nonce)` slot) may be released
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
///   uses for un-committed verdicts) says nothing about how deep a reorg can reach.
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

/// A transaction's preconf eligibility. Decided once at admission, never
/// changed afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Matched the allowlists at admission time.
    Eligible,
    /// Did not match the allowlists at admission time.
    NotEligible,
    /// The verdict a commitment restored from the journal gets — restore is the
    /// one path that creates a record without ever seeing an admission.
    ///
    /// **The variant itself grants nothing.** The "a receipt went out" marker,
    /// and the thing every exemption keys on (the validator's admission-time
    /// policy gates, [`PreconfClassifier::release_preconf_claim`]), is the
    /// `promised` flag on `CachedVerdict` — which
    /// [`PreconfClassifier::mark_promised`] sets *without* rewriting the
    /// verdict. So the ordinary RPC flow leaves `Eligible` + promised, and only
    /// restore ever writes `Promised`; nothing reads this variant as such. The
    /// two stay separate to keep `verdict` write-once, which every consumer
    /// relies on.
    ///
    /// The exemption is owed: the receipt went out before the restart, so the
    /// transaction must come back whatever the current policy says. Without it,
    /// lowering `--preconf.max-gas-per-tx` and restarting would silently drop an
    /// already-acknowledged commitment.
    Promised,
}

impl Verdict {
    /// Whether this transaction belongs to the preconf arm. This is *the*
    /// partition predicate: the pool arm skips exactly these.
    pub const fn is_preconf(self) -> bool {
        matches!(self, Self::Eligible | Self::Promised)
    }
}

/// A frozen verdict plus the instant it was frozen, for the grace period in
/// [`PreconfClassifier::sweep`].
///
/// `slot` is the reverse link into [`VerdictStore::by_slot`], so every removal
/// path can release the claim without scanning the index.
///
/// It is `Option` because the claim may not be *ours*, not because it happens
/// later: both writers insert the record and then call `VerdictStore::claim`
/// under the same lock, which back-fills the field. A non-preconf verdict never
/// claims a slot at all, and a claim that loses to an incumbent returns
/// `Err(owner)` while the record itself stays (see
/// `mark_promised_does_not_displace_an_existing_owner`).
///
/// The invariant is exact: **`slot` is `Some(key)` iff this hash may own
/// `by_slot[key]`.**
///
/// `promised` / `committed_height` carry commitment-tracking state as fields
/// rather than [`Verdict`] variants, because the three have different lifetimes:
/// `verdict` is written once and never rewritten, `promised` is set once when a
/// receipt goes out, and `committed_height` is the only reversible one —
/// `uncommit` clears it on a reorg while the promise stands. See
/// [`Verdict::Promised`] for why the flag and the variant are not the same thing.
#[derive(Debug, Clone, Copy)]
struct CachedVerdict {
    verdict: Verdict,
    at: Instant,
    /// The `(sender, nonce)` slot this verdict claimed, if it claimed one.
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

impl CachedVerdict {
    /// A freshly frozen verdict: nothing promised, nothing committed yet.
    fn new(verdict: Verdict, slot: Option<(Address, u64)>) -> Self {
        Self {
            verdict,
            at: Instant::now(),
            slot,
            promised: false,
            committed_height: None,
            promised_height: None,
        }
    }
}

/// The frozen verdicts, plus the `(sender, nonce)` → hash index that makes the
/// replacement guard race-free.
///
/// ## Why both indexes share one lock
///
/// They are two views of one fact ("this transaction was classified"), and every
/// mutation touches both. Splitting them would mean either a documented lock
/// order (this type currently has none to get wrong — see the module docs) or a
/// window in which a verdict exists without its slot claim. Under one lock,
/// freezing a verdict and claiming its slot happen in a single critical section,
/// which is precisely what closes the race described on
/// [`PreconfClassifier::admit_and_claim`].
#[derive(Debug, Default)]
struct VerdictStore {
    /// Frozen verdict per transaction hash.
    by_hash: HashMap<TxHash, CachedVerdict>,
    /// Which transaction currently owns each `(sender, nonce)` slot.
    ///
    /// Only populated for verdicts where [`Verdict::is_preconf`] holds: a
    /// non-preconf transaction makes no claim the preconf arm cares about, so
    /// letting it occupy a slot would reject replacements for no reason.
    by_slot: HashMap<(Address, u64), TxHash>,
}

impl VerdictStore {
    /// Removes `hash` and, if it owned its `(sender, nonce)` slot, releases it.
    ///
    /// Guarded by an equality check because the slot may already have been
    /// handed to a different transaction (the reclaimable-replacement path);
    /// dropping it unconditionally would evict the new owner's claim.
    fn remove(&mut self, hash: &TxHash) {
        if let Some(cached) = self.by_hash.remove(hash) {
            self.release_slot_of(hash, &cached);
        }
    }

    /// Claims `key` for `hash` when the slot is free, and records the reverse
    /// link so every removal path can release it again.
    ///
    /// The two callers — [`PreconfClassifier::admit_and_claim`] and
    /// [`PreconfClassifier::mark_promised`] — share this body deliberately: they
    /// reach it from different directions (a live admission versus a journal
    /// restore that has just decoded the sender and nonce), and two copies of
    /// the predicate below would be two places to narrow it by mistake.
    ///
    /// Checking and claiming are decoupled:
    ///
    /// * the answer is computed for **every** transaction, whatever its own verdict, because the
    ///   slot records that a preconf transaction is in flight for this `(sender, nonce)` and a
    ///   replacement is just as fatal when it arrives as an ordinary transaction;
    /// * only a **preconf** transaction claims. A non-preconf one has no arm to defend, so
    ///   occupying the slot would reject later replacements for nothing.
    fn claim(&mut self, key: (Address, u64), hash: TxHash, is_preconf: bool) -> SlotClaim {
        let claim = match self.by_slot.entry(key) {
            // Same hash re-entering (retry / re-validation): idempotent.
            Entry::Occupied(slot) if *slot.get() == hash => Ok(()),
            Entry::Occupied(slot) => Err(*slot.get()),
            Entry::Vacant(slot) => {
                if is_preconf {
                    slot.insert(hash);
                }
                Ok(())
            }
        };

        // Record the reverse link exactly when the claim is ours, so
        // `release_slot_of` can find it later. Both callers insert their record
        // with `slot: None` and rely on this back-fill; neither ever writes the
        // field itself.
        if claim.is_ok() &&
            is_preconf &&
            let Some(cached) = self.by_hash.get_mut(&hash)
        {
            cached.slot = Some(key);
        }

        claim
    }

    /// Compare-and-swap: hands `key` from `expected` to `hash`, but **only if
    /// `expected` still holds it**.
    ///
    /// This is the reclaimable-replacement handover. It has to be a CAS rather
    /// than a plain overwrite because the decision "the holder is in a
    /// reclaimable state, so I may take its nonce" is made *before* the inner
    /// validator runs, and that validator is `async`: two same-nonce
    /// transactions can both observe the same reclaimable holder and both come
    /// back to take the slot. An unconditional write makes the later one win the
    /// index silently, while the pool (price) and the fifo (event order) each
    /// pick a winner by their own rule — three layers, three possible answers,
    /// and the transaction the pool accepted can end up skipped by both build
    /// arms. The CAS makes exactly one of them succeed.
    ///
    /// A vacant slot also succeeds: nobody owns the nonce, so there is nothing
    /// to conflict with.
    ///
    /// `is_preconf` describes **us**: a non-preconf replacement still needs the
    /// CAS (to be sure it is the one tearing the holder down) but must not take
    /// the slot afterwards, so on success the mapping is simply released.
    fn replace(
        &mut self,
        expected: &TxHash,
        key: (Address, u64),
        hash: TxHash,
        is_preconf: bool,
    ) -> SlotClaim {
        match self.by_slot.get(&key) {
            // Already ours (re-validation of the same hash): idempotent.
            Some(current) if *current == hash => return Ok(()),
            Some(current) if current != expected => return Err(*current),
            _ => {}
        }

        if is_preconf {
            self.by_slot.insert(key, hash);
            if let Some(cached) = self.by_hash.get_mut(&hash) {
                cached.slot = Some(key);
            }
        } else {
            // Nothing of ours to hang the reverse link on — release instead of
            // occupying, for the same reason `claim` does not claim for a
            // non-preconf transaction.
            self.by_slot.remove(&key);
        }

        Ok(())
    }

    /// Releases `cached`'s slot iff it claimed one and `hash` is still the
    /// recorded owner.
    fn release_slot_of(&mut self, hash: &TxHash, cached: &CachedVerdict) {
        let Some(key) = cached.slot else { return };
        if self.by_slot.get(&key) == Some(hash) {
            self.by_slot.remove(&key);
        }
    }
}

/// Did [`PreconfClassifier::admit_and_claim`] **create** the record, or
/// find one already there?
///
/// It decides who may destroy it when the admission fails. `add_transaction`
/// validates *before* the hash-dedup that answers `AlreadyImported` — that check
/// lives in `TxPool::add_transaction` and takes a `ValidPoolTransaction`, so it
/// only runs where validation already succeeded. An ordinary
/// `eth_sendRawTransaction` resubmit therefore re-runs the whole validation path
/// against a record that belongs to an **earlier, successful** admission. (A p2p
/// re-announcement does not: `retain_unknown` drops already-pooled hashes first.)
///
/// If that re-run happens to fail — `NonceNotConsistent` once the transaction has
/// landed, or `InsufficientFunds`, which on Mantle flips on its own (see
/// [`PreconfClassifier::release_preconf_claim`]) — releasing the record would
/// strand a live fifo entry without its slot, or hand back the nonce of a
/// commitment that is on chain and still inside its retention window.
///
/// So the rule is narrow and structural: **a call may only release what it
/// created.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// This call inserted the record. A failed admission may release it.
    Fresh,
    /// The record was already there. This call must leave it alone.
    Existing,
}

/// Why [`PreconfClassifier::claim_preconf`] refused a request.
///
/// Two failures that look alike from outside and must not be conflated in what
/// the client is told: one is about *who* the sender is, the other about *when*
/// they asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreconfClaimError {
    /// The allowlist does not cover this `(from, to)` — or preconf is off on
    /// this node entirely.
    NotAllowlisted,
    /// The hash already carries a frozen non-preconf verdict, so this request
    /// can never be satisfied: the same raw transaction reached the pool by the
    /// ordinary route first. Carries the verdict that won.
    AlreadyClassified(Verdict),
}

/// Outcome of trying to claim a `(sender, nonce)` slot at admission.
///
/// `Err` carries the hash that already owns the slot so the caller can ask the
/// fifo what state that transaction is in — the claim itself deliberately knows
/// nothing about fifo status (see `PreconfAwareValidator`).
pub type SlotClaim = Result<(), TxHash>;

/// Decides preconf eligibility once per transaction and remembers the answer.
///
/// Held as `Arc<PreconfClassifier>` and shared by the pool validator (the only
/// writer of new verdicts), the pool listener, the payload builder and the RPC
/// handler.
#[derive(Debug)]
pub struct PreconfClassifier {
    /// Mirrors `PreconfConfig::enabled`. When false this node runs no preconf
    /// machinery at all, so nothing is classified and **nothing is cached** —
    /// see [`Self::admit_and_claim`].
    ///
    /// Load-bearing, not an optimisation: the validator decoration is present in
    /// the pool type on *every* node, sequencer or not, while the two things that
    /// remove cached verdicts (the fifo eviction callback and the canonical-state
    /// sweep) are only wired up when preconf is enabled. Caching on a node that
    /// never sweeps is an unbounded leak.
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

    /// The frozen verdicts and the `(sender, nonce)` slot index.
    /// `parking_lot` ⇒ synchronous reads, usable from the builder's sync apply
    /// hook. Both indexes share this one lock — see [`VerdictStore`].
    verdicts: RwLock<VerdictStore>,

    /// Minimum age before a verdict may be swept. Protects the window between
    /// classification and the listener creating the fifo entry.
    grace: Duration,

    /// Warning threshold for the verdict cache. Never enforced by deleting.
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
            verdicts: RwLock::new(VerdictStore::default()),
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
    /// * **`2 × slot_duration`** — the async hop from `validate_transaction` to the listener
    ///   creating the fifo entry. Sub-millisecond in practice; this is generous headroom.
    /// * **`preconf_timeout`** — the whole period in which a client may still be waiting on its
    ///   responder. Sweeping inside it turns a transaction that was about to be preconfirmed into a
    ///   spurious `Timeout`: the listener would ask [`Self::verdict`] afterwards, get `None`, and
    ///   never create the fifo entry.
    ///
    /// Taking the max makes that invariant hold **by construction**. Deriving it
    /// from `slot_duration` alone does not: both knobs are independently
    /// operator-settable (`--preconf.slot-duration-ms` / `--preconf.timeout-ms`)
    /// and `PreconfConfig::validate` relates neither to the other, so
    /// `preconf_timeout = 10s` with `slot_duration = 2s` — a config it accepts —
    /// would leave a 6s window in which a waiting client's verdict is sweepable.
    ///
    /// Note this bounds *spurious timeouts*, not broken commitments: an actual
    /// commitment implies a fifo entry, and [`Self::sweep`] provably never drops
    /// a verdict whose hash is in the fifo (see its docs).
    pub fn from_config(cfg: &PreconfConfig) -> Self {
        let grace = (cfg.slot_duration * 2).max(cfg.preconf_timeout);
        Self {
            enabled: cfg.enabled,
            ..Self::new(cfg.all_preconfs, grace, DEFAULT_VERDICT_CACHE_CAP)
        }
    }

    /// Latches a transaction's classification on pool admission and claims its
    /// `(sender, nonce)` slot.
    ///
    /// **This does not decide eligibility, and deliberately cannot.** The
    /// allowlists are not consulted here and the recipient is not even a
    /// parameter, because a transaction is preconf-eligible only if its client
    /// asked for that through `eth_sendRawTransactionWithPreconf` — and this
    /// call cannot tell which RPC it is serving. Both methods reach the pool
    /// with `TransactionOrigin::External`, and the p2p, reorg-reinject and
    /// journal-restore paths reach it with no RPC at all.
    ///
    /// What it does instead is **latch**: get-or-insert, inserting
    /// [`Verdict::NotEligible`]. An existing verdict is returned untouched, so
    /// the two callers that *do* have the authority to say "preconf" —
    /// [`Self::claim_preconf`] at the RPC boundary and [`Self::mark_promised`]
    /// for a commitment already acknowledged — get there first and win.
    ///
    /// # Why latch, rather than leave non-preconf transactions unrecorded
    ///
    /// Writing nothing looks equivalent — every consumer treats a missing verdict
    /// as `NotEligible` — but the two differ **over time**: `NotEligible` is
    /// frozen, while absence is merely *undecided* and can still become
    /// `Eligible`.
    ///
    /// The pool listener is where that bites. It reads the verdict when it
    /// *processes* the pending event, not when the pool emits it, and the two
    /// are separated by a task scheduling delay. With no record, a plain
    /// transaction admitted at t0 could be stamped `Eligible` at t1 by a preconf
    /// request naming the same hash, and the listener waking at t2 would hand a
    /// transaction that passed none of the preconf gates a fifo entry. Latching
    /// closes that by construction: this write happens inside `validate()`,
    /// which completes before the insertion that emits the event.
    ///
    /// # Why the slot claim lives here
    ///
    /// The replacement guard needs to answer "does an in-flight preconf
    /// transaction already own this `(sender, nonce)`?". Asking the fifo cannot
    /// answer it correctly: the fifo entry is created asynchronously by the pool
    /// listener, so between this call and that push the slot looks free and a
    /// same-nonce replacement slips past the guard. The pool cannot be asked
    /// either — this layer decorates the *validator*, which runs before the pool
    /// takes its write lock, so any answer it gave would be stale by the time the
    /// insertion happens.
    ///
    /// Claiming here fixes that by construction: the claim is made in the same
    /// critical section that freezes the verdict, so two concurrent admissions
    /// of the same `(sender, nonce)` serialize on this lock and exactly one
    /// wins. The loser gets the winner's hash back and asks the fifo only for
    /// that transaction's *status*.
    ///
    /// Returns the frozen verdict plus the claim outcome. `Ok(())` means the
    /// slot is ours (or we already owned it, or we are not preconf and make no
    /// claim); `Err(owner)` means `owner` holds it.
    pub fn admit_and_claim(
        &self,
        hash: TxHash,
        from: &Address,
        nonce: u64,
    ) -> (Verdict, SlotClaim, Admission) {
        // Preconf off ⇒ nothing is eligible and, crucially, nothing is cached —
        // see `Self::enabled` for why that is load-bearing rather than an
        // optimisation.
        if !self.enabled {
            return (Verdict::NotEligible, Ok(()), Admission::Existing);
        }

        let key = (*from, nonce);
        let mut store = self.verdicts.write();
        // Whether *this* call created the record decides who may destroy it on a
        // failed admission — see [`Admission`].
        let admission =
            if store.by_hash.contains_key(&hash) { Admission::Existing } else { Admission::Fresh };
        // `NotEligible` is the *only* verdict this call ever writes. Anything
        // preconf-eligible was recorded by `claim_preconf` or `mark_promised`
        // before the transaction reached the pool, and `or_insert` leaves it be.
        let verdict = store
            .by_hash
            .entry(hash)
            .or_insert(CachedVerdict::new(Verdict::NotEligible, None))
            .verdict;

        // Checking and claiming are decoupled — see `VerdictStore::claim`, which
        // both this and `mark_promised` go through. In particular the check does
        // **not** depend on the incoming transaction's own verdict: a plain
        // submission arriving on a nonce an in-flight commitment owns is
        // `NotEligible`, and gating on that would let exactly that case through,
        // leaving one transaction on each arm.
        let claim = store.claim(key, hash, verdict.is_preconf());

        let len = store.by_hash.len();
        drop(store);

        self.observe_len(len);
        (verdict, claim, admission)
    }

    /// **Where preconf eligibility is decided.** Claims `hash` for the preconf
    /// fast path, on behalf of a client that asked for it through
    /// `eth_sendRawTransactionWithPreconf`.
    ///
    /// Called from the RPC handler *before* the transaction is offered to the
    /// pool, because that is the only place the deciding fact — which RPC method
    /// this is — exists at all. By the time [`Self::admit_and_claim`] runs, the
    /// two methods are indistinguishable.
    ///
    /// # The claim is exclusive, and the verdict store is the lock
    ///
    /// Get-or-insert, so whoever writes first wins, under the same lock every
    /// other verdict write takes. `Err(existing)` means the hash was already
    /// classified and this request can never be satisfied — verdicts are frozen
    /// for life (see the module docs).
    ///
    /// In practice `Err(NotEligible)` means the same raw transaction was already
    /// submitted through plain `eth_sendRawTransaction` (or arrived over p2p).
    /// The caller should surface that as
    /// [`PreconfError::AlreadyPooledWithoutPreconf`](crate::types::PreconfError::AlreadyPooledWithoutPreconf),
    /// not as "not eligible" — the sender may well be allowlisted; the
    /// transaction is simply already in motion by the ordinary route.
    ///
    /// `Ok(())` on an existing `Eligible` verdict is deliberate: that is the
    /// documented same-hash retry after `Timeout` / `Canceled` / `Failed`, and
    /// re-asking must be idempotent rather than an error.
    ///
    /// # What it does *not* do
    ///
    /// It does not claim the `(sender, nonce)` slot. That stays in
    /// [`Self::admit_and_claim`], which runs under the pool's own admission and
    /// so cannot hand a nonce to a transaction the pool then refuses. The
    /// reverse link is back-filled there by `VerdictStore::claim`.
    ///
    /// Returns `Ok(())` without recording anything when preconf is disabled, for
    /// the same reason [`Self::admit_and_claim`] caches nothing there.
    pub fn claim_preconf(
        &self,
        hash: TxHash,
        from: &Address,
        to: Option<&Address>,
    ) -> Result<(), PreconfClaimError> {
        if !self.enabled {
            return Err(PreconfClaimError::NotAllowlisted);
        }
        // Evaluated before the lock, so the allowlist lock and the verdict lock
        // are never held together — the same discipline `admit_and_claim`
        // followed while it still consulted the allowlists.
        if !self.evaluate_whitelist(from, to).is_preconf() {
            return Err(PreconfClaimError::NotAllowlisted);
        }

        let mut store = self.verdicts.write();
        let verdict = store
            .by_hash
            .entry(hash)
            .or_insert(CachedVerdict::new(Verdict::Eligible, None))
            .verdict;
        let len = store.by_hash.len();
        drop(store);

        self.observe_len(len);
        if verdict.is_preconf() {
            Ok(())
        } else {
            Err(PreconfClaimError::AlreadyClassified(verdict))
        }
    }

    /// Undo the effect of an admission that did not take: drop `hash`'s record
    /// unless something else has come to depend on it. Returns `true` if the
    /// record was dropped.
    ///
    /// The counterpart to [`Self::claim_preconf`], and the single home for the
    /// question "may this failed admission destroy the record?". Both callers
    /// ask it — the RPC handler when `add_transaction` refuses, and the
    /// validator when its own gates or the inner validator refuse — and they
    /// must agree, because for one transaction the two are the same record.
    ///
    /// The guard is the `promised` flag, never the [`Verdict::Promised`] variant:
    /// keying on the variant would release the `Eligible` + promised records the
    /// ordinary RPC flow leaves behind, i.e. exactly the ones that must survive.
    /// [`Self::release_unless_committed`] underneath does not catch those either
    /// — it reads `committed_height`, which only [`Self::mark_committed`] sets,
    /// and the canonical notification has not arrived yet. A commitment already
    /// acknowledged to a client would lose its `(sender, nonce)` inside the
    /// retention window, the one thing that window exists to prevent.
    ///
    /// # What makes the window reachable
    ///
    /// Not "the receipt went out, so the nonce has advanced and a resubmit is
    /// refused": once the block is canonical, `committed_height` is set and
    /// [`Self::release_unless_committed`] already refuses. The reachable window
    /// is the earlier one — receipt returned, block **not yet canonical** —
    /// where the transaction is still in the pool, so a same-hash resubmit is
    /// re-validated rather than deduplicated. That re-validation can fail on
    /// account state through no act of the sender: Mantle recomputes
    /// `extra_balance_cost` from the current `l1_block_info` every time, so
    /// `InsufficientFunds` can flip between two blocks (the reachability
    /// `a_repooled_tx_that_fails_revalidation_keeps_its_slot` is built on).
    ///
    /// It is also reachable *with* concurrency, which is why the validator
    /// cannot narrow it to "the record is not mine": `mark_promised` can run on
    /// another task while this admission sits inside the inner validator, so a
    /// `Fresh` admission can come back to find its own hash promised.
    pub fn release_preconf_claim(&self, hash: &TxHash) -> bool {
        if self.is_promised(hash) {
            return false;
        }
        self.release_unless_committed(hash)
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
    /// it needs a record that already exists — and the frozen verdict cannot
    /// serve, because `forward → release_unless_committed` races the notification
    /// and may already have dropped it. A receipt, by contrast, necessarily
    /// precedes the block, so a record written here is in place before either
    /// event can happen. See [`Self::mark_committed`].
    ///
    /// The slot claim **never displaces an existing owner** — not even the way
    /// [`Self::replace_slot`] does with an expected one. If the slot is taken,
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
        let mut store = self.verdicts.write();

        // Get-or-insert, then set `promised`. The insert case is journal restore
        // (this process has never seen the hash); the update case is the RPC
        // handler, where `claim_preconf` froze `Eligible` on the way in. Either
        // way the verdict itself is left alone — see `Verdict::Promised`.
        let cached = store
            .by_hash
            .entry(hash)
            .or_insert_with(|| CachedVerdict::new(Verdict::Promised, None));
        cached.promised = true;
        cached.promised_height = Some(promised_height);
        let is_preconf = cached.verdict.is_preconf();

        let claim = store.claim(key, hash, is_preconf);
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
        let mut store = self.verdicts.write();
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
        let mut store = self.verdicts.write();
        store.by_hash.get_mut(hash).and_then(|cached| cached.committed_height.take()).is_some()
    }

    /// Whether a `Success` receipt for this hash has been returned to a client.
    ///
    /// **Synchronous** — which is the point: the validator decides slot
    /// ownership on a sync path and so cannot reach for the journal's
    /// `contains(&hash).await`.
    pub fn is_promised(&self, hash: &TxHash) -> bool {
        self.verdicts.read().by_hash.get(hash).is_some_and(|cached| cached.promised)
    }

    /// Whether this hash's commitment record may be dropped: it has to have been
    /// observed on chain and buried under [`SEAL_DEPTH`] persisted blocks.
    ///
    /// `false` for a hash with no record at all — a caller asking about an
    /// unknown hash gets "not releasable", never "go ahead".
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
    pub fn is_tracked(&self, hash: &TxHash) -> bool {
        self.verdicts.read().by_hash.contains_key(hash)
    }

    /// Hands the `(sender, nonce)` slot from `expected_owner` to `hash`, **only
    /// if `expected_owner` still holds it**. `Err(current)` means we lost the
    /// race and `current` owns the nonce now.
    ///
    /// The reclaimable-replacement handover. The caller established, *before*
    /// running the inner validator, that `expected_owner` was in a terminal
    /// not-on-chain state and its nonce could therefore be taken; the inner
    /// validator is `async`, so another same-nonce transaction may have made the
    /// same observation meanwhile. See `VerdictStore::replace` for why that has
    /// to be a compare-and-swap.
    ///
    /// On `Ok`, and only then, may the caller tear `expected_owner` down (drop
    /// its fifo entry and verdict): exactly one replacement gets to do that.
    ///
    /// No-op returning `Ok(())` when `hash` has no verdict, or has one that is
    /// not [`Verdict::is_preconf`] — as in `VerdictStore::claim`, there would be
    /// nothing to hang the reverse link on. The CAS is still performed in the
    /// non-preconf case: such a transaction does not want the nonce, but it does
    /// need to know it is the one entitled to evict the holder.
    pub fn replace_slot(
        &self,
        expected_owner: &TxHash,
        sender: &Address,
        nonce: u64,
        hash: TxHash,
    ) -> SlotClaim {
        if !self.enabled {
            return Ok(());
        }
        let mut store = self.verdicts.write();
        let is_preconf = store.by_hash.get(&hash).is_some_and(|cached| cached.verdict.is_preconf());
        store.replace(expected_owner, (*sender, nonce), hash, is_preconf)
    }

    /// The one query for every downstream consumer. Synchronous.
    ///
    /// `None` means "no record", which consumers must read as **not preconf** —
    /// no record ⟺ the preconf arm makes no claim, so the pool arm takes it.
    /// That default is safe rather than merely convenient: classification
    /// happens inside `validate_transaction`, synchronously, *before* the
    /// transaction enters the pool, so by the time any other component can
    /// observe the transaction a verdict necessarily exists.
    pub fn verdict(&self, hash: &TxHash) -> Option<Verdict> {
        self.verdicts.read().by_hash.get(hash).map(|cached| cached.verdict)
    }

    /// Which transaction currently owns `(sender, nonce)`, if any.
    ///
    /// Only preconf verdicts ever own a slot, so `None` means "no in-flight
    /// preconf claim on this nonce".
    pub fn slot_owner(&self, sender: &Address, nonce: u64) -> Option<TxHash> {
        self.verdicts.read().by_slot.get(&(*sender, nonce)).copied()
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

    /// Drops one verdict and releases the slot it owned — **unless the
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
        let mut store = self.verdicts.write();
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

    /// Drops verdicts that are **absent from `live`** and **older than the grace
    /// period**. Returns how many were dropped.
    ///
    /// ## What it can never drop
    ///
    /// `live` is [`PreconfTxSet::snapshot`](crate::PreconfTxSet::snapshot), i.e.
    /// the fifo's `order` deque, and `order` is only ever mutated alongside
    /// `entries` under a single lock — `push_if_absent` inserts into both,
    /// `drop_hash` removes from both. So `live` cannot miss a transaction that
    /// has a fifo entry, and a verdict backing an **actual commitment** is
    /// therefore unsweepable regardless of age: a commitment implies a fifo
    /// entry, and a fifo entry implies membership in `live`.
    ///
    /// The grace period covers the other direction — a verdict that does *not*
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
    /// caller before this call, so a transaction that is older than `grace` and
    /// gets pushed into the fifo in the microseconds between the snapshot and
    /// the `retain` below loses its verdict while holding a fifo entry. Reaching
    /// it requires a transaction that sat without an entry for longer than
    /// `grace` (i.e. parked in `Queued`) being promoted at exactly that instant;
    /// its client has necessarily long since timed out, so the outcome is a fifo
    /// entry whose apply fails on nonce and is reclaimed by
    /// `clean_reclaimable` — never a broken promise. Closing it would mean
    /// taking the fifo's async lock from this sync path, which is the dependency
    /// the whole callback/sweep split exists to avoid.
    ///
    /// ## Why this is also the only fix for the main leak
    ///
    /// A transaction classified at admission that then sits in the `Queued`
    /// sub-pool, never emits a `Pending` event and never gets a fifo entry will
    /// never reach `drop_hash`. This sweep is the only thing that can reclaim
    /// it: absent from `live`, past grace.
    ///
    /// Called from the canonical-state handler, next to the fifo cleanup — once
    /// per canonical notification (≈ one block), nowhere near the admission hot
    /// path, and at that cadence the cache stays small enough to need no
    /// capacity-triggered eviction.
    pub fn sweep(&self, live: &HashSet<TxHash>) -> usize {
        let now = Instant::now();

        let mut store = self.verdicts.write();
        let before = store.by_hash.len();

        // Collect first, then release: `retain`'s closure cannot borrow
        // `by_slot` mutably while `by_hash` is being iterated. Both live under
        // the same lock, so the two steps are still one critical section — no
        // one can observe a dropped verdict whose slot is still claimed.
        let persisted = self.persisted_height.load(Ordering::Relaxed);
        let mut released: Vec<(TxHash, CachedVerdict)> = Vec::new();
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

        metrics::gauge!("preconf.classifier.verdicts").set(len as f64);
        // Published because a leaked slot is harder to diagnose from outside than
        // a leaked verdict — it looks like "that account's transaction is
        // mysteriously rejected".
        //
        // Read it as a **lower bound** on in-flight preconf transactions, not as
        // a count of them: a restored commitment admitted under the promised
        // exemption (see `PreconfAwareValidator`) can hold a fifo entry without
        // owning a slot.
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
    /// Affects **only transactions admitted after this point** — every verdict
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

    /// Number of frozen verdicts — for logging, metrics and assertions.
    pub fn verdict_count(&self) -> usize {
        self.verdicts.read().by_hash.len()
    }

    /// Number of claimed `(sender, nonce)` slots — for assertions. Always
    /// `<= verdict_count()`, since only preconf verdicts claim a slot.
    pub fn slot_count(&self) -> usize {
        self.verdicts.read().by_slot.len()
    }

    /// **Non-authoritative** eligibility preview, for the RPC handler's early
    /// rejection only. Does not write the cache, so it cannot pre-empt the
    /// verdict the validator will freeze a moment later.
    pub fn preview_eligibility(&self, from: &Address, to: Option<&Address>) -> bool {
        self.evaluate_whitelist(from, to).is_preconf()
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
    fn evaluate_whitelist(&self, from: &Address, to: Option<&Address>) -> Verdict {
        if !self.enabled {
            return Verdict::NotEligible;
        }
        if self.all_preconfs {
            return Verdict::Eligible;
        }
        let hit = self.whitelist.read().is_eligible(from, to);
        if hit { Verdict::Eligible } else { Verdict::NotEligible }
    }

    /// Tracks whether the cache is over [`Self::capacity`], warning on the upward
    /// crossing. Never deletes.
    ///
    /// Deleting under pressure would break commitments, and would not even be the
    /// useful thing to do: a verdict is a hash, an enum, an `Instant` and an
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
                "verdict cache above capacity — chain likely stalled (fifo eviction is canon-driven)"
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
        /// verdict at the RPC boundary first, then latch and claim through the
        /// validator.
        ///
        /// Two calls, because that is what production does and the order is the
        /// whole point — `admit_and_claim` alone can only ever produce
        /// `NotEligible`. A test that wants an eligible transaction has to say
        /// so through the same door a client would.
        fn admit_via_preconf_rpc(
            &self,
            hash: TxHash,
            from: &Address,
            nonce: u64,
        ) -> (Verdict, SlotClaim, Admission) {
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
        ) -> (Verdict, SlotClaim, Admission) {
            let _ = self.claim_preconf(hash, from, to);
            self.admit_and_claim(hash, from, nonce)
        }

        /// [`Self::admit_via_preconf_rpc`] for the verdict-only shim above.
        fn classify_verdict_via_preconf_rpc(&self, hash: TxHash, from: &Address) -> Verdict {
            self.admit_via_preconf_rpc(hash, from, u64::from(hash.0[0])).0
        }

        /// [`Self::classify_verdict_via_preconf_rpc`] with an explicit recipient.
        fn classify_verdict_to(
            &self,
            hash: TxHash,
            from: &Address,
            to: Option<&Address>,
        ) -> Verdict {
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
        let c = PreconfClassifier::new(false, grace, DEFAULT_VERDICT_CACHE_CAP);
        c.update_whitelist(pair_set(&[(addr(1), addr(2))]), HashSet::default(), HashSet::default());
        c
    }

    // ===== Retention: a commitment that has been observed on chain keeps its
    // ===== verdict and its nonce until the block is buried `SEAL_DEPTH` deep.

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
    fn a_committed_verdict_survives_the_fifo_forward() {
        let c = classifier(LONG_GRACE);
        committed_commitment(&c);

        // Shallow: one block on top is nowhere near `SEAL_DEPTH`.
        c.observe_persisted(AT + 1);
        assert!(!c.release_unless_committed(&hash(1)), "must refuse to release");

        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible), "verdict kept");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and so is the nonce");
    }

    /// The other side of the same predicate: once the block is buried, the
    /// commitment is irrevocable and tracking it costs a pinned nonce for
    /// nothing.
    #[test]
    fn a_committed_verdict_is_released_once_it_is_deep_enough() {
        let c = classifier(LONG_GRACE);
        committed_commitment(&c);

        c.observe_persisted(AT + SEAL_DEPTH);
        assert!(c.release_unless_committed(&hash(1)));

        assert_eq!(c.verdict(&hash(1)), None);
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
    fn an_uncommitted_verdict_is_released_immediately() {
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
    fn mark_committed_is_a_noop_without_a_promise_record() {
        let c = classifier(LONG_GRACE);
        // Classified and holding a slot, but no receipt ever went out.
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        // And a hash the classifier has never seen at all.
        assert!(!c.mark_committed(&hash(1), AT), "a mere verdict is not a promise");
        assert!(!c.mark_committed(&hash(9), AT), "and an unknown hash is nothing");

        // Neither earns a retention period.
        c.observe_persisted(AT + 1);
        assert!(c.release_unless_committed(&hash(1)));
        assert_eq!(c.slot_count(), 0);
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

        assert!(c.is_promised(&hash(1)), "still an owed commitment");
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
    /// in must carry none of its state: `PreconfAwareValidator` is in the pool
    /// type on *every* node (see `MantleTransactionPool`), so `admit_and_claim`
    /// runs for every transaction a verifier ever sees while nothing on that node
    /// ever sweeps — see the `enabled` field.
    #[test]
    fn disabled_node_classifies_without_caching() {
        let cfg = PreconfConfig::default();
        assert!(!cfg.enabled, "the default config is what a non-sequencer node gets");
        let c = PreconfClassifier::from_config(&cfg);

        for i in 1..=50u8 {
            assert_eq!(
                c.classify_verdict_via_preconf_rpc(hash(i), &addr(1)),
                Verdict::NotEligible,
                "nothing is preconf-eligible on a node with preconf off",
            );
        }

        assert_eq!(c.verdict_count(), 0, "a disabled node must not retain any verdict");
        assert_eq!(c.verdict(&hash(1)), None, "and must report no record downstream");
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
        assert_eq!(c.verdict_count(), 0);
    }

    /// `is_preconf` is *the* partition predicate — the pool arm skips exactly
    /// these — so all three variants are pinned.
    ///
    /// There is deliberately no companion predicate for "exempt from
    /// admission-time policy gates": that exemption keys on the `promised` flag,
    /// not on any verdict, and is applied once by an early return in
    /// `PreconfAwareValidator` rather than gate by gate. See `pool_ext::validator`.
    #[test]
    fn verdict_predicates_are_pinned() {
        assert!(Verdict::Eligible.is_preconf());
        assert!(Verdict::Promised.is_preconf());
        assert!(!Verdict::NotEligible.is_preconf());
    }

    #[test]
    fn unknown_hash_has_no_verdict() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.verdict(&hash(9)), None);
        assert_eq!(c.verdict_count(), 0);
    }

    #[test]
    fn a_preconf_request_is_matched_against_the_allowlist() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.classify_verdict_to(hash(1), &addr(1), Some(&addr(2))), Verdict::Eligible);
        // Sender not allowlisted.
        assert_eq!(c.classify_verdict_to(hash(2), &addr(3), Some(&addr(2))), Verdict::NotEligible);
        // Recipient not allowlisted.
        assert_eq!(c.classify_verdict_to(hash(3), &addr(1), Some(&addr(9))), Verdict::NotEligible);
    }

    /// **The allowlist is consulted at the RPC boundary, never at admission.**
    /// A transaction that reaches the pool without a preconf request — plain
    /// `eth_sendRawTransaction`, p2p, the pool's own reorg reinject — is
    /// `NotEligible` however well allowlisted its sender is.
    #[test]
    fn admission_alone_never_produces_an_eligible_verdict() {
        let c = classifier(LONG_GRACE);
        // `addr(1) -> addr(2)` is exactly the allowlisted pair.
        let (verdict, claim, admission) = c.admit_and_claim(hash(1), &addr(1), 7);
        assert_eq!(verdict, Verdict::NotEligible);
        assert_eq!(admission, Admission::Fresh, "the record is this call's");
        assert_eq!(claim, Ok(()), "and it claims no slot, having nothing to defend");
        assert_eq!(c.slot_count(), 0);
    }

    #[test]
    fn contract_creation_is_not_eligible_without_a_sender_wildcard() {
        let c = classifier(LONG_GRACE);
        // The fixture allowlists the pair `addr(1) -> addr(2)` and no wildcards,
        // and a creation has no recipient for the pair to match against.
        assert_eq!(c.classify_verdict_to(hash(1), &addr(1), None), Verdict::NotEligible);
    }

    #[test]
    fn all_preconfs_ignores_allowlists_including_contract_creation() {
        let c = PreconfClassifier::new(true, LONG_GRACE, DEFAULT_VERDICT_CACHE_CAP);
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(7)), Verdict::Eligible);
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(2), &addr(7)), Verdict::Eligible);
    }

    /// Case A: eligible at admission, then the sender is removed from the
    /// allowlist. The verdict must not flip, or the commitment breaks.
    #[test]
    fn verdict_is_frozen_when_allowlist_shrinks() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(1)), Verdict::Eligible);

        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible));
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(1)), Verdict::Eligible);
    }

    /// Case B: not eligible at admission, then the sender is added. The verdict
    /// must not flip, or the pool arm starts skipping a transaction the preconf
    /// arm has no entry for and it stalls silently.
    #[test]
    fn verdict_is_frozen_when_allowlist_grows() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(3)), Verdict::NotEligible);

        c.update_whitelist(
            pair_set(&[(addr(1), addr(2)), (addr(3), addr(2))]),
            HashSet::default(),
            HashSet::default(),
        );

        assert_eq!(c.verdict(&hash(1)), Some(Verdict::NotEligible));
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(3)), Verdict::NotEligible);
        // A *different* transaction admitted after the update does see it.
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(2), &addr(3)), Verdict::Eligible);
    }

    #[test]
    fn promised_survives_later_classification() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        // Restore pushes the envelope through the validator with an allowlist
        // that no longer contains the sender.
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(9)), Verdict::Promised);
        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Promised));
    }

    /// The journal-restore path in one call: record the promise **and** claim the
    /// nonce it was acknowledged for.
    ///
    /// The release at the end is the real assertion. A claim recorded in
    /// `by_slot` without its reverse link in `CachedVerdict::slot` is
    /// unreleasable, so the nonce would stay blocked until the next sweep — a
    /// leak that no "is the slot claimed?" assertion would catch.
    #[test]
    fn mark_promised_claims_the_slot_and_records_the_reverse_link() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
        assert!(c.is_promised(&hash(1)));
        assert_eq!(
            c.verdict(&hash(1)),
            Some(Verdict::Promised),
            "an unseen hash restores as Promised"
        );

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
        // `Existing`, not `Fresh`: the record was created at the RPC boundary by
        // `claim_preconf`, so the validator's call only found it.
        assert_eq!(
            c.admit_via_preconf_rpc(hash(2), &addr(1), 7),
            (Verdict::Eligible, Ok(()), Admission::Existing)
        );

        assert_eq!(
            c.mark_promised(hash(1), &addr(1), 7, 0),
            Err(hash(2)),
            "reports the incumbent rather than evicting it"
        );
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));
        assert!(c.is_promised(&hash(1)), "the promise is still recorded — only the claim lost");
    }

    /// A non-preconf verdict has no arm to defend, so it must not occupy the
    /// slot: there would be nothing to hang the reverse link on and the nonce
    /// would leak until the next sweep.
    #[test]
    fn mark_promised_claims_nothing_for_a_non_preconf_verdict() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(3), &addr(9)), Verdict::NotEligible);

        assert_eq!(c.mark_promised(hash(3), &addr(9), 7, 0), Ok(()));

        assert_eq!(c.slot_owner(&addr(9), 7), None);
        assert_eq!(c.slot_count(), 0);
    }

    /// **`mark_promised` must not overwrite an existing verdict with
    /// `Promised`**, and this pins that. Overwriting would not change which
    /// transactions are exempt (that keys on the `promised` flag — see
    /// [`Verdict::Promised`]); it would only make a value the rest of the code
    /// treats as frozen start changing mid-life.
    ///
    /// The state this test constructs is reachable only from the RPC path, where
    /// `claim_preconf` froze `Eligible` on the way in: restore runs before
    /// anything in the process can classify, so its record is always a fresh
    /// insert (asserted in `mark_promised_claims_the_slot_*`).
    #[test]
    fn mark_promised_records_the_promise_without_rewriting_the_verdict() {
        let c = classifier(LONG_GRACE);
        assert_eq!(c.admit_via_preconf_rpc(hash(1), &addr(1), 7).0, Verdict::Eligible);

        assert_eq!(c.mark_promised(hash(1), &addr(1), 7, 0), Ok(()));

        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible), "verdict untouched");
        assert!(c.is_promised(&hash(1)), "but the promise is recorded");
    }

    #[test]
    fn forget_drops_only_the_named_verdict() {
        let c = classifier(LONG_GRACE);
        c.classify_verdict_via_preconf_rpc(hash(1), &addr(1));
        c.classify_verdict_via_preconf_rpc(hash(2), &addr(1));

        c.release_unless_committed(&hash(1));

        assert_eq!(c.verdict(&hash(1)), None);
        assert_eq!(c.verdict(&hash(2)), Some(Verdict::Eligible));
    }

    #[test]
    fn sweep_drops_entries_absent_from_live_and_past_grace() {
        let c = classifier(Duration::ZERO);
        c.classify_verdict_via_preconf_rpc(hash(1), &addr(1));
        c.classify_verdict_via_preconf_rpc(hash(2), &addr(3));

        assert_eq!(c.sweep(&HashSet::default()), 2);
        assert_eq!(c.verdict_count(), 0);
    }

    #[test]
    fn sweep_keeps_entries_within_grace() {
        // The window between classification and the listener creating the fifo
        // entry: absent from `live`, but too young to drop.
        let c = classifier(LONG_GRACE);
        c.classify_verdict_via_preconf_rpc(hash(1), &addr(1));

        assert_eq!(c.sweep(&HashSet::default()), 0);
        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible));
    }

    #[test]
    fn sweep_keeps_live_entries_regardless_of_age() {
        let c = classifier(Duration::ZERO);
        c.classify_verdict_via_preconf_rpc(hash(1), &addr(1));
        assert_eq!(c.mark_promised(hash(2), &addr(2), 0, 0), Ok(()));

        assert_eq!(c.sweep(&hashes(&[hash(1), hash(2)])), 0);
        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible));
        assert_eq!(c.verdict(&hash(2)), Some(Verdict::Promised));
    }

    #[test]
    fn sweep_keeps_live_and_drops_the_rest() {
        let c = classifier(Duration::ZERO);
        c.classify_verdict_via_preconf_rpc(hash(1), &addr(1));
        c.classify_verdict_via_preconf_rpc(hash(2), &addr(1));
        c.classify_verdict_via_preconf_rpc(hash(3), &addr(1));

        assert_eq!(c.sweep(&hashes(&[hash(2)])), 2);
        assert_eq!(c.verdict(&hash(2)), Some(Verdict::Eligible));
        assert_eq!(c.verdict_count(), 1);
    }

    #[test]
    fn over_capacity_flags_but_never_deletes() {
        let c = PreconfClassifier::new(true, LONG_GRACE, 2);
        for i in 1..=4 {
            c.classify_verdict_via_preconf_rpc(hash(i), &addr(1));
        }

        assert_eq!(c.verdict_count(), 4, "entries above capacity must be kept");
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
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_VERDICT_CACHE_CAP);
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
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_VERDICT_CACHE_CAP);
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
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_VERDICT_CACHE_CAP);
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

        assert_eq!(c.verdict_count(), 0, "preview must not freeze anything");
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
        assert_eq!(c.capacity, DEFAULT_VERDICT_CACHE_CAP);
        assert_eq!(c.whitelist_counts(), (0, 0, 0));
        assert_eq!(c.verdict_count(), 0);
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
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(1), &addr(200)), Verdict::Eligible);
    }

    // ===================== (sender, nonce) slot index =====================
    //
    // The slot index exists for one reason: the replacement guard must be able
    // to answer "is an in-flight preconf transaction already using this nonce?"
    // *at admission time*, before the pool listener has had a chance to create
    // the fifo entry. Asking the fifo cannot answer that — hence these tests
    // never touch a fifo.

    /// The whole point: a second hash on the same `(sender, nonce)` is told who
    /// holds the slot, **with no fifo involved**. This is the case the
    /// fifo-membership guard misses.
    #[test]
    fn second_tx_on_same_sender_nonce_is_refused_the_slot() {
        let c = classifier(LONG_GRACE);

        let (v1, claim1, _) = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(v1, Verdict::Eligible);
        assert_eq!(claim1, Ok(()), "first eligible tx must own the slot");

        let (v2, claim2, _) = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);
        assert_eq!(v2, Verdict::Eligible, "the verdict is still frozen normally");
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

    /// **The slot is checked whatever the newcomer's own verdict is.**
    ///
    /// Reached by an allowlist update landing between two same-`(sender, nonce)`
    /// submissions: the first is `Eligible` and owns the slot, the second is
    /// judged `NotEligible` because the sender was removed in between. Gating
    /// the check on the newcomer's verdict would let it through, and the pool
    /// would then hold one transaction on each arm for the same nonce —
    /// whichever executes first silently kills the other.
    #[test]
    fn de_whitelisted_replacement_is_still_refused_the_slot() {
        let c = classifier(LONG_GRACE);

        let (v1, claim1, _) = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(v1, Verdict::Eligible);
        assert_eq!(claim1, Ok(()));

        // Governance revokes the rule; the incumbent's verdict stays frozen.
        c.update_whitelist(HashSet::default(), HashSet::default(), HashSet::default());

        let (v2, claim2, _) = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);
        assert_eq!(v2, Verdict::NotEligible, "newcomer is judged under the new allowlist");
        assert_eq!(
            claim2,
            Err(hash(1)),
            "but it must still be told the slot is taken — otherwise both arms end up \
             holding a transaction for the same nonce",
        );
        assert_eq!(c.verdict(&hash(1)), Some(Verdict::Eligible), "incumbent unaffected");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));
    }

    /// The classifier reports a taken slot **truthfully even for `Promised`**;
    /// the decision to let a restored commitment through anyway belongs to the
    /// guard, not here.
    ///
    /// Keeping the report honest matters: if this returned `Ok(())` the index
    /// would silently disagree with reality, and `PreconfAwareValidator` could
    /// no longer tell "nobody holds this nonce" from "a commitment is being
    /// restored over someone else's claim".
    #[test]
    fn promised_is_told_the_truth_about_a_taken_slot() {
        let c = classifier(LONG_GRACE);

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)));

        // A journal entry for a *different* hash on the same (sender, nonce):
        // its claim loses, so it ends up Promised with no slot of its own.
        assert_eq!(c.mark_promised(hash(2), &addr(1), 7, 0), Err(hash(1)));
        let (verdict, claim, _) = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);

        assert_eq!(verdict, Verdict::Promised);
        assert_eq!(claim, Err(hash(1)), "the incumbent is reported, not silently overwritten");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "and it keeps the slot");
    }

    /// A non-preconf transaction must not occupy a slot: it would reject
    /// replacements that the preconf arm has no stake in.
    #[test]
    fn non_preconf_tx_claims_no_slot() {
        let c = classifier(LONG_GRACE);

        let (verdict, claim, _) = c.admit_via_preconf_rpc(hash(1), &addr(9), 7);
        assert_eq!(verdict, Verdict::NotEligible);
        assert_eq!(claim, Ok(()));
        assert_eq!(c.verdict_count(), 1, "the verdict is still frozen");
        assert_eq!(c.slot_count(), 0, "but no slot is claimed");

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

    /// Forgetting a transaction that has already lost the slot to someone else
    /// must not evict the new owner — otherwise a late `drop_hash` for the old
    /// hash would silently unblock the nonce.
    #[test]
    fn forget_does_not_evict_a_slot_already_handed_over() {
        let c = classifier(LONG_GRACE);

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        // Hand the slot over the way the reclaimable-replacement path does.
        let _ = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);
        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));

        c.release_unless_committed(&hash(1));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)), "new owner survives");
    }

    /// Sweeping a verdict must release its slot in the same critical section —
    /// a leaked slot blocks that nonce for as long as the process lives.
    #[test]
    fn sweep_releases_slots_of_dropped_verdicts() {
        let c = classifier(Duration::ZERO);

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.slot_count(), 1);

        assert_eq!(c.sweep(&hashes(&[])), 1);
        assert_eq!(c.verdict_count(), 0);
        assert_eq!(c.slot_count(), 0, "slot released with the verdict");
    }

    /// The mirror of the above: a verdict the sweep keeps must keep its slot.
    #[test]
    fn sweep_keeps_slots_of_surviving_verdicts() {
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

        assert!(c.is_promised(&hash(1)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(1)), "claim preserved");
        c.release_unless_committed(&hash(1));
        assert_eq!(c.slot_count(), 0, "and is still releasable");
    }

    /// A disabled node must carry no slot state either — same reasoning as
    /// `disabled_node_classifies_without_caching`.
    #[test]
    fn disabled_node_claims_no_slots() {
        let c = PreconfClassifier::new(false, LONG_GRACE, DEFAULT_VERDICT_CACHE_CAP);
        let disabled = PreconfClassifier { enabled: false, ..c };

        for i in 0..20u8 {
            assert_eq!(disabled.admit_via_preconf_rpc(hash(i), &addr(1), 7).1, Ok(()));
        }
        assert_eq!(disabled.verdict_count(), 0);
        assert_eq!(disabled.slot_count(), 0);
    }

    /// `replace_slot` for a hash with no verdict would create a claim nothing can
    /// release — the reverse link lives on the verdict. The CAS still succeeds
    /// (we *were* entitled to evict the holder); only the claim is skipped.
    #[test]
    fn replace_slot_without_a_verdict_claims_nothing() {
        let c = classifier(LONG_GRACE);
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);

        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(9)), Ok(()));
        assert_eq!(c.slot_count(), 0, "holder released, nothing claimed in its place");
    }

    /// **The handover is a compare-and-swap** — see `VerdictStore::replace` for
    /// why. Two same-nonce transactions can both observe the same reclaimable
    /// holder before either reaches the inner validator (it is `async`), so both
    /// come back to take the slot; exactly one may win.
    #[test]
    fn replace_slot_refuses_when_the_holder_already_lost_the_slot() {
        let c = classifier(LONG_GRACE);

        // A holds the nonce; B and C both classify while it still does.
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        assert_eq!(c.admit_via_preconf_rpc(hash(2), &addr(1), 7).1, Err(hash(1)));
        assert_eq!(c.admit_via_preconf_rpc(hash(3), &addr(1), 7).1, Err(hash(1)));

        // B wins the race.
        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));

        // C comes back expecting A and must be told it lost, to B.
        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(3)), Err(hash(2)));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)), "the winner keeps it");
    }

    /// Re-validation of the same hash must not be mistaken for a lost race.
    #[test]
    fn replace_slot_is_idempotent_for_the_current_owner() {
        let c = classifier(LONG_GRACE);

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        let _ = c.admit_via_preconf_rpc(hash(2), &addr(1), 7);
        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()));

        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()), "already ours");
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));
    }

    /// A vacant slot is nobody's, so there is nothing to lose the race to.
    #[test]
    fn replace_slot_takes_a_vacant_slot() {
        let c = classifier(LONG_GRACE);
        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        c.release_unless_committed(&hash(1));

        let _ = c.admit_via_preconf_rpc(hash(2), &addr(1), 8);
        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()));
        assert_eq!(c.slot_owner(&addr(1), 7), Some(hash(2)));
    }

    /// A non-preconf replacement is entitled to evict the holder but must not
    /// occupy the nonce — it has no arm to defend, so holding it would refuse
    /// later replacements for nothing.
    #[test]
    fn replace_slot_releases_without_claiming_for_a_non_preconf_tx() {
        let c = classifier(LONG_GRACE);

        let _ = c.admit_via_preconf_rpc(hash(1), &addr(1), 7);
        // addr(9) is not allowlisted ⇒ NotEligible.
        assert_eq!(c.classify_verdict_via_preconf_rpc(hash(2), &addr(9)), Verdict::NotEligible);

        assert_eq!(c.replace_slot(&hash(1), &addr(1), 7, hash(2)), Ok(()));
        assert_eq!(c.slot_owner(&addr(1), 7), None, "released, not taken over");
    }
}
