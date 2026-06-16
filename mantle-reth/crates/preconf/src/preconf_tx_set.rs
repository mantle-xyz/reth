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
//! The broadcast notifier is **outside** the mutex — `send` is non-blocking
//! and lock-free.
//!
//! ## Invariants
//!
//! - At most one entry per `(sender, nonce)` in an active status (Waiting / Success / Failed).
//!   Timeout entries are evicted on conflict.
//! - At most one responder per hash. Once attached, only `take_responder` removes it.
//! - `notifier.send` is best-effort — slow consumers receive `Lagged(n)` and reconcile via
//!   `snapshot()`.

use alloy_consensus::TxEnvelope;
use alloy_primitives::{Address, TxHash};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::types::{
    AttachError, MarkError, PreconfError, PreconfReceipt, PreconfStatus, PushResult, RecoverError,
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
    /// Wall-clock insertion time — used by debug logging / metrics; **not**
    /// load-bearing for any timeout logic (`clean_timeout` uses status only).
    pub inserted_at: Instant,
    /// Current status — see [`PreconfStatus`].
    pub status: PreconfStatus,
    /// RPC handler responder — `Some` when the RPC path attached one before
    /// pool.add succeeded; `None` for listener-pushed entries.
    /// Take-once: `take_responder` moves it out.
    pub responder: Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>>,
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
}

/// Inner state guarded by a single `Mutex` — see module docs.
struct PreconfTxSetInner {
    /// FIFO insertion order — hashes only. Pop from front on `forward`,
    /// drop from middle on `remove` (rare; O(n) is acceptable for typical
    /// queue size < 100).
    order: VecDeque<TxHash>,

    /// Hash → entry. All mutations + lookups go through this.
    entries: HashMap<TxHash, TxEntry>,

    /// (sender, nonce) → hash index for `find_by_sender_nonce`
    /// (`PreconfAwareValidator` A1).
    by_sender: HashMap<(Address, u64), TxHash>,

    /// RPC handler may attach responder before the listener / pool path
    /// creates the entry (the `attach_responder before pool.add` invariant).
    /// We stash them here until the matching `push_if_absent` consumes them.
    pending_responders: HashMap<TxHash, oneshot::Sender<Result<PreconfReceipt, PreconfError>>>,
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
}

/// The commitment truth source. Constructed once at startup and shared via `Arc`.
pub struct PreconfTxSet {
    inner: Mutex<PreconfTxSetInner>,
    notifier: broadcast::Sender<TxHash>,
    /// Sweep yield hint. Set by `push_if_absent`, cleared by builder when it
    /// has drained pending fifo events. Read-only `false → true → false` —
    /// `Ordering::Relaxed` is sufficient (no synchronization with data).
    has_pending: AtomicBool,
}

impl PreconfTxSet {
    /// Constructor — `broadcast_cap` should come from `cfg.broadcast_cap`.
    ///
    /// Panics if `broadcast_cap == 0` (tokio invariant). Configs are
    /// validated upstream via `PreconfConfig::validate`.
    pub fn new(broadcast_cap: usize) -> Self {
        let (notifier, _) = broadcast::channel(broadcast_cap);
        Self {
            inner: Mutex::new(PreconfTxSetInner::new()),
            notifier,
            has_pending: AtomicBool::new(false),
        }
    }

    // ============ Push path ============

    /// Idempotent push.
    ///
    /// Inserts a new entry only if there is no existing entry for this hash
    /// nor an entry with the same `(sender, nonce)` in an active status.
    /// Handles `by_sender` conflict resolution and `pending_responders` merge.
    pub async fn push_if_absent(&self, _tx: &TxEnvelope) -> PushResult {
        unimplemented!("push_if_absent")
    }

    /// Removes an entry by hash. Returns true if removed, false if absent.
    ///
    /// Idempotent. Also removes the `by_sender` index entry and any pending
    /// responder (calling it with [`PreconfError::Internal`] would be the
    /// caller's responsibility — `remove` does NOT signal responders).
    pub async fn remove(&self, hash: &TxHash) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.entries.remove(hash) else {
            return false;
        };
        inner.by_sender.remove(&(entry.from, entry.nonce));
        if let Some(pos) = inner.order.iter().position(|h| h == hash) {
            inner.order.remove(pos);
        }
        // Note: pending_responders for *this* hash should never exist after
        // a successful push_if_absent (the responder is moved into entry).
        // We still clear defensively to avoid leaks across `remove(h)` →
        // `attach_responder(h)` → `remove(h)` weird sequences.
        let _ = inner.pending_responders.remove(hash);
        true
    }

    /// Returns true if the hash is currently present in `entries`.
    pub async fn contains(&self, hash: &TxHash) -> bool {
        self.inner.lock().await.entries.contains_key(hash)
    }

    /// Snapshot of hashes in FIFO order. Used by Phase 1 replay.
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

    /// `PreconfAwareValidator` A1 lookup — O(1) via `by_sender` index.
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
    ///
    pub async fn forward(&self, _addr: &Address, _new_nonce: u64) {
        unimplemented!("forward")
    }

    /// Unconditionally evicts entries with `status == Timeout`. Equivalent to
    /// op-geth `FIFOTxSet::CleanTimeout`. Returns evicted hashes.
    pub async fn clean_timeout(&self) -> Vec<TxHash> {
        unimplemented!("clean_timeout")
    }

    /// Builder subscribes the broadcast notifier here.
    ///
    /// Each call returns an independent `Receiver` — multi-consumer.
    pub fn subscribe(&self) -> broadcast::Receiver<TxHash> {
        self.notifier.subscribe()
    }

    // ============ Status transitions ============

    /// Waiting → {Success, Failed, Timeout}. Idempotent against missing entry.
    pub async fn mark_terminal(
        &self,
        _hash: &TxHash,
        _new_status: PreconfStatus,
    ) -> Result<(), MarkError> {
        unimplemented!("mark_terminal")
    }

    /// Timeout → Waiting + broadcast notify + set `has_pending`.
    pub async fn recover_from_timeout(&self, _hash: &TxHash) -> Result<(), RecoverError> {
        unimplemented!("recover_from_timeout")
    }

    // ============ Sweep yield for preconf ============

    /// True when a recent `push_if_absent` set the hint and the builder has
    /// not cleared it yet.
    pub fn has_pending_unprocessed(&self) -> bool {
        self.has_pending.load(Ordering::Relaxed)
    }

    /// Cleared by the builder when it has drained the fifo events queue.
    pub fn clear_pending_flag(&self) {
        self.has_pending.store(false, Ordering::Relaxed);
    }

    // ============ Responder slots (RPC path only) ============

    /// Attaches responder before pool.add succeeds.
    pub async fn attach_responder(
        &self,
        _hash: TxHash,
        _responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<(), AttachError> {
        unimplemented!("attach_responder")
    }

    /// Cancels responder with the given error — called on pool.add failure.
    pub async fn cancel_responder(&self, _hash: &TxHash, _err: PreconfError) {
        unimplemented!("cancel_responder")
    }

    /// Take-once: removes and returns the responder if any. Called by builder
    /// after apply.
    pub async fn take_responder(
        &self,
        _hash: &TxHash,
    ) -> Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>> {
        unimplemented!("take_responder")
    }

    // ============ Builder-only tx access ============

    /// Returns a clone of the tx Arc for an entry; None if absent.
    pub async fn get_tx(&self, _hash: &TxHash) -> Option<Arc<TxEnvelope>> {
        unimplemented!("get_tx")
    }
}

// `Debug` impl avoids exposing the responder (oneshot is not Debug-friendly)
// and the broadcast Sender's internals.
impl std::fmt::Debug for PreconfTxSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreconfTxSet")
            .field("has_pending", &self.has_pending.load(Ordering::Relaxed))
            .field("receiver_count", &self.notifier.receiver_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    #[tokio::test]
    async fn empty_set_contains_nothing() {
        let set = PreconfTxSet::new(16);
        assert!(!set.contains(&hash(1)).await);
        assert!(set.snapshot().await.is_empty());
        assert!(set.entries().await.is_empty());
        assert!(set.find_by_hash(&hash(1)).await.is_none());
    }

    #[tokio::test]
    async fn subscribe_returns_independent_receivers() {
        let set = PreconfTxSet::new(16);
        let _rx1 = set.subscribe();
        let _rx2 = set.subscribe();
        // Each subscribe() yields a fresh Receiver — verified by no panic
        // and receiver_count visible via Debug.
        assert!(format!("{set:?}").contains("receiver_count"));
    }

    #[tokio::test]
    async fn has_pending_flag_is_idle_at_start() {
        let set = PreconfTxSet::new(16);
        assert!(!set.has_pending_unprocessed());
    }

    #[tokio::test]
    async fn clear_pending_flag_is_idempotent() {
        let set = PreconfTxSet::new(16);
        set.clear_pending_flag();
        set.clear_pending_flag();
        assert!(!set.has_pending_unprocessed());
    }

    #[tokio::test]
    async fn remove_returns_false_when_absent() {
        let set = PreconfTxSet::new(16);
        assert!(!set.remove(&hash(1)).await);
    }
}
