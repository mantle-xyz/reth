//! `PreconfTxSet` — the commitment truth source.
//!
//! ## Responsibilities
//!
//! 1. Track in-flight preconf-eligible transactions in FIFO order
//! 2. Notify the builder via [`tokio::sync::broadcast`] (the single fifo event source)
//! 3. Hold RPC `oneshot::Sender` responders attached by the RPC handler
//! 4. Survive across slots — buffers requests during dead window
//!
//! ## Concurrency model
//!
//! All mutations of inner state go through a single [`tokio::sync::Mutex`].
//! The broadcast notifier and oneshot responders are signalled **outside** the
//! mutex — `send` is non-blocking and lock-free.
//!
//! ## Invariants
//!
//! - At most one entry per `(sender, nonce)` in an **active** status; such an entry blocks a push
//!   for a different hash on that `(sender, nonce)`, while a **reclaimable** incumbent is evicted
//!   in its favour. [`crate::types::PreconfStatus`] owns which states are which.
//! - At most one responder per hash. Either lives inside an existing entry, or in
//!   `pending_responders` until the matching `push_if_absent` consumes it.
//! - `notifier.send` is best-effort — slow consumers receive `Lagged(n)` and reconcile via
//!   `snapshot()`.

use alloy_consensus::{Transaction, TxEnvelope};
// foldhash HashMap: faster than SipHash on high-entropy keys (TxHash /
// Address); matches the allowlist sets in `whitelist.rs`.
// `HashMapExt` brings `::new()` / `::with_capacity()` into scope.
use alloy_primitives::{
    Address, TxHash,
    map::foldhash::{HashMap, HashMapExt},
};
use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedMutexGuard, broadcast, oneshot};
use tracing::error;

use crate::types::{
    AttachError, MarkError, PreconfError, PreconfReceipt, PreconfSource, PreconfStatus, PushResult,
};

/// A single fifo entry.
///
/// Cloned for `snapshot` / `find_*` queries; the responder field is
/// **never** cloned (it's a `oneshot::Sender`, take-once semantics).
#[derive(Debug)]
pub struct TxEntry {
    /// Transaction hash.
    pub hash: TxHash,
    /// The full signed transaction — held by `Arc` so callers can share
    /// without copying tx bytes.
    pub tx: Arc<TxEnvelope>,
    /// Recovered sender. Cached here to avoid re-running ec-recover on every
    /// `find_by_sender_nonce` lookup.
    pub from: Address,
    /// Sender nonce.
    pub nonce: u64,
    /// Wall-clock insertion time. Load-bearing: `builder::dispatch`
    /// pre-apply deadline check aborts with `Timeout` when
    /// `elapsed + SAFETY_MARGIN >= preconf_timeout`.
    pub inserted_at: Instant,
    /// Current status — see [`PreconfStatus`].
    pub status: PreconfStatus,
    /// Origin of the entry — see [`PreconfSource`]. Determines which
    /// pre-apply gates `builder::dispatch::apply_one_preconf` enforces.
    pub source: PreconfSource,
    /// RPC handler responder — `Some` when the RPC path attached one before
    /// pool.add succeeded; `None` for listener-pushed entries.
    /// Take-once: `take_responder` moves it out.
    pub responder: Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>>,
    /// Per-entry lock serialising `apply_fn + mark_succeeded/failed +
    /// send(receipt)` in `builder::dispatch::apply_one_preconf` with any
    /// concurrent `mark_timeout` initiated by the RPC deadline branch
    /// in `rpc::handle_inner`. When dispatch holds this lock the RPC
    /// handler waits for it before deciding whether to mark Timeout —
    /// after acquiring the lock the RPC handler sees the definitive
    /// final status (`Success` / `Failed` / `Waiting`) and either
    /// picks up the receipt from the responder channel or transitions
    /// the entry to `Timeout`. Held only across the "point of no
    /// return" (from just before `apply_fn` to just after
    /// `resp.send(receipt)` in dispatch). Never held while acquiring
    /// `PreconfTxSet::inner` — that direction would deadlock.
    pub apply_lock: Arc<Mutex<()>>,
}

impl TxEntry {
    /// Snapshot clone for read-only queries — drops the responder.
    ///
    /// Public queries (`snapshot`, `entries`, `find_*`) return this lightweight
    /// view to keep the responder strictly inside the fifo.
    pub fn snapshot_view(&self) -> TxEntryView {
        TxEntryView {
            hash: self.hash,
            tx: self.tx.clone(),
            from: self.from,
            nonce: self.nonce,
            inserted_at: self.inserted_at,
            status: self.status,
            source: self.source,
        }
    }
}

/// Read-only view of a `TxEntry` — see [`TxEntry::snapshot_view`].
#[derive(Debug, Clone)]
pub struct TxEntryView {
    /// Transaction hash.
    pub hash: TxHash,
    /// Signed transaction.
    pub tx: Arc<TxEnvelope>,
    /// Recovered sender.
    pub from: Address,
    /// Sender nonce.
    pub nonce: u64,
    /// Insertion wall-clock time.
    pub inserted_at: Instant,
    /// Current status.
    pub status: PreconfStatus,
    /// Origin of the entry — see [`PreconfSource`].
    pub source: PreconfSource,
}

/// A hash-keyed eviction callback: `PreconfTxSet` fires these outward at
/// removal / terminal-transition time so it never has to hold a reference to
/// the pool or the classifier (which would close a dependency cycle).
type EvictFn = Arc<dyn Fn(TxHash) + Send + Sync>;

/// Inner state guarded by a single `Mutex` — see module docs.
struct PreconfTxSetInner {
    /// FIFO insertion order — hashes only. Steady-state size is bounded
    /// by `forward` cleanup on each canon commit (~2s / block on L2);
    /// worst-case burst is bounded by pool ingestion rate.
    order: VecDeque<TxHash>,

    /// Hash → entry. All mutations + lookups go through this.
    entries: HashMap<TxHash, TxEntry>,

    /// (sender, nonce) → hash index for `find_by_sender_nonce`
    /// (`PreconfAwareValidator` replacement check).
    by_sender: HashMap<(Address, u64), TxHash>,

    /// RPC handler may attach responder before the listener / pool path
    /// creates the entry. We stash responders here until the matching
    /// `push_if_absent` consumes them. The `Instant` records the moment
    /// the RPC handler received the client submission — carried into
    /// `TxEntry.inserted_at` on push so the pre-apply deadline gate
    /// measures against the client-visible clock rather than the (often
    /// several-ms-later) pool-listener drain time.
    pending_responders:
        HashMap<TxHash, (Instant, oneshot::Sender<Result<PreconfReceipt, PreconfError>>)>,

    /// Verdict-cache eviction callback, fired from [`Self::drop_hash`].
    ///
    /// The **same** `OnceLock` as [`PreconfTxSet::verdict_evict`] — held here
    /// too because `drop_hash` is a method on the inner type and cannot reach
    /// the outer one. Sharing the cell (rather than copying the closure) keeps
    /// registration a single lock-free `set` on the outer handle.
    verdict_evict: Arc<OnceLock<EvictFn>>,
}

impl PreconfTxSetInner {
    fn new(verdict_evict: Arc<OnceLock<EvictFn>>) -> Self {
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
            by_sender: HashMap::new(),
            pending_responders: HashMap::new(),
            verdict_evict,
        }
    }

    /// Removes a hash from all indices (`entries` / `by_sender` / `order` /
    /// `pending_responders`). Returns the evicted entry if one existed.
    ///
    /// Fast path uses `entry.from + entry.nonce` to key into `by_sender`
    /// directly. Slow path (below) is a defensive fallback: no known caller
    /// path should ever hit it — all normal eviction routes populate
    /// `entries[hash]` before calling `drop_hash`. It exists purely to
    /// recover from unexpected torn state (e.g. a future bug that partially
    /// evicts an entry) so the "clean all indices" contract stays honest.
    fn drop_hash(&mut self, hash: &TxHash) -> Option<TxEntry> {
        let entry = self.entries.remove(hash);
        if let Some(ref e) = entry {
            self.by_sender.remove(&(e.from, e.nonce));
        } else {
            // Slow path — unreachable in nominal operation; only fires when
            // `entries[hash]` was already gone (defensive self-heal). O(n)
            // linear scan; acceptable because it should never run in prod.
            self.by_sender.retain(|_, v| v != hash);
        }
        if let Some(pos) = self.order.iter().position(|h| h == hash) {
            self.order.remove(pos);
        }
        self.pending_responders.remove(hash);

        // The frozen verdict goes with the entry: it exists to stop the pool arm
        // grabbing a tx that still has a live commitment, and on most removal
        // paths there no longer is one.
        //
        // `forward` is a known gap — its predicate is "the sender's nonce moved
        // past this entry", which neither establishes that *this* tx landed nor
        // that the landing is irrevocable, so the commitment can still be live
        // here. Do not read this callback as proof that it is over.
        //
        // Runs under the inner mutex, so the callback must be cheap,
        // non-blocking, and must never re-enter the fifo.
        if let Some(f) = self.verdict_evict.get() {
            f(*hash);
        }
        entry
    }
}

/// The commitment truth source. Constructed once at startup and shared via `Arc`.
pub struct PreconfTxSet {
    inner: Mutex<PreconfTxSetInner>,
    notifier: broadcast::Sender<TxHash>,
    /// Pool eviction callback for **non-on-chain terminal transitions**.
    /// Invoked automatically after any successful
    /// `mark_timeout` / `mark_canceled` / `mark_failed` — the tx is
    /// evicted from the transaction pool synchronously so it cannot
    /// later land via the normal pool iterator path (would violate the
    /// SLA "client saw failure ⇒ tx never on chain" contract).
    ///
    /// Registered once at startup by
    /// [`crate::PreconfServiceBuilder::start`] via
    /// [`Self::set_pool_eviction_callback`]. `OnceLock` for lock-free
    /// reads on the hot path; first registration wins (idempotent for
    /// duplicate `start` calls).
    ///
    /// `None` at test / pass-through paths — mark_* transitions still
    /// succeed, they just don't touch the pool.
    pool_evict: OnceLock<EvictFn>,

    /// Verdict-cache eviction callback — see
    /// [`Self::set_verdict_eviction_callback`]. The same cell is held by
    /// `PreconfTxSetInner`, which is what actually fires it (from
    /// `drop_hash`).
    verdict_evict: Arc<OnceLock<EvictFn>>,
}

impl PreconfTxSet {
    /// Constructor — `broadcast_cap` should come from `cfg.broadcast_cap`.
    ///
    /// Panics if `broadcast_cap == 0` (tokio invariant). Configs are
    /// validated upstream via `PreconfConfig::validate`.
    pub fn new(broadcast_cap: usize) -> Self {
        let (notifier, _) = broadcast::channel(broadcast_cap);
        // Register the gauge at 0 so it has a baseline from startup.
        metrics::gauge!("preconf.fifo.pending").set(0.0);
        let verdict_evict = Arc::new(OnceLock::new());
        Self {
            inner: Mutex::new(PreconfTxSetInner::new(verdict_evict.clone())),
            notifier,
            pool_evict: OnceLock::new(),
            verdict_evict,
        }
    }

    /// Sample the current `Waiting` backlog into the `preconf.fifo.pending`
    /// gauge. Called once per payload build job (~per slot) rather than at
    /// every fifo mutation — the gauge is a sampled quantity, so slot-level
    /// granularity is enough and keeps the mutation paths free of the scan.
    pub async fn publish_pending_gauge(&self) {
        let inner = self.inner.lock().await;
        let pending = inner.entries.values().filter(|e| e.status == PreconfStatus::Waiting).count();
        metrics::gauge!("preconf.fifo.pending").set(pending as f64);
    }

    /// Register the pool-eviction callback fired after any transition
    /// to a non-on-chain terminal state. Called once by
    /// [`crate::PreconfServiceBuilder::start`] with a closure that
    /// forwards to `RestorePool::remove_transactions`.
    ///
    /// Idempotent: `OnceLock::set` silently drops subsequent calls
    /// (first registration wins). Test / pass-through path may leave
    /// it unregistered — mark_* transitions are still functional,
    /// they just don't touch the pool.
    pub fn set_pool_eviction_callback(&self, f: EvictFn) {
        let _ = self.pool_evict.set(f);
    }

    /// Register the verdict-cache eviction callback fired from `drop_hash`,
    /// i.e. on **every** fifo removal path. Called once by
    /// [`crate::PreconfServiceBuilder::start`] with a closure forwarding to
    /// `PreconfClassifier::release_unless_committed`.
    ///
    /// Direction matters: the fifo pushes removals *out* and never holds the
    /// classifier. Neither type references the other.
    ///
    /// Idempotent (`OnceLock::set`, first registration wins). Leaving it
    /// unregistered is valid — removals then don't touch the verdict cache,
    /// which is what test / pass-through paths want.
    pub fn set_verdict_eviction_callback(&self, f: EvictFn) {
        let _ = self.verdict_evict.set(f);
    }

    /// Invoke the pool-eviction callback if registered. Private —
    /// called from within the mark_* methods after a successful
    /// `Waiting → terminal` CAS.
    fn evict_from_pool(&self, hash: TxHash) {
        if let Some(f) = self.pool_evict.get() {
            f(hash);
        }
    }

    // ============ Push path ============

    /// Idempotent push.
    ///
    /// `from` must be the recovered sender; callers (pool listener / RPC
    /// handler) have it pre-validated. Hash + nonce are read from `tx`.
    ///
    /// Returns:
    /// - [`PushResult::Inserted`] — new entry created and broadcast notified.
    /// - [`PushResult::AlreadyExists`] — same hash already present and
    ///   [`PreconfStatus::is_active`]; a no-op.
    /// - [`PushResult::Revived`] — same hash, [`PreconfStatus::is_revivable_by_same_hash`], flipped
    ///   back to `Waiting` and broadcast.
    /// - [`PushResult::ConflictActive`] — same `(from, nonce)`, different hash, incumbent not
    ///   [`PreconfStatus::is_replaceable`]. Carries the incumbent's hash.
    ///
    /// A replaceable incumbent is evicted and the new tx inserted in its place.
    pub async fn push_if_absent(
        &self,
        tx: Arc<TxEnvelope>,
        from: Address,
        source: PreconfSource,
    ) -> PushResult {
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();

        let mut inner = self.inner.lock().await;

        // Same-hash entry already present: revivable → flip back to `Waiting`
        // and broadcast, making it a live dispatch candidate again; active →
        // idempotent no-op, which the RPC handler surfaces as
        // `AlreadyInProgress`. See [`crate::types::PreconfStatus`] for why
        // reviving the same hash is always safe.
        if let Some(existing) = inner.entries.get_mut(&hash) {
            if existing.status.is_revivable_by_same_hash() {
                // `attach_responder`'s reclaimable-state branch already installed
                // the fresh responder and refreshed `inserted_at`, so dispatch's
                // deadline gate measures against this resubmit; only the status
                // flip is left.
                existing.status = PreconfStatus::Waiting;
                drop(inner);
                let _ = self.notifier.send(hash);
                return PushResult::Revived;
            }
            return PushResult::AlreadyExists;
        }

        // Replacement check: same `(sender, nonce)`, different hash. Only
        // [`PreconfStatus::is_replaceable`] states release the slot; `Waiting` /
        // `Success` block it, since `Success` is on chain or in-flight and
        // replacing it would double-apply. An abandoned commitment does not
        // block it — see [`crate::types::PreconfStatus`].
        if let Some(existing_hash) = inner.by_sender.get(&(from, nonce)).copied() {
            let existing_status = inner.entries.get(&existing_hash).map(|e| e.status);
            match existing_status {
                Some(s) if !s.is_replaceable() => {
                    return PushResult::ConflictActive(existing_hash);
                }
                Some(_) => {
                    // Replaceable — evict, then fall through to insert.
                    inner.drop_hash(&existing_hash);
                }
                None => {
                    // Invariant violation: `by_sender[(from, nonce)]` points
                    // to a hash with no matching entry. Self-heal by clearing
                    // the by_sender slot (needed so the new insert can claim
                    // it) AND sweeping any lingering `order` /
                    // `pending_responders` references via `drop_hash`
                    // (drop_hash alone would skip by_sender because the
                    // entry is already gone). `debug_assert` ensures CI /
                    // unit tests catch this — production keeps running
                    // rather than tearing down the sequencer.
                    error!(
                        target: "mantle::preconf",
                        sender = ?from,
                        nonce,
                        dangling_hash = ?existing_hash,
                        "preconf_tx_set: dangling by_sender index detected; self-healing"
                    );
                    debug_assert!(
                        false,
                        "by_sender[({from:?}, {nonce})] -> {existing_hash:?} but entry missing"
                    );
                    inner.by_sender.remove(&(from, nonce));
                    inner.drop_hash(&existing_hash);
                }
            }
        }

        // If the RPC handler pre-registered a responder, carry the
        // origin-instant recorded at that time into the entry so the
        // deadline gate ticks from the client's clock. Non-RPC paths
        // (listener-only push, journal replay) fall back to push time —
        // Replay-source entries bypass the gate entirely, and
        // listener-only entries have no client SLA to protect.
        let (responder, inserted_at) = match inner.pending_responders.remove(&hash) {
            Some((origin_instant, resp)) => (Some(resp), origin_instant),
            None => (None, Instant::now()),
        };
        let entry = TxEntry {
            hash,
            tx,
            from,
            nonce,
            inserted_at,
            status: PreconfStatus::Waiting,
            source,
            responder,
            apply_lock: Arc::new(Mutex::new(())),
        };
        inner.entries.insert(hash, entry);
        inner.by_sender.insert((from, nonce), hash);
        inner.order.push_back(hash);
        drop(inner);

        let _ = self.notifier.send(hash);

        PushResult::Inserted
    }

    /// Removes `hash` only if safe to evict: a reclaimable terminal state
    /// (`Timeout` / `Canceled` / `Failed`) and not mid-apply. Returns true iff
    /// removed. Status check and removal share one `inner` lock, so a
    /// concurrent `Timeout → Waiting` revival can't be evicted; the `try_lock`
    /// on `apply_lock` (non-blocking — a blocking acquire under `inner` would
    /// invert the `inner → apply_lock` order and deadlock) skips an entry
    /// dispatch is still finalizing, so its receipt is never stranded.
    /// Idempotent; the only public eviction path (unconditional removal stays
    /// internal to `drop_hash`).
    pub async fn remove_reclaimable(&self, hash: &TxHash) -> bool {
        let mut inner = self.inner.lock().await;
        let safe_to_drop = match inner.entries.get(hash) {
            Some(e) => {
                matches!(
                    e.status,
                    PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed
                ) && e.apply_lock.try_lock().is_ok()
            }
            None => false,
        };
        if safe_to_drop { inner.drop_hash(hash).is_some() } else { false }
    }

    /// Returns true if the hash is currently present in `entries`.
    pub async fn contains(&self, hash: &TxHash) -> bool {
        self.inner.lock().await.entries.contains_key(hash)
    }

    /// Snapshot of hashes in FIFO order. Used by the payload builder when
    /// it starts a new job to replay any pending commitments accumulated
    /// since the previous block.
    pub async fn snapshot(&self) -> Vec<TxHash> {
        self.inner.lock().await.order.iter().copied().collect()
    }

    /// Snapshot of `TxEntryView` in FIFO order — drops responders.
    pub async fn entries(&self) -> Vec<TxEntryView> {
        let inner = self.inner.lock().await;
        inner
            .order
            .iter()
            .filter_map(|h| inner.entries.get(h).map(TxEntry::snapshot_view))
            .collect()
    }

    /// `PreconfAwareValidator` replacement-check lookup — O(1) via `by_sender`.
    pub async fn find_by_sender_nonce(&self, addr: &Address, nonce: u64) -> Option<TxEntryView> {
        let inner = self.inner.lock().await;
        let hash = inner.by_sender.get(&(*addr, nonce))?;
        inner.entries.get(hash).map(TxEntry::snapshot_view)
    }

    /// Look up an entry by hash.
    pub async fn find_by_hash(&self, hash: &TxHash) -> Option<TxEntryView> {
        let inner = self.inner.lock().await;
        inner.entries.get(hash).map(TxEntry::snapshot_view)
    }

    /// Acquire the per-entry `apply_lock` — held across `apply_fn +
    /// mark_* + send(receipt)` in dispatch, and acquired by the RPC
    /// deadline branch to serialize with dispatch's "point of no
    /// return". Returns `None` if no entry with `hash` exists.
    ///
    /// Implementation: clones the entry's `Arc<Mutex<()>>` under
    /// `inner`, then drops `inner` before calling `.lock_owned().await`
    /// so that waiters do not hold `inner`. Lock ordering must remain
    /// `apply_lock → inner` — never the reverse.
    pub async fn lock_for_apply(&self, hash: &TxHash) -> Option<OwnedMutexGuard<()>> {
        let lock_arc = {
            let inner = self.inner.lock().await;
            inner.entries.get(hash)?.apply_lock.clone()
        };
        Some(lock_arc.lock_owned().await)
    }

    /// Drops entries with `from == addr && nonce < new_nonce`.
    ///
    /// Sole production caller is the `PayloadJob` prologue
    /// (`builder::payload_builder`'s `sync_fifo_forward_to_head`), which reads
    /// each sender's nonce from the parent-block state. It used to run in
    /// `canon_handler`; that sweep was moved because it raced new payload jobs.
    ///
    /// The predicate is "this sender's nonce has moved past the entry", which
    /// is **not** the same as "this entry's tx landed" — a different tx taking
    /// the nonce drops the entry just the same. Callers that need to know
    /// *which* tx advanced the nonce cannot get it from here.
    pub async fn forward(&self, addr: &Address, new_nonce: u64) {
        let mut inner = self.inner.lock().await;
        let to_drop: Vec<TxHash> = inner
            .by_sender
            .iter()
            .filter(|((a, n), _)| a == addr && *n < new_nonce)
            .map(|(_, h)| *h)
            .collect();
        for h in to_drop {
            inner.drop_hash(&h);
        }
    }

    /// Evicts every entry in a [`PreconfStatus::is_replaceable`] state, returning
    /// the evicted hashes. Broader than op-geth's `FIFOTxSet::CleanTimeout`,
    /// which only clears the timeout case: this fifo splits "not on chain" into
    /// three states and all must be swept, or a stale entry pins its
    /// `(sender, nonce)` forever. An abandoned commitment is swept like the rest
    /// — see [`crate::types::PreconfStatus`].
    pub async fn clean_reclaimable(&self) -> Vec<TxHash> {
        let mut inner = self.inner.lock().await;
        let to_drop: Vec<TxHash> = inner
            .entries
            .iter()
            .filter(|(_, e)| e.status.is_replaceable())
            .map(|(h, _)| *h)
            .collect();
        for h in &to_drop {
            inner.drop_hash(h);
        }
        to_drop
    }

    /// Evict `pending_responders` slots older than `max_age`; returns the
    /// count dropped.
    ///
    /// Backstop for an orphaned responder: if the tx never reaches
    /// `SubPool::Pending` (so the Pending-only listener never pushes) **and**
    /// the RPC future is cancelled before Step 5's cleanup runs, the responder
    /// has no other GC path — `drop_hash` only runs for `entries`-backed
    /// hashes, never a lone pending responder. Past `max_age` (well beyond
    /// `preconf_timeout`) it can't deliver anything useful, so dropping its
    /// `oneshot::Sender` is safe (a live receiver just sees `RecvError`).
    pub async fn expire_pending_responders(&self, max_age: Duration) -> usize {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        let expired: Vec<TxHash> = inner
            .pending_responders
            .iter()
            .filter(|(_, (origin, _))| now.saturating_duration_since(*origin) > max_age)
            .map(|(hash, _)| *hash)
            .collect();
        for hash in &expired {
            inner.pending_responders.remove(hash);
        }
        if !expired.is_empty() {
            metrics::counter!("preconf.pending_responders.expired_total")
                .increment(expired.len() as u64);
        }
        expired.len()
    }

    /// Builder subscribes the broadcast notifier here.
    ///
    /// Each call returns an independent `Receiver` — multi-consumer.
    pub fn subscribe(&self) -> broadcast::Receiver<TxHash> {
        self.notifier.subscribe()
    }

    // ============ Status transitions ============

    /// `Waiting → Success`. Called by the builder after a successful EVM apply.
    ///
    /// Terminal for the build that set it — no `mark_*` moves it again, and
    /// [`Self::forward`] is the only path that drops it (a `Success` entry is
    /// neither replaceable nor reclaimable). The one way out is
    /// [`Self::reset_success_to_waiting`], which the *next* payload job's
    /// carryover preamble uses on a `Success` entry that outlived the block it
    /// was applied to (`replay_fifo_carryover` in `payload_builder`): the client
    /// already holds a receipt, so the commitment still has to land, in a block
    /// that will actually commit.
    pub async fn mark_succeeded(&self, hash: &TxHash) -> Result<(), MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Waiting {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        entry.status = PreconfStatus::Success;
        Ok(())
    }

    /// `Waiting → Failed`. **Soft terminal** — revivable via same-hash resubmit,
    /// and its `(sender, nonce)` is released; see
    /// [`crate::types::PreconfStatus`], which also covers how far "not on chain"
    /// reaches. Called by the builder when `apply_fn` returned Err (in-flight
    /// nonce / balance race, block gas exhausted at builder level). Reclaimable
    /// because all three causes are typically transient, and SDKs retry them
    /// alike.
    ///
    /// Both sources land here; only the reporting differs — a `Replay` entry is a
    /// breach, logged `error!` and counted by dispatch's breach arm.
    ///
    /// On success, invokes the pool-eviction callback (if registered) to
    /// synchronously remove `hash` from the transaction pool, closing the SLA
    /// window where the client saw `Failed` but the tx could still land later
    /// via the pool best-tx iterator.
    pub async fn mark_failed(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Failed).await?;
        self.evict_from_pool(*hash);
        Ok(())
    }

    /// `Waiting → Timeout`. **Soft terminal**, revivable by a same-hash retry
    /// (`attach_responder` refreshes `inserted_at`, then
    /// [`Self::push_if_absent`] flips the entry back to `Waiting`). Called by
    /// the RPC handler when the client-side `preconf_timeout` fires before a
    /// receipt is delivered, or by dispatch's pre-apply deadline gate.
    ///
    /// Same pool-eviction hook as `mark_failed` / `mark_canceled`.
    pub async fn mark_timeout(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Timeout).await?;
        self.evict_from_pool(*hash);
        Ok(())
    }

    /// `Waiting → Canceled`. **Soft terminal** — like `Timeout`, revivable
    /// via same-hash retry through `attach_responder` +
    /// [`Self::push_if_absent`]. Signals **server pre-apply
    /// rejection** (block gas budget exhausted, admin action, ...): the
    /// EVM was never run, so the tx is not on chain at this point, and the
    /// pool-eviction hook below is what keeps it off — see
    /// [`crate::types::PreconfStatus`] for how far that reaches.
    /// Semantically distinct from `Timeout` (client's deadline hit).
    ///
    /// Same pool-eviction hook as `mark_failed` / `mark_timeout`.
    pub async fn mark_canceled(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Canceled).await?;
        self.evict_from_pool(*hash);
        Ok(())
    }

    /// Shared CAS body: only allows the `Waiting → target` transition.
    /// Any other source status returns `IllegalTransition(current)`.
    async fn transition_from_waiting(
        &self,
        hash: &TxHash,
        target: PreconfStatus,
    ) -> Result<(), MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Waiting {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        entry.status = target;
        Ok(())
    }

    /// `Success → Waiting` — for stale-in-flight replay.
    ///
    /// A `Success` entry that still exists in the fifo means "applied
    /// to some in-flight builder, but that builder's block never made
    /// it to canon" — `forward()` would have dropped the entry
    /// entirely on canon commit. On a new payload job start, such
    /// entries represent commitments the client was already promised;
    /// the mantle preconf SLA ("receipt returned → tx must land on
    /// chain") requires re-applying them to the new build.
    ///
    /// This API performs two coupled changes atomically under the
    /// fifo lock:
    ///
    /// 1. `status: Success → Waiting` so the entry becomes eligible for re-apply.
    /// 2. `source: * → Replay` so `builder::dispatch`'s pre-apply deadline and per-block gas budget
    ///    gates bypass it.
    ///
    /// A broadcast notify is still fired for callers that prefer a
    /// broadcast-driven pickup path. The `build_payload` preamble does
    /// NOT rely on the broadcast: it drives apply directly ahead of
    /// the select! loop so the stale entries land before any
    /// concurrently-pushed fresh RPC entries.
    ///
    /// Any other status returns `IllegalTransition(current)`; a
    /// missing entry returns `NotFound`.
    pub async fn reset_success_to_waiting(&self, hash: &TxHash) -> Result<(), MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Success {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        entry.status = PreconfStatus::Waiting;
        entry.source = PreconfSource::Replay;
        drop(inner);

        // The only signal this path emits, and it has to exist: a commitment that
        // applies cleanly every round never fails, so it can loop indefinitely
        // without touching `preconf.tx.commitment_broken_total` while each round
        // bumps `preconf.tx.success_total` — a stuck commitment reading as
        // throughput. Steady state is ~0 (a canonical block advances the nonce
        // and `forward` drops the entry first); a rising rate means blocks are
        // built and not adopted, so pair it with
        // `preconf.build.watchdog_cancel_total`.
        metrics::counter!("preconf.tx.replay_round_total").increment(1);

        let _ = self.notifier.send(*hash);
        Ok(())
    }

    // ============ Responder slots (RPC path only) ============

    /// Attaches responder. If a matching entry already exists, the responder
    /// is parked inside the entry; otherwise it goes into `pending_responders`
    /// and gets merged at the matching `push_if_absent`.
    ///
    /// `origin_instant` is the moment the RPC handler received the client
    /// submission. When the responder gets merged into a pending entry via
    /// `push_if_absent`, this instant becomes `TxEntry.inserted_at` so the
    /// pre-apply deadline gate in `dispatch` measures against the client's
    /// clock rather than the pool-listener drain time. Passing
    /// `Instant::now()` at the call site is fine for RPC callers that
    /// want to include only downstream latency in the deadline budget.
    ///
    /// Returns `AlreadyAttached` if any responder slot for this hash is
    /// already occupied.
    pub async fn attach_responder(
        &self,
        hash: TxHash,
        origin_instant: Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<(), AttachError> {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.entries.get_mut(&hash) {
            match entry.status {
                // A prior client already resolved on this hash — the
                // apply succeeded and the receipt was delivered via
                // `mark_succeeded` + `take_responder`. Any second
                // submission would have nothing new to await, so
                // surface as `AlreadyInProgress` at the caller.
                PreconfStatus::Success => {
                    return Err(AttachError::AlreadyAttached);
                }
                // Waiting — the entry is live. Allow attach only when
                // no responder is currently registered (fresh listener-
                // only push, or the RPC handler that owns the slot has
                // taken its responder). If a responder is present, a
                // client is actively waiting and we must not overwrite
                // its `oneshot::Sender`.
                PreconfStatus::Waiting => {
                    if entry.responder.is_some() {
                        return Err(AttachError::AlreadyAttached);
                    }
                    entry.responder = Some(responder);
                    return Ok(());
                }
                // Same-hash retry after a reclaimable terminal state. Install the
                // fresh responder and refresh `inserted_at` so
                // `builder::dispatch`'s deadline gate measures against this
                // submission rather than the already-expired first; the
                // subsequent `push_if_absent` flips the entry back to `Waiting`.
                //
                // Must stay in sync with
                // [`PreconfStatus::is_revivable_by_same_hash`].
                PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed => {
                    entry.responder = Some(responder);
                    entry.inserted_at = origin_instant;
                    return Ok(());
                }
            }
        }
        if inner.pending_responders.contains_key(&hash) {
            return Err(AttachError::AlreadyAttached);
        }
        inner.pending_responders.insert(hash, (origin_instant, responder));
        Ok(())
    }

    /// Cancels the responder for `hash` (if any) with the given error.
    /// No-op if no responder is registered. The send is fire-and-forget —
    /// the receiver may have already dropped (client timed out).
    ///
    /// Belt-and-braces cleanup: after taking from the primary slot, an
    /// unconditional `pending_responders.remove(hash)` runs. In the normal
    /// case (invariant #2 holds) this is a no-op. If invariant #2 is ever
    /// violated (both slots occupied for the same hash), the ghost
    /// responder in `pending_responders` is dropped rather than leaked as
    /// a zombie — its client will observe `RecvError` instead of a stuck
    /// oneshot. Minimal-cost defense; no logging or `debug_assert` because
    /// the primary slot's caller already saw a Some(responder) result.
    pub async fn cancel_responder(&self, hash: &TxHash, err: PreconfError) {
        let responder = {
            let mut inner = self.inner.lock().await;
            let r = inner
                .entries
                .get_mut(hash)
                .and_then(|e| e.responder.take())
                .or_else(|| inner.pending_responders.remove(hash).map(|(_, r)| r));
            // Drop any ghost pending responder (invariant #2 violation);
            // no-op in the normal case.
            inner.pending_responders.remove(hash);
            r
        };
        if let Some(r) = responder {
            let _ = r.send(Err(err));
        }
    }

    /// Take-once: removes and returns the responder if any. Called by the
    /// builder after a successful apply, to deliver the receipt.
    ///
    /// Belt-and-braces cleanup (symmetric to `cancel_responder`): after
    /// selecting from the primary slot, an unconditional
    /// `pending_responders.remove(hash)` drops any ghost that would
    /// otherwise leak under invariant #2 violation. The caller sends
    /// Ok(receipt) via the returned Sender; the ghost's receiver
    /// observes `RecvError`.
    pub async fn take_responder(
        &self,
        hash: &TxHash,
    ) -> Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>> {
        let mut inner = self.inner.lock().await;
        let r = inner
            .entries
            .get_mut(hash)
            .and_then(|e| e.responder.take())
            .or_else(|| inner.pending_responders.remove(hash).map(|(_, r)| r));
        // Drop any ghost pending responder (invariant #2 violation);
        // no-op in the normal case.
        inner.pending_responders.remove(hash);
        r
    }

    // ============ Builder-only tx access ============

    /// Returns a clone of the tx `Arc` for an entry; `None` if absent.
    pub async fn get_tx(&self, hash: &TxHash) -> Option<Arc<TxEnvelope>> {
        self.inner.lock().await.entries.get(hash).map(|e| e.tx.clone())
    }
}

// `Debug` impl avoids exposing the responder (oneshot is not Debug-friendly)
// and the broadcast Sender's internals.
impl std::fmt::Debug for PreconfTxSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfTxSet")
            .field("receiver_count", &self.notifier.receiver_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{B256, Signature};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    /// Build a synthetic `TxEnvelope` with a caller-chosen hash and nonce.
    /// The signature is a fixed dummy; we only exercise the fifo state
    /// machine, not signature recovery.
    fn make_tx(nonce: u64, hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, ..Default::default() };
        let sig = Signature::test_signature();
        let hash = B256::from([hash_byte; 32]);
        Arc::new(TxEnvelope::Eip1559(Signed::new_unchecked(inner, sig, hash)))
    }

    /// Every fifo removal path must drop the frozen verdict, because
    /// `drop_hash` is the single point they all converge on. Asserted through
    /// two different public entry points so the hook is proven to sit at that
    /// convergence point rather than on one route.
    #[tokio::test]
    async fn removal_paths_fire_the_verdict_eviction_callback() {
        let set = PreconfTxSet::new(16);
        let seen: Arc<std::sync::Mutex<Vec<TxHash>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        set.set_verdict_eviction_callback(Arc::new(move |hash| sink.lock().unwrap().push(hash)));

        // Route 1 — explicit removal. `remove_reclaimable` is the only explicit
        // removal the fifo exposes, and it drops an entry solely in a
        // reclaimable state, so flip it to `Timeout` first. That CAS does not go
        // through `drop_hash`, so it fires no verdict eviction of its own — the
        // expected sequence below still names each hash exactly once.
        set.push_if_absent(make_tx(0, 1), addr(1), PreconfSource::Rpc).await;
        assert!(set.mark_timeout(&h(1)).await.is_ok());
        assert!(set.remove_reclaimable(&h(1)).await);

        // Route 2 — replacement inside `push_if_absent`: same (sender, nonce),
        // different hash, incumbent in a reclaimable state.
        set.push_if_absent(make_tx(7, 2), addr(2), PreconfSource::Rpc).await;
        assert!(set.mark_timeout(&h(2)).await.is_ok());
        set.push_if_absent(make_tx(7, 3), addr(2), PreconfSource::Rpc).await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec![h(1), h(2)],
            "both removal routes must evict, and only the removed hashes",
        );
    }

    /// Leaving the callback unregistered must stay valid — that is the test /
    /// pass-through path, and `drop_hash` runs on every removal.
    #[tokio::test]
    async fn removal_without_verdict_callback_is_a_noop() {
        let set = PreconfTxSet::new(16);
        set.push_if_absent(make_tx(0, 1), addr(1), PreconfSource::Rpc).await;
        assert!(set.mark_timeout(&h(1)).await.is_ok());
        assert!(set.remove_reclaimable(&h(1)).await);
        assert!(!set.contains(&h(1)).await);
    }

    #[tokio::test]
    async fn empty_set_contains_nothing() {
        let set = PreconfTxSet::new(16);
        assert!(!set.contains(&h(1)).await);
        assert!(set.snapshot().await.is_empty());
        assert!(set.entries().await.is_empty());
        assert!(set.find_by_hash(&h(1)).await.is_none());
    }

    #[tokio::test]
    async fn subscribe_returns_independent_receivers() {
        let set = PreconfTxSet::new(16);
        let _rx1 = set.subscribe();
        let _rx2 = set.subscribe();
        assert!(format!("{set:?}").contains("receiver_count"));
    }

    #[tokio::test]
    async fn remove_reclaimable_returns_false_when_absent() {
        let set = PreconfTxSet::new(16);
        assert!(!set.remove_reclaimable(&h(1)).await);
    }

    #[tokio::test]
    async fn remove_reclaimable_refuses_waiting_entry() {
        // A `Waiting` entry (an in-apply entry's state) must never be evicted.
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        assert!(!set.remove_reclaimable(tx.tx_hash()).await, "Waiting must not be removed");
        assert!(set.contains(tx.tx_hash()).await, "entry must survive");
    }

    #[tokio::test]
    async fn remove_reclaimable_removes_terminal_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_timeout(tx.tx_hash()).await.unwrap();
        assert!(set.remove_reclaimable(tx.tx_hash()).await, "Timeout is reclaimable");
        assert!(!set.contains(tx.tx_hash()).await);
    }

    #[tokio::test]
    async fn remove_reclaimable_refuses_revived_entry() {
        // Same-hash resubmit revives `Timeout` → `Waiting`; the atomic status
        // re-read must catch the flip and decline (else a landing tx's
        // receipt is stranded).
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_timeout(tx.tx_hash()).await.unwrap();
        assert_eq!(
            set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await,
            PushResult::Revived
        );
        assert!(!set.remove_reclaimable(tx.tx_hash()).await, "revived Waiting must not be removed");
        assert!(set.contains(tx.tx_hash()).await);
    }

    #[tokio::test]
    async fn remove_reclaimable_declines_while_apply_lock_held() {
        // Terminal status but dispatch still holds `apply_lock` through
        // `take_responder`: `try_lock` must decline, not snatch the responder.
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_failed(tx.tx_hash()).await.unwrap();

        let guard = set.lock_for_apply(tx.tx_hash()).await.expect("entry present");
        assert!(
            !set.remove_reclaimable(tx.tx_hash()).await,
            "must decline while apply_lock is held"
        );
        assert!(set.contains(tx.tx_hash()).await);

        drop(guard);
        assert!(set.remove_reclaimable(tx.tx_hash()).await, "removable once lock released");
        assert!(!set.contains(tx.tx_hash()).await);
    }

    // ============ push_if_absent ============

    #[tokio::test]
    async fn push_inserts_new_entry_and_broadcasts() {
        let set = PreconfTxSet::new(16);
        let mut rx = set.subscribe();
        let tx = make_tx(0, 1);
        let result = set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        assert_eq!(result, PushResult::Inserted);

        assert!(set.contains(tx.tx_hash()).await);
        assert_eq!(set.snapshot().await, vec![*tx.tx_hash()]);
        assert!(set.find_by_sender_nonce(&addr(1), 0).await.is_some());
        assert_eq!(rx.try_recv().unwrap(), *tx.tx_hash());
    }

    #[tokio::test]
    async fn push_same_hash_returns_already_exists() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        assert_eq!(
            set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await,
            PushResult::Inserted
        );
        assert_eq!(
            set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await,
            PushResult::AlreadyExists
        );
        // Single entry — no duplicates.
        assert_eq!(set.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn push_conflict_active_blocks_replacement() {
        let set = PreconfTxSet::new(16);
        let tx1 = make_tx(0, 1);
        let tx2 = make_tx(0, 2); // same nonce, different hash
        assert_eq!(
            set.push_if_absent(tx1.clone(), addr(1), PreconfSource::Rpc).await,
            PushResult::Inserted
        );
        assert_eq!(
            set.push_if_absent(tx2.clone(), addr(1), PreconfSource::Rpc).await,
            PushResult::ConflictActive(*tx1.tx_hash())
        );
        // tx2 not inserted.
        assert!(!set.contains(tx2.tx_hash()).await);
    }

    #[tokio::test]
    async fn push_conflict_after_timeout_evicts_and_inserts() {
        let set = PreconfTxSet::new(16);
        let tx1 = make_tx(0, 1);
        let tx2 = make_tx(0, 2);
        set.push_if_absent(tx1.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_timeout(tx1.tx_hash()).await.unwrap();

        let r = set.push_if_absent(tx2.clone(), addr(1), PreconfSource::Rpc).await;
        assert_eq!(r, PushResult::Inserted);
        assert!(!set.contains(tx1.tx_hash()).await);
        assert!(set.contains(tx2.tx_hash()).await);
    }

    /// Symmetric to `push_conflict_after_timeout_evicts_and_inserts`:
    /// once the sitting entry has been `mark_failed`-ed (reth builder
    /// pre-execute reject; tx NOT on chain), a different-hash tx for
    /// the same (sender, nonce) must be admissible. Locks the
    /// "Failed is reclaimable" replacement branch.
    #[tokio::test]
    async fn push_conflict_after_failed_evicts_and_inserts() {
        let set = PreconfTxSet::new(16);
        let tx1 = make_tx(0, 1);
        let tx2 = make_tx(0, 2);
        set.push_if_absent(tx1.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_failed(tx1.tx_hash()).await.unwrap();

        let r = set.push_if_absent(tx2.clone(), addr(1), PreconfSource::Rpc).await;
        assert_eq!(r, PushResult::Inserted);
        assert!(!set.contains(tx1.tx_hash()).await);
        assert!(set.contains(tx2.tx_hash()).await);
    }

    /// Same-hash resubmit after `mark_failed` revives the entry to
    /// `Waiting` (Revived branch of `push_if_absent`) and broadcasts,
    /// so dispatch picks the tx up for a fresh apply attempt.
    #[tokio::test]
    async fn push_same_hash_after_failed_revives_to_waiting() {
        let set = PreconfTxSet::new(16);
        let mut rx = set.subscribe();
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        // Drain the initial broadcast so the assertion below only sees
        // the revival notify.
        let _ = rx.try_recv();
        set.mark_failed(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Failed,);

        let r = set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        assert_eq!(r, PushResult::Revived);
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Waiting,);
        // Revive broadcasts the hash so dispatch re-picks it up.
        assert_eq!(rx.try_recv().unwrap(), *tx.tx_hash());
    }

    // ============ status transitions ============

    #[tokio::test]
    async fn mark_succeeded_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn mark_failed_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_failed(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Failed);
    }

    #[tokio::test]
    async fn mark_timeout_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_timeout(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Timeout);
    }

    /// Symmetric to `mark_timeout_from_waiting`: `Waiting → Canceled`
    /// CAS. Locks the semantic distinction from `Failed` (Canceled means
    /// server pre-apply rejection; tx will NOT be on chain).
    #[tokio::test]
    async fn mark_canceled_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_canceled(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Canceled);
    }

    #[tokio::test]
    async fn second_transition_rejects_non_waiting_source() {
        // Any subsequent mark_* after the first must hit IllegalTransition.
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();

        // mark_failed after Success → reject.
        let err = set.mark_failed(tx.tx_hash()).await.unwrap_err();
        assert_eq!(err, MarkError::IllegalTransition(PreconfStatus::Success));

        // mark_timeout after Success → also reject.
        let err = set.mark_timeout(tx.tx_hash()).await.unwrap_err();
        assert_eq!(err, MarkError::IllegalTransition(PreconfStatus::Success));
    }

    #[tokio::test]
    async fn mark_transition_returns_not_found_for_unknown_hash() {
        let set = PreconfTxSet::new(16);
        assert_eq!(set.mark_succeeded(&h(99)).await.unwrap_err(), MarkError::NotFound);
        assert_eq!(set.mark_failed(&h(99)).await.unwrap_err(), MarkError::NotFound);
        assert_eq!(set.mark_timeout(&h(99)).await.unwrap_err(), MarkError::NotFound);
    }

    // ============ reset_success_to_waiting ============

    /// Happy path: `Success → Waiting` transition, entry stays in fifo,
    /// broadcast re-fires. This is the primary mechanism by which stale
    /// in-flight commitments (applied to a dropped payload job's
    /// builder) get replayed by the next job.
    #[tokio::test]
    async fn reset_success_to_waiting_transitions_and_rebroadcasts() {
        let set = PreconfTxSet::new(16);
        let mut rx = set.subscribe();
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();
        // Drain the initial push notify so we can attribute the
        // post-reset broadcast to the reset itself.
        let _ = rx.try_recv();

        set.reset_success_to_waiting(tx.tx_hash()).await.unwrap();

        let view = set.find_by_hash(tx.tx_hash()).await.unwrap();
        // Status regressed to Waiting; entry still present.
        assert_eq!(view.status, PreconfStatus::Waiting);
        // Source promoted so dispatch gates (timeout / gas budget) bypass
        // this stale entry — the commitment was already returned to the
        // client, gates must not drop it.
        assert_eq!(view.source, PreconfSource::Replay);
        // Hash re-broadcast so the dispatch loop picks it up.
        assert_eq!(rx.try_recv().unwrap(), *tx.tx_hash());
    }

    /// Only `Success` may transition; every other status returns
    /// `IllegalTransition(current)` and the entry is untouched. Locks
    /// the CAS boundary so a future refactor doesn't accidentally allow
    /// e.g. `Failed → Waiting` (which would resurrect a builder-rejected
    /// tx with stale state).
    #[tokio::test]
    async fn reset_success_to_waiting_rejects_non_success_states() {
        for pre_status in [
            PreconfStatus::Waiting,
            PreconfStatus::Failed,
            PreconfStatus::Timeout,
            PreconfStatus::Canceled,
        ] {
            let set = PreconfTxSet::new(16);
            let tx = make_tx(0, 1);
            set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
            match pre_status {
                PreconfStatus::Waiting => {}
                PreconfStatus::Failed => set.mark_failed(tx.tx_hash()).await.unwrap(),
                PreconfStatus::Timeout => set.mark_timeout(tx.tx_hash()).await.unwrap(),
                PreconfStatus::Canceled => set.mark_canceled(tx.tx_hash()).await.unwrap(),
                _ => unreachable!(),
            }

            let err = set.reset_success_to_waiting(tx.tx_hash()).await.unwrap_err();
            assert_eq!(
                err,
                MarkError::IllegalTransition(pre_status),
                "reset must reject state {pre_status:?}",
            );
            // Entry unchanged.
            assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, pre_status);
        }
    }

    #[tokio::test]
    async fn reset_success_to_waiting_returns_not_found_for_unknown_hash() {
        let set = PreconfTxSet::new(16);
        let err = set.reset_success_to_waiting(&h(99)).await.unwrap_err();
        assert_eq!(err, MarkError::NotFound);
    }

    // ============ forward ============

    #[tokio::test]
    async fn forward_drops_older_nonces_only() {
        let set = PreconfTxSet::new(16);
        let t5 = make_tx(5, 5);
        let t6 = make_tx(6, 6);
        let t7 = make_tx(7, 7);
        let other = make_tx(5, 50); // different sender
        set.push_if_absent(t5.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(t6.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(t7.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(other.clone(), addr(2), PreconfSource::Rpc).await;

        set.forward(&addr(1), 7).await;

        assert!(!set.contains(t5.tx_hash()).await);
        assert!(!set.contains(t6.tx_hash()).await);
        assert!(set.contains(t7.tx_hash()).await);
        assert!(set.contains(other.tx_hash()).await); // unrelated sender untouched
    }

    // ============ clean_reclaimable ============

    #[tokio::test]
    async fn clean_reclaimable_evicts_timeout_canceled_and_failed_entries() {
        // 5 entries: Waiting, Success, Failed, Timeout, Canceled.
        // `clean_reclaimable` must drop the last three (Failed / Timeout
        // / Canceled — all reclaimable, all "not on chain"), keep the
        // first two (Waiting is live, Success is on-chain-or-in-flight).
        let set = PreconfTxSet::new(16);
        let t_wait = make_tx(0, 1);
        let t_ok = make_tx(1, 2);
        let t_fail = make_tx(2, 3);
        let t_to = make_tx(3, 4);
        let t_cancel = make_tx(4, 5);
        set.push_if_absent(t_wait.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(t_ok.clone(), addr(2), PreconfSource::Rpc).await;
        set.push_if_absent(t_fail.clone(), addr(3), PreconfSource::Rpc).await;
        set.push_if_absent(t_to.clone(), addr(4), PreconfSource::Rpc).await;
        set.push_if_absent(t_cancel.clone(), addr(5), PreconfSource::Rpc).await;
        set.mark_succeeded(t_ok.tx_hash()).await.unwrap();
        set.mark_failed(t_fail.tx_hash()).await.unwrap();
        set.mark_timeout(t_to.tx_hash()).await.unwrap();
        set.mark_canceled(t_cancel.tx_hash()).await.unwrap();

        let mut evicted = set.clean_reclaimable().await;
        evicted.sort();
        let mut expected = vec![*t_fail.tx_hash(), *t_to.tx_hash(), *t_cancel.tx_hash()];
        expected.sort();
        assert_eq!(evicted, expected);

        // Kept.
        assert!(set.contains(t_wait.tx_hash()).await);
        assert!(set.contains(t_ok.tx_hash()).await);
        // Evicted.
        assert!(!set.contains(t_fail.tx_hash()).await);
        assert!(!set.contains(t_to.tx_hash()).await);
        assert!(!set.contains(t_cancel.tx_hash()).await);
    }

    // ===== A broken commitment: terminal, and its slot released

    /// Drive a `Replay` entry to a breach through the public API: it lands in
    /// `Failed`, and its `Replay` source is what distinguishes it from an
    /// RPC-side rejection.
    async fn broken_commitment(set: &PreconfTxSet, nonce: u64, hash_byte: u8, sender: Address) {
        let tx = make_tx(nonce, hash_byte);
        set.push_if_absent(tx.clone(), sender, PreconfSource::Replay).await;
        set.mark_failed(tx.tx_hash()).await.unwrap();
        let e = set.find_by_hash(tx.tx_hash()).await.unwrap();
        assert_eq!(e.status, PreconfStatus::Failed);
        assert_eq!(e.source, PreconfSource::Replay, "the source is the breach marker");
        assert!(e.status.is_replaceable(), "and the nonce is released");
    }

    /// A commitment we could not honour is swept by `clean_reclaimable`, and its
    /// `(sender, nonce)` is released — see [`crate::types::PreconfStatus`].
    #[tokio::test]
    async fn clean_reclaimable_sweeps_broken_commitments() {
        let set = PreconfTxSet::new(16);
        broken_commitment(&set, 0, 1, addr(1)).await;
        // A genuinely reclaimable neighbour, to prove the sweep still runs.
        let t_to = make_tx(0, 2);
        set.push_if_absent(t_to.clone(), addr(2), PreconfSource::Rpc).await;
        set.mark_timeout(t_to.tx_hash()).await.unwrap();

        let mut evicted = set.clean_reclaimable().await;
        evicted.sort();
        let mut want = vec![h(1), *t_to.tx_hash()];
        want.sort();
        assert_eq!(evicted, want, "the broken commitment is swept alongside the Timeout");
        assert!(!set.contains(&h(1)).await, "its entry is gone");
        assert!(
            set.find_by_sender_nonce(&addr(1), 0).await.is_none(),
            "and its (sender, nonce) is free again",
        );
    }

    /// A **different** hash may take the nonce of a commitment we could not
    /// honour. That is the whole point of releasing the slot: the sender — the
    /// party the promise was made to — can move on, rather than being left with
    /// no way out but resubmitting the very transaction the EVM had just
    /// rejected.
    #[tokio::test]
    async fn a_different_hash_may_replace_a_broken_commitment() {
        let set = PreconfTxSet::new(16);
        broken_commitment(&set, 0, 1, addr(1)).await;

        // Same (sender, nonce), different hash — e.g. a fee-bumped replacement.
        let bump = make_tx(0, 2);
        assert_eq!(
            set.push_if_absent(bump, addr(1), PreconfSource::Rpc).await,
            PushResult::Inserted,
        );
        assert!(!set.contains(&h(1)).await, "the broken entry is displaced");
        assert_eq!(
            set.find_by_sender_nonce(&addr(1), 0).await.map(|e| e.hash),
            Some(h(2)),
            "and the replacement owns the slot",
        );
    }

    /// A **same-hash** resubmit still revives it: no nonce changes hands, and it
    /// gives a commitment we owe another chance to land.
    #[tokio::test]
    async fn a_same_hash_resubmit_revives_a_broken_commitment() {
        let set = PreconfTxSet::new(16);
        broken_commitment(&set, 0, 1, addr(1)).await;

        assert_eq!(
            set.push_if_absent(make_tx(0, 1), addr(1), PreconfSource::Rpc).await,
            PushResult::Revived,
        );

        let e = set.find_by_hash(&h(1)).await.unwrap();
        assert_eq!(e.status, PreconfStatus::Waiting);
    }

    /// Reachability premise of D4's *second* door (`rpc.rs`'s deadline branch):
    /// a replaying commitment sits in the fifo as `Waiting` / `Replay` with its
    /// responder already taken, and `attach_responder` therefore **accepts** a
    /// same-hash resubmit onto it. That resubmit's RPC handler is the one whose
    /// deadline must not be allowed to `mark_timeout` the commitment.
    #[tokio::test]
    async fn attach_responder_accepts_a_resubmit_onto_a_replaying_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Replay).await;

        let e = set.find_by_hash(&h(1)).await.unwrap();
        assert_eq!(e.status, PreconfStatus::Waiting);
        assert_eq!(e.source, PreconfSource::Replay);
        assert!(set.take_responder(&h(1)).await.is_none(), "replay entries carry no responder");

        let (resp_tx, _resp_rx) = oneshot::channel();
        assert!(
            set.attach_responder(h(1), Instant::now(), resp_tx).await.is_ok(),
            "a same-hash resubmit attaches to a live replaying entry — this is the door",
        );
    }

    // ============ responder lifecycle ============

    #[tokio::test]
    async fn attach_responder_to_existing_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;

        let (s, _r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), Instant::now(), s).await.unwrap();

        // Take-once: first take returns Some, second returns None.
        assert!(set.take_responder(tx.tx_hash()).await.is_some());
        assert!(set.take_responder(tx.tx_hash()).await.is_none());
    }

    #[tokio::test]
    async fn attach_responder_before_push_merges_on_push() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let (s, _r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), Instant::now(), s).await.unwrap();

        // Pre-push: responder lives in pending_responders.
        // Push must consume it and move it into the entry.
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        assert!(set.take_responder(tx.tx_hash()).await.is_some());
    }

    #[tokio::test]
    async fn push_consumes_pending_responder_so_second_attach_rejected() {
        // Stronger invariant check: after push_if_absent merges a pending
        // responder into the new entry, `pending_responders[hash]` must be
        // empty. A subsequent `attach_responder` must therefore land on the
        // entry path and see `responder.is_some()` → `AlreadyAttached`.
        //
        // Regression guard: if `push_if_absent` forgot to take from
        // `pending_responders`, the entry would carry `responder = None`,
        // and this second attach would silently succeed (leaving the
        // original responder leaked in `pending_responders`).
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let (s1, _r1) = oneshot::channel();
        let (s2, _r2) = oneshot::channel();

        set.attach_responder(*tx.tx_hash(), Instant::now(), s1).await.unwrap();
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;

        let err = set.attach_responder(*tx.tx_hash(), Instant::now(), s2).await.unwrap_err();
        assert_eq!(err, AttachError::AlreadyAttached);
    }

    #[tokio::test]
    async fn pending_responder_delivered_through_post_push_cancel() {
        // End-to-end: attach before push → push → cancel must deliver the
        // error to the originally attached receiver (proves the responder
        // was migrated, not orphaned in `pending_responders`).
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let (s, r) = oneshot::channel();

        set.attach_responder(*tx.tx_hash(), Instant::now(), s).await.unwrap();
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;

        let received = r.await.unwrap();
        assert_eq!(received, Err(PreconfError::NotPreconfEligible));
    }

    #[tokio::test]
    async fn attach_twice_returns_already_attached() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let (s1, _r1) = oneshot::channel();
        let (s2, _r2) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), Instant::now(), s1).await.unwrap();
        let err = set.attach_responder(*tx.tx_hash(), Instant::now(), s2).await.unwrap_err();
        assert_eq!(err, AttachError::AlreadyAttached);
    }

    #[tokio::test]
    async fn cancel_responder_sends_error_to_receiver() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let (s, r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), Instant::now(), s).await.unwrap();

        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;
        let received = r.await.unwrap();
        assert_eq!(received, Err(PreconfError::NotPreconfEligible));
    }

    #[tokio::test]
    async fn cancel_responder_silently_drops_when_none_attached() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        // No attach — cancel is a no-op.
        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;
    }

    // ============ get_tx ============

    #[tokio::test]
    async fn get_tx_returns_arc_clone() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let fetched = set.get_tx(tx.tx_hash()).await.unwrap();
        assert!(Arc::ptr_eq(&fetched, &tx));
    }

    #[tokio::test]
    async fn get_tx_returns_none_when_absent() {
        let set = PreconfTxSet::new(16);
        assert!(set.get_tx(&h(99)).await.is_none());
    }

    /// Locks invariant #2 mechanically: `snapshot_view` returns a view
    /// **without** the responder. If someone later widens `TxEntryView` to
    /// carry the responder, the size check + explicit field-parity assertion
    /// catches the regression before it can leak a `oneshot::Sender` outside
    /// the fifo.
    #[tokio::test]
    async fn snapshot_view_omits_responder_by_construction() {
        let (resp_tx, _resp_rx) = oneshot::channel();
        let entry = TxEntry {
            hash: h(1),
            tx: make_tx(0, 1),
            from: addr(2),
            nonce: 0,
            inserted_at: Instant::now(),
            status: PreconfStatus::Waiting,
            source: PreconfSource::Rpc,
            responder: Some(resp_tx),
            apply_lock: Arc::new(Mutex::new(())),
        };
        let view = entry.snapshot_view();
        assert_eq!(view.hash, entry.hash);
        assert_eq!(view.from, entry.from);
        assert_eq!(view.nonce, entry.nonce);
        assert_eq!(view.status, entry.status);
        // Structural: TxEntryView has no `responder` field.
        assert!(std::mem::size_of::<TxEntryView>() < std::mem::size_of::<TxEntry>());
    }

    /// `push_if_absent` self-heal path: `by_sender[(from, nonce)] = ghost`
    /// with no matching `entries[ghost]`. In debug builds this trips a
    /// `debug_assert!` (intentional dev-time signal), so this test runs
    /// only in release mode. `cargo test --release` executes it.
    ///
    /// TODO: replace with `tracing-test` capture to also verify the
    /// `error!()` line fires. For now assert observable side effects only.
    #[tokio::test]
    #[cfg(not(debug_assertions))]
    async fn push_if_absent_self_heals_dangling_by_sender() {
        let set = PreconfTxSet::new(4);
        let ghost = h(9);
        let from = addr(1);
        let nonce = 0u64;

        // Prime by_sender + order with ghost — no entries[ghost].
        {
            let mut inner = set.inner.lock().await;
            inner.by_sender.insert((from, nonce), ghost);
            inner.order.push_back(ghost);
        }

        // New push at same (from, nonce) with a real tx: dangling entry must
        // self-heal, then the fresh insert succeeds.
        let tx = make_tx(nonce, 1); // real hash != ghost
        let result = set.push_if_absent(tx.clone(), from, PreconfSource::Rpc).await;
        assert_eq!(result, PushResult::Inserted);

        let inner = set.inner.lock().await;
        // by_sender now points at the real hash, not ghost.
        assert_eq!(inner.by_sender.get(&(from, nonce)), Some(&h(1)));
        // Ghost cleaned from order; only real hash remains.
        assert!(!inner.order.contains(&ghost));
        assert!(inner.order.contains(&h(1)));
        // Real entry exists.
        assert!(inner.entries.contains_key(&h(1)));
    }

    /// `drop_hash` must be tolerant of partially-populated index state: if
    /// `entries[hash]` is missing, it should still clean `order` /
    /// `by_sender` / `pending_responders`. Non-self-heal companion to the
    /// "dangling `by_sender`" case — here the direction is opposite: entry
    /// gone first, aux indices need scrubbing.
    #[tokio::test]
    async fn drop_hash_tolerates_missing_entry() {
        let set = PreconfTxSet::new(4);
        let ghost = h(9);

        // Prime just the auxiliary indices with a ghost hash — no `entries`.
        {
            let mut inner = set.inner.lock().await;
            inner.order.push_back(ghost);
            inner.by_sender.insert((addr(9), 42), ghost);
            let (tx, _rx) = oneshot::channel();
            inner.pending_responders.insert(ghost, (Instant::now(), tx));
        }

        // Drop the ghost — no entry to remove, but aux indices should still
        // be cleaned.
        {
            let mut inner = set.inner.lock().await;
            let evicted = inner.drop_hash(&ghost);
            assert!(evicted.is_none());
            assert!(inner.order.is_empty());
            assert!(inner.by_sender.is_empty());
            assert!(inner.pending_responders.is_empty());
        }
    }

    /// `expire_pending_responders` drops aged slots, keeps fresh ones, and
    /// releases the evicted slot's `oneshot::Sender` (receiver sees `RecvError`).
    #[tokio::test]
    async fn expire_pending_responders_drops_only_aged_slots() {
        let set = PreconfTxSet::new(4);
        let aged = h(1);
        let fresh = h(2);

        let (aged_tx, aged_rx) = oneshot::channel::<Result<PreconfReceipt, PreconfError>>();
        let (fresh_tx, _fresh_rx) = oneshot::channel::<Result<PreconfReceipt, PreconfError>>();
        {
            let mut inner = set.inner.lock().await;
            // `aged` stamped 10s in the past; `fresh` at ~now.
            inner
                .pending_responders
                .insert(aged, (Instant::now() - Duration::from_secs(10), aged_tx));
            inner.pending_responders.insert(fresh, (Instant::now(), fresh_tx));
        }

        // Sweep with a 5s TTL: `aged` (10s) evicted, `fresh` (~0s) retained.
        let dropped = set.expire_pending_responders(Duration::from_secs(5)).await;
        assert_eq!(dropped, 1, "only the aged slot should be swept");

        {
            let inner = set.inner.lock().await;
            assert!(!inner.pending_responders.contains_key(&aged), "aged slot removed");
            assert!(inner.pending_responders.contains_key(&fresh), "fresh slot retained");
        }

        // Evicted slot's sender was dropped → receiver observes RecvError.
        assert!(aged_rx.await.is_err(), "orphaned responder's receiver must observe RecvError");
    }

    /// `cancel_responder` belt-and-braces cleanup: even if invariant #2 is
    /// violated (both `entry.responder` and `pending_responders[hash]` hold
    /// a responder), the ghost in `pending_responders` must be dropped so the
    /// client observes `RecvError` rather than waiting forever. The
    /// primary slot (entry.responder) still gets the typed `Err(...)`.
    #[tokio::test]
    async fn cancel_responder_drops_ghost_pending_slot() {
        let set = PreconfTxSet::new(4);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();

        // Legit path: attach responder before push, push consumes it into
        // entry.responder.
        let (primary_tx, mut primary_rx) = oneshot::channel();
        set.attach_responder(hash, Instant::now(), primary_tx).await.unwrap();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        // Simulate invariant-#2 violation: insert a *different* responder
        // back into pending_responders under the same hash.
        let (ghost_tx, mut ghost_rx) = oneshot::channel();
        {
            let mut inner = set.inner.lock().await;
            inner.pending_responders.insert(hash, (Instant::now(), ghost_tx));
        }

        // Cancel. Primary slot (entry.responder) gets the typed error;
        // ghost slot is silently dropped (Sender drops → RecvError).
        set.cancel_responder(&hash, PreconfError::NotPreconfEligible).await;

        // Primary receiver: typed error delivered.
        let delivered = primary_rx.try_recv().expect("primary responder cancelled");
        assert!(matches!(delivered, Err(PreconfError::NotPreconfEligible)));

        // Ghost receiver: sender dropped, so try_recv returns Closed.
        let ghost = ghost_rx.try_recv();
        assert!(
            matches!(ghost, Err(oneshot::error::TryRecvError::Closed)),
            "ghost responder must be dropped (RecvError-visible), got {ghost:?}"
        );

        // pending_responders now empty — no zombie.
        let inner = set.inner.lock().await;
        assert!(inner.pending_responders.is_empty());
    }

    /// Symmetric to `cancel_responder_drops_ghost_pending_slot`: even under
    /// invariant #2 violation (both slots occupied), `take_responder`
    /// returns the primary responder AND drops the ghost. Caller then
    /// sends Ok(receipt) via the returned Sender; ghost's receiver sees
    /// `RecvError`.
    #[tokio::test]
    async fn take_responder_drops_ghost_pending_slot() {
        let set = PreconfTxSet::new(4);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();

        // Legit path: attach → push consumes into entry.responder.
        let (primary_tx, mut primary_rx) = oneshot::channel();
        set.attach_responder(hash, Instant::now(), primary_tx).await.unwrap();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        // Invariant-#2 violation: re-insert a ghost into pending_responders.
        let (ghost_tx, mut ghost_rx) = oneshot::channel();
        {
            let mut inner = set.inner.lock().await;
            inner.pending_responders.insert(hash, (Instant::now(), ghost_tx));
        }

        // take_responder returns primary; ghost is silently dropped.
        let taken = set.take_responder(&hash).await.expect("primary responder taken");
        // Caller uses the returned Sender to deliver Ok(receipt).
        let receipt = PreconfReceipt {
            tx_hash: hash,
            block_height: 1,
            status: true,
            logs: vec![],
            gas_used: 21_000,
            reason: String::new(),
            revert_data: Default::default(),
        };
        taken.send(Ok(receipt.clone())).unwrap();
        let delivered = primary_rx.try_recv().expect("primary receiver got value");
        assert_eq!(delivered, Ok(receipt));

        // Ghost dropped — Sender gone, Receiver sees Closed.
        let ghost = ghost_rx.try_recv();
        assert!(
            matches!(ghost, Err(oneshot::error::TryRecvError::Closed)),
            "ghost responder must be dropped, got {ghost:?}"
        );

        let inner = set.inner.lock().await;
        assert!(inner.pending_responders.is_empty());
    }

    /// `attach_responder`'s `origin_instant` argument must land in
    /// `TxEntry.inserted_at` on the subsequent `push_if_absent`.
    /// Dispatch's deadline gate reads `entry.inserted_at.elapsed()`
    /// against `preconf_timeout`, so if the RPC-supplied instant is not
    /// threaded through, the gate would tick from listener-drain time
    /// rather than client-visible time.
    #[tokio::test]
    async fn attach_responder_origin_instant_lands_in_tx_entry() {
        let set = PreconfTxSet::new(4);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();

        // Anchor an instant well before the push, then sleep to give
        // the wall clock a measurable gap.
        let origin = Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;

        let (resp_tx, _resp_rx) = oneshot::channel();
        set.attach_responder(hash, origin, resp_tx).await.unwrap();
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;

        let entry = set.find_by_hash(&hash).await.expect("entry inserted");
        // TxEntry.inserted_at should equal (or be extremely close to) the
        // origin — NOT the push time. Compare by asserting the delta from
        // origin is under 1ms (Instant equality is not guaranteed after
        // clone).
        let drift = entry.inserted_at.saturating_duration_since(origin);
        assert!(
            drift < std::time::Duration::from_millis(1),
            "inserted_at drifted {drift:?} from origin; expected < 1ms"
        );
        // And it should be at least 10ms before "now" (the push time).
        let elapsed_since_push_prep = origin.elapsed();
        assert!(
            elapsed_since_push_prep >= std::time::Duration::from_millis(10),
            "test sleep did not create observable gap: elapsed={elapsed_since_push_prep:?}"
        );
    }

    /// `PushResult::ConflictActive(hash)` carries the **old**
    /// (colliding) hash so `PreconfPoolListener` can log both the new
    /// and existing hash on a slot collision. Doc-only assertion until
    /// now; this test locks the payload semantic.
    #[tokio::test]
    async fn push_conflict_active_carries_existing_hash() {
        let set = PreconfTxSet::new(4);
        let sender = addr(1);

        // First push at (sender, nonce=0) — Inserted.
        let tx_a = make_tx(0, 1); // nonce=0, hash byte 1
        let hash_a = *tx_a.tx_hash();
        assert!(matches!(
            set.push_if_absent(tx_a, sender, PreconfSource::Rpc).await,
            PushResult::Inserted
        ));

        // Second push at same (sender, nonce=0) with a different hash —
        // ConflictActive, payload must be `hash_a`, NOT the new tx's
        // hash.
        let tx_b = make_tx(0, 2); // nonce=0, hash byte 2 (different from tx_a)
        match set.push_if_absent(tx_b, sender, PreconfSource::Rpc).await {
            PushResult::ConflictActive(existing) => {
                assert_eq!(existing, hash_a, "ConflictActive must report the existing (old) hash");
            }
            other => panic!("expected ConflictActive, got {other:?}"),
        }
    }

    /// `PreconfTxSet::new(broadcast_cap = 0)` must panic. The
    /// broadcast channel needs at least capacity 1 (subscribers must
    /// be able to hold one buffered event before falling behind);
    /// tokio panics on `broadcast::channel(0)`, so this test also
    /// serves as an early-signal upstream contract check.
    #[test]
    #[should_panic]
    fn preconf_tx_set_new_panics_on_zero_broadcast_cap() {
        let _ = PreconfTxSet::new(0);
    }

    /// `mark_timeout` / `mark_canceled` / `mark_failed` must invoke the
    /// registered pool-eviction callback with the hash.
    /// `mark_succeeded` does NOT (canon commit's `mined_transactions`
    /// handles that path). Callback firing is verified via a
    /// `Vec<TxHash>` sink protected by `Mutex`.
    #[tokio::test]
    async fn mark_terminal_transitions_invoke_pool_eviction_callback() {
        use std::sync::Mutex as StdMutex;

        let set = PreconfTxSet::new(4);
        let evicted: Arc<StdMutex<Vec<TxHash>>> = Arc::new(StdMutex::new(Vec::new()));

        // Register sink.
        let sink = evicted.clone();
        set.set_pool_eviction_callback(Arc::new(move |h| {
            sink.lock().unwrap().push(h);
        }));

        // Push 4 waiting entries, one per terminal transition path.
        for (i, mark_kind) in ["timeout", "canceled", "failed", "succeeded"].iter().enumerate() {
            let tx = make_tx(i as u64, (i + 1) as u8);
            let hash = *tx.tx_hash();
            set.push_if_absent(tx, addr((i + 1) as u8), PreconfSource::Rpc).await;
            match *mark_kind {
                "timeout" => set.mark_timeout(&hash).await.unwrap(),
                "canceled" => set.mark_canceled(&hash).await.unwrap(),
                "failed" => set.mark_failed(&hash).await.unwrap(),
                "succeeded" => set.mark_succeeded(&hash).await.unwrap(),
                _ => unreachable!(),
            }
        }

        // Three non-on-chain terminals fired eviction; Success did not.
        let evicted_hashes = evicted.lock().unwrap().clone();
        assert_eq!(
            evicted_hashes.len(),
            3,
            "3 evictions expected (timeout / canceled / failed), got {evicted_hashes:?}"
        );
        // Order corresponds to iteration: timeout, canceled, failed.
        assert_eq!(evicted_hashes[0], h(1), "timeout hash");
        assert_eq!(evicted_hashes[1], h(2), "canceled hash");
        assert_eq!(evicted_hashes[2], h(3), "failed hash");
        // succeeded's hash h(4) must NOT be in the list.
        assert!(!evicted_hashes.contains(&h(4)), "succeeded must not trigger eviction");
    }

    /// Without a registered callback, `mark_*` transitions must still
    /// succeed silently. Guards against a regression where the hook
    /// accidentally panics or errors when unregistered.
    #[tokio::test]
    async fn mark_terminals_are_silent_without_pool_eviction_callback() {
        let set = PreconfTxSet::new(4);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        // No callback registered — mark_timeout should just succeed.
        set.mark_timeout(&hash).await.unwrap();
        let entry = set.find_by_hash(&hash).await.expect("entry present");
        assert_eq!(entry.status, PreconfStatus::Timeout);
    }

    /// `set_pool_eviction_callback` must be idempotent (`OnceLock`
    /// first-write wins). Guards against silent behavior split if
    /// `service_builder::start` gets called twice with different
    /// closures.
    #[tokio::test]
    async fn set_pool_eviction_callback_is_first_write_wins() {
        use std::sync::Mutex as StdMutex;

        let set = PreconfTxSet::new(4);
        let first_evicted: Arc<StdMutex<Vec<TxHash>>> = Arc::new(StdMutex::new(Vec::new()));
        let second_evicted: Arc<StdMutex<Vec<TxHash>>> = Arc::new(StdMutex::new(Vec::new()));

        let sink1 = first_evicted.clone();
        set.set_pool_eviction_callback(Arc::new(move |h| sink1.lock().unwrap().push(h)));

        let sink2 = second_evicted.clone();
        set.set_pool_eviction_callback(Arc::new(move |h| sink2.lock().unwrap().push(h)));

        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;
        set.mark_timeout(&hash).await.unwrap();

        // First callback wins; second is a silent drop.
        assert_eq!(first_evicted.lock().unwrap().len(), 1);
        assert!(second_evicted.lock().unwrap().is_empty());
    }
}

/// Stateful property model for [`PreconfTxSet`].
///
/// Replays random push / mark / remove / clean / forward sequences against the
/// real fifo and an independent reference model, checking after every step that
/// they agree and that structural invariants hold (slot uniqueness, index
/// consistency). Explores same-`(sender, nonce)` / same-hash collisions that
/// hand-written cases hit only sparsely. Deterministic: all mutations serialise
/// behind one lock, so one run per sequence is representative.
#[cfg(test)]
mod proptest_model {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{B256, Signature};
    use proptest::prelude::*;
    use std::collections::{BTreeMap, HashSet};

    // Small domains so slot/hash collisions are frequent.
    const SENDERS: u8 = 2;
    const NONCES: u8 = 3;
    const VARIANTS: u8 = 2;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }
    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }
    fn make_tx(nonce: u64, hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, ..Default::default() };
        let sig = Signature::test_signature();
        Arc::new(TxEnvelope::Eip1559(Signed::new_unchecked(
            inner,
            sig,
            B256::from([hash_byte; 32]),
        )))
    }

    /// A synthetic tx identity. Maps 1:1 to a hash byte, so a given hash
    /// always carries the same `(sender, nonce)` — matching reality, where a
    /// signed tx's hash is derived from its content. `variant` lets two txs
    /// share a `(sender, nonce)` slot with different hashes (the replacement
    /// case).
    #[derive(Clone, Copy, Debug)]
    struct TxId {
        sender: u8,
        nonce: u8,
        variant: u8,
    }

    impl TxId {
        /// Unique in `0..(SENDERS * NONCES * VARIANTS)`.
        fn hash_byte(self) -> u8 {
            (self.sender * NONCES + self.nonce) * VARIANTS + self.variant
        }
        fn tx(self) -> Arc<TxEnvelope> {
            make_tx(self.nonce as u64, self.hash_byte())
        }
        fn addr(self) -> Address {
            addr(self.sender)
        }
        fn hash(self) -> TxHash {
            h(self.hash_byte())
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Push(TxId),
        MarkSucceeded(TxId),
        MarkFailed(TxId),
        MarkTimeout(TxId),
        MarkCanceled(TxId),
        RemoveReclaimable(TxId),
        CleanReclaimable,
        Forward { sender: u8, new_nonce: u8 },
    }

    fn txid() -> impl Strategy<Value = TxId> {
        (0..SENDERS, 0..NONCES, 0..VARIANTS).prop_map(|(sender, nonce, variant)| TxId {
            sender,
            nonce,
            variant,
        })
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            txid().prop_map(Op::Push),
            txid().prop_map(Op::MarkSucceeded),
            txid().prop_map(Op::MarkFailed),
            txid().prop_map(Op::MarkTimeout),
            txid().prop_map(Op::MarkCanceled),
            txid().prop_map(Op::RemoveReclaimable),
            Just(Op::CleanReclaimable),
            // `new_nonce` up to NONCES+1 so a forward can clear the whole sender.
            (0..SENDERS, 0..(NONCES + 1))
                .prop_map(|(sender, new_nonce)| Op::Forward { sender, new_nonce }),
        ]
    }

    /// Reference model: `hash_byte -> (sender, nonce, status)`. Maintains "at
    /// most one entry per `(sender, nonce)`" by construction — the property
    /// the real fifo must also hold.
    type Model = BTreeMap<u8, (u8, u8, PreconfStatus)>;

    fn reclaimable(s: PreconfStatus) -> bool {
        matches!(s, PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed)
    }

    fn model_mark(model: &mut Model, hb: u8, target: PreconfStatus) {
        // `transition_from_waiting`: only Waiting moves; anything else is a no-op.
        if let Some((_, _, st)) = model.get_mut(&hb) &&
            *st == PreconfStatus::Waiting
        {
            *st = target;
        }
    }

    fn model_apply(model: &mut Model, op: &Op) {
        match op {
            Op::Push(id) => {
                let hb = id.hash_byte();
                if let Some((_, _, st)) = model.get_mut(&hb) {
                    // Same hash: reclaimable revives to Waiting; active is a no-op.
                    if reclaimable(*st) {
                        *st = PreconfStatus::Waiting;
                    }
                    return;
                }
                // Same (sender, nonce), different hash?
                let slot = model
                    .iter()
                    .find(|(_, (s, n, _))| *s == id.sender && *n == id.nonce)
                    .map(|(hb, (_, _, st))| (*hb, *st));
                match slot {
                    // Active slot blocks the replacement (ConflictActive) — no insert.
                    Some((_, st)) if !reclaimable(st) => {}
                    // Reclaimable slot is evicted, then the new tx takes it.
                    Some((old, _)) => {
                        model.remove(&old);
                        model.insert(hb, (id.sender, id.nonce, PreconfStatus::Waiting));
                    }
                    None => {
                        model.insert(hb, (id.sender, id.nonce, PreconfStatus::Waiting));
                    }
                }
            }
            Op::MarkSucceeded(id) => model_mark(model, id.hash_byte(), PreconfStatus::Success),
            Op::MarkFailed(id) => model_mark(model, id.hash_byte(), PreconfStatus::Failed),
            Op::MarkTimeout(id) => model_mark(model, id.hash_byte(), PreconfStatus::Timeout),
            Op::MarkCanceled(id) => model_mark(model, id.hash_byte(), PreconfStatus::Canceled),
            Op::RemoveReclaimable(id) => {
                let hb = id.hash_byte();
                if matches!(model.get(&hb), Some((_, _, st)) if reclaimable(*st)) {
                    model.remove(&hb);
                }
            }
            Op::CleanReclaimable => model.retain(|_, (_, _, st)| !reclaimable(*st)),
            Op::Forward { sender, new_nonce } => {
                model.retain(|_, (s, n, _)| !(*s == *sender && *n < *new_nonce))
            }
        }
    }

    async fn apply_real(set: &PreconfTxSet, op: &Op) {
        match op {
            Op::Push(id) => {
                set.push_if_absent(id.tx(), id.addr(), PreconfSource::Rpc).await;
            }
            Op::MarkSucceeded(id) => {
                let _ = set.mark_succeeded(&id.hash()).await;
            }
            Op::MarkFailed(id) => {
                let _ = set.mark_failed(&id.hash()).await;
            }
            Op::MarkTimeout(id) => {
                let _ = set.mark_timeout(&id.hash()).await;
            }
            Op::MarkCanceled(id) => {
                let _ = set.mark_canceled(&id.hash()).await;
            }
            Op::RemoveReclaimable(id) => {
                set.remove_reclaimable(&id.hash()).await;
            }
            Op::CleanReclaimable => {
                set.clean_reclaimable().await;
            }
            Op::Forward { sender, new_nonce } => {
                set.forward(&addr(*sender), *new_nonce as u64).await;
            }
        }
    }

    async fn run_and_check(ops: &[Op]) {
        let set = PreconfTxSet::new(64);
        let mut model = Model::new();

        for (i, op) in ops.iter().enumerate() {
            apply_real(&set, op).await;
            model_apply(&mut model, op);

            let views = set.entries().await;

            // (A) Real fifo agrees with the reference model, hash by hash.
            assert_eq!(views.len(), model.len(), "step {i}: entry count diverged after {op:?}");
            for v in &views {
                let hb = v.hash.0[0]; // h(byte) has every byte == byte
                let (s, n, st) = *model.get(&hb).unwrap_or_else(|| {
                    panic!("step {i}: real has hash {hb} not in model ({op:?})")
                });
                assert_eq!(v.from, addr(s), "step {i}: sender mismatch for hash {hb}");
                assert_eq!(v.nonce, u64::from(n), "step {i}: nonce mismatch for hash {hb}");
                assert_eq!(v.status, st, "step {i}: status mismatch for hash {hb}");
            }

            // (B) Slot uniqueness — at most one entry per (sender, nonce);
            // a duplicate is the "pool ghost" that breaks replacement safety.
            let mut slots = HashSet::new();
            for v in &views {
                assert!(
                    slots.insert((v.from, v.nonce)),
                    "step {i}: duplicate (sender, nonce) slot after {op:?}"
                );
            }

            // (C) Index consistency: `order` (snapshot) and `entries` hold the
            // exact same hash set with no duplicates, and every entry is
            // reachable via both by-hash and by-(sender,nonce) lookups.
            let snap = set.snapshot().await;
            assert_eq!(snap.len(), views.len(), "step {i}: snapshot/entries length diverged");
            let snap_set: HashSet<_> = snap.iter().copied().collect();
            assert_eq!(snap_set.len(), snap.len(), "step {i}: duplicate hash in order");
            for v in &views {
                assert!(snap_set.contains(&v.hash), "step {i}: entry missing from order index");
                assert_eq!(
                    set.find_by_sender_nonce(&v.from, v.nonce).await.map(|x| x.hash),
                    Some(v.hash),
                    "step {i}: by_sender index disagrees for {:?}",
                    v.hash
                );
                assert!(
                    set.find_by_hash(&v.hash).await.is_some(),
                    "step {i}: find_by_hash missing"
                );
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Any sequence of fifo operations keeps the real `PreconfTxSet` in
        /// lock-step with the reference model and never violates slot
        /// uniqueness or index consistency.
        #[test]
        fn preconf_tx_set_matches_reference_model(ops in prop::collection::vec(op(), 1..40)) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(run_and_check(&ops));
        }
    }
}

/// Stateful property model for [`PreconfTxSet`]'s **responder** machine — the
/// half the fifo model skips.
///
/// Holds a real `oneshot::Receiver` per attached responder and checks, after
/// each op, that a hash has at most one responder (in `entry.responder` xor
/// `pending_responders`), that `push` migrates a pending responder onto its
/// entry, and that a responder leaving the set resolves its receiver exactly
/// once (Ok via `take`, Err via `cancel`, `RecvError` when dropped) — never
/// silently leaked. A final `expire_pending_responders` must GC every pending
/// slot. Each hash owns its slot, isolating this from the replacement logic.
#[cfg(test)]
mod proptest_responder_model {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{B256, Bytes, Log, Signature};
    use proptest::prelude::*;
    use tokio::sync::oneshot::error::TryRecvError;

    const HASHES: u8 = 4;

    fn h(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }
    fn make_tx(nonce: u64, hash_byte: u8) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, ..Default::default() };
        let sig = Signature::test_signature();
        Arc::new(TxEnvelope::Eip1559(Signed::new_unchecked(
            inner,
            sig,
            B256::from([hash_byte; 32]),
        )))
    }
    fn receipt(hash_byte: u8) -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: h(hash_byte),
            block_height: 0,
            status: true,
            logs: Vec::<Log>::new(),
            gas_used: 0,
            reason: String::new(),
            revert_data: Bytes::new(),
        }
    }
    fn some_err() -> PreconfError {
        PreconfError::Internal("model".into())
    }

    type Rx = oneshot::Receiver<Result<PreconfReceipt, PreconfError>>;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Loc {
        Pending,
        Entry,
    }

    /// Per-hash model: entry status (if any) and the currently-held responder
    /// (location + its receiver, so we can observe the receiver's fate).
    #[derive(Default)]
    struct HashState {
        entry: Option<PreconfStatus>,
        held: Option<(Loc, Rx)>,
    }

    #[derive(Clone, Debug)]
    enum Op {
        Attach(u8),
        Push(u8),
        MarkTimeout(u8),
        MarkFailed(u8),
        MarkCanceled(u8),
        MarkSucceeded(u8),
        Take(u8),
        Cancel(u8),
        Forward(u8),
        CleanReclaimable,
    }

    fn hb() -> impl Strategy<Value = u8> {
        0..HASHES
    }
    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            hb().prop_map(Op::Attach),
            hb().prop_map(Op::Push),
            hb().prop_map(Op::MarkTimeout),
            hb().prop_map(Op::MarkFailed),
            hb().prop_map(Op::MarkCanceled),
            hb().prop_map(Op::MarkSucceeded),
            hb().prop_map(Op::Take),
            hb().prop_map(Op::Cancel),
            (0..=HASHES).prop_map(Op::Forward),
            Just(Op::CleanReclaimable),
        ]
    }

    fn reclaimable(s: PreconfStatus) -> bool {
        matches!(s, PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed)
    }
    fn assert_closed(mut rx: Rx, ctx: &str) {
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Closed)),
            "{ctx}: receiver must be Closed"
        );
    }
    fn assert_value(mut rx: Rx, ctx: &str) {
        assert!(rx.try_recv().is_ok(), "{ctx}: receiver must have a value");
    }

    async fn run_and_check(ops: &[Op]) {
        let set = PreconfTxSet::new(64);
        let sender = Address::from([0u8; 20]);
        let mut model: Vec<HashState> = (0..HASHES).map(|_| HashState::default()).collect();

        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Attach(b) => {
                    let b = *b;
                    let (tx, rx) = oneshot::channel();
                    let r = set.attach_responder(h(b), Instant::now(), tx).await;
                    let st = &mut model[b as usize];
                    match st.entry {
                        Some(PreconfStatus::Success) => {
                            assert!(r.is_err(), "step {i}: attach on Success must reject");
                            assert_closed(rx, &format!("step {i}: rejected attach"));
                        }
                        Some(PreconfStatus::Waiting) => {
                            if st.held.is_some() {
                                assert!(
                                    r.is_err(),
                                    "step {i}: attach over live responder must reject"
                                );
                                assert_closed(rx, &format!("step {i}: rejected attach"));
                            } else {
                                assert!(r.is_ok(), "step {i}: attach on bare Waiting must succeed");
                                st.held = Some((Loc::Entry, rx));
                            }
                        }
                        Some(_) => {
                            // Reclaimable: installs onto the entry, overwriting any prior
                            // responder.
                            assert!(r.is_ok(), "step {i}: attach on reclaimable must succeed");
                            if let Some((_, old)) = st.held.take() {
                                assert_closed(old, &format!("step {i}: overwritten responder"));
                            }
                            st.held = Some((Loc::Entry, rx));
                        }
                        None => {
                            if st.held.is_some() {
                                assert!(r.is_err(), "step {i}: attach over pending must reject");
                                assert_closed(rx, &format!("step {i}: rejected attach"));
                            } else {
                                assert!(r.is_ok(), "step {i}: first attach must succeed");
                                st.held = Some((Loc::Pending, rx));
                            }
                        }
                    }
                }
                Op::Push(b) => {
                    let b = *b;
                    set.push_if_absent(make_tx(u64::from(b), b), sender, PreconfSource::Rpc).await;
                    let st = &mut model[b as usize];
                    match st.entry {
                        None => {
                            st.entry = Some(PreconfStatus::Waiting);
                            // A pending responder migrates onto the fresh entry.
                            if let Some((Loc::Pending, rx)) = st.held.take() {
                                st.held = Some((Loc::Entry, rx));
                            }
                        }
                        Some(s) if reclaimable(s) => {
                            st.entry = Some(PreconfStatus::Waiting); // revived; responder unchanged
                        }
                        Some(_) => { /* Waiting/Success: AlreadyExists, no change */ }
                    }
                }
                Op::MarkTimeout(b) => mark(&set, &mut model, *b, PreconfStatus::Timeout).await,
                Op::MarkFailed(b) => mark(&set, &mut model, *b, PreconfStatus::Failed).await,
                Op::MarkCanceled(b) => mark(&set, &mut model, *b, PreconfStatus::Canceled).await,
                Op::MarkSucceeded(b) => mark(&set, &mut model, *b, PreconfStatus::Success).await,
                Op::Take(b) => {
                    let b = *b;
                    let r = set.take_responder(&h(b)).await;
                    let st = &mut model[b as usize];
                    if let Some((_, rx)) = st.held.take() {
                        let s = r.unwrap_or_else(|| {
                            panic!("step {i}: take must return the held responder")
                        });
                        let _ = s.send(Ok(receipt(b)));
                        assert_value(rx, &format!("step {i}: taken responder delivered"));
                    } else {
                        assert!(r.is_none(), "step {i}: take with no responder must be None");
                    }
                }
                Op::Cancel(b) => {
                    let b = *b;
                    set.cancel_responder(&h(b), some_err()).await;
                    let st = &mut model[b as usize];
                    if let Some((_, rx)) = st.held.take() {
                        assert_value(rx, &format!("step {i}: canceled responder got error"));
                    }
                }
                Op::Forward(new_nonce) => {
                    set.forward(&sender, u64::from(*new_nonce)).await;
                    for b in 0..HASHES {
                        let st = &mut model[b as usize];
                        // forward only drops *entries* (nonce == b) below new_nonce.
                        if st.entry.is_some() && u64::from(b) < u64::from(*new_nonce) {
                            st.entry = None;
                            if let Some((_, rx)) = st.held.take() {
                                assert_closed(rx, &format!("step {i}: forward-dropped responder"));
                            }
                        }
                    }
                }
                Op::CleanReclaimable => {
                    set.clean_reclaimable().await;
                    for b in 0..HASHES {
                        let st = &mut model[b as usize];
                        if matches!(st.entry, Some(s) if reclaimable(s)) {
                            st.entry = None;
                            if let Some((_, rx)) = st.held.take() {
                                assert_closed(rx, &format!("step {i}: clean-dropped responder"));
                            }
                        }
                    }
                }
            }

            check_invariants(&set, &mut model, i).await;
        }

        // Final GC: every lingering *pending* responder must be expired (its
        // receiver Closed); entry-held responders are untouched.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let expired = set.expire_pending_responders(Duration::ZERO).await;
        let mut expected = 0usize;
        for st in &mut model {
            if let Some((Loc::Pending, rx)) = st.held.take() {
                expected += 1;
                assert_closed(rx, "final expire");
            }
        }
        assert_eq!(expired, expected, "expire count must match lingering pending responders");
        assert!(
            set.inner.lock().await.pending_responders.is_empty(),
            "no pending responder may survive expire"
        );
    }

    async fn mark(set: &PreconfTxSet, model: &mut [HashState], b: u8, target: PreconfStatus) {
        let _ = match target {
            PreconfStatus::Success => set.mark_succeeded(&h(b)).await,
            PreconfStatus::Failed => set.mark_failed(&h(b)).await,
            PreconfStatus::Timeout => set.mark_timeout(&h(b)).await,
            PreconfStatus::Canceled => set.mark_canceled(&h(b)).await,
            // Not a `mark_*` target, and `op()` never proposes it: `Waiting` is
            // entered only by `push_if_absent`, fresh or revived. Spelled out
            // rather than caught by a wildcard so that adding a status forces
            // this decision again.
            PreconfStatus::Waiting => unreachable!(),
        };
        let st = &mut model[b as usize];
        if st.entry == Some(PreconfStatus::Waiting) {
            st.entry = Some(target); // responder untouched by mark_*
        }
    }

    async fn check_invariants(set: &PreconfTxSet, model: &mut [HashState], step: usize) {
        {
            let inner = set.inner.lock().await;
            for hash in inner.pending_responders.keys() {
                assert!(
                    !inner.entries.contains_key(hash),
                    "step {step}: hash in both pending_responders and entries"
                );
            }
            for b in 0..model.len() as u8 {
                let hash = h(b);
                let has_pending = inner.pending_responders.contains_key(&hash);
                let entry_resp = inner.entries.get(&hash).is_some_and(|e| e.responder.is_some());
                // Invariant #2 within one hash: at most one responder location.
                assert!(!(has_pending && entry_resp), "step {step}: two responders for {b}");
                let expect = match model[b as usize].held {
                    None => (false, false),
                    Some((Loc::Pending, _)) => (true, false),
                    Some((Loc::Entry, _)) => (false, true),
                };
                assert_eq!(
                    (has_pending, entry_resp),
                    expect,
                    "step {step}: responder location mismatch for {b}"
                );
                assert_eq!(
                    inner.entries.get(&hash).map(|e| e.status),
                    model[b as usize].entry,
                    "step {step}: entry status mismatch for {b}"
                );
            }
        }
        // Held responders must not have resolved yet (no premature send/drop).
        for (b, st) in model.iter_mut().enumerate() {
            if let Some((_, rx)) = st.held.as_mut() {
                assert!(
                    matches!(rx.try_recv(), Err(TryRecvError::Empty)),
                    "step {step}: held responder for {b} resolved prematurely"
                );
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 192, ..ProptestConfig::default() })]

        /// Any sequence of responder operations keeps every `oneshot` responder
        /// singly-located, correctly migrated on push, and delivered/dropped
        /// exactly once — no responder is silently leaked, and every pending
        /// slot is GC-able.
        #[test]
        fn preconf_tx_set_responder_lifecycle(ops in prop::collection::vec(op(), 1..40)) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(run_and_check(&ops));
        }
    }
}
