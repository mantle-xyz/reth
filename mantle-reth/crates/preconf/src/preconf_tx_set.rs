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
//! - At most one entry per `(sender, nonce)` whose status is not `Timeout`.
//!   A new push with the same `(sender, nonce)` evicts an existing `Timeout`
//!   entry; an active (Waiting / Success / Failed) entry blocks the push.
//! - At most one responder per hash. Either lives inside an existing entry, or
//!   in `pending_responders` until the matching `push_if_absent` consumes it.
//! - `notifier.send` is best-effort — slow consumers receive `Lagged(n)` and
//!   reconcile via `snapshot()`.

use alloy_consensus::{Transaction, TxEnvelope};
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
use tracing::error;

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
    /// (`PreconfAwareValidator` replacement check).
    by_sender: HashMap<(Address, u64), TxHash>,

    /// RPC handler may attach responder before the listener / pool path
    /// creates the entry. We stash responders here until the matching
    /// `push_if_absent` consumes them.
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

    /// Removes a hash from all indices (`entries` / `by_sender` / `order` /
    /// `pending_responders`). Returns the evicted entry if one existed.
    fn drop_hash(&mut self, hash: &TxHash) -> Option<TxEntry> {
        let entry = self.entries.remove(hash);
        if let Some(ref e) = entry {
            self.by_sender.remove(&(e.from, e.nonce));
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
    /// Sweep yield hint. Set by `push_if_absent` / `recover_from_timeout`,
    /// cleared by the builder when it has drained pending fifo events.
    /// Read-only `false → true → false` — `Ordering::Relaxed` is sufficient.
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
    /// `from` must be the recovered sender; callers (pool listener / RPC
    /// handler) have it pre-validated. Hash + nonce are read from `tx`.
    ///
    /// Returns:
    /// - [`PushResult::Inserted`] — new entry created and broadcast notified.
    /// - [`PushResult::AlreadyExists`] — same hash already present (no-op).
    /// - [`PushResult::ConflictActive(existing_hash)`] — same `(from, nonce)`
    ///   but different hash, and the existing entry is not `Timeout`.
    ///
    /// When the existing entry IS `Timeout`, it is evicted and the new tx
    /// is inserted in its place.
    pub async fn push_if_absent(&self, tx: Arc<TxEnvelope>, from: Address) -> PushResult {
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();

        let mut inner = self.inner.lock().await;

        if inner.entries.contains_key(&hash) {
            return PushResult::AlreadyExists;
        }

        // Replacement check: same `(sender, nonce)` but a different hash.
        // op-geth-equivalent: only a `Timeout` commitment releases the
        // (sender, nonce) slot; Waiting / Success / Failed all block the
        // replacement attempt.
        if let Some(existing_hash) = inner.by_sender.get(&(from, nonce)).copied() {
            let existing_status = inner.entries.get(&existing_hash).map(|e| e.status);
            match existing_status {
                Some(s) if s != PreconfStatus::Timeout => {
                    return PushResult::ConflictActive(existing_hash);
                }
                Some(_) => {
                    // Timeout — evict, then fall through to insert.
                    inner.drop_hash(&existing_hash);
                }
                None => {
                    // Stale by_sender index — `by_sender[(from, nonce)]`
                    // points to a hash with no matching entry. This violates
                    // the internal invariant that `by_sender` and `entries`
                    // are kept in sync; surface it via logs and self-heal.
                    error!(
                        target: "mantle::preconf",
                        sender = ?from,
                        nonce,
                        dangling_hash = ?existing_hash,
                        "preconf_tx_set: dangling by_sender index detected; \
                         self-healing by removing stale entry"
                    );
                    inner.by_sender.remove(&(from, nonce));
                }
            }
        }

        let responder = inner.pending_responders.remove(&hash);
        let entry = TxEntry {
            hash,
            tx,
            from,
            nonce,
            inserted_at: Instant::now(),
            status: PreconfStatus::Waiting,
            responder,
        };
        inner.entries.insert(hash, entry);
        inner.by_sender.insert((from, nonce), hash);
        inner.order.push_back(hash);
        drop(inner);

        // Signal order matters: set the sweep-yield hint BEFORE broadcasting,
        // otherwise a fast builder can drain the broadcast event and call
        // `clear_pending_flag` before the `store(true)` lands — leaving the
        // flag stuck at `true` with no further event to drive it back down.
        self.has_pending.store(true, Ordering::Relaxed);
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

    /// Unconditionally evicts entries with `status == Timeout`. Equivalent to
    /// op-geth `FIFOTxSet::CleanTimeout`. Returns evicted hashes.
    pub async fn clean_timeout(&self) -> Vec<TxHash> {
        let mut inner = self.inner.lock().await;
        let to_drop: Vec<TxHash> = inner
            .entries
            .iter()
            .filter(|(_, e)| e.status == PreconfStatus::Timeout)
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

    /// `Waiting → Failed`. Truly terminal — symmetric to `mark_succeeded`.
    /// Called by builder when EVM apply returned a revert/halt.
    pub async fn mark_failed(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Failed).await
    }

    /// `Waiting → Timeout`. **Soft terminal** — unlike `Success` / `Failed`,
    /// a `Timeout` entry can be revived via [`Self::recover_from_timeout`]
    /// (used by the H4 client-retry path). Called by RPC handler when the
    /// client-side `preconf_timeout` fires before a receipt is delivered.
    pub async fn mark_timeout(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Timeout).await
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

    /// `Timeout → Waiting` + broadcast notify + set `has_pending`. Only legal
    /// from `Timeout`; other states return `NotTimeout(current)`.
    pub async fn recover_from_timeout(&self, hash: &TxHash) -> Result<(), RecoverError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(RecoverError::NotFound)?;
        if entry.status != PreconfStatus::Timeout {
            return Err(RecoverError::NotTimeout(entry.status));
        }
        entry.status = PreconfStatus::Waiting;
        drop(inner);

        // See `push_if_absent` for why the flag is set before the broadcast.
        self.has_pending.store(true, Ordering::Relaxed);
        let _ = self.notifier.send(*hash);
        Ok(())
    }

    // ============ Sweep yield for preconf ============

    /// True when a recent `push_if_absent` / `recover_from_timeout` set the
    /// hint and the builder has not cleared it yet.
    pub fn has_pending_unprocessed(&self) -> bool {
        self.has_pending.load(Ordering::Relaxed)
    }

    /// Cleared by the builder when it has drained the fifo events queue.
    pub fn clear_pending_flag(&self) {
        self.has_pending.store(false, Ordering::Relaxed);
    }

    // ============ Responder slots (RPC path only) ============

    /// Attaches responder. If a matching entry already exists, the responder
    /// is parked inside the entry; otherwise it goes into `pending_responders`
    /// and gets merged at the matching `push_if_absent`.
    ///
    /// Returns `AlreadyAttached` if any responder slot for this hash is
    /// already occupied.
    pub async fn attach_responder(
        &self,
        hash: TxHash,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> Result<(), AttachError> {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.entries.get_mut(&hash) {
            if entry.responder.is_some() {
                return Err(AttachError::AlreadyAttached);
            }
            entry.responder = Some(responder);
            return Ok(());
        }
        if inner.pending_responders.contains_key(&hash) {
            return Err(AttachError::AlreadyAttached);
        }
        inner.pending_responders.insert(hash, responder);
        Ok(())
    }

    /// Cancels the responder for `hash` (if any) with the given error.
    /// No-op if no responder is registered. The send is fire-and-forget —
    /// the receiver may have already dropped (client timed out).
    pub async fn cancel_responder(&self, hash: &TxHash, err: PreconfError) {
        let responder = {
            let mut inner = self.inner.lock().await;
            inner
                .entries
                .get_mut(hash)
                .and_then(|e| e.responder.take())
                .or_else(|| inner.pending_responders.remove(hash))
        };
        if let Some(r) = responder {
            let _ = r.send(Err(err));
        }
    }

    /// Take-once: removes and returns the responder if any. Called by the
    /// builder after a successful apply, to deliver the receipt.
    pub async fn take_responder(
        &self,
        hash: &TxHash,
    ) -> Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>> {
        let mut inner = self.inner.lock().await;
        let from_entry = inner.entries.get_mut(hash).and_then(|e| e.responder.take());
        if from_entry.is_some() {
            return from_entry;
        }
        inner.pending_responders.remove(hash)
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
            .field("has_pending", &self.has_pending.load(Ordering::Relaxed))
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
        assert!(!set.remove(&h(1)).await);
    }

    // ============ push_if_absent ============

    #[tokio::test]
    async fn push_inserts_new_entry_and_broadcasts() {
        let set = PreconfTxSet::new(16);
        let mut rx = set.subscribe();
        let tx = make_tx(0, 1);
        let result = set.push_if_absent(tx.clone(), addr(1)).await;
        assert_eq!(result, PushResult::Inserted);

        assert!(set.contains(tx.tx_hash()).await);
        assert_eq!(set.snapshot().await, vec![*tx.tx_hash()]);
        assert!(set.find_by_sender_nonce(&addr(1), 0).await.is_some());
        assert!(set.has_pending_unprocessed());
        assert_eq!(rx.try_recv().unwrap(), *tx.tx_hash());
    }

    #[tokio::test]
    async fn push_same_hash_returns_already_exists() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        assert_eq!(set.push_if_absent(tx.clone(), addr(1)).await, PushResult::Inserted);
        assert_eq!(set.push_if_absent(tx.clone(), addr(1)).await, PushResult::AlreadyExists);
        // Single entry — no duplicates.
        assert_eq!(set.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn push_conflict_active_blocks_replacement() {
        let set = PreconfTxSet::new(16);
        let tx1 = make_tx(0, 1);
        let tx2 = make_tx(0, 2); // same nonce, different hash
        assert_eq!(set.push_if_absent(tx1.clone(), addr(1)).await, PushResult::Inserted);
        assert_eq!(
            set.push_if_absent(tx2.clone(), addr(1)).await,
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
        set.push_if_absent(tx1.clone(), addr(1)).await;
        set.mark_timeout(tx1.tx_hash()).await.unwrap();

        let r = set.push_if_absent(tx2.clone(), addr(1)).await;
        assert_eq!(r, PushResult::Inserted);
        assert!(!set.contains(tx1.tx_hash()).await);
        assert!(set.contains(tx2.tx_hash()).await);
    }

    // ============ status transitions ============

    #[tokio::test]
    async fn mark_succeeded_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn mark_failed_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        set.mark_failed(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Failed);
    }

    #[tokio::test]
    async fn mark_timeout_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        set.mark_timeout(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Timeout);
    }

    #[tokio::test]
    async fn second_transition_rejects_non_waiting_source() {
        // Any subsequent mark_* after the first must hit IllegalTransition.
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
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

    // ============ recover_from_timeout ============

    #[tokio::test]
    async fn recover_from_timeout_returns_to_waiting_and_notifies() {
        let set = PreconfTxSet::new(16);
        let mut rx = set.subscribe();
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        let _ = rx.try_recv(); // drain push notify
        set.mark_timeout(tx.tx_hash()).await.unwrap();
        set.clear_pending_flag();

        set.recover_from_timeout(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Waiting);
        assert_eq!(rx.try_recv().unwrap(), *tx.tx_hash());
        assert!(set.has_pending_unprocessed());
    }

    #[tokio::test]
    async fn recover_from_non_timeout_rejects() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        let err = set.recover_from_timeout(tx.tx_hash()).await.unwrap_err();
        assert_eq!(err, RecoverError::NotTimeout(PreconfStatus::Waiting));
    }

    #[tokio::test]
    async fn recover_from_timeout_unknown_hash() {
        let set = PreconfTxSet::new(16);
        let err = set.recover_from_timeout(&h(99)).await.unwrap_err();
        assert_eq!(err, RecoverError::NotFound);
    }

    // ============ forward ============

    #[tokio::test]
    async fn forward_drops_older_nonces_only() {
        let set = PreconfTxSet::new(16);
        let t5 = make_tx(5, 5);
        let t6 = make_tx(6, 6);
        let t7 = make_tx(7, 7);
        let other = make_tx(5, 50); // different sender
        set.push_if_absent(t5.clone(), addr(1)).await;
        set.push_if_absent(t6.clone(), addr(1)).await;
        set.push_if_absent(t7.clone(), addr(1)).await;
        set.push_if_absent(other.clone(), addr(2)).await;

        set.forward(&addr(1), 7).await;

        assert!(!set.contains(t5.tx_hash()).await);
        assert!(!set.contains(t6.tx_hash()).await);
        assert!(set.contains(t7.tx_hash()).await);
        assert!(set.contains(other.tx_hash()).await); // unrelated sender untouched
    }

    // ============ clean_timeout ============

    #[tokio::test]
    async fn clean_timeout_evicts_only_timeout_entries() {
        let set = PreconfTxSet::new(16);
        let t1 = make_tx(0, 1);
        let t2 = make_tx(1, 2);
        let t3 = make_tx(2, 3);
        set.push_if_absent(t1.clone(), addr(1)).await;
        set.push_if_absent(t2.clone(), addr(2)).await;
        set.push_if_absent(t3.clone(), addr(3)).await;
        set.mark_timeout(t2.tx_hash()).await.unwrap();

        let evicted = set.clean_timeout().await;
        assert_eq!(evicted, vec![*t2.tx_hash()]);
        assert!(set.contains(t1.tx_hash()).await);
        assert!(!set.contains(t2.tx_hash()).await);
        assert!(set.contains(t3.tx_hash()).await);
    }

    // ============ responder lifecycle ============

    #[tokio::test]
    async fn attach_responder_to_existing_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;

        let (s, _r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), s).await.unwrap();

        // Take-once: first take returns Some, second returns None.
        assert!(set.take_responder(tx.tx_hash()).await.is_some());
        assert!(set.take_responder(tx.tx_hash()).await.is_none());
    }

    #[tokio::test]
    async fn attach_responder_before_push_merges_on_push() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let (s, _r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), s).await.unwrap();

        // Pre-push: responder lives in pending_responders.
        // Push must consume it and move it into the entry.
        set.push_if_absent(tx.clone(), addr(1)).await;
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

        set.attach_responder(*tx.tx_hash(), s1).await.unwrap();
        set.push_if_absent(tx.clone(), addr(1)).await;

        let err = set.attach_responder(*tx.tx_hash(), s2).await.unwrap_err();
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

        set.attach_responder(*tx.tx_hash(), s).await.unwrap();
        set.push_if_absent(tx.clone(), addr(1)).await;
        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;

        let received = r.await.unwrap();
        assert_eq!(received, Err(PreconfError::NotPreconfEligible));
    }

    #[tokio::test]
    async fn attach_twice_returns_already_attached() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        let (s1, _r1) = oneshot::channel();
        let (s2, _r2) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), s1).await.unwrap();
        let err = set.attach_responder(*tx.tx_hash(), s2).await.unwrap_err();
        assert_eq!(err, AttachError::AlreadyAttached);
    }

    #[tokio::test]
    async fn cancel_responder_sends_error_to_receiver() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        let (s, r) = oneshot::channel();
        set.attach_responder(*tx.tx_hash(), s).await.unwrap();

        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;
        let received = r.await.unwrap();
        assert_eq!(received, Err(PreconfError::NotPreconfEligible));
    }

    #[tokio::test]
    async fn cancel_responder_silently_drops_when_none_attached() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        // No attach — cancel is a no-op.
        set.cancel_responder(tx.tx_hash(), PreconfError::NotPreconfEligible).await;
    }

    // ============ get_tx ============

    #[tokio::test]
    async fn get_tx_returns_arc_clone() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1)).await;
        let fetched = set.get_tx(tx.tx_hash()).await.unwrap();
        assert!(Arc::ptr_eq(&fetched, &tx));
    }

    #[tokio::test]
    async fn get_tx_returns_none_when_absent() {
        let set = PreconfTxSet::new(16);
        assert!(set.get_tx(&h(99)).await.is_none());
    }
}
