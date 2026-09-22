//! `PreconfTxSet` — the commitment truth source.
//!
//! ## Responsibilities
//!
//! 1. Track unsealed preconf-eligible transactions in FIFO order
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
//! - At most one entry per `(sender, nonce)` whose status is not `Timeout`. A new push with the
//!   same `(sender, nonce)` evicts an existing `Timeout` entry; an active (Waiting / Success /
//!   Failed) entry blocks the push.
//! - At most one responder per hash. Either lives inside an existing entry, or in
//!   `pending_responders` until the matching `push_if_absent` consumes it.
//! - `notifier.send` is best-effort — slow consumers receive `Lagged(n)` and reconcile via
//!   `snapshot()`.

use alloy_consensus::{Transaction, TxEnvelope};
// foldhash HashMap: faster than SipHash on high-entropy keys (TxHash /
// Address); matches `PreconfConfig::from_preconfs` in `config.rs`.
// `HashMapExt` brings `::new()` / `::with_capacity()` into scope.
use alloy_primitives::{
    Address, TxHash,
    map::foldhash::{HashMap, HashMapExt},
};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, broadcast, oneshot};
use tracing::error;

use crate::types::{
    AttachError, PreconfError, PreconfReceipt, PreconfSource, PreconfStatus, PushResult,
};

/// A single fifo entry.
///
/// Cloned for `snapshot` / `find_*` queries; the responder field is
/// **never** cloned (it's a `oneshot::Sender`, take-once semantics).
///
/// Only two statuses ever persist here: `Waiting` (in flight) and `Success`
/// (promised, awaiting canon). Every terminal-failure outcome removes the
/// entry in the same critical section that records it, so `Failed` /
/// `Timeout` / `Canceled` exist only as wire answers, never as stored state.
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
    /// Wall-clock insertion time. Load-bearing: the fifo deadline sweep
    /// finalises the entry as `Timeout` once
    /// `elapsed >= preconf_timeout`.
    pub inserted_at: Instant,
    /// Current status — see [`PreconfStatus`].
    pub status: PreconfStatus,
    /// Origin of the entry — see [`PreconfSource`]. `Replay` marks an
    /// already-promised commitment, which is exempt from failure
    /// finalisation: it must land, so no build may declare it dead.
    pub source: PreconfSource,
    /// RPC handler responder — `Some` when the RPC path attached one before
    /// pool.add succeeded; `None` for listener-pushed entries.
    /// Take-once: `take_responder` moves it out.
    pub responder: Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>>,
    /// How many builds currently hold this entry. A failure is terminal only at
    /// zero — builds judge against different in-flight block states, so one
    /// build's rejection is not the transaction's fate. `Success` is *not*
    /// terminal and takes holds too: a promise can still be overturned by every
    /// build failing it. `Arc` so a hold releases the entry *instance* (ABA).
    applying: Arc<AtomicUsize>,
}

/// A single build's registered interest in one fifo entry. Consumed by exactly
/// one of `finish_success` / `finish_failure` / `release`; dropping it
/// unconsumed decrements as a safety net but cannot finalise, leaving the entry
/// to the deadline.
#[derive(Debug)]
pub struct ApplyHold {
    hash: TxHash,
    applying: Arc<AtomicUsize>,
    consumed: bool,
}

/// Saturating decrement — a count that has been zeroed by a terminal verdict
/// must stay at zero. `Drop` cannot read the entry's status, so it cannot know
/// to hold back; clamping here is what makes that safe.
fn release_one(count: &AtomicUsize) -> usize {
    count
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| Some(c.saturating_sub(1)))
        .unwrap_or(0)
        .saturating_sub(1)
}

/// Why [`PreconfTxSet::finalize_timeout`] did or did not act. `Absent` is
/// distinct from `Exempt` because only the former leaves the caller holding
/// cleanup it must do itself — a parked responder and a pool entry no build
/// will ever see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutOutcome {
    /// The entry was `Waiting`; it has been answered and removed.
    Finalised,
    /// No entry under this hash.
    Absent,
    /// The entry exists but is already promised, or is a replay that must land.
    Exempt,
}

impl ApplyHold {
    /// The hash this hold registered against.
    pub const fn hash(&self) -> TxHash {
        self.hash
    }

    /// Consume the hold, returning the count *after* decrementing.
    fn take(mut self) -> usize {
        self.consumed = true;
        release_one(&self.applying)
    }
}

impl Drop for ApplyHold {
    fn drop(&mut self) {
        if !self.consumed {
            release_one(&self.applying);
        }
    }
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
}

impl PreconfTxSetInner {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
            by_sender: HashMap::new(),
            pending_responders: HashMap::new(),
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
    pool_evict: OnceLock<Arc<dyn Fn(TxHash) + Send + Sync>>,
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
        Self { inner: Mutex::new(PreconfTxSetInner::new()), notifier, pool_evict: OnceLock::new() }
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
    pub fn set_pool_eviction_callback(&self, f: Arc<dyn Fn(TxHash) + Send + Sync>) {
        let _ = self.pool_evict.set(f);
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
    /// - [`PushResult::AlreadyExists`] — same hash already present (no-op).
    /// - [`PushResult::ConflictActive(existing_hash)`] — same `(from, nonce)` but different hash,
    ///   and the existing entry is not `Timeout`.
    ///
    /// When the existing entry IS `Timeout`, it is evicted and the new tx
    /// is inserted in its place.
    pub async fn push_if_absent(
        &self,
        tx: Arc<TxEnvelope>,
        from: Address,
        source: PreconfSource,
    ) -> PushResult {
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();

        let mut inner = self.inner.lock().await;

        // Same hash already present — idempotent no-op. There is no revive
        // branch any more: a terminal outcome removes the entry in the same
        // critical section that records it, so only `Waiting` and `Success`
        // can be found here and neither may be displaced. Callers surface this
        // as `AlreadyInProgress`.
        if inner.entries.contains_key(&hash) {
            return PushResult::AlreadyExists;
        }

        // Same `(sender, nonce)`, different hash: the slot is taken, full
        // stop. With several payload builds in flight over the incumbent, no
        // single one of them has the standing to declare it dead, so there is
        // no handover — the slot frees when the incumbent is finalised, which
        // the client deadline bounds.
        if let Some(existing_hash) = inner.by_sender.get(&(from, nonce)).copied() {
            if inner.entries.contains_key(&existing_hash) {
                return PushResult::ConflictActive(existing_hash);
            }
            // Dangling `by_sender` index with no entry behind it. Self-heal so
            // the new insert can claim the slot; `drop_hash` also sweeps any
            // lingering `order` / `pending_responders` references.
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
            applying: Arc::new(AtomicUsize::new(0)),
        };
        inner.entries.insert(hash, entry);
        inner.by_sender.insert((from, nonce), hash);
        inner.order.push_back(hash);
        drop(inner);

        let _ = self.notifier.send(hash);

        PushResult::Inserted
    }

    // ============ Build registration & verdicts ============

    /// Demote every `Success` entry **not** in `landed` to `Replay`. Surviving
    /// a canonical block without landing is the evidence that the block
    /// carrying it was not adopted, so it becomes an ordinary carried-over
    /// promise: still must-land, but overturnable if every build fails it, and
    /// therefore unable to pin its slot forever.
    pub async fn demote_unlanded_promises(&self, landed: &HashSet<TxHash>) -> usize {
        let mut inner = self.inner.lock().await;
        let mut demoted = 0;
        for entry in inner.entries.values_mut() {
            if entry.status == PreconfStatus::Success &&
                entry.source == PreconfSource::Rpc &&
                !landed.contains(&entry.hash)
            {
                entry.source = PreconfSource::Replay;
                demoted += 1;
            }
        }
        demoted
    }

    /// Register this build against every entry in one `inner` acquisition, at
    /// build start. Registering progressively would leave the tail unbacked
    /// while the head is worked on, and a concurrent build failing one of those
    /// would see a zero count and finalise it. Returned in **fifo order** —
    /// this is also the build's dispatch order, which same-sender nonce
    /// sequencing depends on.
    pub async fn register_all(&self) -> Vec<ApplyHold> {
        let inner = self.inner.lock().await;
        inner
            .order
            .iter()
            .filter_map(|hash| inner.entries.get(hash))
            .map(|entry| {
                entry.applying.fetch_add(1, Ordering::SeqCst);
                ApplyHold { hash: entry.hash, applying: entry.applying.clone(), consumed: false }
            })
            .collect()
    }

    /// Register one build's interest in `hash`, for entries that appear *after*
    /// [`Self::register_all`] ran. `None` only when the entry is gone —
    /// `Success` still takes holds, since a promise is overturnable by every
    /// build failing it.
    pub async fn register(&self, hash: &TxHash) -> Option<ApplyHold> {
        let inner = self.inner.lock().await;
        let entry = inner.entries.get(hash)?;
        entry.applying.fetch_add(1, Ordering::SeqCst);
        Some(ApplyHold { hash: *hash, applying: entry.applying.clone(), consumed: false })
    }

    /// Record a successful apply: `Waiting → Success`, responder answered. No
    /// quorum — carryover can still keep the promise if this block is dropped.
    /// Returns `false` when the deadline already finalised the entry, the one
    /// way an in-flight success is preempted; the receipt is then discarded and
    /// the responder left to whoever finalised it.
    pub async fn finish_success(&self, hold: ApplyHold, receipt: PreconfReceipt) -> bool {
        let hash = hold.hash();
        let mut inner = self.inner.lock().await;
        let _ = hold.take();
        let Some(entry) = inner.entries.get_mut(&hash) else { return false };
        if entry.status != PreconfStatus::Waiting {
            // Another build already promised it; this one's receipt is
            // redundant and its responder is not ours to answer.
            return false;
        }
        entry.status = PreconfStatus::Success;
        let responder = entry.responder.take();
        drop(inner);
        if let Some(r) = responder {
            let _ = r.send(Ok(receipt));
        }
        true
    }

    /// Record a failing verdict — the build **tried** this tx and the builder
    /// rejected it. Terminal only once this was the last holder: one build's
    /// rejection is not the transaction's fate. On finalisation the entry is
    /// removed outright, freeing the slot for an immediate resubmit, and the
    /// pool eviction hook fires.
    ///
    /// A build that merely deferred the tx, or exited before reaching it, must
    /// **not** call this — it has no verdict to cast. Dropping its
    /// [`ApplyHold`] releases the count without voting.
    pub async fn finish_failure(&self, hold: ApplyHold, err: PreconfError) -> bool {
        let hash = hold.hash();
        let responder = {
            let mut inner = self.inner.lock().await;
            let Some(entry) = inner.entries.get(&hash) else {
                let _ = hold.take();
                return false;
            };
            // A success from *this* round is protected for one block: it has a
            // real chance of being the adopted one. `canon_handler` demotes it
            // to `Replay` once a canonical block has gone by without it, and
            // from then on it is overturnable like anything else.
            if entry.status == PreconfStatus::Success && entry.source == PreconfSource::Rpc {
                let _ = hold.take();
                return false;
            }
            let was_promised =
                entry.status == PreconfStatus::Success || entry.source == PreconfSource::Replay;
            if hold.take() > 0 {
                return false;
            }
            // Shares the critical section with the removal: otherwise a
            // resubmit sees "gone from the fifo, still in the pool", gets
            // `AlreadyImported`, fires no `Pending` event and waits for
            // nothing. Safe to nest — every path here takes fifo before pool,
            // and reth notifies listeners with `try_send`, never blocking.
            self.evict_from_pool(hash);
            if was_promised {
                // Every build that signed up failed it, so it is not landing
                // anywhere — but a receipt already went out. Loud and counted:
                // this is the one case where we knowingly break a promise, and
                // it must never be mistaken for an ordinary rejection.
                metrics::counter!("preconf.tx.commitment_broken_total").increment(1);
                error!(
                    target: "mantle::preconf",
                    ?hash,
                    "commitment broken: every build failed an already-promised tx"
                );
            }
            inner.drop_hash(&hash).and_then(|mut e| e.responder.take())
        };
        if let Some(r) = responder {
            let _ = r.send(Err(err));
        }
        true
    }

    /// Finalise one entry as `Timeout`. The caller's deadline is the authority,
    /// so there is no age test. Two callers: the build's pre-apply gate (which
    /// walks every entry, making it the general sweep) and the RPC handler when
    /// its client gives up with no build running. `Success` and `Replay` are
    /// never finalised.
    pub async fn finalize_timeout(&self, hash: &TxHash, timeout: Duration) -> TimeoutOutcome {
        let responder = {
            let mut inner = self.inner.lock().await;
            let Some(entry) = inner.entries.get(hash) else { return TimeoutOutcome::Absent };
            if entry.status != PreconfStatus::Waiting || entry.source == PreconfSource::Replay {
                return TimeoutOutcome::Exempt;
            }
            // Pool eviction shares the critical section with the removal — see
            // `finalize_non_success` for why the two must not be observable
            // apart.
            self.evict_from_pool(*hash);
            inner.drop_hash(hash).and_then(|mut e| e.responder.take())
        };
        if let Some(r) = responder {
            let _ = r.send(Err(PreconfError::Timeout { timeout_ms: timeout.as_millis() as u64 }));
        }
        TimeoutOutcome::Finalised
    }

    /// Holder count for `hash`; `None` when the entry is gone. Test-only —
    /// production never reads the count except inside a verdict.
    #[cfg(test)]
    pub async fn applying_count(&self, hash: &TxHash) -> Option<usize> {
        let inner = self.inner.lock().await;
        inner.entries.get(hash).map(|e| e.applying.load(Ordering::SeqCst))
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

    /// Drops entries with `from == addr && nonce < new_nonce` — called by
    /// `canon_handler` when a block including this sender's tx is sealed.
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

    /// Evicts every entry in a reclaimable terminal state —
    /// `Timeout` (client deadline elapsed), `Canceled` (block-gas-budget pre-apply
    /// reject, e.g. block gas budget), and `Failed` (reth builder
    /// pre-execute reject; tx NOT on chain). Broader than op-geth's
    /// `FIFOTxSet::CleanTimeout` which only clears the timeout case;
    /// the split into three states in this fifo means all must be swept
    /// together to avoid stale entries pinning the (sender, nonce) slot
    /// forever. Returns evicted hashes.
    pub async fn clean_reclaimable(&self) -> Vec<TxHash> {
        let mut inner = self.inner.lock().await;
        let to_drop: Vec<TxHash> = inner
            .entries
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.status,
                    PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed
                )
            })
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
    // No standalone `mark_*`: a status change is one part of a verdict that also
    // settles the holder count, hands over the responder and, when terminal,
    // removes the entry. `reset_success_to_waiting` is the exception — it
    // re-arms a promise rather than ending one.

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
                // Reclaimable — this is a same-hash retry after a
                // `Timeout` (client deadline), `Canceled` (block-gas-budget pre-apply
                // reject), or `Failed` (reth builder pre-execute reject;
                // tx NOT on chain). Install the fresh responder and
                // refresh `inserted_at` so `builder::dispatch`'s
                // deadline gate measures against the second submission
                // rather than the (already-expired) first. The
                // subsequent `push_if_absent` from the pool listener
                // flips the entry back to `Waiting` and broadcasts.
                // A terminal outcome removes the entry in the same critical
                // section that records it, so `entries` only ever holds
                // `Waiting` / `Success`. Reaching here means that invariant
                // broke; refuse rather than attach to a corpse.
                _ => return Err(AttachError::AlreadyAttached),
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

    fn receipt(hash_byte: u8) -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: h(hash_byte),
            block_height: 0,
            status: true,
            logs: Vec::new(),
            gas_used: 0,
            reason: String::new(),
            revert_data: alloy_primitives::Bytes::new(),
        }
    }

    fn some_err() -> PreconfError {
        PreconfError::Internal("test".into())
    }

    /// Drive an entry to a terminal outcome the way a build does. Terminal
    /// means *gone*, so callers assert on absence rather than on a status.
    async fn finalize(set: &PreconfTxSet, hash: &TxHash) {
        let hold = set.register(hash).await.expect("entry present");
        assert!(set.finish_failure(hold, some_err()).await, "sole holder must finalise");
    }

    /// Record a successful apply the way a build does.
    async fn succeed(set: &PreconfTxSet, hash: &TxHash) {
        let hold = set.register(hash).await.expect("entry present");
        assert!(set.finish_success(hold, receipt(0)).await, "must record Success");
    }

    // ============ Build holds & verdicts ============

    /// A failing build is not the transaction's fate: builds judge against
    /// different in-flight block states, so a rejection is terminal only once
    /// the last registered build has given up.
    #[tokio::test]
    async fn failure_is_terminal_only_for_the_last_holder() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        let a = set.register(&hash).await.expect("entry present");
        let b = set.register(&hash).await.expect("entry present");

        assert!(!set.finish_failure(a, some_err()).await, "another build still holds it");
        assert!(set.contains(&hash).await);

        assert!(set.finish_failure(b, some_err()).await, "last holder finalises");
        assert!(!set.contains(&hash).await, "finalising removes the entry outright");
    }

    /// `register_all` returns holds in fifo insertion order — that order is the
    /// build's dispatch order, and same-sender nonce sequencing rides on it.
    #[tokio::test]
    async fn register_all_returns_holds_in_fifo_order() {
        let set = PreconfTxSet::new(16);
        let mut expected = Vec::new();
        for i in 0..3u8 {
            let tx = make_tx(0, 0xc0 + i);
            expected.push(*tx.tx_hash());
            set.push_if_absent(tx, addr(i + 1), PreconfSource::Rpc).await;
        }

        let hashes: Vec<_> = set.register_all().await.iter().map(ApplyHold::hash).collect();
        assert_eq!(hashes, expected);
    }

    /// It registers, and does nothing else: no status or source is rewritten,
    /// so a promise stays a promise and the carryover list is a pure read.
    #[tokio::test]
    async fn register_all_does_not_rewrite_entries() {
        let set = PreconfTxSet::new(16);
        let waiting = make_tx(0, 1);
        let promised = make_tx(0, 2);
        set.push_if_absent(waiting.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(promised.clone(), addr(2), PreconfSource::Rpc).await;
        succeed(&set, promised.tx_hash()).await;

        let holds = set.register_all().await;
        assert_eq!(holds.len(), 2, "both are carried");

        let w = set.find_by_hash(waiting.tx_hash()).await.unwrap();
        assert_eq!((w.status, w.source), (PreconfStatus::Waiting, PreconfSource::Rpc));
        let p = set.find_by_hash(promised.tx_hash()).await.unwrap();
        assert_eq!((p.status, p.source), (PreconfStatus::Success, PreconfSource::Rpc));
    }

    #[tokio::test]
    async fn register_all_on_empty_fifo_is_noop() {
        let set = PreconfTxSet::new(16);
        assert!(set.register_all().await.is_empty());
    }

    /// `Success` is not a wall: a promise is still overturnable by every build
    /// failing it, so builds sign up for it like any other entry. Only an
    /// absent entry refuses.
    #[tokio::test]
    async fn success_still_takes_registrations() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;
        succeed(&set, &hash).await;

        let hold = set.register(&hash).await;
        assert!(hold.is_some(), "a promise is not terminal");
        assert_eq!(set.applying_count(&hash).await, Some(1));

        assert!(set.register(&h(0xfe)).await.is_none(), "an absent entry refuses");
    }

    /// The count is a plain holder count again: a success does not zero it, and
    /// each build releases its own hold. `Drop` clamps at zero so a stray
    /// release can never wrap the `usize`.
    #[tokio::test]
    async fn the_count_tracks_holders_and_never_underflows() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        let a = set.register(&hash).await.unwrap();
        let b = set.register(&hash).await.unwrap();
        assert_eq!(set.applying_count(&hash).await, Some(2));

        assert!(set.finish_success(a, receipt(1)).await);
        assert_eq!(set.applying_count(&hash).await, Some(1), "B still holds it");

        assert!(!set.finish_failure(b, some_err()).await, "Rpc-sourced success is protected");
        assert_eq!(set.applying_count(&hash).await, Some(0));
        drop(set.register(&hash).await);
        assert_eq!(set.applying_count(&hash).await, Some(0), "no underflow");
    }

    /// Success needs no quorum — it is a promise the carryover machinery can
    /// still keep if this build's block is discarded.
    #[tokio::test]
    async fn success_needs_no_quorum_and_keeps_the_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        let a = set.register(&hash).await.unwrap();
        let _b = set.register(&hash).await.unwrap();
        assert!(set.finish_success(a, receipt(1)).await, "recorded despite a second holder");
        assert_eq!(set.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Success);
    }

    /// A success from this round is protected: it has a real chance of being
    /// the adopted block. Once a canonical block goes by without it, the
    /// demotion makes it overturnable again.
    #[tokio::test]
    async fn a_fresh_success_is_protected_until_demoted() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;
        succeed(&set, &hash).await;

        let a = set.register(&hash).await.unwrap();
        assert!(!set.finish_failure(a, some_err()).await, "protected while Rpc-sourced");
        assert!(set.contains(&hash).await);

        // A canonical block went by without it.
        assert_eq!(set.demote_unlanded_promises(&HashSet::new()).await, 1);
        let b = set.register(&hash).await.unwrap();
        assert!(set.finish_failure(b, some_err()).await, "demoted, so now overturnable");
        assert!(!set.contains(&hash).await);
    }

    /// A `Replay` commitment is not immortal: if every build that signed up
    /// fails it, it is finalised and its slot freed, loudly. Otherwise a wedged
    /// promise would pin `(sender, nonce)` forever.
    #[tokio::test]
    async fn a_replay_commitment_dies_when_every_build_fails_it() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Replay).await;

        let a = set.register(&hash).await.unwrap();
        let b = set.register(&hash).await.unwrap();
        assert!(!set.finish_failure(a, some_err()).await, "another build still holds it");
        assert!(set.finish_failure(b, some_err()).await, "last holder finalises");
        assert!(!set.contains(&hash).await);
    }

    /// The deadline sweep is the only thing that can preempt an in-flight
    /// success: once it has finalised the entry, a late success is refused and
    /// its receipt dropped.
    #[tokio::test]
    async fn the_deadline_sweep_preempts_a_late_success() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        let hold = set.register(&hash).await.unwrap();
        assert_eq!(set.finalize_timeout(&hash, Duration::ZERO).await, TimeoutOutcome::Finalised);
        assert!(!set.finish_success(hold, receipt(1)).await, "entry already finalised");
        assert!(!set.contains(&hash).await);
    }

    /// `Success` entries and `Replay` commitments are never finalised.
    #[tokio::test]
    async fn the_deadline_spares_promised_entries() {
        let set = PreconfTxSet::new(16);
        let done = make_tx(0, 1);
        let replay = make_tx(0, 2);
        set.push_if_absent(done.clone(), addr(1), PreconfSource::Rpc).await;
        set.push_if_absent(replay.clone(), addr(2), PreconfSource::Replay).await;
        let hold = set.register(done.tx_hash()).await.unwrap();
        assert!(set.finish_success(hold, receipt(1)).await);

        assert_eq!(
            set.finalize_timeout(done.tx_hash(), Duration::ZERO).await,
            TimeoutOutcome::Exempt,
        );
        assert_eq!(
            set.finalize_timeout(replay.tx_hash(), Duration::ZERO).await,
            TimeoutOutcome::Exempt,
        );
        assert!(set.contains(done.tx_hash()).await);
        assert!(set.contains(replay.tx_hash()).await);
    }

    /// Dropping a hold without a verdict decrements but never finalises — the
    /// path a fatal build abort takes, since a broken build has no standing to
    /// declare a transaction dead.
    #[tokio::test]
    async fn dropping_a_hold_decrements_without_finalising() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        drop(set.register(&hash).await.unwrap());
        assert!(set.contains(&hash).await, "a dropped hold must not finalise");

        // The count really did come back down: the next holder is now the last.
        let next = set.register(&hash).await.unwrap();
        assert!(set.finish_failure(next, some_err()).await);
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

    // ============ status transitions ============

    /// A promised entry is final: neither a failing build nor the deadline
    /// may overturn it.
    #[tokio::test]
    async fn nothing_overturns_a_recorded_success() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;
        let hold = set.register(&hash).await.unwrap();
        succeed(&set, &hash).await;

        assert!(!set.finish_failure(hold, some_err()).await, "a live hold cannot overturn it");
        assert_eq!(
            set.finalize_timeout(&hash, Duration::ZERO).await,
            TimeoutOutcome::Exempt,
            "nor can the deadline",
        );
        assert_eq!(set.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Success);
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
            applying: Arc::new(AtomicUsize::new(0)),
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
        finalize(&set, &hash).await;

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
        Succeed(TxId),
        Fail(TxId),
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
            txid().prop_map(Op::Succeed),
            txid().prop_map(Op::Fail),
            // `new_nonce` up to NONCES+1 so a forward can clear the whole sender.
            (0..SENDERS, 0..(NONCES + 1))
                .prop_map(|(sender, new_nonce)| Op::Forward { sender, new_nonce }),
        ]
    }

    /// Reference model: `hash_byte -> (sender, nonce, status)`. Maintains "at
    /// most one entry per `(sender, nonce)`" by construction — the property
    /// the real fifo must also hold.
    type Model = BTreeMap<u8, (u8, u8, PreconfStatus)>;

    fn receipt(hash_byte: u8) -> PreconfReceipt {
        PreconfReceipt {
            tx_hash: h(hash_byte),
            block_height: 0,
            status: true,
            logs: Vec::new(),
            gas_used: 0,
            reason: String::new(),
            revert_data: alloy_primitives::Bytes::new(),
        }
    }

    fn some_err() -> PreconfError {
        PreconfError::Internal("model".into())
    }

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
                    // The slot is taken, full stop — no handover exists any more.
                    Some(_) => {}
                    None => {
                        model.insert(hb, (id.sender, id.nonce, PreconfStatus::Waiting));
                    }
                }
            }
            // Success is recorded in place; a failure by the sole holder is
            // terminal, and terminal means the entry is gone.
            Op::Succeed(id) => model_mark(model, id.hash_byte(), PreconfStatus::Success),
            Op::Fail(id) => {
                let hb = id.hash_byte();
                if matches!(model.get(&hb), Some((_, _, PreconfStatus::Waiting))) {
                    model.remove(&hb);
                }
            }
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
            Op::Succeed(id) => {
                if let Some(hold) = set.register(&id.hash()).await {
                    set.finish_success(hold, receipt(id.hash_byte())).await;
                }
            }
            Op::Fail(id) => {
                if let Some(hold) = set.register(&id.hash()).await {
                    set.finish_failure(hold, some_err()).await;
                }
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
        Fail(u8),
        Succeed(u8),
        Take(u8),
        Cancel(u8),
        Forward(u8),
    }

    fn hb() -> impl Strategy<Value = u8> {
        0..HASHES
    }
    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            hb().prop_map(Op::Attach),
            hb().prop_map(Op::Push),
            hb().prop_map(Op::Fail),
            hb().prop_map(Op::Succeed),
            hb().prop_map(Op::Take),
            hb().prop_map(Op::Cancel),
            (0..=HASHES).prop_map(Op::Forward),
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
                Op::Fail(b) => fail(&set, &mut model, *b, i).await,
                Op::Succeed(b) => succeed(&set, &mut model, *b).await,
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

    /// A build records a success: status flips in place and the responder is
    /// handed the receipt.
    async fn succeed(set: &PreconfTxSet, model: &mut [HashState], b: u8) {
        if let Some(hold) = set.register(&h(b)).await {
            set.finish_success(hold, receipt(b)).await;
        }
        let st = &mut model[b as usize];
        if st.entry == Some(PreconfStatus::Waiting) {
            st.entry = Some(PreconfStatus::Success);
            if let Some((_, rx)) = st.held.take() {
                assert_value(rx, "succeed delivered the receipt");
            }
        }
    }

    /// The sole holder fails: terminal, so the entry is removed and the
    /// responder answered with the error.
    async fn fail(set: &PreconfTxSet, model: &mut [HashState], b: u8, step: usize) {
        if let Some(hold) = set.register(&h(b)).await {
            set.finish_failure(hold, some_err()).await;
        }
        let st = &mut model[b as usize];
        if st.entry == Some(PreconfStatus::Waiting) {
            st.entry = None;
            if let Some((_, rx)) = st.held.take() {
                assert_value(rx, &format!("step {step}: failed entry answered its responder"));
            }
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
