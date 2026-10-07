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
//! The broadcast notifier is signalled **outside** it — that fans out to every
//! subscriber. A client's `oneshot` responder is answered **inside** it, so
//! that an entry leaving the queue and its client learning why are one event
//! rather than two; see "Completion protocol" in the impl. Both sends are
//! non-blocking and lock-free, so neither can re-enter the queue.
//!
//! ## Invariants
//!
//! - At most one entry per `(sender, nonce)`, and it blocks a push for a different hash on that
//!   `(sender, nonce)`. Every entry present is live, so the incumbent always wins — a commitment
//!   that ends is removed rather than parked. See [`crate::types::PreconfStatus`].
//! - At most one responder per hash, and it lives inside the entry. It is installed with the entry,
//!   under the same lock, so the builder cannot reach an entry that has nobody to answer — which is
//!   what let the stash this replaced be deleted.
//! - `notifier.send` is best-effort — slow consumers receive `Lagged(n)` and reconcile via
//!   `snapshot()`.
//!
//! ## Lock order
//!
//! ```text
//! builder (sync):  writes AccountView only, never touches `inner`
//! admit   (async): reads AccountView first, then takes `inner`
//! forbidden:       taking `inner` while holding the AccountView write lock
//! ```
//!
//! Acyclic, so there is no order to get wrong. The worst race between the two:
//! `admit` reads a nonce the builder consumes a moment later, the transaction
//! hits `nonce_too_low` at apply and the client is told `BuilderRejected`. Not a
//! safety problem, and strictly better than waiting out the timeout.
//!
//! Sibling of the older invariant on [`TxEntry::apply_lock`] — never acquired
//! while holding `inner`.

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
// foldhash HashMap: faster than SipHash on high-entropy keys (TxHash /
// Address); matches the allowlist sets in `whitelist.rs`.
// `HashMapExt` brings `::new()` / `::with_capacity()` into scope.
use alloy_primitives::{
    Address, B256, KECCAK256_EMPTY, TxHash,
    map::foldhash::{HashMap, HashMapExt},
};
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{Arc, OnceLock},
    time::Instant,
};
use tokio::sync::{Mutex, OwnedMutexGuard, broadcast, oneshot};
use tracing::{debug, error};

use crate::{
    classifier::PreconfClassifier,
    types::{MarkError, PreconfError, PreconfReceipt, PreconfSource, PreconfStatus, PushResult},
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
    /// Encoded size in bytes, against the queue's byte ceiling. Kept rather
    /// than re-encoded: the ceiling is checked on every admission and the
    /// figure never changes.
    pub size: usize,
    /// Wall-clock insertion time. Load-bearing: `builder::dispatch`'s
    /// pre-apply deadline check ends the commitment when
    /// `elapsed + SAFETY_MARGIN >= preconf_timeout`.
    pub inserted_at: Instant,
    /// Current status — see [`PreconfStatus`].
    pub status: PreconfStatus,
    /// Origin of the entry — see [`PreconfSource`]. Determines which
    /// pre-apply gates `builder::dispatch::apply_one_preconf` enforces.
    pub source: PreconfSource,
    /// The client's channel — `Some` for an RPC submission, `None` for a
    /// replay. Taken only by a completion (`complete_failure` /
    /// `begin_success`), which answers it in the same step.
    pub responder: Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>>,
    /// Per-entry lock serialising dispatch's completion with the RPC deadline
    /// branch. A completion holds it from the decision through the send, so a
    /// deadline acquiring it afterwards finds the result already in the
    /// channel. Never held while acquiring `PreconfTxSet::inner` — that
    /// direction deadlocks.
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

/// How much the queue may hold, read from the node's own `PoolConfig` so
/// that tuning `--txpool.*` moves both paths together.
///
/// Passed in rather than stored: the queue exists before the pool config is
/// known, and the alternative is a setter nobody can see was never called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// Total entries.
    pub max_txs: usize,
    /// Total encoded bytes.
    pub max_size: usize,
    /// Entries one sender may hold.
    pub max_account_slots: usize,
    /// Entries a sender with an EIP-7702 delegation may hold.
    pub max_inflight_delegated: usize,
    /// Summed `gas_limit` over every entry.
    ///
    /// The other three bound what the queue costs to hold. This one bounds
    /// whether it can drain: a block absorbs at most `preconf_max_gas_per_block`
    /// of preconf gas, so a backlog larger than a client's deadline is worth of
    /// blocks is a queue of promises that cannot be kept. Refusing on arrival
    /// beats accepting and timing out.
    pub max_queued_gas: u64,
}

/// What admission needs about a transaction that the queue cannot work out
/// for itself.
#[derive(Debug)]
pub struct AdmitRequest {
    /// The signed transaction.
    pub tx: Arc<TxEnvelope>,
    /// Recovered sender.
    pub from: Address,
    /// Where the request came from.
    pub source: PreconfSource,
    /// The client's channel, paired with the instant its request arrived, so
    /// the dispatch deadline measures the budget the client is actually
    /// waiting out rather than the time this call happened to run.
    pub responder: Option<(Instant, oneshot::Sender<Result<PreconfReceipt, PreconfError>>)>,
    /// The sender's nonce as of the parent block. The queue layers the block
    /// being built on top of it — see [`PreconfTxSet::next_nonce`].
    pub chain_nonce: u64,
    /// The sender's on-chain code hash, from the validator's outcome. Neither
    /// absent nor `KECCAK_EMPTY` means it carries an EIP-7702 delegation.
    pub bytecode_hash: Option<B256>,
}

/// A receipt owed to a client, held back until the journal has it.
///
/// `#[must_use]`: dropping one leaves a client waiting on a transaction that
/// did land.
#[must_use = "a success ticket holds a client's receipt; send it once the journal has the commitment"]
#[derive(Debug)]
pub struct SuccessTicket {
    responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
}

impl SuccessTicket {
    /// Deliver the receipt. Call once the journal write has returned, with the
    /// entry's `apply_lock` still held.
    pub fn send(self, receipt: PreconfReceipt) {
        let _ = self.responder.send(Ok(receipt));
    }
}

/// A hash-keyed eviction callback: `PreconfTxSet` fires these outward at
/// removal time so it never has to hold a reference to
/// the pool or the classifier (which would close a dependency cycle).
type EvictFn = Arc<dyn Fn(TxHash) + Send + Sync>;

/// Inner state guarded by a single `Mutex` — see module docs.
struct PreconfTxSetInner {
    /// FIFO insertion order — hashes only. Steady-state size is bounded by the
    /// `forward_all` sweep each payload build opens with (~2s / block on L2);
    /// worst-case burst is bounded by pool ingestion rate.
    order: VecDeque<TxHash>,

    /// Hash → entry. All mutations + lookups go through this.
    entries: HashMap<TxHash, TxEntry>,

    /// sender → its in-flight nonces, in order.
    ///
    /// Nested and ordered rather than keyed on `(sender, nonce)`, because
    /// admission asks three questions of a sender's whole chain and none of
    /// them can be answered from a flat key: how many entries it holds, how far
    /// its nonces run without a gap, and what they cost in total. A flat map
    /// answers each by scanning every entry in the fifo.
    by_sender: HashMap<Address, BTreeMap<u64, TxHash>>,

    /// Record-eviction callback, fired from [`Self::drop_hash`].
    ///
    /// The **same** `OnceLock` as [`PreconfTxSet::record_evict`] — held here
    /// too because `drop_hash` is a method on the inner type and cannot reach
    /// the outer one. Sharing the cell (rather than copying the closure) keeps
    /// registration a single lock-free `set` on the outer handle.
    record_evict: Arc<OnceLock<EvictFn>>,
}

impl PreconfTxSetInner {
    fn new(record_evict: Arc<OnceLock<EvictFn>>) -> Self {
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
            by_sender: HashMap::new(),
            record_evict,
        }
    }

    /// Removes a hash from all indices (`entries` / `by_sender` / `order`).
    /// Returns the evicted entry if one existed.
    ///
    /// Fast path uses `entry.from + entry.nonce` to key into `by_sender`
    /// directly. Slow path (below) is a defensive fallback: no known caller
    /// path should ever hit it — all normal eviction routes populate
    /// `entries[hash]` before calling `drop_hash`. It exists purely to
    /// recover from unexpected torn state (e.g. a future bug that partially
    /// evicts an entry) so the "clean all indices" contract stays honest.
    fn drop_hash(&mut self, hash: &TxHash) -> Option<TxEntry> {
        let entry = self.unindex(hash);
        if let Some(pos) = self.order.iter().position(|h| h == hash) {
            self.order.remove(pos);
        }
        Self::report_unanswered(entry.as_ref());
        entry
    }

    /// An entry leaving with its responder still attached is a client that
    /// will never be told anything. Reported, not prevented: this is where
    /// every removal converges, so it catches paths that do not exist yet, but
    /// it cannot invent an answer for one.
    ///
    /// Not a `Drop` impl on [`TxEntry`]: the queue is legitimately torn down
    /// with live responders, and an assertion there would fire while
    /// unwinding.
    fn report_unanswered(entry: Option<&TxEntry>) {
        if entry.is_some_and(|e| e.responder.is_some()) {
            let hash = entry.map(|e| e.hash);
            error!(
                target: "mantle::preconf",
                ?hash,
                "entry removed while a client was still waiting on it; \
                 the completion protocol was bypassed"
            );
            metrics::counter!("preconf.fifo.responder_dropped_total").increment(1);
        }
    }

    /// [`Self::drop_hash`] for many hashes at once.
    ///
    /// Leaving `order` is what costs: a queue has no index, so removing one
    /// hash means finding it, and doing that per hash costs the queue's length
    /// each time. Nothing at the handful of commitments preconf alone holds;
    /// quadratic at the tens of thousands a journal replay restores. One pass
    /// drops them all.
    fn drop_hashes(&mut self, hashes: &[TxHash]) {
        if hashes.is_empty() {
            return;
        }
        for hash in hashes {
            let entry = self.unindex(hash);
            Self::report_unanswered(entry.as_ref());
        }
        let dropping: HashSet<TxHash> = hashes.iter().copied().collect();
        self.order.retain(|hash| !dropping.contains(hash));
    }

    /// Drop one `(sender, nonce)` from the nested index, and the sender with
    /// it once nothing of theirs is left.
    ///
    /// Pruning the empty map is not tidiness: `senders()` and `forward_all`
    /// both walk the outer keys, and a sender that kept an empty bucket would
    /// be swept for a chain it no longer has on every build.
    fn unindex_sender_nonce(
        by_sender: &mut HashMap<Address, BTreeMap<u64, TxHash>>,
        from: Address,
        nonce: u64,
    ) {
        if let Some(nonces) = by_sender.get_mut(&from) {
            nonces.remove(&nonce);
            if nonces.is_empty() {
                by_sender.remove(&from);
            }
        }
    }

    /// Remove `hash` from every index except `order`, returning its entry.
    ///
    /// Split from [`Self::drop_hash`] because `order` is the one index that
    /// cannot be left in constant time, so the two ways of leaving it — find
    /// this one, or sweep for all of them — share everything else.
    fn unindex(&mut self, hash: &TxHash) -> Option<TxEntry> {
        let entry = self.entries.remove(hash);
        if let Some(ref e) = entry {
            Self::unindex_sender_nonce(&mut self.by_sender, e.from, e.nonce);
        } else {
            // Slow path — unreachable in nominal operation; only fires when
            // `entries[hash]` was already gone (defensive self-heal). O(n)
            // linear scan; acceptable because it should never run in prod.
            self.by_sender.retain(|_, nonces| {
                nonces.retain(|_, v| v != hash);
                !nonces.is_empty()
            });
        }

        // The commitment record goes with the entry: it exists to stop the pool arm
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
        if let Some(f) = self.record_evict.get() {
            f(*hash);
        }
        entry
    }
}

/// Where a sender's next usable nonce is measured from, for the block being
/// built.
///
/// An enum rather than two maps because the variants are mutually exclusive:
/// once a transaction states its own nonce, that answer already covers
/// everything that ran before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonceBasis {
    /// A transaction carrying its own nonce has executed: this *is* the next
    /// usable nonce.
    Absolute(u64),
    /// Only nonce-less transactions have executed for this sender — deposits
    /// and the post-execution transaction. They advance the account nonce but
    /// report `0` for it, so only the count can be recorded; the chain
    /// supplies where to count from.
    BumpsFromChain(u64),
}

/// What the block currently being built has done to the accounts the
/// admission path asks about.
///
/// Held under `parking_lot::RwLock` **beside** the `tokio::sync::Mutex` on
/// `inner`, never inside it, for the same reason the classifier's record
/// store is (see `classifier.rs` "Locking"): the writer is the build loop, a
/// sync `fn` that cannot `.await`.
///
/// Cleared at the start of every build rather than re-seeded, so a cancelled
/// or discarded build cannot leave a sender pinned at a nonce the chain will
/// never reach — an absent sender falls back to chain state, always a valid
/// answer.
///
/// **Known window:** a superseded build is cancelled asynchronously, so it can
/// still record a transaction after its replacement cleared the map. The
/// residue reads high, never low, and high only costs a `nonce_too_low` at
/// apply. A generation token on the reset would close it if it ever matters.
#[derive(Debug, Default)]
struct AccountView {
    /// Only senders this build has touched. Absent ⇒ ask the chain.
    basis: HashMap<Address, NonceBasis>,
}

impl AccountView {
    /// Start of a build: forget the previous one.
    fn reset(&mut self) {
        self.basis.clear();
    }

    /// A transaction carrying its own nonce has executed.
    fn observe_executed(&mut self, signer: Address, nonce: u64) {
        let next = nonce.saturating_add(1);
        let basis = match self.basis.get(&signer) {
            // Transactions do not have to arrive in nonce order; only the
            // highest means anything.
            Some(NonceBasis::Absolute(held)) => NonceBasis::Absolute((*held).max(next)),
            // An absolute answer supersedes the count: this transaction's own
            // nonce already reflects the deposits that ran ahead of it.
            _ => NonceBasis::Absolute(next),
        };
        self.basis.insert(signer, basis);
    }

    /// A transaction that carries no nonce of its own has executed — see
    /// [`NonceBasis::BumpsFromChain`].
    fn observe_nonceless(&mut self, signer: Address) {
        let basis = match self.basis.get(&signer) {
            // The exact value is already known, so stay exact.
            Some(NonceBasis::Absolute(held)) => NonceBasis::Absolute(held.saturating_add(1)),
            Some(NonceBasis::BumpsFromChain(n)) => NonceBasis::BumpsFromChain(n.saturating_add(1)),
            None => NonceBasis::BumpsFromChain(1),
        };
        self.basis.insert(signer, basis);
    }

    /// The next nonce `sender` can use, given what the chain says.
    ///
    /// Composed here rather than handing callers the raw basis: an answer
    /// below the truth refuses a transaction that was in order, and that rule
    /// should live in one place.
    fn next_nonce(&self, sender: &Address, chain_nonce: u64) -> u64 {
        match self.basis.get(sender) {
            // Clamped up: a discarded build can leave a value behind that
            // the chain has since passed, and stale-low is the direction that
            // refuses valid transactions.
            Some(NonceBasis::Absolute(held)) => (*held).max(chain_nonce),
            // No clamp possible here, and none needed — double-counting after
            // the chain moves on reads high, and clears at the next reset.
            Some(NonceBasis::BumpsFromChain(n)) => chain_nonce.saturating_add(*n),
            None => chain_nonce,
        }
    }
}

/// The commitment truth source. Constructed once at startup and shared via `Arc`.
pub struct PreconfTxSet {
    inner: Mutex<PreconfTxSetInner>,

    /// What the in-flight build has executed — see [`AccountView`]. Beside
    /// `inner`, never within it; see the module's "Lock order".
    accounts: RwLock<AccountView>,

    notifier: broadcast::Sender<TxHash>,

    /// Record-eviction callback — see
    /// [`Self::set_record_eviction_callback`]. The same cell is held by
    /// `PreconfTxSetInner`, which is what actually fires it (from
    /// `drop_hash`).
    record_evict: Arc<OnceLock<EvictFn>>,
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
        let record_evict = Arc::new(OnceLock::new());
        Self {
            inner: Mutex::new(PreconfTxSetInner::new(record_evict.clone())),
            accounts: RwLock::new(AccountView::default()),
            notifier,
            record_evict,
        }
    }

    // ============ Account view ============
    //
    // Sync throughout: the writers are the build loop, which cannot `.await`.

    /// Start of a build job — see [`AccountView::reset`].
    pub fn reset_accounts(&self) {
        self.accounts.write().reset();
    }

    /// Note a transaction the build executed that carried its own nonce.
    pub fn observe_executed(&self, signer: Address, nonce: u64) {
        self.accounts.write().observe_executed(signer, nonce);
    }

    /// Note a transaction the build executed that carried no nonce of its own
    /// — see [`NonceBasis::BumpsFromChain`].
    pub fn observe_nonceless(&self, signer: Address) {
        self.accounts.write().observe_nonceless(signer);
    }

    /// The next nonce `sender` can use, measured from `chain_nonce`.
    ///
    /// The caller supplies the chain value because it has already read the
    /// state for its own reasons; this only layers the in-flight block on top.
    pub fn next_nonce(&self, sender: &Address, chain_nonce: u64) -> u64 {
        self.accounts.read().next_nonce(sender, chain_nonce)
    }

    /// Sample the queue's occupancy into gauges. Called once per payload build
    /// job (~per slot) rather than at every fifo mutation — these are sampled
    /// quantities, so slot-level granularity is enough and keeps the mutation
    /// paths free of the scan.
    ///
    /// All three ceilings `admit` enforces are reported, because a ceiling
    /// without a utilisation reading fails silently: requests start being
    /// refused and nothing says which limit did it.
    pub async fn publish_pending_gauge(&self) {
        let inner = self.inner.lock().await;
        let pending = inner.entries.values().filter(|e| e.status == PreconfStatus::Waiting).count();
        let bytes: usize = inner.entries.values().map(|e| e.size).sum();
        let gas: u64 = inner.entries.values().map(|e| Transaction::gas_limit(e.tx.as_ref())).sum();
        let entries = inner.entries.len();
        drop(inner);

        metrics::gauge!("preconf.fifo.pending").set(pending as f64);
        metrics::gauge!("preconf.fifo.entries").set(entries as f64);
        metrics::gauge!("preconf.fifo.bytes_used").set(bytes as f64);
        metrics::gauge!("preconf.fifo.gas_used").set(gas as f64);
    }

    /// Register the record-eviction callback fired from `drop_hash`,
    /// i.e. on **every** fifo removal path. Called once by
    /// [`crate::PreconfServiceBuilder::start`] with a closure forwarding to
    /// `PreconfClassifier::release_unless_committed`.
    ///
    /// Direction matters: the fifo pushes removals *out* and never holds the
    /// classifier. Neither type references the other.
    ///
    /// Idempotent (`OnceLock::set`, first registration wins). Leaving it
    /// unregistered is valid — removals then don't touch the commitment cache,
    /// which is what test / pass-through paths want.
    pub fn set_record_eviction_callback(&self, f: EvictFn) {
        let _ = self.record_evict.set(f);
    }

    // ============ Admission ============

    /// Decide and enqueue in one step: every remaining rule, then the entry,
    /// under a single hold of the lock.
    ///
    /// Splitting the two is what the old path did, and every gap between them
    /// was a window — a nonce checked and then taken by someone else, a
    /// transaction accepted into the pool that the fifo then refused. Here
    /// there is nothing between deciding and being in the queue.
    ///
    /// The caller has already decoded the transaction, established that the
    /// sender may use the preconf path, and put it through the validator; what
    /// is left are exactly the rules that depend on what this queue already
    /// holds.
    ///
    /// **Not among them: what the transaction costs.** Neither the fee cap nor
    /// the balance it draws on is a queue rule, and for one reason — both are
    /// measured against the block the transaction executes in, and only the EVM
    /// knows that block. The queue would be judging a fee cap against whichever
    /// build last published a base fee (the previous block, whenever no build
    /// is open) and a balance against canonical state (which the block being
    /// built may already have spent or credited). Both refusals still reach the
    /// client under their own names —
    /// [`BaseFeeTooLow`](crate::types::PreconfError::BaseFeeTooLow) and
    /// [`InsufficientFunds`](crate::types::PreconfError::InsufficientFunds) —
    /// from the builder; see [`crate::apply::BuilderRejected`].
    ///
    /// Ordering within the lock is by cost, cheapest first, so a request that
    /// is going to be refused holds the lock for as little as possible.
    ///
    /// The slot claim reaches into the classifier while this lock is held. That
    /// is the direction every fifo removal already takes — `drop_hash` fires
    /// the record eviction from inside here — so it adds no new order.
    pub async fn admit(
        &self,
        classifier: &PreconfClassifier,
        req: AdmitRequest,
        capacity: Capacity,
    ) -> Result<(), PreconfError> {
        let AdmitRequest { tx, from, source, responder, chain_nonce, bytecode_hash } = req;
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();
        let size = tx.encoded_2718().len();
        let gas_limit = Transaction::gas_limit(tx.as_ref());

        // Read before the lock: the account view is a different lock, and
        // taking them in this order is the one the module's "Lock order"
        // allows.
        let next_nonce = self.next_nonce(&from, chain_nonce);

        let mut inner = self.inner.lock().await;

        // Same hash again — a plain duplicate, a replaying commitment whose
        // receipt went out in an earlier process, or one already applied. No
        // entry here can produce a second receipt, so all three are refused.
        if inner.entries.contains_key(&hash) {
            return Err(PreconfError::AlreadyInProgress);
        }

        // ── Capacity ───────────────────────────────────────────────────
        // Refusing rather than evicting: every entry here is a promise
        // already made, so there is no one to displace.
        if inner.entries.len() >= capacity.max_txs {
            metrics::counter!("preconf.admit.rejected_entries_total").increment(1);
            return Err(PreconfError::QueueFull {
                in_use: inner.entries.len() as u64,
                capacity: capacity.max_txs as u64,
                unit: "entries",
            });
        }
        let used_bytes: usize = inner.entries.values().map(|e| e.size).sum();
        if used_bytes.saturating_add(size) > capacity.max_size {
            metrics::counter!("preconf.admit.rejected_bytes_total").increment(1);
            return Err(PreconfError::QueueFull {
                in_use: used_bytes as u64,
                capacity: capacity.max_size as u64,
                unit: "bytes",
            });
        }
        // Counting replayed entries too: a commitment already promised consumes
        // the same drain rate as anything else, so it really does push what
        // comes after it out of reach.
        let used_gas: u64 =
            inner.entries.values().map(|e| Transaction::gas_limit(e.tx.as_ref())).sum();
        if used_gas.saturating_add(gas_limit) > capacity.max_queued_gas {
            metrics::counter!("preconf.admit.rejected_gas_total").increment(1);
            return Err(PreconfError::QueueFull {
                in_use: used_gas,
                capacity: capacity.max_queued_gas,
                unit: "gas",
            });
        }

        let held = inner.by_sender.get(&from);
        let holds_this_nonce = held.is_some_and(|nonces| nonces.contains_key(&nonce));
        // A replacement takes a slot rather than adding one, so it is not
        // counted against the ceiling.
        if !holds_this_nonce && held.is_some_and(|n| n.len() >= capacity.max_account_slots) {
            metrics::counter!("preconf.admit.rejected_account_slots_total").increment(1);
            return Err(PreconfError::AccountSlotsFull {
                in_use: held.map_or(0, BTreeMap::len) as u64,
                max: capacity.max_account_slots as u64,
            });
        }

        // ── Nonce continuity ───────────────────────────────────────────
        // `next_nonce` already folds in what the block being built has
        // executed; this adds what is queued behind it, so long as it runs
        // without a gap. A gap means the transactions after it cannot be
        // applied either, so the chain stops there.
        let pending = held.map_or(0, |nonces| {
            let mut run = 0;
            for &queued in nonces.range(next_nonce..).map(|(n, _)| n) {
                if queued != next_nonce.saturating_add(run) {
                    break;
                }
                run += 1;
            }
            run
        });
        let expected = next_nonce.saturating_add(pending);
        if nonce > expected {
            return Err(PreconfError::NonceGap { tx_nonce: nonce, pending_nonce: expected });
        }

        // ── The (sender, nonce) slot ───────────────────────────────────
        // Two indices answer this, and both have to. The queue knows who is
        // waiting on the nonce; the classifier knows who *owns* it, which
        // outlasts the entry — a commitment whose receipt has gone out keeps
        // its nonce through the retention window with nothing left in here.
        //
        // The incumbent always wins: a queued entry is either still to be
        // applied or already in a block, so neither hands its nonce over.
        if let Some(incumbent) = held.and_then(|nonces| nonces.get(&nonce)).copied() {
            if inner.entries.contains_key(&incumbent) {
                return Err(PreconfError::ReplaceActiveCommitment);
            }
            // The queue's two indices disagree. Production heals and carries on
            // rather than tearing down the sequencer.
            error!(
                target: "mantle::preconf",
                sender = ?from, nonce, dangling_hash = ?incumbent,
                "preconf_tx_set: dangling by_sender index detected; self-healing"
            );
            debug_assert!(false, "by_sender[{from:?}][{nonce}] -> {incumbent:?} but entry missing");
            PreconfTxSetInner::unindex_sender_nonce(&mut inner.by_sender, from, nonce);
            inner.drop_hash(&incumbent);
        }
        if let Err(owner) = classifier.slot_conflict(hash, &from, nonce) {
            debug!(
                target: "mantle::preconf",
                ?hash, ?owner, sender = ?from, nonce,
                "nonce already owned by another commitment"
            );
            return Err(PreconfError::ReplaceActiveCommitment);
        }

        // ── Delegated senders ──────────────────────────────────────────
        // A sender with code can change what its queued transactions mean
        // with a single new delegation, so the ordinary pool caps how many it
        // may have in flight. Same cap here, from the same config.
        if bytecode_hash.is_some_and(|code| code != KECCAK256_EMPTY) {
            let inflight = inner.by_sender.get(&from).map_or(0, BTreeMap::len);
            if inflight >= capacity.max_inflight_delegated {
                metrics::counter!("preconf.admit.rejected_delegated_total").increment(1);
                return Err(PreconfError::DelegatedInflightLimit {
                    in_use: inflight as u64,
                    max: capacity.max_inflight_delegated as u64,
                });
            }
        }

        // ── Enqueue ────────────────────────────────────────────────────
        // The responder goes in with the entry, under this same lock, so the
        // builder cannot reach an entry that has nobody to answer.
        let (responder, inserted_at) = match responder {
            Some((origin_instant, resp)) => (Some(resp), origin_instant),
            None => (None, Instant::now()),
        };
        inner.entries.insert(
            hash,
            TxEntry {
                hash,
                tx,
                from,
                nonce,
                size,
                inserted_at,
                status: PreconfStatus::Waiting,
                source,
                responder,
                apply_lock: Arc::new(Mutex::new(())),
            },
        );
        inner.by_sender.entry(from).or_default().insert(nonce, hash);
        inner.order.push_back(hash);
        drop(inner);

        let _ = self.notifier.send(hash);
        Ok(())
    }

    // ============ Push path ============

    /// Idempotent push.
    ///
    /// `from` must be the recovered sender; the callers (journal restore and
    /// reorg re-injection) have it pre-validated. Hash + nonce are read from
    /// `tx`.
    ///
    /// Returns:
    /// - [`PushResult::Inserted`] — new entry created and broadcast notified.
    /// - [`PushResult::AlreadyExists`] — same hash already present; a no-op.
    /// - [`PushResult::ConflictActive`] — same `(from, nonce)`, different hash. The incumbent keeps
    ///   the slot; the hash it carries says which one.
    pub async fn push_if_absent(
        &self,
        tx: Arc<TxEnvelope>,
        from: Address,
        source: PreconfSource,
    ) -> PushResult {
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();

        let mut inner = self.inner.lock().await;

        // Same-hash entry already present: an idempotent no-op, which the RPC
        // handler surfaces as `AlreadyInProgress`.
        if inner.entries.contains_key(&hash) {
            return PushResult::AlreadyExists;
        }

        // Replacement check: same `(sender, nonce)`, different hash. The
        // incumbent never releases the slot — replacing it would double-apply.
        if let Some(existing_hash) =
            inner.by_sender.get(&from).and_then(|nonces| nonces.get(&nonce)).copied()
        {
            if inner.entries.contains_key(&existing_hash) {
                return PushResult::ConflictActive(existing_hash);
            }
            // Invariant violation: `by_sender[(from, nonce)]` points at a hash
            // with no entry. Both halves are needed — `drop_hash` alone skips
            // `by_sender` because the entry is already gone, and the new insert
            // cannot claim the slot until it is cleared.
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
            PreconfTxSetInner::unindex_sender_nonce(&mut inner.by_sender, from, nonce);
            inner.drop_hash(&existing_hash);
        }

        // No responder: the only path that has one installs it with the
        // entry, under this same lock (see `admit`). What is left here is
        // journal replay and the reverted-chain replay, and neither has a
        // client waiting.
        let (responder, inserted_at) = (None, Instant::now());
        let size = tx.encoded_2718().len();
        let entry = TxEntry {
            hash,
            tx,
            from,
            nonce,
            size,
            inserted_at,
            status: PreconfStatus::Waiting,
            source,
            responder,
            apply_lock: Arc::new(Mutex::new(())),
        };
        inner.entries.insert(hash, entry);
        inner.by_sender.entry(from).or_default().insert(nonce, hash);
        inner.order.push_back(hash);
        drop(inner);

        let _ = self.notifier.send(hash);

        PushResult::Inserted
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

    /// Look up an entry by its `(sender, nonce)` slot — O(1) via `by_sender`.
    ///
    /// The queue's own rules read that index directly, under `inner`; this is
    /// the only way to read it from outside, and the only reason it is public.
    /// `entries` and `snapshot` are both built from `order`, so neither can
    /// observe `by_sender` drifting out of step with them — the divergence the
    /// admission and push paths log and self-heal rather than trust.
    pub async fn find_by_sender_nonce(&self, addr: &Address, nonce: u64) -> Option<TxEntryView> {
        let inner = self.inner.lock().await;
        let hash = inner.by_sender.get(addr)?.get(&nonce)?;
        inner.entries.get(hash).map(TxEntry::snapshot_view)
    }

    /// Look up an entry by hash.
    pub async fn find_by_hash(&self, hash: &TxHash) -> Option<TxEntryView> {
        let inner = self.inner.lock().await;
        inner.entries.get(hash).map(TxEntry::snapshot_view)
    }

    /// Acquire the per-entry `apply_lock` — held by dispatch across its
    /// pre-apply gates, `apply_fn` and the completion that follows. `None` if
    /// no entry with `hash` exists.
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

    /// Drops every entry a sender's nonce has moved past: `heads[from]` present
    /// and `nonce < heads[from]`. Senders absent from `heads` keep everything.
    ///
    /// Every sender at once, deliberately, because a pass reads every held
    /// entry — asking per sender costs senders times entries, which is nothing
    /// at the handful of commitments preconf alone holds and quadratic at the
    /// tens of thousands a journal replay can restore. There is no single-sender
    /// version for that reason: its only natural use is in a loop.
    ///
    /// The production caller is the `PayloadJob` prologue
    /// (`builder::payload_builder`'s `sync_fifo_forward_to_head`), which reads
    /// each sender's nonce from the parent-block state. It used to run in
    /// `canon_handler`; that sweep was moved because it raced new payload jobs.
    ///
    /// The predicate is "this sender's nonce has moved past the entry", which
    /// is **not** the same as "this entry's tx landed" — a different tx taking
    /// the nonce drops the entry just the same. Callers that need to know
    /// *which* tx advanced the nonce cannot get it from here.
    ///
    /// ## Why this claims each entry's `apply_lock` first
    ///
    /// This is the only removal that is not itself a completion, so it is the
    /// only one that can take an entry out from under one. That matters because
    /// the lock lives *inside* the entry: remove the entry and
    /// [`Self::lock_for_apply`] has nothing to hand out, so a reader waiting to
    /// find out what happened — `rpc`'s deadline branch — stops waiting and
    /// reads an empty channel instead.
    ///
    /// One completion cannot avoid the gap: [`Self::begin_success`] takes the
    /// responder, and the receipt only goes out after the journal write, which
    /// is an `await`. Removing the entry during that write leaves the client
    /// told `Timeout` for a transaction that is in the block and on disk.
    ///
    /// `try_lock`, not `lock`: a held lock means someone is finishing, and they
    /// will — this runs at every build, so leaving the entry for the next one
    /// costs a slot and blocks nothing.
    ///
    /// This does not make "no entry ⇒ nobody was holding its lock" true on its
    /// own. [`Self::complete_failure`] takes no `apply_lock` at three of its
    /// call sites, and can still remove an entry out from under one. What
    /// covers that is the status it insists on: it ends only a `Waiting` entry,
    /// and the completion that spans an `await` has already moved its entry to
    /// `Success`, so the two cannot reach the same one. The pair of them —
    /// this lock and that CAS — is what makes "no entry ⇒ no completion in
    /// flight" hold.
    pub async fn forward_all<S: std::hash::BuildHasher>(
        &self,
        heads: &std::collections::HashMap<Address, u64, S>,
    ) {
        // Who is past their nonce, and the lock that says whether anyone is
        // mid-completion on them. A hash with no entry is the dangling-index
        // self-heal: there is no lock to claim and nothing to answer, so it
        // goes straight to the removal pass.
        let mut candidates: Vec<(TxHash, Arc<Mutex<()>>)> = Vec::new();
        let mut to_drop: Vec<TxHash> = Vec::new();
        {
            let inner = self.inner.lock().await;
            let passed: Vec<TxHash> = inner
                .by_sender
                .iter()
                .filter_map(|(sender, nonces)| heads.get(sender).map(|head| (nonces, *head)))
                .flat_map(|(nonces, head)| nonces.range(..head).map(|(_, hash)| *hash))
                .collect();
            candidates.reserve(passed.len());
            for hash in passed {
                match inner.entries.get(&hash) {
                    Some(entry) => candidates.push((hash, entry.apply_lock.clone())),
                    None => to_drop.push(hash),
                }
            }
        }

        // Claimed outside `inner` — the order is `apply_lock → inner`, never
        // the reverse.
        let mut guards = Vec::with_capacity(candidates.len());
        for (hash, lock) in std::mem::take(&mut candidates) {
            if let Ok(guard) = lock.try_lock_owned() {
                to_drop.push(hash);
                guards.push(guard);
            }
        }
        if to_drop.is_empty() {
            return;
        }

        let mut inner = self.inner.lock().await;
        // `inner` was released while the locks were claimed, and a
        // `complete_failure` needs no `apply_lock`, so an entry may have ended
        // in between. Re-ask before removing.
        to_drop.retain(|hash| {
            inner
                .entries
                .get(hash)
                .is_none_or(|e| heads.get(&e.from).is_some_and(|head| e.nonce < *head))
        });
        // Anyone still waiting is owed an answer. A `Success` entry has none,
        // so this usually collects nothing.
        let stranded: Vec<(u64, oneshot::Sender<Result<PreconfReceipt, PreconfError>>)> = to_drop
            .iter()
            .filter_map(|hash| {
                let entry = inner.entries.get_mut(hash)?;
                entry.responder.take().map(|resp| (entry.nonce, resp))
            })
            .collect();
        inner.drop_hashes(&to_drop);
        // Still under `inner` — see "Completion protocol" for why the answer
        // may not wait until after the lock.
        for (tx_nonce, resp) in stranded {
            let _ = resp.send(Err(PreconfError::NonceSuperseded { tx_nonce }));
        }
        drop(inner);
        drop(guards);
    }

    /// Every sender holding an entry.
    ///
    /// Cheaper than reading the entries for it: this walks one index and copies
    /// addresses, where [`Self::entries`] clones a view — transaction handle
    /// included — for each of them.
    pub async fn senders(&self) -> HashSet<Address> {
        self.inner.lock().await.by_sender.keys().copied().collect()
    }

    /// Builder subscribes the broadcast notifier here.
    ///
    /// Each call returns an independent `Receiver` — multi-consumer.
    pub fn subscribe(&self) -> broadcast::Receiver<TxHash> {
        self.notifier.subscribe()
    }

    // ============ Completion protocol ============
    //
    // Ending a commitment is one operation: decide, take the responder, answer
    // it. Splitting the three leaves a gap in which a removal can take the
    // responder with it, which `drop_hash` reports as
    // `preconf.fifo.responder_dropped_total`.
    //
    // **The answer goes out under `inner`, not after it.** The entry vanishing
    // and the result arriving have to be one event, because a reader cannot
    // wait for the second after seeing the first: `rpc`'s deadline branch finds
    // the entry through `inner`, and if it is gone there is no `apply_lock`
    // left to block on — `lock_for_apply` reads `None` and goes straight to
    // `try_recv`. Answering after the lock would leave exactly that reader
    // concluding nothing happened while the answer was a few instructions away.
    //
    // Sending under the lock is cheap: `oneshot::Sender::send` stores the value
    // and schedules a waker, never polling inline, so nothing re-enters the
    // queue while `inner` is held. The broadcast notifier stays outside — that
    // one fans out to every subscriber and is a different cost.
    //
    // The one completion that cannot do this is [`Self::begin_success`]: the
    // journal must hold the commitment before the client holds the receipt, and
    // that write is an `await`. It hands back a [`SuccessTicket`] instead, and
    // the caller's `apply_lock` is what covers the gap — which works only
    // because nothing removes a `Success` entry while that lock is held.

    /// End a commitment with a reason, and answer the client.
    ///
    /// The entry is **removed**, the responder taken, and `err` sent — all
    /// three under one hold of `inner`, so no reader can catch the entry gone
    /// and the channel still empty.
    ///
    /// Every failure carries a concrete [`PreconfError`], so "ended but never
    /// answered" is not a state this can produce. `Ok(())` with no responder
    /// attached is the replay case: nobody was waiting.
    pub async fn complete_failure(
        &self,
        hash: &TxHash,
        err: PreconfError,
    ) -> Result<(), MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Waiting {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        // The responder leaves with the entry, so nothing can observe one that
        // owes an answer.
        let responder = entry.responder.take();
        inner.drop_hash(hash);
        // Still under `inner` — see "Completion protocol" above.
        if let Some(r) = responder {
            let _ = r.send(Err(err));
        }
        Ok(())
    }

    /// `Waiting → Success`, taking the responder but **not** answering it.
    ///
    /// The receipt must not go out before the commitment is on disk: a client
    /// holding a receipt no journal remembers is what a restart must never
    /// produce. The caller writes the journal between this and
    /// [`SuccessTicket::send`], holding its `apply_lock` throughout.
    ///
    /// The responder is taken here, not after that write — the write is an
    /// `await`, and a responder left in the entry across it can be taken by a
    /// concurrent removal.
    pub async fn begin_success(&self, hash: &TxHash) -> Result<Option<SuccessTicket>, MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Waiting {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        entry.status = PreconfStatus::Success;
        Ok(entry.responder.take().map(|responder| SuccessTicket { responder }))
    }

    // ============ Status transitions ============

    /// `Waiting → Success` on its own. **Tests only.**
    ///
    /// Production goes through [`Self::begin_success`], which makes the same
    /// transition and takes the responder with it. This leaves the responder
    /// attached — a shape only a test needs.
    ///
    /// Terminal for the build that set it — nothing moves it again, and
    /// [`Self::forward_all`] is the only path that drops it. The one way out is
    /// [`Self::reset_success_to_waiting`], which the *next* payload job's
    /// carryover preamble uses on a `Success` entry that outlived the block it
    /// was applied to (`replay_fifo_carryover` in `payload_builder`): the client
    /// already holds a receipt, so the commitment still has to land, in a block
    /// that will actually commit.
    #[cfg(test)]
    pub(crate) async fn mark_succeeded(&self, hash: &TxHash) -> Result<(), MarkError> {
        let mut inner = self.inner.lock().await;
        let entry = inner.entries.get_mut(hash).ok_or(MarkError::NotFound)?;
        if entry.status != PreconfStatus::Waiting {
            return Err(MarkError::IllegalTransition(entry.status));
        }
        entry.status = PreconfStatus::Success;
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

    /// Install a responder on an existing entry. **Tests only.**
    ///
    /// Production installs it with the entry — see `admit`. What is exercised
    /// here is everything that happens to a responder *after* it is in place
    /// (delivery, cancellation, being dropped when its entry goes), and those
    /// paths do not care how it arrived.
    #[cfg(test)]
    pub(crate) async fn set_responder(
        &self,
        hash: TxHash,
        origin_instant: Instant,
        responder: oneshot::Sender<Result<PreconfReceipt, PreconfError>>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        match inner.entries.get_mut(&hash) {
            Some(entry) => {
                entry.responder = Some(responder);
                entry.inserted_at = origin_instant;
                true
            }
            None => false,
        }
    }

    /// Answer `hash`'s client with an error and leave the entry where it is.
    /// No-op if no responder is attached. The send is fire-and-forget — the
    /// receiver may already have dropped (client gave up).
    ///
    /// **Not a completion.** One caller: a replaying commitment whose client
    /// timed out. The commitment outlives that client and still has to land, so
    /// the entry stays. Every other failure uses [`Self::complete_failure`].
    pub async fn cancel_responder(&self, hash: &TxHash, err: PreconfError) {
        let mut inner = self.inner.lock().await;
        let responder = inner.entries.get_mut(hash).and_then(|e| e.responder.take());
        // Under `inner` for the same reason completions are: detaching a
        // responder and resolving it must not be two observable events.
        if let Some(r) = responder {
            let _ = r.send(Err(err));
        }
    }

    /// Detach the responder without answering it. **Tests only.**
    ///
    /// Production has no such operation: taking a responder and answering it
    /// are one step, so no path can hold one it has not committed to
    /// resolving.
    #[cfg(test)]
    pub(crate) async fn take_responder(
        &self,
        hash: &TxHash,
    ) -> Option<oneshot::Sender<Result<PreconfReceipt, PreconfError>>> {
        let mut inner = self.inner.lock().await;
        inner.entries.get_mut(hash).and_then(|e| e.responder.take())
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
mod admit_tests {
    //! The seven rules admission applies, one at a time.
    //!
    //! All of them run under one hold of the lock, which is the point — but it
    //! also means a rule that never fires is indistinguishable from one that
    //! passes. Each case below arranges for exactly one of them to be the
    //! reason.

    use super::*;
    use crate::config::PreconfConfig;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::Signature;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    /// Any sender is eligible, so nothing here is refused for being off the
    /// allowlist — that decision belongs a step earlier.
    fn classifier() -> PreconfClassifier {
        let cfg = PreconfConfig { enabled: true, all_preconfs: true, ..Default::default() };
        PreconfClassifier::from_config(&cfg)
    }

    fn tx(nonce: u64, hash_byte: u8, max_fee: u128) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, max_fee_per_gas: max_fee, ..Default::default() };
        Arc::new(TxEnvelope::Eip1559(Signed::new_unchecked(
            inner,
            Signature::test_signature(),
            B256::from([hash_byte; 32]),
        )))
    }

    /// `tx` with a gas limit, for the cases about the queue's gas ceiling.
    fn tx_gas(nonce: u64, hash_byte: u8, gas_limit: u64) -> Arc<TxEnvelope> {
        let inner = TxEip1559 { nonce, gas_limit, ..Default::default() };
        Arc::new(TxEnvelope::Eip1559(Signed::new_unchecked(
            inner,
            Signature::test_signature(),
            B256::from([hash_byte; 32]),
        )))
    }

    /// Ceilings high enough to stay out of the way; each case lowers the one
    /// it is about.
    fn cap() -> Capacity {
        Capacity {
            max_txs: 1_000,
            max_size: 1_000_000,
            max_account_slots: 16,
            max_inflight_delegated: 1,
            max_queued_gas: u64::MAX,
        }
    }

    struct Req(AdmitRequest);

    impl Req {
        fn new(tx: Arc<TxEnvelope>, from: Address) -> Self {
            Self(AdmitRequest {
                tx,
                from,
                source: PreconfSource::Rpc,
                responder: None,
                chain_nonce: 0,
                bytecode_hash: None,
            })
        }
        fn chain_nonce(mut self, n: u64) -> Self {
            self.0.chain_nonce = n;
            self
        }
        fn delegated(mut self) -> Self {
            self.0.bytecode_hash = Some(B256::repeat_byte(0x7a));
            self
        }
    }

    /// Admission has to record the commitment before it can claim a nonce, which
    /// is the order the real path runs in.
    async fn admit(
        set: &PreconfTxSet,
        c: &PreconfClassifier,
        req: Req,
        capacity: Capacity,
    ) -> Result<(), PreconfError> {
        set.admit(c, req.0, capacity).await
    }

    #[tokio::test]
    async fn a_transaction_that_breaks_no_rule_is_queued_and_announced() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let mut rx = set.subscribe();

        let out = admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), ());
        assert_eq!(rx.try_recv().unwrap(), B256::from([1u8; 32]), "the builder is told");
        assert_eq!(set.snapshot().await.len(), 1);
    }

    // ── Capacity ───────────────────────────────────────────────────────

    /// Refused, not evicted: every entry already here is a promise, so there
    /// is nobody to displace in favour of the newcomer.
    #[tokio::test]
    async fn a_full_queue_refuses_the_newcomer_rather_than_evicting() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let full = Capacity { max_txs: 1, ..cap() };

        admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), full).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(0, 2, 0), addr(2)), full).await;

        assert!(matches!(out, Err(PreconfError::QueueFull { unit: "entries", .. })), "{out:?}");
        assert_eq!(set.snapshot().await.len(), 1, "the incumbent stays");
    }

    #[tokio::test]
    async fn the_byte_ceiling_is_enforced_separately_from_the_entry_count() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let tight = Capacity { max_size: 1, ..cap() };

        let out = admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), tight).await;

        assert!(matches!(out, Err(PreconfError::QueueFull { unit: "bytes", .. })), "{out:?}");
    }

    /// **The ceiling the other three cannot express.** Entries and bytes bound
    /// what the queue costs to hold; gas bounds whether it can ever drain,
    /// because a block absorbs at most `preconf_max_gas_per_block` of it.
    ///
    /// Summed, not per-transaction: each of these two is comfortably under the
    /// ceiling on its own, and the pair is over.
    #[tokio::test]
    async fn the_gas_ceiling_bounds_the_queue_not_the_transaction() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let tight = Capacity { max_queued_gas: 6_000_000, ..cap() };

        admit(&set, &c, Req::new(tx_gas(0, 1, 4_000_000), addr(1)), tight).await.unwrap();
        let out = admit(&set, &c, Req::new(tx_gas(0, 2, 4_000_000), addr(2)), tight).await;

        assert!(
            matches!(
                out,
                Err(PreconfError::QueueFull {
                    unit: "gas",
                    in_use: 4_000_000,
                    capacity: 6_000_000
                })
            ),
            "{out:?}"
        );
        assert_eq!(set.snapshot().await.len(), 1, "the incumbent stays");
    }

    /// A transaction that alone exceeds the ceiling is refused on the same
    /// rule, with the queue empty — so the check is on the running total plus
    /// this one, not on the total already held.
    #[tokio::test]
    async fn a_transaction_larger_than_the_whole_ceiling_is_refused() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let tight = Capacity { max_queued_gas: 6_000_000, ..cap() };

        let out = admit(&set, &c, Req::new(tx_gas(0, 1, 7_000_000), addr(1)), tight).await;

        assert!(
            matches!(out, Err(PreconfError::QueueFull { unit: "gas", in_use: 0, .. })),
            "{out:?}"
        );
    }

    /// **A replay entry's gas counts.** It is a promise already made, so it
    /// enters without being asked (`push_if_absent` has no ceiling) — but it
    /// consumes the same drain rate as anything else, so it really does push
    /// what comes after it out of reach. Not counting it would let a stuck
    /// commitment be invisible in the one dimension that matters.
    #[tokio::test]
    async fn a_replay_entry_s_gas_is_counted_against_a_later_admission() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let tight = Capacity { max_queued_gas: 6_000_000, ..cap() };

        set.push_if_absent(tx_gas(0, 1, 5_000_000), addr(1), PreconfSource::Replay).await;
        let out = admit(&set, &c, Req::new(tx_gas(0, 2, 2_000_000), addr(2)), tight).await;

        assert!(
            matches!(out, Err(PreconfError::QueueFull { unit: "gas", in_use: 5_000_000, .. })),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn one_sender_may_not_take_more_than_its_share_of_the_queue() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let two = Capacity { max_account_slots: 2, ..cap() };
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), two).await.unwrap();
        admit(&set, &c, Req::new(tx(1, 2, 0), alice), two).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(2, 3, 0), alice), two).await;

        assert!(
            matches!(out, Err(PreconfError::AccountSlotsFull { in_use: 2, max: 2 })),
            "{out:?}"
        );
        assert!(
            admit(&set, &c, Req::new(tx(0, 4, 0), addr(2)), two).await.is_ok(),
            "the ceiling is per sender, not for the queue",
        );
    }

    /// A replayed commitment has no client, and must not acquire one: its
    /// receipt went out in an earlier process, so there is no second receipt to
    /// hand a resubmitting client.
    #[tokio::test]
    async fn a_replayed_commitment_refuses_to_take_on_a_client() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let tx = tx(0, 1, 0);
        let hash = *tx.tx_hash();
        // The shape a journal restore or reorg reinject leaves behind.
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Replay).await;

        let (resp_tx, _rx) = oneshot::channel();
        let mut req = Req::new(tx, addr(1));
        req.0.responder = Some((Instant::now(), resp_tx));
        let out = admit(&set, &c, req, cap()).await;

        assert!(matches!(out, Err(PreconfError::AlreadyInProgress)), "{out:?}");
        assert!(
            set.inner.lock().await.entries[&hash].responder.is_none(),
            "and the refused request must not have left its channel behind",
        );
    }

    // ── Base fee ───────────────────────────────────────────────────────
    //
    // The queue holds no opinion on it. Whichever base fee a transaction is
    // measured against belongs to the block it executes in, and only the EVM
    // knows that block — so the fee cap is not a queue rule at all. See the
    // `admit` rustdoc.

    /// A fee cap far under any plausible base fee is still queued. The refusal
    /// comes from the EVM, as `BuilderRejected::BaseFeeTooLow`, and reaches the
    /// client under the same name it used to carry from here.
    #[tokio::test]
    async fn a_fee_cap_under_any_base_fee_is_still_queued() {
        let set = PreconfTxSet::new(8);
        let c = classifier();

        let out = admit(&set, &c, Req::new(tx(0, 1, 1), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), ());
    }

    /// And a build running makes no difference — there is no floor for it to
    /// publish. Pins that the judgement really did move rather than merely
    /// changing where its input comes from.
    #[tokio::test]
    async fn an_open_build_does_not_give_the_queue_a_floor() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        set.reset_accounts();

        let out = admit(&set, &c, Req::new(tx(0, 1, 1), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), ());
    }

    // ── Nonce continuity ───────────────────────────────────────────────

    #[tokio::test]
    async fn a_nonce_past_the_end_of_the_senders_chain_is_a_gap() {
        let set = PreconfTxSet::new(8);
        let c = classifier();

        let out = admit(&set, &c, Req::new(tx(5, 1, 0), addr(1)).chain_nonce(0), cap()).await;

        assert!(
            matches!(out, Err(PreconfError::NonceGap { tx_nonce: 5, pending_nonce: 0 })),
            "{out:?}",
        );
    }

    #[tokio::test]
    async fn each_admitted_nonce_extends_how_far_the_next_one_may_reach() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        admit(&set, &c, Req::new(tx(1, 2, 0), alice), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(3, 3, 0), alice), cap()).await;

        assert!(
            matches!(out, Err(PreconfError::NonceGap { tx_nonce: 3, pending_nonce: 2 })),
            "two queued, so 2 is the next reachable nonce; got {out:?}",
        );
        assert!(admit(&set, &c, Req::new(tx(2, 4, 0), alice), cap()).await.is_ok());
    }

    /// The baseline is not the chain's alone — what the block being built has
    /// already executed moves it, which is the whole reason the account view
    /// exists.
    #[tokio::test]
    async fn what_the_block_has_executed_moves_the_baseline() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        set.reset_accounts();

        let before = admit(&set, &c, Req::new(tx(1, 1, 0), alice).chain_nonce(0), cap()).await;
        assert!(matches!(before, Err(PreconfError::NonceGap { .. })), "{before:?}");

        set.observe_executed(alice, 0);
        let after = admit(&set, &c, Req::new(tx(1, 2, 0), alice).chain_nonce(0), cap()).await;

        assert_eq!(after.unwrap(), (), "nonce 0 has run, so nonce 1 is next");
    }

    // ── Balance ────────────────────────────────────────────────────────
    //
    // Not a queue rule either, and for the same reason the fee cap is not:
    // what a sender can pay for depends on what the block being built has
    // already done to its balance, which only the EVM knows. See the `admit`
    // rustdoc.

    /// Each transaction is affordable alone; the chain is not. Both are queued
    /// — the EVM refuses the second, as `BuilderRejected::InsufficientFunds`,
    /// and the client is told under the same name either way.
    #[tokio::test]
    async fn a_senders_chain_may_total_more_than_its_balance() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(1, 2, 0), alice), cap()).await;

        assert_eq!(out.unwrap(), ());
        assert_eq!(set.snapshot().await.len(), 2, "both are the builder's to judge");
    }

    // ── The (sender, nonce) slot ───────────────────────────────────────

    /// **The case the queue's own index cannot answer.** The commitment's
    /// entry is gone — its receipt went out and it was swept — but it still
    /// owns the nonce until the retention window closes, because it is on
    /// chain or about to be.
    #[tokio::test]
    async fn a_nonce_owned_by_a_commitment_with_no_entry_left_is_still_taken() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        let first = B256::from([1u8; 32]);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        // The receipt goes out, and with it the claim on the nonce.
        c.mark_promised(first, &alice, 0, 7).expect("the slot was free");
        set.mark_succeeded(&first).await.unwrap();
        // Swept from the queue; the classifier keeps the slot.
        set.forward_all(&std::collections::HashMap::from([(alice, 1u64)])).await;
        assert!(set.find_by_sender_nonce(&alice, 0).await.is_none(), "the premise: no entry");

        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice), cap()).await;

        assert!(matches!(out, Err(PreconfError::ReplaceActiveCommitment)), "{out:?}");
    }

    /// A different hash on a nonce someone else still holds, which is why the
    /// answer is not [`PreconfError::AlreadyInProgress`] — that one is reserved
    /// for the *same* hash arriving twice.
    #[tokio::test]
    async fn an_in_flight_incumbent_keeps_its_nonce() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice), cap()).await;

        assert!(matches!(out, Err(PreconfError::ReplaceActiveCommitment)), "{out:?}");
    }

    // ── Same hash again ────────────────────────────────────────────────

    #[tokio::test]
    async fn the_same_hash_while_someone_is_waiting_on_it_is_refused() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let (resp, _rx) = oneshot::channel();
        let mut first = Req::new(tx(0, 1, 0), addr(1));
        first.0.responder = Some((Instant::now(), resp));

        admit(&set, &c, first, cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await;

        assert!(matches!(out, Err(PreconfError::AlreadyInProgress)), "{out:?}");
    }

    #[tokio::test]
    async fn a_successful_entry_does_not_accept_a_new_responder() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let hash = B256::from([1u8; 32]);
        admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await.unwrap();
        set.mark_succeeded(&hash).await.unwrap();

        let (resp, rx) = oneshot::channel();
        let mut again = Req::new(tx(0, 1, 0), addr(1));
        again.0.responder = Some((Instant::now(), resp));
        let out = admit(&set, &c, again, cap()).await;

        assert!(matches!(out, Err(PreconfError::AlreadyInProgress)), "{out:?}");
        assert!(rx.await.is_err(), "the completed entry must not retain a responder");
        assert_eq!(set.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Success);
    }

    /// An entry with nobody waiting on it is still a live commitment — a
    /// replaying one. A resubmit cannot attach to it: there is no receipt left
    /// to answer with.
    #[tokio::test]
    async fn the_same_hash_with_nobody_waiting_on_it_is_still_refused() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let hash = B256::from([1u8; 32]);

        // Queued with no responder, as journal restore leaves it.
        admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await.unwrap();

        let (resp, rx) = oneshot::channel();
        let mut again = Req::new(tx(0, 1, 0), addr(1));
        again.0.responder = Some((Instant::now(), resp));
        let out = admit(&set, &c, again, cap()).await;

        assert!(matches!(out, Err(PreconfError::AlreadyInProgress)), "{out:?}");
        assert!(rx.await.is_err(), "the refused responder is not installed");
        assert_eq!(
            set.find_by_hash(&hash).await.unwrap().status,
            PreconfStatus::Waiting,
            "still queued, and its status was never touched",
        );
    }

    // ── Delegated senders ──────────────────────────────────────────────

    /// A sender with code can re-delegate with one transaction and change
    /// what everything queued behind it would execute, so the ordinary pool
    /// caps how many it may have in flight. Same cap, same config.
    #[tokio::test]
    async fn a_delegated_sender_is_held_to_a_tighter_ceiling() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice).delegated(), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(1, 2, 0), alice).delegated(), cap()).await;

        assert!(
            matches!(out, Err(PreconfError::DelegatedInflightLimit { in_use: 1, max: 1 })),
            "{out:?}",
        );
    }

    /// The gate is the sender's code, not the queue's suspicion: an ordinary
    /// account stays on the ordinary ceiling. Were this to fail, every sender
    /// would be capped at one in flight.
    #[tokio::test]
    async fn an_ordinary_sender_is_not_held_to_the_delegated_ceiling() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(1, 2, 0), alice), cap()).await;

        assert_eq!(out.unwrap(), ());
    }

    /// An empty code hash is what an account without a delegation reports, so
    /// reading "has a hash" as "is delegated" would cap everyone.
    #[tokio::test]
    async fn an_empty_code_hash_does_not_read_as_a_delegation() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        let empty = |mut r: Req| {
            r.0.bytecode_hash = Some(KECCAK256_EMPTY);
            r
        };

        admit(&set, &c, empty(Req::new(tx(0, 1, 0), alice)), cap()).await.unwrap();
        let out = admit(&set, &c, empty(Req::new(tx(1, 2, 0), alice)), cap()).await;

        assert_eq!(out.unwrap(), ());
    }
}

#[cfg(test)]
mod account_view_tests {
    //! The account view on its own, without a build around it.
    //!
    //! Every case here is about one question: what does `admit` get told, and
    //! is a wrong answer wrong in the safe direction. A value below the truth
    //! refuses a transaction that was in order; a value above it costs a
    //! `nonce_too_low` at apply. Only the first is a defect.

    use super::{AccountView, NonceBasis};
    use alloy_primitives::Address;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    /// Nothing has run for this sender, so the chain is the whole answer.
    #[test]
    fn an_untouched_sender_reads_straight_through_to_the_chain() {
        let view = AccountView::default();

        assert_eq!(view.next_nonce(&addr(0xaa), 42), 42);
    }

    #[test]
    fn an_executed_transaction_hands_back_the_nonce_after_it() {
        let mut view = AccountView::default();

        view.observe_executed(addr(0xaa), 7);

        assert_eq!(view.next_nonce(&addr(0xaa), 0), 8);
    }

    /// Transactions do not have to reach the build in nonce order, so the
    /// later call must not be able to walk the answer backwards.
    #[test]
    fn a_lower_nonce_arriving_later_does_not_move_the_answer_back() {
        let mut view = AccountView::default();

        view.observe_executed(addr(0xaa), 7);
        view.observe_executed(addr(0xaa), 5);

        assert_eq!(view.next_nonce(&addr(0xaa), 0), 8);
    }

    #[test]
    fn senders_are_tracked_independently() {
        let mut view = AccountView::default();

        view.observe_executed(addr(0xaa), 3);

        assert_eq!(view.next_nonce(&addr(0xaa), 0), 4);
        assert_eq!(view.next_nonce(&addr(0xbb), 0), 0, "untouched, so still the chain");
    }

    /// A deposit advances the account nonce but reports `0` for it, so the
    /// count is the only honest record — and it has to be counted from the
    /// chain, not from zero. Writing `1` here is the defect this guards: a
    /// sender sitting at nonce 50 would be told its next nonce is 1, and
    /// every transaction it sends would read as a gap.
    #[test]
    fn a_nonceless_transaction_counts_from_the_chain_rather_than_from_zero() {
        let mut view = AccountView::default();

        view.observe_nonceless(addr(0xaa));

        assert_eq!(view.next_nonce(&addr(0xaa), 50), 51);
    }

    #[test]
    fn nonceless_transactions_accumulate() {
        let mut view = AccountView::default();

        view.observe_nonceless(addr(0xaa));
        view.observe_nonceless(addr(0xaa));

        assert_eq!(view.next_nonce(&addr(0xaa), 50), 52);
    }

    /// The absolute answer already accounts for whatever ran ahead of it, so
    /// it replaces the count rather than adding to it. Adding would double
    /// count the deposit and refuse the sender's next transaction.
    #[test]
    fn an_absolute_nonce_supersedes_the_count_it_follows() {
        let mut view = AccountView::default();
        let alice = addr(0xaa);

        // Chain says 50. A deposit runs (51), then the sender's own tx at 51.
        view.observe_nonceless(alice);
        view.observe_executed(alice, 51);

        assert_eq!(view.next_nonce(&alice, 50), 52);
        assert!(matches!(view.basis.get(&alice), Some(NonceBasis::Absolute(52))));
    }

    /// The other order: once the exact value is known, a later deposit can
    /// stay exact instead of falling back to counting.
    #[test]
    fn a_nonceless_transaction_after_an_absolute_one_stays_absolute() {
        let mut view = AccountView::default();
        let alice = addr(0xaa);

        view.observe_executed(alice, 5);
        view.observe_nonceless(alice);

        assert_eq!(view.next_nonce(&alice, 0), 7);
        assert!(matches!(view.basis.get(&alice), Some(NonceBasis::Absolute(7))));
    }

    /// A build that was discarded while the chain moved on leaves a value
    /// behind that is too low. Too low is the direction that refuses valid
    /// transactions, so the chain wins.
    #[test]
    fn a_stale_absolute_below_the_chain_is_clamped_up_to_it() {
        let mut view = AccountView::default();

        view.observe_executed(addr(0xaa), 2);

        assert_eq!(view.next_nonce(&addr(0xaa), 100), 100);
    }

    /// Clearing, not re-seeding: a cancelled build must not be able to pin a
    /// sender at a nonce the chain will never reach.
    #[test]
    fn reset_forgets_every_sender() {
        let mut view = AccountView::default();
        view.observe_executed(addr(0xaa), 7);
        view.observe_nonceless(addr(0xbb));

        view.reset();

        assert_eq!(view.next_nonce(&addr(0xaa), 3), 3);
        assert_eq!(view.next_nonce(&addr(0xbb), 3), 3);
    }

    #[test]
    fn neither_counter_overflows_at_the_top_of_the_range() {
        let mut view = AccountView::default();
        let alice = addr(0xaa);
        let bob = addr(0xbb);

        view.observe_executed(alice, u64::MAX);
        view.observe_nonceless(alice);
        view.observe_nonceless(bob);

        assert_eq!(view.next_nonce(&alice, 0), u64::MAX);
        assert_eq!(view.next_nonce(&bob, u64::MAX), u64::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{B256, Bloom, Signature};

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

    /// Every fifo removal path must drop the commitment record, because
    /// `drop_hash` is the single point they all converge on. Asserted through
    /// two different public entry points so the hook is proven to sit at that
    /// convergence point rather than on one route.
    #[tokio::test]
    async fn removal_paths_fire_the_record_eviction_callback() {
        let set = PreconfTxSet::new(16);
        let seen: Arc<std::sync::Mutex<Vec<TxHash>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        set.set_record_eviction_callback(Arc::new(move |hash| sink.lock().unwrap().push(hash)));

        // Route 1 — `complete_failure`, the single-entry removal, via `drop_hash`.
        set.push_if_absent(make_tx(0, 1), addr(1), PreconfSource::Rpc).await;
        set.complete_failure(&h(1), PreconfError::BuilderRejected("rejected".into()))
            .await
            .unwrap();

        // Route 2 — `forward_all`, the bulk removal, via `drop_hashes`.
        set.push_if_absent(make_tx(7, 2), addr(2), PreconfSource::Rpc).await;
        forward_one(&set, &addr(2), 8).await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec![h(1), h(2)],
            "both removal routes must evict, and only the removed hashes",
        );
    }

    /// Leaving the callback unregistered must stay valid — that is the test /
    /// pass-through path, and `drop_hash` runs on every removal.
    #[tokio::test]
    async fn removal_without_record_callback_is_a_noop() {
        let set = PreconfTxSet::new(16);
        set.push_if_absent(make_tx(0, 1), addr(1), PreconfSource::Rpc).await;
        set.complete_failure(&h(1), PreconfError::BuilderRejected("rejected".into()))
            .await
            .unwrap();
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

    // ── Completion protocol ────────────────────────────────────────────

    /// Attach a client channel to an already-queued entry.
    ///
    /// `admit` is the only production path that installs one, and it needs a
    /// classifier and a decoded transaction; these tests are about what
    /// happens *after* an entry has a client, so they reach for the field
    /// directly rather than reconstruct admission.
    async fn give_it_a_client(
        set: &PreconfTxSet,
        hash: &TxHash,
    ) -> oneshot::Receiver<Result<PreconfReceipt, PreconfError>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        set.inner.lock().await.entries.get_mut(hash).expect("queued").responder = Some(resp_tx);
        resp_rx
    }

    /// Ending and answering are one operation, and ending is what removes the
    /// entry: after it returns there is no moment where a finished commitment
    /// sits in the queue owing someone an answer.
    #[tokio::test]
    async fn complete_failure_ends_the_entry_and_answers_the_client() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let mut resp_rx = give_it_a_client(&set, tx.tx_hash()).await;

        set.complete_failure(
            tx.tx_hash(),
            PreconfError::BlockGasBudgetExceeded { max: 1, used: 1, limit: 1 },
        )
        .await
        .expect("a Waiting entry may be completed");

        assert!(!set.contains(tx.tx_hash()).await, "ending it is what removes it");
        // `try_recv`, not `await`: the send happens before `complete_failure`
        // returns, so the result is already there — and a regression that
        // leaves the responder in the entry would block `await` forever
        // instead of failing.
        assert!(
            matches!(resp_rx.try_recv(), Ok(Err(PreconfError::BlockGasBudgetExceeded { .. }))),
            "the client is told why, not left on a closed channel",
        );
    }

    /// **The entry vanishing and the answer arriving are one event.**
    ///
    /// `rpc`'s deadline branch finds the entry through `inner`, and if it is
    /// gone there is no `apply_lock` left to wait on — `lock_for_apply` reads
    /// `None` and goes straight to `try_recv`. So any gap between the removal
    /// and the send is a window in which that reader sees a commitment that is
    /// gone with no reason attached, and answers `Timeout` for a transaction
    /// that failed for a stated reason.
    ///
    /// Spins on exactly that transition from a second thread: the moment the
    /// entry stops being readable, the result must already be in the channel.
    ///
    /// **What this can and cannot catch**, measured rather than assumed. It
    /// fails on the first round if an `await` appears between the removal and
    /// the send — which is the regression worth guarding, since that is what
    /// widens the window to something a client can land in. It does **not**
    /// fail on the ordering this replaced, where the send merely moved after
    /// the guard was dropped: an observer has to take `inner` to learn the
    /// entry is gone, and being woken for that lock costs far more than the
    /// handful of instructions to the send, so the observer always loses. That
    /// window is real but unobservable from inside the process, which is also
    /// why the fix is an ordering argument rather than a test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_entry_is_never_observably_gone_before_its_client_is_told() {
        for round in 0..512u32 {
            let set = Arc::new(PreconfTxSet::new(16));
            let tx = make_tx(0, 1);
            let hash = *tx.tx_hash();
            set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;
            let mut resp_rx = give_it_a_client(&set, &hash).await;

            let completer = tokio::spawn({
                let set = set.clone();
                async move {
                    let _ = set
                        .complete_failure(
                            &hash,
                            PreconfError::BlockGasBudgetExceeded { max: 1, used: 1, limit: 1 },
                        )
                        .await;
                }
            });

            loop {
                if set.find_by_hash(&hash).await.is_none() {
                    assert!(
                        !matches!(resp_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
                        "round {round}: the entry was gone while its client had no answer",
                    );
                    break;
                }
            }
            completer.await.unwrap();
        }
    }

    /// A commitment being completed is not swept out from under it.
    ///
    /// `begin_success` takes the responder and the receipt only goes out after
    /// the journal write — an `await`. If `forward_all` removed the entry
    /// during that window, `lock_for_apply` would hand `rpc`'s deadline branch
    /// a `None`, and with nothing to wait on it would answer `Timeout` for a
    /// transaction already in the block and on disk.
    ///
    /// Holding the `apply_lock` is what dispatch does across that whole span,
    /// so holding it here stands in for "a completion is in flight". The entry
    /// must survive; the next build's sweep collects it.
    #[tokio::test]
    async fn a_commitment_mid_completion_is_not_swept() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        let hash = *tx.tx_hash();
        set.push_if_absent(tx, addr(1), PreconfSource::Rpc).await;

        // What dispatch holds from before `apply_fn` until after the receipt
        // goes out.
        let guard = set.lock_for_apply(&hash).await.expect("queued");

        set.forward_all(&std::collections::HashMap::from([(addr(1), 1u64)])).await;
        assert!(
            set.contains(&hash).await,
            "an entry whose apply_lock is held is mid-completion; sweeping it strands the client",
        );

        // Released — the commitment is finished, and the next sweep may take it.
        drop(guard);
        set.forward_all(&std::collections::HashMap::from([(addr(1), 1u64)])).await;
        assert!(!set.contains(&hash).await, "once nobody is finishing it, it goes");
    }

    /// Only `Waiting` ends, and the first ending takes the entry with it — so
    /// the losing side of a race finds nothing rather than a terminal state to
    /// argue with, and cannot overwrite the first answer.
    #[tokio::test]
    async fn completing_twice_is_refused() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;

        set.complete_failure(tx.tx_hash(), PreconfError::BuilderRejected("first".into()))
            .await
            .unwrap();
        let second =
            set.complete_failure(tx.tx_hash(), PreconfError::Timeout { timeout_ms: 1 }).await;

        assert!(matches!(second, Err(MarkError::NotFound)), "{second:?}");
    }

    /// The journal has to have the commitment before the client has the
    /// receipt, so the responder comes out now and goes out later — but it
    /// comes out *now*, because the journal write is an await.
    #[tokio::test]
    async fn begin_success_takes_the_responder_without_sending_it() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let mut resp_rx = give_it_a_client(&set, tx.tx_hash()).await;

        let ticket = set
            .begin_success(tx.tx_hash())
            .await
            .expect("a Waiting entry may succeed")
            .expect("this one has a client");

        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Success,);
        assert!(
            matches!(resp_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "nothing may reach the client before the journal has it",
        );

        ticket.send(PreconfReceipt {
            tx_hash: *tx.tx_hash(),
            block_height: 7,
            status: true,
            logs: Vec::new(),
            gas_used: 21_000,
            reason: String::new(),
            revert_data: alloy_primitives::Bytes::new(),
            tx_index: 0,
            cumulative_gas_used: 21_000,
            logs_bloom: Bloom::default(),
            log_index_base: 0,
            tx_type: 2,
            from: Address::ZERO,
            to: None,
            contract_address: None,
            effective_gas_price: 0,
            block_timestamp: 0,
            l1_fields: Default::default(),
        });
        assert!(matches!(resp_rx.try_recv(), Ok(Ok(r)) if r.block_height == 7));
    }

    /// A replayed commitment has no client — its receipt went out in an
    /// earlier process. Both completions must say so rather than fail.
    #[tokio::test]
    async fn completing_an_entry_nobody_waits_on_is_not_an_error() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Replay).await;

        assert!(set.begin_success(tx.tx_hash()).await.expect("transition is legal").is_none());
    }

    /// `forward_all` removes by nonce, not by outcome — a sender whose nonce
    /// the chain has moved past may still have a client waiting on an entry
    /// that can now never be applied.
    ///
    /// Removing it silently leaves that client on a closed channel, which says
    /// nothing about why. The removal owes an answer like any other.
    #[tokio::test]
    async fn forward_all_tells_the_client_its_nonce_was_taken() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let mut resp_rx = give_it_a_client(&set, tx.tx_hash()).await;

        // The sender is at nonce 1 on chain now; the entry sits at 0.
        set.forward_all(&std::collections::HashMap::from([(addr(1), 1u64)])).await;

        assert!(!set.contains(tx.tx_hash()).await, "the entry goes, as before");
        assert!(
            matches!(resp_rx.try_recv(), Ok(Err(PreconfError::NonceSuperseded { tx_nonce: 0 }))),
            "and the client is told why, not handed a closed channel",
        );
    }

    #[tokio::test]
    async fn subscribe_returns_independent_receivers() {
        let set = PreconfTxSet::new(16);
        let _rx1 = set.subscribe();
        let _rx2 = set.subscribe();
        assert!(format!("{set:?}").contains("receiver_count"));
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

    #[tokio::test]
    async fn mark_succeeded_from_waiting() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Success);
    }

    #[tokio::test]
    async fn second_transition_rejects_non_waiting_source() {
        // Only `Waiting` is a legal source; once an entry has succeeded, every
        // further transition must bounce off it.
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        set.mark_succeeded(tx.tx_hash()).await.unwrap();

        let err = set.mark_succeeded(tx.tx_hash()).await.unwrap_err();
        assert_eq!(err, MarkError::IllegalTransition(PreconfStatus::Success));

        let err = set
            .complete_failure(tx.tx_hash(), PreconfError::BuilderRejected("late".into()))
            .await
            .unwrap_err();
        assert_eq!(err, MarkError::IllegalTransition(PreconfStatus::Success));
        // A rejected failure leaves the entry alone rather than removing it.
        assert!(set.contains(tx.tx_hash()).await);
    }

    #[tokio::test]
    async fn mark_transition_returns_not_found_for_unknown_hash() {
        let set = PreconfTxSet::new(16);
        assert_eq!(set.mark_succeeded(&h(99)).await.unwrap_err(), MarkError::NotFound);
        assert_eq!(
            set.complete_failure(&h(99), PreconfError::BuilderRejected("gone".into()))
                .await
                .unwrap_err(),
            MarkError::NotFound
        );
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

    /// Only `Success` may transition; a still-`Waiting` entry returns
    /// `IllegalTransition(Waiting)` and is left untouched. Locks the CAS
    /// boundary so a future refactor doesn't let a replay reset an entry the
    /// builder has not finished with.
    #[tokio::test]
    async fn reset_success_to_waiting_rejects_a_waiting_entry() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;

        let err = set.reset_success_to_waiting(tx.tx_hash()).await.unwrap_err();
        assert_eq!(err, MarkError::IllegalTransition(PreconfStatus::Waiting));
        // Entry unchanged.
        assert_eq!(set.find_by_hash(tx.tx_hash()).await.unwrap().status, PreconfStatus::Waiting);
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

        forward_one(&set, &addr(1), 7).await;

        assert!(!set.contains(t5.tx_hash()).await);
        assert!(!set.contains(t6.tx_hash()).await);
        assert!(set.contains(t7.tx_hash()).await);
        assert!(set.contains(other.tx_hash()).await); // unrelated sender untouched
    }

    // ============ responder lifecycle ============

    #[tokio::test]
    async fn cancel_responder_sends_error_to_receiver() {
        let set = PreconfTxSet::new(16);
        let tx = make_tx(0, 1);
        set.push_if_absent(tx.clone(), addr(1), PreconfSource::Rpc).await;
        let (s, r) = oneshot::channel();
        assert!(set.set_responder(*tx.tx_hash(), Instant::now(), s).await);

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

    /// Forward one sender. Production forwards them all at once — see
    /// [`PreconfTxSet::forward_all`] for why there is no such method.
    pub(super) async fn forward_one(set: &PreconfTxSet, addr: &Address, new_nonce: u64) {
        let mut one = std::collections::HashMap::new();
        one.insert(*addr, new_nonce);
        set.forward_all(&one).await;
    }

    /// A fifo of `entries` transactions spread evenly over `senders` accounts.
    /// Each sender's transactions sit together, so the entries a forward drops
    /// — one per sender — are spread evenly through the queue rather than
    /// bunched at its head. Bunched, a scan that starts at the head finds them
    /// immediately and the measurement flatters itself.
    fn crowded(entries: usize, senders: usize) -> Vec<(Arc<TxEnvelope>, Address)> {
        let per_sender = entries / senders;
        (0..entries)
            .map(|i| {
                let mut sender = [0u8; 20];
                sender[..8].copy_from_slice(&((i / per_sender) as u64).to_be_bytes());
                let mut hash = [0u8; 32];
                hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
                let inner = alloy_consensus::TxLegacy {
                    nonce: (i % per_sender) as u64,
                    gas_limit: 21_000,
                    ..Default::default()
                };
                let tx = TxEnvelope::Legacy(Signed::new_unchecked(
                    inner,
                    Signature::test_signature(),
                    B256::from(hash),
                ));
                (Arc::new(tx), Address::from(sender))
            })
            .collect()
    }

    /// Time one build's worth of canon-forward: read the senders, drop what
    /// their nonces have passed.
    async fn sweep(entries: usize, senders: usize) -> std::time::Duration {
        let set = PreconfTxSet::new(1 << 17);
        for (tx, sender) in crowded(entries, senders) {
            set.push_if_absent(tx, sender, PreconfSource::Replay).await;
        }
        let started = std::time::Instant::now();
        let heads: HashMap<Address, u64> =
            set.senders().await.into_iter().map(|sender| (sender, 1)).collect();
        set.forward_all(&heads).await;
        started.elapsed()
    }

    /// The per-build canon-forward stays linear in the number of held entries.
    ///
    /// Two things used to make it grow faster than that, both invisible at the
    /// handful of commitments preconf alone holds and ruinous at the tens of
    /// thousands a journal replay restores: the fifo was asked once per sender
    /// and walked every entry for each answer, and every dropped entry was then
    /// hunted down in a queue that has no index. Measured with both in place,
    /// ten times the entries and ten times the senders cost seventy-four times
    /// the work — 75ms, ahead of the block's first transaction.
    ///
    /// Asserts the ratio rather than a wall-clock figure, which would only be
    /// flaky on shared CI. Ten times the input should cost about ten times the
    /// work; the bound leaves room for the constants without leaving room for
    /// either of those two coming back.
    #[tokio::test]
    async fn forwarding_every_sender_stays_linear_in_the_entries_held() {
        let small = sweep(4_000, 400).await;
        let large = sweep(40_000, 4_000).await;
        println!("canon-forward: 4k/400 {small:?}, 40k/4k {large:?}");

        let ratio = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);
        assert!(
            ratio < 25.0,
            "canon-forward must stay linear in the entries held; \
             4k/400 took {small:?}, 40k/4k took {large:?} ({ratio:.1}x)",
        );
    }

    /// Locks the "at most one responder per hash, and it lives inside the
    /// entry" invariant mechanically: `snapshot_view` returns a view
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
            size: 0,
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
    /// `entries[hash]` is missing, it should still clean `order` and
    /// `by_sender`. Non-self-heal companion to the
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
            inner.by_sender.entry(addr(9)).or_default().insert(42, ghost);
        }

        // Drop the ghost — no entry to remove, but aux indices should still
        // be cleaned.
        {
            let mut inner = set.inner.lock().await;
            let evicted = inner.drop_hash(&ghost);
            assert!(evicted.is_none());
            assert!(inner.order.is_empty());
            assert!(inner.by_sender.is_empty());
        }
    }

    /// `PushResult::ConflictActive(hash)` carries the **old**
    /// (colliding) hash so a caller can log both the new
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
        CompleteFailure(TxId),
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
            txid().prop_map(Op::CompleteFailure),
            // `new_nonce` up to NONCES+1 so a forward can clear the whole sender.
            (0..SENDERS, 0..(NONCES + 1))
                .prop_map(|(sender, new_nonce)| Op::Forward { sender, new_nonce }),
        ]
    }

    /// Reference model: `hash_byte -> (sender, nonce, status)`. Maintains "at
    /// most one entry per `(sender, nonce)`" by construction — the property
    /// the real fifo must also hold.
    type Model = BTreeMap<u8, (u8, u8, PreconfStatus)>;

    fn model_mark(model: &mut Model, hb: u8, target: PreconfStatus) {
        // Only a `Waiting` entry moves; anything else is a no-op.
        if let Some((_, _, st)) = model.get_mut(&hb)
            && *st == PreconfStatus::Waiting
        {
            *st = target;
        }
    }

    fn model_apply(model: &mut Model, op: &Op) {
        match op {
            Op::Push(id) => {
                let hb = id.hash_byte();
                // Same hash: a no-op, since every entry present is live.
                if model.contains_key(&hb) {
                    return;
                }
                // Same (sender, nonce), different hash: the incumbent is live
                // too, so it keeps the slot (ConflictActive) — no insert.
                let occupied = model.values().any(|(s, n, _)| *s == id.sender && *n == id.nonce);
                if !occupied {
                    model.insert(hb, (id.sender, id.nonce, PreconfStatus::Waiting));
                }
            }
            Op::MarkSucceeded(id) => model_mark(model, id.hash_byte(), PreconfStatus::Success),
            // Ending a commitment removes it — there is no terminal state left
            // for the model to hold.
            Op::CompleteFailure(id) => {
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
            Op::MarkSucceeded(id) => {
                let _ = set.mark_succeeded(&id.hash()).await;
            }
            Op::CompleteFailure(id) => {
                let _ = set
                    .complete_failure(&id.hash(), PreconfError::BuilderRejected("model".into()))
                    .await;
            }
            Op::Forward { sender, new_nonce } => {
                super::tests::forward_one(set, &addr(*sender), *new_nonce as u64).await;
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
/// Holds a real `oneshot::Receiver` per installed responder and checks, after
/// every op, the one property that matters: **a responder leaving the set
/// resolves its receiver exactly once** — `Ok` via `take`, `Err` via
/// `cancel`, `RecvError` when its entry is dropped — and never leaks
/// silently. Each hash owns its own slot, isolating this from the replacement
/// logic.
///
/// The responder is installed with a test-only setter rather than through
/// admission: what is under test is everything that happens to it afterwards,
/// and admission would drag nonce continuity and capacity into a model that
/// deliberately pokes hashes in any order.
#[cfg(test)]
mod proptest_responder_model {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{B256, Bloom, Bytes, Log, Signature};
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
            tx_index: 0,
            cumulative_gas_used: 0,
            logs_bloom: Bloom::default(),
            log_index_base: 0,
            tx_type: 2,
            from: Address::ZERO,
            to: None,
            contract_address: None,
            effective_gas_price: 0,
            block_timestamp: 0,
            l1_fields: Default::default(),
        }
    }
    fn some_err() -> PreconfError {
        PreconfError::Internal("model".into())
    }

    type Rx = oneshot::Receiver<Result<PreconfReceipt, PreconfError>>;

    /// Per-hash model: entry status (if any) and the currently-held responder
    /// (location + its receiver, so we can observe the receiver's fate).
    #[derive(Default)]
    struct HashState {
        entry: Option<PreconfStatus>,
        held: Option<Rx>,
    }

    #[derive(Clone, Debug)]
    enum Op {
        SetResponder(u8),
        Push(u8),
        CompleteFailure(u8),
        MarkSucceeded(u8),
        Take(u8),
        Cancel(u8),
        Forward(u8),
    }

    fn hb() -> impl Strategy<Value = u8> {
        0..HASHES
    }
    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            hb().prop_map(Op::SetResponder),
            hb().prop_map(Op::Push),
            hb().prop_map(Op::CompleteFailure),
            hb().prop_map(Op::MarkSucceeded),
            hb().prop_map(Op::Take),
            hb().prop_map(Op::Cancel),
            (0..=HASHES).prop_map(Op::Forward),
        ]
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
                Op::SetResponder(b) => {
                    let b = *b;
                    let (tx, rx) = oneshot::channel();
                    let installed = set.set_responder(h(b), Instant::now(), tx).await;
                    let st = &mut model[b as usize];
                    if st.entry.is_some() {
                        assert!(installed, "step {i}: an entry must take a responder");
                        // Whatever was there is displaced, and displacing must
                        // resolve it rather than leak it.
                        if let Some(old) = st.held.take() {
                            assert_closed(old, &format!("step {i}: displaced responder"));
                        }
                        st.held = Some(rx);
                    } else {
                        assert!(!installed, "step {i}: no entry, nowhere to put it");
                        assert_closed(rx, &format!("step {i}: nothing took it"));
                    }
                }
                Op::Push(b) => {
                    let b = *b;
                    set.push_if_absent(make_tx(u64::from(b), b), sender, PreconfSource::Rpc).await;
                    let st = &mut model[b as usize];
                    match st.entry {
                        None => st.entry = Some(PreconfStatus::Waiting),
                        Some(_) => { /* Waiting/Success: AlreadyExists, no change */ }
                    }
                }
                Op::CompleteFailure(b) => {
                    let b = *b;
                    let r = set.complete_failure(&h(b), some_err()).await;
                    let st = &mut model[b as usize];
                    match st.entry {
                        Some(PreconfStatus::Waiting) => {
                            assert!(r.is_ok(), "step {i}: a waiting entry can fail");
                            // The entry is gone, and the client was told why
                            // rather than being left to read a closed channel.
                            st.entry = None;
                            if let Some(rx) = st.held.take() {
                                assert_value(rx, &format!("step {i}: failed responder answered"));
                            }
                        }
                        other => {
                            assert!(r.is_err(), "step {i}: {other:?} is not a waiting entry");
                        }
                    }
                }
                Op::MarkSucceeded(b) => mark(&set, &mut model, *b, PreconfStatus::Success).await,
                Op::Take(b) => {
                    let b = *b;
                    let r = set.take_responder(&h(b)).await;
                    let st = &mut model[b as usize];
                    if let Some(rx) = st.held.take() {
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
                    if let Some(rx) = st.held.take() {
                        assert_value(rx, &format!("step {i}: canceled responder got error"));
                    }
                }
                Op::Forward(new_nonce) => {
                    super::tests::forward_one(&set, &sender, u64::from(*new_nonce)).await;
                    for b in 0..HASHES {
                        let st = &mut model[b as usize];
                        // forward only drops *entries* (nonce == b) below new_nonce.
                        if st.entry.is_some() && u64::from(b) < u64::from(*new_nonce) {
                            st.entry = None;
                            if let Some(rx) = st.held.take() {
                                // The removal owes an answer: the sender's
                                // nonce moved past this entry, so it can never
                                // be applied.
                                assert_value(rx, &format!("step {i}: forward-dropped responder"));
                            }
                        }
                    }
                }
            }

            check_invariants(&set, &mut model, i).await;
        }
    }

    async fn mark(set: &PreconfTxSet, model: &mut [HashState], b: u8, target: PreconfStatus) {
        let _ = match target {
            PreconfStatus::Success => set.mark_succeeded(&h(b)).await,
            // Not a transition target, and `op()` never proposes it:
            // `Waiting` is entered only by `push_if_absent`. Spelled out rather
            // than caught by a wildcard so that adding a status forces this
            // decision again.
            PreconfStatus::Waiting => unreachable!(),
        };
        let st = &mut model[b as usize];
        if st.entry == Some(PreconfStatus::Waiting) {
            st.entry = Some(target); // responder untouched by mark_succeeded
        }
    }

    async fn check_invariants(set: &PreconfTxSet, model: &mut [HashState], step: usize) {
        {
            let inner = set.inner.lock().await;
            for b in 0..model.len() as u8 {
                let hash = h(b);
                let entry_resp = inner.entries.get(&hash).is_some_and(|e| e.responder.is_some());
                assert_eq!(
                    entry_resp,
                    model[b as usize].held.is_some(),
                    "step {step}: responder presence mismatch for {b}"
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
            if let Some(rx) = st.held.as_mut() {
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
