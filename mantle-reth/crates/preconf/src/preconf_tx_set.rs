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
    Address, B256, KECCAK256_EMPTY, TxHash, U256,
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
    /// What this transaction can take from the sender's balance: value plus
    /// the full gas allowance, plus Mantle's L1 and operator fees.
    ///
    /// Carried rather than derived because the fee part is not a function of
    /// the transaction — it is recomputed from the current L1 block info — so
    /// the only honest figure is the one admission saw. Summing a sender's
    /// chain is what the cumulative-balance check does, and it does it under
    /// the lock, where there is no asking anyone.
    ///
    /// Zero on entries that did not come through admission — journal replay
    /// and reorg re-injection. Those undercount a later admission's sum; the
    /// alternative is inventing a figure for them.
    pub cost: U256,
    /// Encoded size in bytes, against the queue's byte ceiling. Kept rather
    /// than re-encoded: the ceiling is checked on every admission and the
    /// figure never changes.
    pub size: usize,
    /// Wall-clock insertion time. Load-bearing: `builder::dispatch`
    /// pre-apply deadline check aborts with `Timeout` when
    /// `elapsed + SAFETY_MARGIN >= preconf_timeout`.
    pub inserted_at: Instant,
    /// Current status — see [`PreconfStatus`].
    pub status: PreconfStatus,
    /// Origin of the entry — see [`PreconfSource`]. Determines which
    /// pre-apply gates `builder::dispatch::apply_one_preconf` enforces.
    pub source: PreconfSource,
    /// The client's channel — `Some` for an RPC submission, `None` for a
    /// replay. Take-once: `take_responder` moves it out.
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
    /// The sender's balance as of the parent block.
    pub chain_balance: U256,
    /// The base fee to hold the transaction to when no build is open. The
    /// queue prefers the block being built when there is one.
    pub chain_base_fee: u64,
    /// Value plus the full gas allowance plus Mantle's L1 and operator fees.
    /// Computed by the caller because the fee part comes from the current L1
    /// block info, not from the transaction.
    pub cost: U256,
    /// The sender's on-chain code hash, from the validator's verdict. Neither
    /// absent nor `KECCAK_EMPTY` means it carries an EIP-7702 delegation.
    pub bytecode_hash: Option<B256>,
}

/// What [`PreconfTxSet::admit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// A new entry, broadcast to the builder.
    Inserted,
    /// The same hash was here in a state that allows a retry, and is now
    /// waiting again.
    Revived,
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

    /// sender → its in-flight nonces, in order.
    ///
    /// Nested and ordered rather than keyed on `(sender, nonce)`, because
    /// admission asks three questions of a sender's whole chain and none of
    /// them can be answered from a flat key: how many entries it holds, how far
    /// its nonces run without a gap, and what they cost in total. A flat map
    /// answers each by scanning every entry in the fifo.
    by_sender: HashMap<Address, BTreeMap<u64, TxHash>>,

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
            verdict_evict,
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
        entry
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
            self.unindex(hash);
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
/// `inner`, never inside it, for the same reason the classifier's verdict
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
    /// The base fee of the block being built, constant across it.
    ///
    /// `None` means no build has run in this process yet. Distinct from a
    /// base fee that genuinely is zero: a plain `0` would make a base-fee
    /// floor silently admit everything until the first build.
    base_fee: Option<u64>,
}

impl AccountView {
    /// Start of a build: forget the previous one, pin this block's base fee.
    fn reset(&mut self, base_fee: u64) {
        self.basis.clear();
        self.base_fee = Some(base_fee);
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
            accounts: RwLock::new(AccountView::default()),
            notifier,
            verdict_evict,
        }
    }

    // ============ Account view ============
    //
    // Sync throughout: the writers are the build loop, which cannot `.await`.

    /// Start of a build job — see [`AccountView::reset`].
    pub fn reset_accounts(&self, base_fee: u64) {
        self.accounts.write().reset(base_fee);
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

    /// Base fee of the block being built; `None` before the first build of
    /// this process.
    pub fn build_base_fee(&self) -> Option<u64> {
        self.accounts.read().base_fee
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
    /// is left are the rules that depend on what this queue already holds, plus
    /// the base fee of the block being built.
    ///
    /// Ordering within the lock is by cost, cheapest first, so a request that
    /// is going to be refused holds the lock for as little as possible.
    ///
    /// The slot claim reaches into the classifier while this lock is held. That
    /// is the direction every fifo removal already takes — `drop_hash` fires
    /// the verdict eviction from inside here — so it adds no new order.
    pub async fn admit(
        &self,
        classifier: &PreconfClassifier,
        req: AdmitRequest,
        capacity: Capacity,
    ) -> Result<Admitted, PreconfError> {
        let AdmitRequest {
            tx,
            from,
            source,
            responder,
            chain_nonce,
            chain_balance,
            chain_base_fee,
            bytecode_hash,
            cost,
        } = req;
        let hash = *tx.tx_hash();
        let nonce = tx.nonce();
        let size = tx.encoded_2718().len();
        let gas_limit = Transaction::gas_limit(tx.as_ref());

        // Read before the lock: the account view is a different lock, and
        // taking them in this order is the one the module's "Lock order"
        // allows.
        // The block being built when there is one, the chain's tip otherwise
        // — a floor that is absent admits everything, which is not a floor.
        let base_fee = self.build_base_fee().unwrap_or(chain_base_fee);
        let next_nonce = self.next_nonce(&from, chain_nonce);

        let mut inner = self.inner.lock().await;

        // Same hash again, which is two different situations.
        //
        // A revivable terminal state is the documented retry after `Timeout` /
        // `Canceled` / `Failed`: flip it back to waiting and let the builder
        // know.
        //
        // A *live* entry with nobody waiting on it is the other one, and it is
        // not a duplicate request. A replaying commitment sits here as
        // `Waiting` with its responder already consumed — the receipt went out
        // in an earlier process — so a client resubmitting those bytes has no
        // one answering for it. Refusing would leave it with no way to learn
        // the commitment's fate. Its status is left alone: the entry is
        // already queued, and the builder has already been told.
        //
        // A live entry that *does* have a responder is the genuine duplicate.
        if let Some(existing) = inner.entries.get_mut(&hash) {
            let revivable = existing.status.is_revivable_by_same_hash();
            if !revivable && existing.responder.is_some() {
                return Err(PreconfError::AlreadyInProgress);
            }
            if revivable {
                existing.status = PreconfStatus::Waiting;
            }
            if let Some((origin_instant, resp)) = responder {
                existing.responder = Some(resp);
                // The resubmit's clock, not the original's: the deadline gate
                // is measuring a promise made to whoever is waiting now.
                existing.inserted_at = origin_instant;
            }
            drop(inner);
            if revivable {
                let _ = self.notifier.send(hash);
            }
            return Ok(Admitted::Revived);
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

        // ── Base fee ───────────────────────────────────────────────────
        if tx.max_fee_per_gas() < u128::from(base_fee) {
            metrics::counter!("preconf.admit.rejected_base_fee_total").increment(1);
            return Err(PreconfError::BaseFeeTooLow { tx_max_fee: tx.max_fee_per_gas(), base_fee });
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

        // ── Cumulative balance ─────────────────────────────────────────
        // Only what this queue holds. The ordinary pool's claim on the same
        // balance is deliberately not counted: it is not visible from here,
        // and reaching for it would put a read of the pool back into a path
        // that exists to no longer have one.
        let committed: U256 = held.map_or(U256::ZERO, |nonces| {
            nonces
                .range(next_nonce..nonce)
                .filter_map(|(_, h)| inner.entries.get(h))
                .fold(U256::ZERO, |sum, e| sum.saturating_add(e.cost))
        });
        let required = committed.saturating_add(cost);
        if required > chain_balance {
            metrics::counter!("preconf.admit.rejected_funds_total").increment(1);
            return Err(PreconfError::InsufficientFunds { balance: chain_balance, required });
        }

        // ── The (sender, nonce) slot ───────────────────────────────────
        // Two indices answer this, and both have to. The queue knows who is
        // waiting on the nonce; the classifier knows who *owns* it, which
        // outlasts the entry — a commitment whose receipt has gone out keeps
        // its nonce through the retention window with nothing left in here.
        //
        // The incumbent is *identified* here and evicted only at the very
        // end. A refusal in between would otherwise destroy a perfectly good
        // entry on behalf of a replacement that never arrived: the sender
        // loses the transaction it had and gets nothing for it.
        let mut replacing = None;
        if let Some(incumbent) = held.and_then(|nonces| nonces.get(&nonce)).copied() {
            match inner.entries.get(&incumbent).map(|e| e.status) {
                Some(status) if !status.is_replaceable() => {
                    return Err(PreconfError::ReplaceActiveCommitment);
                }
                Some(_) => replacing = Some(incumbent),
                None => {
                    // The queue's two indices disagree. Production heals and
                    // carries on rather than tearing down the sequencer.
                    error!(
                        target: "mantle::preconf",
                        sender = ?from, nonce, dangling_hash = ?incumbent,
                        "preconf_tx_set: dangling by_sender index detected; self-healing"
                    );
                    debug_assert!(
                        false,
                        "by_sender[{from:?}][{nonce}] -> {incumbent:?} but entry missing"
                    );
                    PreconfTxSetInner::unindex_sender_nonce(&mut inner.by_sender, from, nonce);
                    inner.drop_hash(&incumbent);
                }
            }
        }
        if let Err(owner) = classifier.claim_admission_slot(hash, &from, nonce, replacing) {
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
        //
        // A replacement takes a nonce this sender already holds, so it is not
        // one more in flight.
        if bytecode_hash.is_some_and(|code| code != KECCAK256_EMPTY) {
            let inflight = inner.by_sender.get(&from).map_or(0, BTreeMap::len) -
                usize::from(replacing.is_some());
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
        // Now, and not before: every rule has passed, so the incumbent is
        // genuinely being replaced rather than merely being in the way.
        if let Some(incumbent) = replacing {
            inner.drop_hash(&incumbent);
        }

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
                cost,
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
        Ok(Admitted::Inserted)
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
                // Only the status flip is left. Both callers of this path are
                // replays, which carry no responder and bypass the deadline
                // gate; a client resubmit goes through `admit` instead.
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
        if let Some(existing_hash) =
            inner.by_sender.get(&from).and_then(|nonces| nonces.get(&nonce)).copied()
        {
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
                    // it) AND sweeping any lingering `order` reference via
                    // `drop_hash`
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
                    PreconfTxSetInner::unindex_sender_nonce(&mut inner.by_sender, from, nonce);
                    inner.drop_hash(&existing_hash);
                }
            }
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
            // Not an admission, so no figure was ever computed — see the field.
            cost: U256::ZERO,
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

    /// Replacement-check lookup at admission — O(1) via `by_sender`.
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
    pub async fn forward_all<S: std::hash::BuildHasher>(
        &self,
        heads: &std::collections::HashMap<Address, u64, S>,
    ) {
        let mut inner = self.inner.lock().await;
        let to_drop: Vec<TxHash> = inner
            .by_sender
            .iter()
            .filter_map(|(sender, nonces)| heads.get(sender).map(|head| (nonces, *head)))
            .flat_map(|(nonces, head)| nonces.range(..head).map(|(_, hash)| *hash))
            .collect();
        inner.drop_hashes(&to_drop);
    }

    /// Every sender holding an entry.
    ///
    /// Cheaper than reading the entries for it: this walks one index and copies
    /// addresses, where [`Self::entries`] clones a view — transaction handle
    /// included — for each of them.
    pub async fn senders(&self) -> HashSet<Address> {
        self.inner.lock().await.by_sender.keys().copied().collect()
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
        inner.drop_hashes(&to_drop);
        to_drop
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
    /// [`Self::forward_all`] is the only path that drops it (a `Success` entry is
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
    /// Nothing else has to be undone: the transaction was never in the pool,
    /// so marking it here is the whole of taking it out of circulation.
    pub async fn mark_failed(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Failed).await?;
        Ok(())
    }

    /// `Waiting → Timeout`. **Soft terminal**, revivable by a same-hash retry
    /// through [`Self::admit`]. Called by the RPC handler when the client-side
    /// `preconf_timeout` fires before a receipt is delivered, or by dispatch's
    /// pre-apply deadline gate.
    pub async fn mark_timeout(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Timeout).await?;
        Ok(())
    }

    /// `Waiting → Canceled`. **Soft terminal** — like `Timeout`, revivable via
    /// same-hash retry through [`Self::admit`]. Signals **server pre-apply
    /// rejection** (block gas budget exhausted, admin action, ...): the EVM was
    /// never run, so the tx is not on chain — see
    /// [`crate::types::PreconfStatus`] for how far that reaches.
    /// Semantically distinct from `Timeout` (client's deadline hit).
    pub async fn mark_canceled(&self, hash: &TxHash) -> Result<(), MarkError> {
        self.transition_from_waiting(hash, PreconfStatus::Canceled).await?;
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

    /// Cancels the responder for `hash` (if any) with the given error.
    /// No-op if no responder is registered. The send is fire-and-forget —
    /// the receiver may have already dropped (client timed out).
    ///
    /// In the normal
    /// case (invariant #2 holds) this is a no-op. If invariant #2 is ever
    /// violated (both slots occupied for the same hash), the ghost responder is
    /// dropped rather than leaked as a zombie — its client will observe
    /// `RecvError` instead of a stuck
    pub async fn cancel_responder(&self, hash: &TxHash, err: PreconfError) {
        let responder = {
            let mut inner = self.inner.lock().await;
            inner.entries.get_mut(hash).and_then(|e| e.responder.take())
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
        inner.entries.get_mut(hash).and_then(|e| e.responder.take())
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
                chain_balance: U256::MAX,
                chain_base_fee: 0,
                cost: U256::ZERO,
                bytecode_hash: None,
            })
        }
        fn chain_nonce(mut self, n: u64) -> Self {
            self.0.chain_nonce = n;
            self
        }
        fn balance(mut self, b: u64) -> Self {
            self.0.chain_balance = U256::from(b);
            self
        }
        fn cost(mut self, c: u64) -> Self {
            self.0.cost = U256::from(c);
            self
        }
        fn delegated(mut self) -> Self {
            self.0.bytecode_hash = Some(B256::repeat_byte(0x7a));
            self
        }
    }

    /// Admission has to claim the verdict before it can claim a nonce, which
    /// is the order the real path runs in.
    async fn admit(
        set: &PreconfTxSet,
        c: &PreconfClassifier,
        req: Req,
        capacity: Capacity,
    ) -> Result<Admitted, PreconfError> {
        let hash = *req.0.tx.tx_hash();
        let from = req.0.from;
        let _ = c.claim_preconf(hash, &from, Some(&addr(0xee)));
        set.admit(c, req.0, capacity).await
    }

    #[tokio::test]
    async fn a_transaction_that_breaks_no_rule_is_queued_and_announced() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let mut rx = set.subscribe();

        let out = admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted);
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

    /// A replacement takes a nonce that is already counted, so holding the
    /// sender at its ceiling must not block it — otherwise a sender that
    /// filled its slots could never correct any of them.
    #[tokio::test]
    async fn a_replacement_does_not_count_against_a_full_sender() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let one = Capacity { max_account_slots: 1, ..cap() };
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), one).await.unwrap();
        set.mark_timeout(&B256::from([1u8; 32])).await.unwrap();

        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice), one).await;

        assert_eq!(out.unwrap(), Admitted::Inserted);
    }

    // ── Base fee ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_fee_cap_under_the_blocks_base_fee_is_refused_outright() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        set.reset_accounts(1_000_000_000);

        let out = admit(&set, &c, Req::new(tx(0, 1, 500_000_000), addr(1)), cap()).await;

        assert!(
            matches!(
                out,
                Err(PreconfError::BaseFeeTooLow {
                    tx_max_fee: 500_000_000,
                    base_fee: 1_000_000_000
                })
            ),
            "{out:?}",
        );
    }

    #[tokio::test]
    async fn a_fee_cap_at_the_base_fee_clears_it() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        set.reset_accounts(1_000_000_000);

        let out = admit(&set, &c, Req::new(tx(0, 1, 1_000_000_000), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted, "the floor is inclusive");
    }

    /// Between builds the chain's own floor applies. Skipping the check
    /// while no build is open would admit everything in that window, which is
    /// most of the time on an idle node — and the transaction still could not
    /// execute.
    #[tokio::test]
    async fn between_builds_the_chains_floor_still_applies() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        assert_eq!(set.build_base_fee(), None, "the premise: no build has run");

        let mut req = Req::new(tx(0, 1, 500_000_000), addr(1));
        req.0.chain_base_fee = 1_000_000_000;
        let out = admit(&set, &c, req, cap()).await;

        assert!(
            matches!(out, Err(PreconfError::BaseFeeTooLow { base_fee: 1_000_000_000, .. })),
            "{out:?}",
        );
    }

    /// And the block being built wins over the chain when there is one: it is
    /// the floor the transaction will actually be measured against.
    #[tokio::test]
    async fn an_open_build_supersedes_the_chains_floor() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        set.reset_accounts(100);

        let mut req = Req::new(tx(0, 1, 500), addr(1));
        req.0.chain_base_fee = 1_000_000_000;
        let out = admit(&set, &c, req, cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted, "judged against 100, not the stale tip");
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
        set.reset_accounts(0);

        let before = admit(&set, &c, Req::new(tx(1, 1, 0), alice).chain_nonce(0), cap()).await;
        assert!(matches!(before, Err(PreconfError::NonceGap { .. })), "{before:?}");

        set.observe_executed(alice, 0);
        let after = admit(&set, &c, Req::new(tx(1, 2, 0), alice).chain_nonce(0), cap()).await;

        assert_eq!(after.unwrap(), Admitted::Inserted, "nonce 0 has run, so nonce 1 is next");
    }

    // ── Cumulative balance ─────────────────────────────────────────────

    /// Each transaction is affordable alone; the chain is not. That is the
    /// case the ordinary pool parks silently and this refuses outright.
    #[tokio::test]
    async fn a_senders_queued_chain_must_fit_its_balance_as_a_whole() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice).balance(100).cost(60), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(1, 2, 0), alice).balance(100).cost(60), cap()).await;

        assert!(
            matches!(out, Err(PreconfError::InsufficientFunds { required, .. }) if required == U256::from(120)),
            "60 + 60 against 100; got {out:?}",
        );
    }

    #[tokio::test]
    async fn a_chain_that_fits_is_not_refused_for_its_total() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice).balance(100).cost(40), cap()).await.unwrap();
        let out = admit(&set, &c, Req::new(tx(1, 2, 0), alice).balance(100).cost(40), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted);
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

    #[tokio::test]
    async fn a_timed_out_incumbent_hands_its_nonce_over() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        let first = B256::from([1u8; 32]);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        set.mark_timeout(&first).await.unwrap();

        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted);
        assert_eq!(
            set.find_by_sender_nonce(&alice, 0).await.map(|e| e.hash),
            Some(B256::from([2u8; 32])),
            "and the newcomer holds the nonce",
        );
    }

    /// A replacement that is refused must leave the incumbent where it was.
    /// Evicting first and deciding afterwards costs the sender the
    /// transaction it had and gives it nothing in return — and the incumbent
    /// was replaceable, not wrong.
    #[tokio::test]
    async fn a_refused_replacement_leaves_the_incumbent_where_it_was() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        let first = B256::from([1u8; 32]);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice).delegated(), cap()).await.unwrap();
        set.mark_timeout(&first).await.unwrap();

        // Replaceable, so the slot is available — but the replacement is
        // refused after that point, by a rule further down.
        let over_cap = Capacity { max_inflight_delegated: 0, ..cap() };
        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice).delegated(), over_cap).await;

        assert!(matches!(out, Err(PreconfError::DelegatedInflightLimit { .. })), "{out:?}");
        assert_eq!(
            set.find_by_sender_nonce(&alice, 0).await.map(|e| e.hash),
            Some(first),
            "the incumbent must survive a replacement that never landed",
        );
    }

    /// `Failed` releases the nonce for the same reason `Timeout` does: the
    /// transaction is not on chain and is not going to be, so holding the
    /// nonce would strand the sender behind a commitment that is over.
    #[tokio::test]
    async fn a_failed_incumbent_also_hands_its_nonce_over() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let alice = addr(1);
        let first = B256::from([1u8; 32]);

        admit(&set, &c, Req::new(tx(0, 1, 0), alice), cap()).await.unwrap();
        set.mark_failed(&first).await.unwrap();

        let out = admit(&set, &c, Req::new(tx(0, 2, 0), alice), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Inserted);
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

    /// A replaying commitment is queued with its responder already consumed —
    /// the receipt went out in an earlier process. A client resubmitting
    /// those bytes has nobody answering for it, so refusing would leave it no
    /// way to learn the commitment's fate.
    #[tokio::test]
    async fn the_same_hash_with_nobody_waiting_on_it_takes_the_new_responder() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let hash = B256::from([1u8; 32]);

        // Queued with no responder, as journal restore leaves it.
        admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await.unwrap();

        let (resp, _rx) = oneshot::channel();
        let mut again = Req::new(tx(0, 1, 0), addr(1));
        again.0.responder = Some((Instant::now(), resp));
        let out = admit(&set, &c, again, cap()).await;

        assert_eq!(out.unwrap(), Admitted::Revived);
        assert_eq!(
            set.find_by_hash(&hash).await.unwrap().status,
            PreconfStatus::Waiting,
            "still queued, and its status was never touched",
        );
    }

    /// The documented retry: the client was told it timed out, and asks again
    /// with the same bytes.
    #[tokio::test]
    async fn the_same_hash_after_a_timeout_comes_back_to_life() {
        let set = PreconfTxSet::new(8);
        let c = classifier();
        let hash = B256::from([1u8; 32]);

        admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await.unwrap();
        set.mark_timeout(&hash).await.unwrap();

        let out = admit(&set, &c, Req::new(tx(0, 1, 0), addr(1)), cap()).await;

        assert_eq!(out.unwrap(), Admitted::Revived);
        assert_eq!(set.find_by_hash(&hash).await.unwrap().status, PreconfStatus::Waiting);
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

        assert_eq!(out.unwrap(), Admitted::Inserted);
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

        assert_eq!(out.unwrap(), Admitted::Inserted);
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
    fn reset_forgets_every_sender_and_pins_the_new_base_fee() {
        let mut view = AccountView::default();
        view.observe_executed(addr(0xaa), 7);
        view.observe_nonceless(addr(0xbb));

        view.reset(1_000_000_000);

        assert_eq!(view.next_nonce(&addr(0xaa), 3), 3);
        assert_eq!(view.next_nonce(&addr(0xbb), 3), 3);
        assert_eq!(view.base_fee, Some(1_000_000_000));
    }

    /// `None` rather than `0`: a base-fee floor reading a plain zero would
    /// admit everything in the window between startup and the first build,
    /// and never know it was doing so.
    #[test]
    fn the_base_fee_is_absent_until_a_build_has_run() {
        let mut view = AccountView::default();
        assert_eq!(view.base_fee, None);

        view.reset(0);

        assert_eq!(view.base_fee, Some(0), "zero is a base fee, not the absence of one");
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

        forward_one(&set, &addr(1), 7).await;

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

    #[tokio::test]
    async fn snapshot_view_omits_responder_by_construction() {
        let (resp_tx, _resp_rx) = oneshot::channel();
        let entry = TxEntry {
            hash: h(1),
            tx: make_tx(0, 1),
            from: addr(2),
            nonce: 0,
            cost: U256::ZERO,
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
            hb().prop_map(Op::SetResponder),
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
                            if let Some(rx) = st.held.take() {
                                assert_closed(rx, &format!("step {i}: clean-dropped responder"));
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
