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
    collections::VecDeque,
    sync::{Arc, OnceLock},
    time::Instant,
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
    /// [`PreconfTxSet::inner`] — that direction would deadlock.
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
        Self { inner: Mutex::new(PreconfTxSetInner::new()), notifier, pool_evict: OnceLock::new() }
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

        // Same-hash entry already present — three outcomes depending on
        // its current status:
        //
        // - `Timeout` / `Canceled` / `Failed` (reclaimable) → revive to `Waiting`. Adopt the fresh
        //   responder + origin_instant from `pending_responders` (if the RPC handler pre-attached
        //   one for this resubmit) so dispatch's deadline gate measures against the new submission,
        //   then broadcast. Closes the "same-hash resubmit after non-Success terminal" loop that
        //   would otherwise wedge under the pool-eviction callback. `Failed` is included because
        //   its trigger — reth builder pre-execute reject (in-flight nonce / balance race, block
        //   gas exhausted at builder level) — is typically a within-slot state race, not an
        //   intrinsic tx defect; next-slot retry naturally resolves it.
        // - `Waiting` / `Success` (active) → idempotent no-op; callers should treat this as
        //   "someone else already owns the slot" (the RPC handler surfaces it as
        //   `AlreadyInProgress`).
        if let Some(existing) = inner.entries.get_mut(&hash) {
            if matches!(
                existing.status,
                PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed
            ) {
                // `attach_responder`'s reclaimable-state branch already
                // installed the fresh responder + refreshed
                // `inserted_at` before this push landed. All we need
                // here is the status flip + broadcast that turns the
                // entry back into a live dispatch candidate.
                existing.status = PreconfStatus::Waiting;
                drop(inner);
                let _ = self.notifier.send(hash);
                return PushResult::Revived;
            }
            return PushResult::AlreadyExists;
        }

        // Replacement check: same `(sender, nonce)` but a different hash.
        // Reclaimable terminal states release the slot — `Timeout`
        // (client deadline), `Canceled` (block-gas-budget pre-apply reject), and
        // `Failed` (reth builder pre-execute reject; tx NOT on chain,
        // same "safe to replace" property as the other two).
        // `Waiting` / `Success` block replacement (`Success` is either
        // on chain or in-flight, replacement would double-apply).
        if let Some(existing_hash) = inner.by_sender.get(&(from, nonce)).copied() {
            let existing_status = inner.entries.get(&existing_hash).map(|e| e.status);
            match existing_status {
                Some(s)
                    if !matches!(
                        s,
                        PreconfStatus::Timeout | PreconfStatus::Canceled | PreconfStatus::Failed
                    ) =>
                {
                    return PushResult::ConflictActive(existing_hash);
                }
                Some(_) => {
                    // Timeout / Canceled / Failed — evict, then fall through to insert.
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

    /// Removes an entry by hash. Returns true if removed, false if absent.
    ///
    /// Idempotent. Also removes the `by_sender` index entry and any pending
    /// responder. Does NOT signal the responder — callers that want to
    /// fail-fast the RPC client should use `cancel_responder` instead.
    pub async fn remove(&self, hash: &TxHash) -> bool {
        let mut inner = self.inner.lock().await;
        inner.drop_hash(hash).is_some()
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

    /// Builder subscribes the broadcast notifier here.
    ///
    /// Each call returns an independent `Receiver` — multi-consumer.
    pub fn subscribe(&self) -> broadcast::Receiver<TxHash> {
        self.notifier.subscribe()
    }

    // ============ Status transitions ============

    /// `Waiting → Success`. Truly terminal — once set, only `forward` or
    /// `remove` can drop the entry; the status itself never moves again.
    /// Called by builder after a successful EVM apply.
    pub async fn mark_succeeded(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Success).await
    }

    /// `Waiting → Failed`. **Soft terminal** — like `Timeout` /
    /// `Canceled`, a `Failed` entry is **revivable** via same-hash
    /// resubmit (`push_if_absent`'s Timeout/Canceled/Failed → Waiting
    /// branch). Called by builder when `apply_fn` returned Err
    /// (in-flight nonce / balance race, block gas exhausted at builder
    /// level). **tx NOT on chain**.
    ///
    /// Reclaim rationale: all three "not on chain" causes are typically
    /// transient (deadline overreach, block gas budget resets next slot, or
    /// in-flight state race that clears when the next slot's fresh
    /// block state comes in). SDKs retry all three the same way.
    ///
    /// On success, invokes the pool-eviction callback (if registered)
    /// to synchronously remove `hash` from the transaction pool. This
    /// closes the SLA window where the client saw `Failed` but the tx
    /// could still land later via the pool best-tx iterator.
    pub async fn mark_failed(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Failed).await?;
        self.evict_from_pool(*hash);
        Ok(())
    }

    /// `Waiting → Timeout`. **Soft terminal** — a `Timeout` entry is
    /// **revivable**: a same-hash retry from the client re-runs through
    /// `attach_responder` (which refreshes the responder + `inserted_at`)
    /// and the subsequent listener push through [`Self::push_if_absent`]
    /// flips the entry back to `Waiting`. Called by the RPC handler when
    /// the client-side `preconf_timeout` fires before a receipt is
    /// delivered, or by dispatch's pre-apply deadline gate.
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
    /// rejection** (block gas budget exhausted, admin action, ...) — the
    /// EVM was never run, so the tx is guaranteed not to land on chain.
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
                // Reclaimable — this is a same-hash retry after a
                // `Timeout` (client deadline), `Canceled` (block-gas-budget pre-apply
                // reject), or `Failed` (reth builder pre-execute reject;
                // tx NOT on chain). Install the fresh responder and
                // refresh `inserted_at` so `builder::dispatch`'s
                // deadline gate measures against the second submission
                // rather than the (already-expired) first. The
                // subsequent `push_if_absent` from the pool listener
                // flips the entry back to `Waiting` and broadcasts.
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
    async fn remove_returns_false_when_absent() {
        let set = PreconfTxSet::new(16);
        assert!(!set.remove(&h(1)).await);
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
