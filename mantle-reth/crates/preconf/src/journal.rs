//! Append-only journal of preconf commitments for restart-safe operation.
//!
//! When the RPC handler returns a successful preconf event to a client,
//! the sequencer has made a promise — "this transaction will be in a
//! sealed block." If the node crashes before the tx is sealed, we owe
//! the client honest replay on restart: the tx must re-enter the pool,
//! the fifo must be re-populated, and the canonical-state handler must
//! be ready to forward-clean it once it does land.
//!
//! [`PreconfJournal`] is the on-disk substrate for that promise. The
//! file format is **JSON Lines**: one [`JournalEntry`] per line, append
//! only, line endings as record separators. Choosing a self-describing
//! text format over a binary one trades a few bytes per record for
//! trivial human-readability during incident triage — at 100 TPS the
//! file grows by ~50 KB/s, comfortably within disk budget for a
//! short-lived journal that gets rotated periodically.
//!
//! **No in-memory view of which commitments are still owed lives here.** It
//! used to: a `sealed` set, fed by the canonical-state handler, that rotation
//! consulted to drop already-on-chain entries and the pool listener consulted to
//! spot reorg reinjects. That made two structures track "this commitment is
//! over", and the journal's notion of over — *canonical once* — is one a reorg
//! can undo. [`PreconfClassifier`] now owns that decision; rotation receives it
//! as the `retain` predicate [`PreconfJournal::rotate`] takes, and the listener
//! asks `PreconfClassifier::is_promised` directly.
//!
//! The journal exposes `append_promised` / `load` / `rotate` for the durability
//! path, plus the startup helper [`restore_preconf_state`] and the background
//! rotation loop [`spawn_rejournal_loop`].

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_consensus::TxEnvelope;
use alloy_primitives::{Address, Bytes, TxHash};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use crate::{PreconfClassifier, PreconfTxSet};

/// One persisted preconf commitment. Carries everything needed to
/// re-inject the transaction into the pool on restart and to recognise
/// it later when it appears on chain.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Transaction hash — primary key.
    pub hash: TxHash,
    /// RLP-encoded transaction bytes. Used to re-inject the tx into
    /// the pool on startup if the pool's own journal lost it.
    pub tx_rlp: Bytes,
    /// Predicted L2 block height the commitment was promised for.
    /// Informational on restart; the canonical chain is authoritative.
    pub block_height: u64,
    /// Wall-clock ms at which the commitment was made. Used by
    /// operators to correlate journal entries against logs / metrics.
    pub committed_at_ms: u64,
}

/// Errors surfaced by the journal IO surface.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Underlying filesystem / IO error.
    #[error("journal IO: {0}")]
    Io(#[from] io::Error),
    /// A JSON record failed to deserialise. Wraps the source error and
    /// the offending line number so operators can pinpoint corruption.
    #[error("journal deserialisation at line {line}: {source}")]
    Decode {
        /// 1-based line number in the journal file.
        line: usize,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A JSON record serialised but contained a payload `serde_json`
    /// can not encode. Practically only happens on `Bytes` of unusual
    /// content; left distinct from `Io` for telemetry parity with the
    /// decode path.
    #[error("journal serialisation: {0}")]
    Encode(#[source] serde_json::Error),
}

/// On-disk append-only journal of preconf commitments.
///
/// The journal is `Sync`: a single instance can be held by an `Arc` and
/// shared between the RPC handler (writer) and the rejournal loop
/// (rotation). All writes serialise through an async `Mutex` around the file
/// handle.
///
/// It holds **no** view of which commitments are still owed. It used to keep a
/// `sealed` set for that, which made it a second tracker of "this commitment is
/// over" alongside the classifier — with a weaker notion of over ("canonical
/// once", which a reorg undoes). That decision now belongs to the classifier
/// alone and reaches rotation through the `retain` predicate
/// [`Self::rotate`] takes.
#[derive(Debug)]
pub struct PreconfJournal {
    /// Path to the journal file. Stored for rotation, which writes a
    /// sibling tmp file and atomically renames into place.
    path: PathBuf,
    /// Append handle protected by a `Mutex` because the trait
    /// `tokio::io::AsyncWriteExt::write_all` takes `&mut self`.
    writer: Mutex<File>,
    /// On-disk size cap in bytes that arms size-triggered rotation.
    /// Config validation guarantees a positive value whenever the journal
    /// is enabled (see [`crate::PreconfConfig`]).
    max_size: u64,
    /// Running on-disk byte count. Maintained by `append_promised`
    /// (`+= line.len()`, under the writer lock) and reset by `rotate`
    /// to the kept-bytes total. Avoids a `stat` syscall on the hot path.
    size_bytes: AtomicU64,
    /// Pinged by `append_promised` when `size_bytes` crosses `max_size`,
    /// waking [`run_rejournal_loop`] to force a rotation off the hot
    /// path. `notify_one` coalesces a burst of appends into a single
    /// pending permit.
    rotate_notify: Notify,
    /// Age after which `rotate` abandons an entry that `retain` still accepts
    /// (drops it even though it never landed). `None` disables it — then only
    /// `retain` drops entries. Set via [`Self::with_abandon_after`]; left `None`
    /// in most tests.
    abandon_unsealed_after: Option<Duration>,
}

/// Rotation intervals an **unsealed** commitment survives before `rotate`
/// abandons it as permanently un-landable (a promised tx re-lands within the
/// reorg/replay window — seconds — so outliving many cadences means it never
/// will). Abandon age = `rejournal_interval ×` this, so keep the cadence well
/// below `abandon window / this`.
pub const UNSEALED_ABANDON_ROTATIONS: u32 = 30;

impl PreconfJournal {
    /// Open (or create) the journal file at `path` in append mode.
    ///
    /// If the file already exists, its contents are left untouched —
    /// recovery callers should invoke [`Self::load`] before starting
    /// to append.
    ///
    /// `max_size` is the on-disk size cap that arms size-triggered
    /// rotation: once the file grows to `max_size` bytes, `append_promised`
    /// wakes [`run_rejournal_loop`] to rotate off the hot path. The byte
    /// counter is seeded from the existing file so the cap is enforced
    /// across restarts against pre-existing survivors. Config validation
    /// guarantees a positive cap whenever the journal is enabled.
    pub async fn open(path: impl AsRef<Path>, max_size: u64) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            // Best-effort dir creation; operator may have pre-created.
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).await?;
        let init_size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        // Seed the gauge from the on-disk size (carried across restarts).
        metrics::gauge!("preconf.journal.size_bytes").set(init_size as f64);
        let journal = Self {
            path,
            writer: Mutex::new(file),
            max_size,
            size_bytes: AtomicU64::new(init_size),
            rotate_notify: Notify::new(),
            abandon_unsealed_after: None,
        };
        // No in-memory set to seed from the file any more. What the seeding was
        // for — letting a post-restart commitment be recognised as ours — is now
        // done by `restore_preconf_state`, which marks every entry promised on
        // the classifier before admitting any of them.
        Ok(journal)
    }

    /// Path the journal is bound to. Stable for the lifetime of the
    /// instance.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Enable age-based abandonment: `rotate` drops an **unsealed** entry once
    /// its `committed_at_ms` is older than `ttl`. Fluent so production can chain
    /// it after [`Self::open`] without touching the test call sites.
    #[must_use]
    pub const fn with_abandon_after(mut self, ttl: Duration) -> Self {
        self.abandon_unsealed_after = Some(ttl);
        self
    }

    /// Append one commitment to the journal. Performs an explicit
    /// `flush` (not `sync_all`) so the bytes leave the runtime's
    /// buffer before this call returns; durability against power loss
    /// would require an additional `sync_all`, traded off against
    /// per-tx latency.
    pub async fn append_promised(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        // Encode first so a bad serialise does not partially write.
        let mut line = serde_json::to_vec(entry).map_err(JournalError::Encode)?;
        line.push(b'\n');
        let len = line.len() as u64;
        // The size counter is bumped *under the writer lock*, so the
        // on-disk write and the count stay consistent w.r.t. `rotate`'s
        // swap+reset (which also holds the writer lock). `notify_one`
        // is deferred until after the lock is released.
        let new_size = {
            let mut writer = self.writer.lock().await;
            writer.write_all(&line).await?;
            writer.flush().await?;
            self.size_bytes.fetch_add(len, Ordering::Relaxed) + len
        };
        metrics::gauge!("preconf.journal.size_bytes").set(new_size as f64);
        // Size-triggered rotation: wake the rejournal loop when the file
        // crosses the cap. The heavy `rotate()` runs off this hot path;
        // the loop rate-limits repeated triggers (see `run_rejournal_loop`).
        if new_size >= self.max_size {
            self.rotate_notify.notify_one();
        }
        Ok(())
    }

    /// Read the journal file from disk and return every entry that
    /// parsed successfully. Lines that fail to parse are logged as
    /// `warn` and skipped — the journal is best-effort recovery
    /// substrate, not a database transaction log, so a single bad
    /// line should not block startup.
    ///
    /// Returns the count of skipped corrupt lines as the second
    /// tuple field for telemetry / metric reporting.
    pub async fn load(&self) -> Result<(Vec<JournalEntry>, usize), JournalError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            // A missing journal is the normal first-boot path. Surface
            // an empty result instead of forcing every caller to match
            // on `ErrorKind::NotFound`.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(JournalError::Io(e)),
        };
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut out = Vec::new();
        let mut bad = 0usize;
        let mut line_no = 0usize;
        while let Some(line) = lines.next_line().await? {
            line_no += 1;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalEntry>(&line) {
                Ok(entry) => out.push(entry),
                Err(source) => {
                    warn!(
                        target: "mantle::preconf::journal",
                        line = line_no,
                        ?source,
                        "skipping corrupt journal entry"
                    );
                    bad += 1;
                }
            }
        }
        debug!(
            target: "mantle::preconf::journal",
            loaded = out.len(),
            bad,
            "journal load complete"
        );
        Ok((out, bad))
    }

    /// Rewrite the journal file keeping only the entries `retain` accepts,
    /// then atomically swap it for the live file.
    ///
    /// Implementation: read all entries, filter, write to a sibling
    /// `<path>.tmp`, then `rename` over the live file. On Unix the
    /// rename is atomic with respect to crashes — a power loss
    /// mid-rotate leaves either the old file or the new one intact,
    /// never a half-written hybrid.
    ///
    /// The caller is expected to be the rotation loop, not the hot
    /// RPC path. While rotation runs, `append_promised` is blocked
    /// behind the same writer `Mutex` — typically a few ms even at
    /// 100 TPS.
    pub async fn rotate(
        &self,
        retain: impl Fn(&TxHash) -> bool,
    ) -> Result<RotateStats, JournalError> {
        // Records `preconf.journal.rotate_duration_ms` on every exit path
        // (including the `?` early returns below).
        let _timer = RotateTimer(std::time::Instant::now());

        // Hold the writer lock for the WHOLE rotate (load → rename → reset),
        // not just the swap: an `append_promised` landing between `load()` and
        // the rename would otherwise be silently discarded by it. Appends
        // block until the new file is in place, then land in it.
        let mut writer = self.writer.lock().await;

        let (entries, bad_before) = self.load().await?;

        // Age-based abandonment: drop an entry older than the TTL that `retain`
        // still accepts — promised but permanently un-landable, else it replays
        // every restart. `committed_at_ms == 0` (a broken clock at write time)
        // is exempt: it carries no usable age, so only `retain` can drop it.
        let abandon_ttl_ms = self.abandon_unsealed_after.map(|d| d.as_millis() as u64);
        let now_ms = now_unix_ms();
        let expired = |e: &JournalEntry| -> bool {
            abandon_ttl_ms.is_some_and(|ttl| {
                e.committed_at_ms != 0 && now_ms.saturating_sub(e.committed_at_ms) > ttl
            })
        };

        // Which records may go is not this type's decision. `retain` is supplied
        // by the caller and reads the classifier: keep anything still being
        // tracked, drop what has been buried `SEAL_DEPTH` persisted blocks deep.
        // The journal used to answer this from its own `sealed` set, which meant
        // two structures tracking "this commitment is over" with two different
        // notions of over — and the journal's notion was "canonical once", which
        // a reorg can undo. Now there is one owner and the file layer only does
        // file work.
        //
        // The predicate is evaluated once per record here, so a concurrent
        // update lands in the *next* tick rather than half of this one.
        let mut kept = 0usize;
        let mut dropped = 0usize;
        let mut abandoned: Vec<TxHash> = Vec::new();
        let mut kept_bytes = 0u64;
        let tmp_path = tmp_path_for(&self.path);

        {
            let mut tmp =
                OpenOptions::new().create(true).truncate(true).write(true).open(&tmp_path).await?;
            for entry in &entries {
                if !retain(&entry.hash) {
                    dropped += 1;
                    continue;
                }
                if expired(entry) {
                    dropped += 1;
                    abandoned.push(entry.hash);
                    continue;
                }
                let mut line = serde_json::to_vec(entry).map_err(JournalError::Encode)?;
                line.push(b'\n');
                tmp.write_all(&line).await?;
                kept_bytes += line.len() as u64;
                kept += 1;
            }
            tmp.flush().await?;
        }

        // Atomic swap (writer lock held since the top): rename into place,
        // then re-open the writer against the new inode.
        tokio::fs::rename(&tmp_path, &self.path).await?;
        *writer = OpenOptions::new().create(true).append(true).open(&self.path).await?;
        // Reset the byte counter to the new file's true size while still
        // holding the writer lock, so it stays consistent with any
        // `append_promised` that serialises before/after this swap.
        self.size_bytes.store(kept_bytes, Ordering::Relaxed);
        metrics::gauge!("preconf.journal.size_bytes").set(kept_bytes as f64);

        // No in-memory set to prune alongside the file: the classifier owns
        // commitment tracking, and `retain` is how this rotate consulted it.
        if !abandoned.is_empty() {
            metrics::counter!("preconf.journal.abandoned_total").increment(abandoned.len() as u64);
        }

        Ok(RotateStats { kept, dropped, bad_lines_skipped: bad_before, abandoned })
    }
}

/// Telemetry-friendly summary of a single [`PreconfJournal::rotate`]
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateStats {
    /// Entries written to the new file.
    pub kept: usize,
    /// Sealed entries dropped during rotation.
    pub dropped: usize,
    /// Corrupt lines observed during the read pass. Rotate silently
    /// removes them from the rewritten file (they're not carried over
    /// into the new generation) — this count is reported for
    /// operator-facing metrics only, not for retry / repair logic.
    pub bad_lines_skipped: usize,
    /// Hashes dropped by **age**, not by `retain`.
    ///
    /// Reported rather than merely counted so the caller — which holds the
    /// classifier, as this type deliberately does not — can tell whether the
    /// commitment it just gave up on disk is one the process is still trying to
    /// honour in memory. Those two disagreeing is worth a warning; the journal
    /// itself cannot detect it.
    pub abandoned: Vec<TxHash>,
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Wall-clock ms since the Unix epoch, `0` if the clock predates 1970 (mirrors
/// [`JournalEntry::committed_at_ms`]'s fallback so `rotate`'s age check is safe).
fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Records `preconf.journal.rotate_duration_ms` on drop, so every exit path
/// of [`PreconfJournal::rotate`] (including the `?` early returns) is timed.
struct RotateTimer(std::time::Instant);

impl Drop for RotateTimer {
    fn drop(&mut self) {
        metrics::histogram!("preconf.journal.rotate_duration_ms")
            .record(self.0.elapsed().as_millis() as f64);
    }
}

// ─── Pool interaction trait ─────────────────────────────────────────────────

/// Minimal pool-side surface [`restore_preconf_state`] needs.
///
/// Production callers wrap a real [`reth_transaction_pool::TransactionPool`]
/// via [`RestorePoolAdapter`](crate::RestorePoolAdapter); tests inject a stub
/// that records `contains` / `add_envelope` calls without standing up the full
/// reth pool.
///
/// Kept here, alongside the journal, so the restore helper has no
/// dependency on `reth-transaction-pool` itself — that wiring lives at
/// the call site, not in the journal module.
#[async_trait::async_trait]
pub trait RestorePool: Send + Sync {
    /// Whether the pool already knows about this tx (e.g. via its own
    /// journal). Currently unused by [`restore_preconf_state`] — the
    /// unified `add_envelope` path handles both "new admit" and
    /// "already imported" branches — but kept on the trait for
    /// metric / telemetry callers that want an explicit pre-check.
    async fn contains(&self, hash: &TxHash) -> bool;

    /// Decode + recover + attempt to admit `tx_rlp` into the pool.
    ///
    /// Returns `Ok(recovered)` in both of the following cases:
    /// - the tx was newly admitted;
    /// - the pool rejected admission with `AlreadyImported` (e.g. reth's local-tx backup restored
    ///   the same tx first).
    ///
    /// In either case the caller needs the recovered envelope + sender
    /// to push into the fifo — whether the pool already had the tx is
    /// orthogonal.
    ///
    /// Only genuine pool errors (bad signature, nonce mismatch on the
    /// post-restart state, ...) surface as `Err(reason)` — the restore
    /// helper logs and skips those entries.
    async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip>;

    /// Recover just the `(sender, nonce)` of `tx_rlp` — no pool involvement.
    ///
    /// Exists so [`restore_preconf_state`]'s pre-pass can claim each
    /// commitment's slot before *any* entry is admitted; see
    /// [`PreconfClassifier::claim_slot`](crate::PreconfClassifier::claim_slot)
    /// for why that has to happen before `add_envelope`.
    ///
    /// `None` for anything that does not decode or whose signature does not
    /// recover — the entry will fail `add_envelope` for the same reason a moment
    /// later, which is where it gets logged.
    ///
    /// This decodes the same bytes `add_envelope` decodes again. Splitting
    /// `add_envelope` into recover + admit would avoid that, at the cost of a
    /// wider trait and a two-step protocol its three implementors would all have
    /// to get right; one extra ec-recover per journal entry, once per process
    /// start, is the cheaper trade.
    fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)>;

    /// Synchronously remove transactions from the pool by hash. Used
    /// by [`PreconfTxSet`]'s pool-eviction
    /// callback path — every transition to a non-on-chain terminal
    /// state (`Timeout` / `Canceled` / `Failed`) triggers a same-hash
    /// eviction to close the "client saw failure but tx later lands"
    /// SLA gap.
    ///
    /// Idempotent — absent hashes are silently ignored (reth's
    /// `pool.remove_transactions` returns an empty `Vec` in that
    /// case). Sync because reth's `TransactionPool::remove_transactions`
    /// is sync and holds only the pool's internal mutex briefly.
    fn remove_transactions(&self, hashes: Vec<TxHash>);
}

/// Whether a commitment is on the canonical chain, as far as this node can
/// tell.
///
/// Three-valued on purpose: [`Self::Unknown`] is a **fact about our knowledge**,
/// not a failure to be folded into either answer. Collapsing it into `No` turns
/// every honoured commitment on an index-pruned node into a false "commitment
/// lost" alarm; collapsing it into `Yes` reinstates the very
/// silently-wrong-report this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnChain {
    /// Found on the canonical chain, in the block at `height`.
    ///
    /// The height is carried because "on chain" is revocable: restore has to
    /// start the retention clock (`PreconfClassifier::mark_committed`) for this
    /// commitment, and that clock is a block depth. Without it a restored
    /// commitment that had already landed would keep a promise record that no
    /// rotation could ever drop.
    Yes {
        /// Canonical block number the transaction was found in.
        height: u64,
    },
    /// Not on the canonical chain, and the index that would have found it is
    /// intact — so the miss is trustworthy.
    No,
    /// Cannot be determined: the transaction-lookup index has been pruned, or
    /// the query itself failed.
    Unknown,
}

/// The chain-side lookup [`restore_preconf_state`] needs to tell "this
/// commitment landed" apart from "some other transaction consumed its nonce".
///
/// Separate from [`RestorePool`] because it is not about the pool: the pool has
/// no way to answer it. Its only chain-derived signal is the sender's account
/// nonce, and a nonce is consumed by *a* transaction, not necessarily by ours —
/// see [`RestoreSkip::NonceConsumed`].
pub trait CommitmentChainView: Send + Sync {
    /// Is `hash` on the canonical chain?
    fn commitment_on_chain(&self, hash: &TxHash) -> OnChain;
}

/// Why a journal entry was not re-admitted to the pool.
///
/// Deliberately says only what the **pool** can distinguish. Which of the three
/// things a consumed nonce means — commitment honoured, nonce stolen, or
/// unknowable — is resolved by [`restore_preconf_state`] with a
/// [`CommitmentChainView`], because the pool cannot answer it: its only
/// chain-derived signal is the sender's account nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSkip {
    /// The sender's account nonce has moved past this transaction, so the pool
    /// refuses it — but by **some** transaction, not necessarily this one.
    ///
    /// This variant used to be called `AlreadyOnChain` and was taken to mean the
    /// commitment had been kept. It cannot mean that on its own:
    /// `is_nonce_too_low()` reduces to `NonceNotConsistent { tx, state } =>
    /// tx < state`, and the producing check (`validate_sender_nonce`) compares
    /// the transaction's nonce against the *account's* and never looks at the
    /// hash — so a different transaction on the same nonce yields a
    /// byte-identical error.
    NonceConsumed(String),
    /// Anything else: corrupt bytes, a variant that cannot be preconfirmed, or
    /// a pool refusal that is not "nonce already consumed". A real failure —
    /// the commitment is lost.
    Rejected(String),
}

/// Output of a successful [`RestorePool::add_envelope`] — the decoded
/// envelope plus the recovered sender. The journal needs both: the
/// envelope to push into the fifo, the sender as the `from` field
/// keyed by `(sender, nonce)` for the replacement guard.
#[derive(Debug)]
pub struct RestoredEnvelope {
    /// Decoded, sender-recovered transaction envelope.
    pub envelope: TxEnvelope,
    /// Sender address recovered from the signature.
    pub from: Address,
}

// ─── Startup restore ────────────────────────────────────────────────────────

/// Walk the journal at startup and re-establish the in-memory state
/// the running system expects.
///
/// Order matters:
///
/// 1. Mark **every** loaded hash [`Verdict::Promised`] up front, before admitting any of them. This
///    is what carries "a receipt for this tx already went out to a client" across the restart, and
///    it has to happen first — step 2 runs the pool validator, which classifies. See below.
/// 2. Decode + attempt to admit into the pool via [`RestorePool::add_envelope`]. The trait treats
///    `AlreadyImported` as success — reth's own local-tx backup may have restored the same tx from
///    disk before this call, and either outcome yields the recovered envelope needed for the fifo
///    push.
/// 3. Push the recovered envelope into the fifo with
///    [`PreconfSource::Replay`](crate::types::PreconfSource::Replay) so the dispatch layer's
///    deadline / gas-budget gates bypass the tx (SLA: "receipt returned → tx must land").
///
/// ## Why step 1 has to come first
///
/// Restore deliberately does not re-derive eligibility — a commitment already
/// acknowledged to a client must come back regardless of what current policy
/// says. That used to happen *by accident*: cold start ran after restore, so the
/// allowlists were still empty, the validator classified every restored tx as
/// ineligible, and its per-tx gas ceiling therefore did not apply.
///
/// Cold start now runs before restore (it has to — a verdict frozen against
/// empty allowlists would never flip back), so that accident is gone: without
/// step 1 a restored tx would be classified `Eligible` and re-judged against the
/// *current* `preconf_max_gas_per_tx`. Lower that flag and restart, and
/// `add_envelope` starts rejecting commitments that a client was already told had
/// succeeded — silently, since restore skips and logs.
///
/// [`Verdict::Promised`] makes the exemption explicit instead of incidental:
/// `admit_and_claim` is get-or-insert, so the verdict installed here
/// survives step 2, and the validator returns a `Promised` transaction straight
/// to its inner validator — ahead of the ceiling and every other preconf gate.
///
/// Non-recoverable failures (corrupt tx bytes, pool refusal for reasons
/// other than `AlreadyImported`) are logged and skipped — best-effort
/// restore, never block startup.
///
/// [`Verdict::Promised`]: crate::classifier::Verdict::Promised
pub async fn restore_preconf_state<P: RestorePool, C: CommitmentChainView>(
    journal: &PreconfJournal,
    pool: &P,
    chain: &C,
    fifo: &Arc<PreconfTxSet>,
    classifier: &PreconfClassifier,
) {
    // Prune before replay: one rotate drops age-abandoned entries so we neither
    // re-inject them into the fifo nor re-`add_envelope` them every restart.
    // Best-effort — on failure we just replay the un-pruned file.
    //
    // `retain` keeps **everything**, deliberately. Every other caller passes a
    // predicate that reads the classifier, but at this point the classifier is
    // empty — restore is what populates it, a few lines below — so a
    // classifier-backed predicate here would discard every commitment we owe on
    // the very restart that is supposed to honour them. Age-abandonment is the
    // only pruning that is safe to apply before the records exist.
    if let Err(e) = journal.rotate(|_| true).await {
        warn!(
            target: "mantle::preconf::journal",
            ?e,
            "pre-restore rotate failed; restoring against un-pruned journal"
        );
    }
    let (entries, bad_lines) = match journal.load().await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                target: "mantle::preconf::journal",
                ?e,
                "journal load failed; continuing without restore"
            );
            return;
        }
    };
    info!(
        target: "mantle::preconf::journal",
        count = entries.len(),
        bad_lines,
        "preconf journal load"
    );

    // Step 1 — every hash is a live commitment, and owns its nonce, before any
    // of them is admitted.
    //
    // A pre-pass, not folded into the loop below, for two reasons.
    //
    // **The verdict.** `add_envelope` hands the tx to the pool, which runs the
    // validator, which classifies whatever is not yet marked. Two journal entries
    // sharing a `(sender, nonce)` are enough to matter: admitting the first one
    // would classify the *second* against the current allowlists and subject it
    // to the replacement guard, rather than treating it as the commitment it is.
    // Marking the whole set up front closes that in one step; marking each entry
    // just before its own admission would leave the tail exposed.
    //
    // **The slot.** `admit_promised` has only a hash — it runs before the
    // envelope is decoded — so it cannot record `(sender, nonce)`, and the claim
    // would otherwise be back-filled only when this entry reaches the validator.
    // In between, the nonce reads as free to the replacement guard, so a
    // same-nonce transaction admitted in that interval takes the slot and the
    // commitment loses the nonce it was already acknowledged for. Nothing can
    // admit in that interval as the node is wired today — `cli::node` runs this
    // before `spawn_maintenance_tasks` (reth's local-tx backup loader), the RPC
    // server and the network — but that is a guarantee held by startup *ordering*,
    // which a later refactor can silently move. Claiming here makes it structural.
    // One call now does both: `mark_promised` records "a receipt for this went
    // out" (in the previous process, which is why nothing in this one would
    // otherwise know) **and** claims the slot. They were two steps —
    // `admit_promised` then `claim_slot` — only because the first ran before the
    // envelope was decoded and so had no sender/nonce to claim with; `recover_slot`
    // below produces exactly those, so the split bought nothing.
    for entry in &entries {
        match pool.recover_slot(&entry.tx_rlp) {
            Some((from, nonce)) => {
                if let Err(owner) = classifier.mark_promised(entry.hash, &from, nonce) {
                    // Someone already owns the nonce, so this commitment is the
                    // one that will lose it. Deliberately not seized — see
                    // `mark_promised`. `add_envelope` below decides the outcome
                    // (typically `ReplacementUnderpriced`) and logs it.
                    warn!(
                        target: "mantle::preconf::journal",
                        hash = ?entry.hash,
                        ?owner,
                        "a same-nonce tx already owns this slot; commitment may not be honoured"
                    );
                }
            }
            // Undecodable envelope. **No record is written**, deliberately.
            //
            // These are the same bytes that decoded successfully in the RPC
            // handler before the receipt went out, and `recover_slot` shares its
            // first step with `add_envelope`
            // (`recover_raw_transaction::<PoolPooledTx<P>>`), so this entry is
            // also about to be rejected by `add_envelope` below — it will never
            // enter the pool and never get a fifo entry. A record for it would be
            // unusable, and a `Promised` one would break the rule that a promise
            // record names the `(sender, nonce)` it was issued against.
            //
            // Reaching this means the file was corrupted in a way that survived
            // JSON parsing, or this binary no longer supports that transaction
            // type. Either way it is a broken commitment we cannot even name, so
            // it is an `error!`, not a `debug!`.
            None => {
                error!(
                    target: "mantle::preconf::journal",
                    hash = ?entry.hash,
                    "journal entry does not decode; its commitment cannot be honoured or even \
                     attributed to a (sender, nonce)"
                );
                metrics::counter!("preconf.journal.restore_undecodable").increment(1);
            }
        }
    }

    let mut restored = 0usize;
    let mut honored = 0usize;
    let mut nonce_taken = 0usize;
    let mut unknown = 0usize;
    let mut rejected = 0usize;

    for entry in entries {
        let recovered = match pool.add_envelope(&entry.tx_rlp).await {
            Ok(rec) => rec,
            Err(RestoreSkip::NonceConsumed(reason)) => {
                // The nonce is gone, but the pool cannot say to whom. Ask the
                // chain: the three answers mean entirely different things, and
                // only the first is the commitment having been kept.
                match chain.commitment_on_chain(&entry.hash) {
                    OnChain::Yes { height } => {
                        // The promise was kept before the restart. Start its
                        // retention clock at the block it actually landed in:
                        // rotation keeps a record only while the classifier is
                        // still tracking it, so without this the entry would be
                        // immortal — nothing else will ever report this block,
                        // it is already in the past, and every future restart
                        // would replay (and complain about) the same entry.
                        //
                        // Note this deliberately does not release the tracking
                        // outright, which is what the old `mark_sealed` amounted
                        // to. Landing is revocable; if the block is shallow, a
                        // reorg right after startup must still find the
                        // commitment holding its nonce.
                        classifier.mark_committed(&entry.hash, height);
                        debug!(
                            target: "mantle::preconf::journal",
                            hash = ?entry.hash,
                            height,
                            reason,
                            "restored tx is already on chain; commitment was kept"
                        );
                        honored += 1;
                    }
                    OnChain::No => {
                        // A different transaction took this nonce. The commitment
                        // is broken and cannot be recovered while that stays true
                        // — the hash is bound to its nonce by its own signature.
                        //
                        // Deliberately **not** sealed: sealing is the grounds on
                        // which rotation forgets an entry, and there is nothing
                        // here to forget yet. If the transaction that took the
                        // nonce is itself reverted, this commitment becomes
                        // applicable again. The cost is that the entry is
                        // immortal until then, and every restart repeats this
                        // warning — which is the right noise for a broken
                        // promise.
                        warn!(
                            target: "mantle::preconf::journal",
                            hash = ?entry.hash,
                            reason,
                            "restored tx is NOT on chain but its nonce was consumed by another \
                             transaction; commitment is broken"
                        );
                        nonce_taken += 1;
                    }
                    OnChain::Unknown => {
                        // Not sealed either: an entry we cannot judge must be
                        // kept. Actionable for operators, since it means the
                        // transaction-lookup index this check needs has been
                        // pruned away.
                        warn!(
                            target: "mantle::preconf::journal",
                            hash = ?entry.hash,
                            reason,
                            "restored tx's nonce was consumed but whether the tx itself landed \
                             cannot be determined (transaction-lookup index pruned?); entry kept"
                        );
                        unknown += 1;
                    }
                }
                continue;
            }
            Err(RestoreSkip::Rejected(reason)) => {
                warn!(
                    target: "mantle::preconf::journal",
                    hash = ?entry.hash,
                    reason,
                    "pool rejected restored tx; commitment cannot be honoured"
                );
                rejected += 1;
                continue;
            }
        };

        // Push to fifo. `ConflictActive` happens if a fresher tx
        // already occupies the (sender, nonce) slot — accept the
        // newer entry, don't shove a stale journaled one over it.
        let _ = fifo
            .push_if_absent(
                Arc::new(recovered.envelope),
                recovered.from,
                crate::types::PreconfSource::Replay,
            )
            .await;

        restored += 1;
    }

    // `nonce_taken` is the one to alert on: each is a receipt handed out for a
    // transaction that can no longer land. It is published as a counter too,
    // because a log line that only appears at startup is easy to miss.
    metrics::counter!("preconf.journal.restore_nonce_taken").increment(nonce_taken as u64);
    metrics::counter!("preconf.journal.restore_unknown").increment(unknown as u64);
    info!(
        target: "mantle::preconf::journal",
        restored,
        honored,
        nonce_taken,
        unknown,
        rejected,
        "preconf restore complete"
    );
}

// ─── Background rotation loop ───────────────────────────────────────────────

/// Runs the journal rotation loop until `shutdown` resolves, then performs
/// one final rotation and returns.
///
/// The loop wakes every `interval`, calls [`PreconfJournal::rotate`],
/// and logs the [`RotateStats`]. Rotation failures are logged but do
/// not terminate the loop — the next tick retries.
///
/// `shutdown` is any future that resolves to `()` when the caller wants
/// the loop to stop. `select!` is not preemptive across awaits *within*
/// a rotate call, so a shutdown signal fired mid-rotate is observed
/// only when the current rotate resolves — this guarantees the file
/// is never left in a half-written state.
///
/// After the shutdown signal is observed, one **final** rotation is
/// attempted so any entries that have accumulated since the last tick
/// (in particular sealed hashes reported by the canonical-state handler
/// on the way down) are dropped from the on-disk file before the
/// process exits. Callers that hold a reth `GracefulShutdownGuard` must
/// keep it alive across this call so the `TaskManager` waits for the
/// final rotate.
///
/// The first rotation is skipped (the interval's immediate-first tick
/// is consumed at start) so a long-running node does not rotate a
/// nearly-empty journal in the first few seconds after boot.
pub async fn run_rejournal_loop<F, T>(
    journal: Arc<PreconfJournal>,
    classifier: Arc<PreconfClassifier>,
    interval: Duration,
    shutdown: F,
) -> T
where
    F: Future<Output = T>,
{
    // A record may go once the classifier has stopped tracking its commitment:
    // observed on chain and buried `SEAL_DEPTH` persisted blocks deep. Anything
    // else — never landed, landed but shallow, landed and then reorged out — is
    // still owed and stays in the file.
    let retain = |hash: &TxHash| !classifier.is_retention_expired(hash);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the immediate-first tick so we don't rotate at t=0.
    ticker.tick().await;
    // Rate-limit size-triggered rotations so a burst of appends over the
    // cap (e.g. survivors alone exceed `max_size`) can't spin the
    // expensive full-file `rotate()` on every append. Capped at the
    // periodic interval so it never rotates more often than the timer
    // would anyway. `None` ⇒ never rotated yet ⇒ first trigger is honoured.
    let min_gap = interval.min(SIZE_ROTATE_MIN_GAP);
    let mut last_rotate: Option<Instant> = None;
    debug!(
        target: "mantle::preconf::journal",
        ?interval,
        ?min_gap,
        "preconf journal rotation loop started"
    );
    tokio::pin!(shutdown);
    let signal_output = loop {
        tokio::select! {
            // `biased` — drain shutdown first so a torn-down
            // service does not perform one more rotation after
            // the shutdown signal.
            biased;
            output = &mut shutdown => {
                debug!(target: "mantle::preconf::journal", "rotation loop shutting down");
                break output;
            }
            // Size-triggered rotation. Honour only if `min_gap` has
            // elapsed since the last rotate; otherwise drop this wake —
            // a later append re-notifies (the file is still over the cap)
            // and the periodic ticker is the safety net.
            _ = journal.rotate_notify.notified() => {
                let now = Instant::now();
                if last_rotate.is_none_or(|t| now.duration_since(t) >= min_gap) {
                    log_rotate(journal.rotate(&retain).await, "size", &classifier);
                    last_rotate = Some(now);
                }
            }
            _ = ticker.tick() => {
                log_rotate(journal.rotate(&retain).await, "tick", &classifier);
                last_rotate = Some(Instant::now());
            }
        }
    };

    // Final rotate on shutdown — persist any sealed hashes accumulated
    // since the last tick before exiting. `signal_output` is held alive
    // across this await so callers passing a graceful-shutdown guard as
    // `T` keep their runtime's shutdown latch open until the final
    // on-disk write completes. Failures are logged; we do not surface
    // them because the process is going away anyway.
    log_rotate(journal.rotate(&retain).await, "shutdown", &classifier);

    signal_output
}

/// Minimum wall-clock gap between two size-triggered rotations, before
/// being capped at the periodic interval (see [`run_rejournal_loop`]).
/// Set to the default L2 slot (2s): a burst of size triggers within one
/// slot collapses to a single rotate, so the expensive full-file rewrite
/// runs at most once per slot off the hot path.
const SIZE_ROTATE_MIN_GAP: Duration = Duration::from_secs(2);

/// Log a rotation outcome uniformly across the tick / size / shutdown
/// trigger sites.
fn log_rotate(
    result: Result<RotateStats, JournalError>,
    reason: &'static str,
    classifier: &PreconfClassifier,
) {
    match result {
        Ok(stats) => {
            // An age-abandoned entry the classifier is still tracking means the
            // two halves of commitment tracking have diverged: this process is
            // still replaying the commitment, but a restart from here on would
            // not know it was ever owed. Silently dropping that is what this
            // warning exists to prevent.
            for hash in &stats.abandoned {
                if classifier.is_promised(hash) {
                    warn!(
                        target: "mantle::preconf::journal",
                        ?hash, reason,
                        "abandoned a journal entry for a commitment still being tracked in \
                         memory; it will not survive a restart"
                    );
                }
            }
            debug!(
                target: "mantle::preconf::journal",
                reason,
                kept = stats.kept,
                dropped = stats.dropped,
                abandoned = stats.abandoned.len(),
                bad = stats.bad_lines_skipped,
                "journal rotation"
            );
        }
        Err(e) => warn!(
            target: "mantle::preconf::journal",
            reason,
            ?e,
            "journal rotation failed"
        ),
    }
}

/// Spawns [`run_rejournal_loop`] on the ambient tokio runtime, driven
/// by a `oneshot::Receiver<()>` shutdown channel.
///
/// This is a convenience wrapper primarily used by tests and any caller
/// that owns its own runtime. Production wiring in `mantle-reth-cli`
/// uses [`run_rejournal_loop`] directly under
/// `TaskExecutor::spawn_critical_with_graceful_shutdown_signal` so
/// the reth `TaskManager` participates in the graceful shutdown handoff.
pub fn spawn_rejournal_loop(
    journal: Arc<PreconfJournal>,
    classifier: Arc<PreconfClassifier>,
    interval: Duration,
    shutdown_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `oneshot::Receiver` resolves `Err` when the sender is dropped;
        // treat both `Ok(())` and drop as shutdown signals so callers
        // don't have to send explicitly.
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        let () = run_rejournal_loop(journal, classifier, interval, shutdown).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn entry(byte: u8, height: u64) -> JournalEntry {
        JournalEntry {
            hash: TxHash::from([byte; 32]),
            tx_rlp: Bytes::from(vec![byte; 4]),
            block_height: height,
            committed_at_ms: 1_000 + u64::from(byte),
        }
    }

    async fn fresh_journal() -> (TempDir, PreconfJournal) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        (dir, j)
    }

    /// A `JournalEntry` with an explicit commit timestamp.
    fn entry_at(byte: u8, height: u64, committed_at_ms: u64) -> JournalEntry {
        JournalEntry {
            hash: TxHash::from([byte; 32]),
            tx_rlp: Bytes::from(vec![byte; 4]),
            block_height: height,
            committed_at_ms,
        }
    }

    /// With abandonment enabled, `rotate` drops an unsealed entry older than the
    /// TTL and keeps a fresh one (sealed entries are dropped regardless of age).
    #[tokio::test]
    async fn rotate_abandons_stale_unsealed_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = PreconfJournal::open(&path, 0)
            .await
            .unwrap()
            .with_abandon_after(Duration::from_secs(60));

        let now = now_unix_ms();
        let stale = entry_at(1, 10, 1_000); // ~epoch ⇒ far older than 60s
        let fresh = entry_at(2, 11, now); // just committed
        j.append_promised(&stale).await.unwrap();
        j.append_promised(&fresh).await.unwrap();

        let stats = j.rotate(|_| true).await.unwrap();
        assert_eq!(stats.dropped, 1, "only the stale unsealed entry is abandoned");
        assert_eq!(
            stats.abandoned,
            vec![stale.hash],
            "and it is reported by hash, not merely counted — the caller holds the \
             classifier and is the only one that can tell whether the commitment \
             just given up on disk is still being tracked in memory",
        );

        let (survivors, _) = j.load().await.unwrap();
        assert_eq!(survivors, vec![fresh], "fresh unsealed entry survives; stale one abandoned");
    }

    /// Default (abandonment disabled) preserves the old behaviour: a stale
    /// unsealed entry is kept, no matter how old.
    #[tokio::test]
    async fn rotate_without_abandon_keeps_stale_unsealed() {
        let (_dir, j) = fresh_journal().await;
        let stale = entry_at(1, 10, 1_000);
        j.append_promised(&stale).await.unwrap();

        let stats = j.rotate(|_| true).await.unwrap();
        assert_eq!(stats.dropped, 0);
        let (survivors, _) = j.load().await.unwrap();
        assert_eq!(survivors, vec![stale], "no abandonment configured ⇒ survivor kept");
    }

    #[tokio::test]
    async fn open_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("preconf.jsonl");
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        assert!(path.exists(), "open must create the file");
        assert_eq!(j.path(), path);
    }

    #[tokio::test]
    async fn append_then_load_roundtrip() {
        let (_dir, j) = fresh_journal().await;
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, vec![e1, e2]);
        assert_eq!(bad, 0);
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        // Construct manually — `open` would create the file.
        let j = PreconfJournal {
            path: path.clone(),
            writer: Mutex::new(File::create(dir.path().join("dummy")).await.unwrap()),
            max_size: 0,
            size_bytes: AtomicU64::new(0),
            rotate_notify: Notify::new(),
            abandon_unsealed_after: None,
        };
        let (loaded, bad) = j.load().await.unwrap();
        assert!(loaded.is_empty());
        assert_eq!(bad, 0);
    }

    #[tokio::test]
    async fn load_skips_corrupt_lines_and_reports_count() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // Hand-craft a file with one good line, one corrupt line, one good line.
        let good = serde_json::to_string(&entry(7, 70)).unwrap();
        let bad = "{this is not json}";
        let last = serde_json::to_string(&entry(8, 80)).unwrap();
        tokio::fs::write(&path, format!("{good}\n{bad}\n{last}\n")).await.unwrap();
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        let (loaded, bad_count) = j.load().await.unwrap();
        assert_eq!(loaded, vec![entry(7, 70), entry(8, 80)]);
        assert_eq!(bad_count, 1);
    }

    #[tokio::test]
    async fn load_ignores_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let good = serde_json::to_string(&entry(3, 30)).unwrap();
        // Triple blank lines around the good one.
        tokio::fs::write(&path, format!("\n\n{good}\n\n")).await.unwrap();
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, vec![entry(3, 30)]);
        assert_eq!(bad, 0);
    }

    /// `abandoned` reports **age** drops only. A record `retain` refused is a
    /// commitment the classifier says is over, and reporting it would make the
    /// caller warn about something that ended legitimately — the two reasons for
    /// leaving the file are not interchangeable.
    #[tokio::test]
    async fn rotate_reports_only_age_drops_as_abandoned() {
        let (_dir, j) = fresh_journal().await;
        let a = entry_at(1, 10, now_unix_ms());
        let b = entry_at(2, 11, now_unix_ms());
        j.append_promised(&a).await.unwrap();
        j.append_promised(&b).await.unwrap();

        // `retain` refuses `a`; abandonment is disabled on this journal.
        let stats = j.rotate(|h| *h != a.hash).await.unwrap();

        assert_eq!(stats.dropped, 1);
        assert!(
            stats.abandoned.is_empty(),
            "a record the classifier said to drop is not an abandonment — reporting it \
             would make the caller warn about a commitment that is legitimately over",
        );
    }

    /// Rotation keeps whatever `retain` says to keep. The predicate is supplied
    /// by the caller (production passes "the classifier is still tracking this")
    /// rather than read from a set the journal owns — that set was a second,
    /// weaker notion of "this commitment is over" living alongside the
    /// classifier's.
    #[tokio::test]
    async fn rotate_keeps_exactly_what_retain_accepts() {
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        let e_b = entry(2, 11);
        let e_c = entry(3, 12);
        j.append_promised(&e_a).await.unwrap();
        j.append_promised(&e_b).await.unwrap();
        j.append_promised(&e_c).await.unwrap();

        let dropped_hash = e_b.hash;
        let stats = j.rotate(|h| *h != dropped_hash).await.unwrap();
        assert_eq!(stats.kept, 2);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.bad_lines_skipped, 0);

        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![e_a, e_c]);
    }

    /// The safe direction: a predicate that keeps everything drops nothing, so a
    /// classifier that is still tracking every commitment cannot lose one to a
    /// rotation tick.
    #[tokio::test]
    async fn rotate_keeps_everything_when_retain_always_true() {
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        let e_b = entry(2, 11);
        j.append_promised(&e_a).await.unwrap();
        j.append_promised(&e_b).await.unwrap();

        let stats = j.rotate(|_| true).await.unwrap();
        assert_eq!((stats.kept, stats.dropped), (2, 0));
        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![e_a, e_b]);
    }

    #[tokio::test]
    async fn rotate_then_append_writes_to_new_file_handle() {
        // Verify the writer is re-opened against the new inode after
        // rotation — a subsequent append must land in the rotated file.
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        j.append_promised(&e_a).await.unwrap();
        let dropped_hash = e_a.hash;
        j.rotate(|h| *h != dropped_hash).await.unwrap();

        let e_b = entry(2, 11);
        j.append_promised(&e_b).await.unwrap();

        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![e_b]);
    }

    // ── restore_preconf_state ──────────────────────────────────────

    /// Stub pool that records `contains` / `add_envelope` calls. We
    /// don't go through real reth pool machinery — that's wired in by
    /// the cli crate at a later phase. The stub fabricates plausible
    /// envelopes for every `add_envelope` call.
    struct StubPool {
        // Hashes the stub will report as already-present.
        known: HashSet<TxHash>,
        // Counts for assertions.
        contains_calls: std::sync::Mutex<Vec<TxHash>>,
        add_calls: std::sync::Mutex<Vec<Bytes>>,
        // Whether add_envelope should return Err.
        reject_add: bool,
    }

    impl StubPool {
        fn new() -> Self {
            Self {
                known: HashSet::new(),
                contains_calls: std::sync::Mutex::new(Vec::new()),
                add_calls: std::sync::Mutex::new(Vec::new()),
                reject_add: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl RestorePool for StubPool {
        async fn contains(&self, hash: &TxHash) -> bool {
            self.contains_calls.lock().unwrap().push(*hash);
            self.known.contains(hash)
        }
        fn remove_transactions(&self, _hashes: Vec<TxHash>) {
            // No mark_* fires in journal-only tests; keep no-op.
        }
        /// Must agree with the `(from, nonce)` `add_envelope` fabricates below —
        /// restore claims the slot from this and pushes the fifo entry from that,
        /// so a mismatch would silently test nothing.
        fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
            let seed = tx_rlp.first().copied().unwrap_or(0);
            Some((Address::from([seed; 20]), u64::from(seed)))
        }
        async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
            self.add_calls.lock().unwrap().push(tx_rlp.clone());
            if self.reject_add {
                return Err(RestoreSkip::Rejected("rejected by stub".into()));
            }
            // Fabricate an envelope. We use a deterministic dummy
            // legacy tx — the journal restore code only needs `envelope`
            // and `from` to be present; nothing reads their content
            // beyond push_if_absent's bookkeeping.
            use alloy_consensus::{Signed, TxLegacy};
            use alloy_primitives::Signature;
            let nonce = u64::from(tx_rlp.first().copied().unwrap_or(0));
            let inner = TxLegacy { nonce, ..Default::default() };
            let sig = Signature::test_signature();
            // Derive a non-deterministic but stable hash from the rlp byte.
            let hash_byte = tx_rlp.first().copied().unwrap_or(0);
            let hash = TxHash::from([hash_byte; 32]);
            let envelope = TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash));
            let from = Address::from([hash_byte; 20]);
            Ok(RestoredEnvelope { envelope, from })
        }
    }

    /// Scripted chain view: answers every hash the same way.
    ///
    /// [`OnChain::Yes`] is what tests that predate the three-way split want —
    /// back then a consumed nonce *was* "the commitment landed", so passing `Yes`
    /// keeps their subject unchanged.
    struct StubChain(OnChain);

    impl CommitmentChainView for StubChain {
        fn commitment_on_chain(&self, _hash: &TxHash) -> OnChain {
            self.0
        }
    }

    /// The chain view for tests that are not about this distinction.
    fn landed() -> StubChain {
        StubChain(OnChain::Yes { height: LANDED_AT })
    }

    /// Height `landed()` reports. Arbitrary; the tests only care about it
    /// relative to the persisted watermark they publish.
    const LANDED_AT: u64 = 10;

    /// The production rotation predicate: a record stays while the classifier is
    /// still tracking its commitment. Tests use it so they exercise the same
    /// decision production does, rather than a hand-written stand-in.
    fn retain_tracked(c: &PreconfClassifier) -> impl Fn(&TxHash) -> bool + '_ {
        move |h| !c.is_retention_expired(h)
    }

    /// Classifier for restore tests, with **empty allowlists on purpose**.
    ///
    /// That is the real production condition — cold start may legitimately have
    /// loaded two empty lists (governance allows nobody), and restore must still
    /// bring commitments back. It is also the condition `restart_replay.rs`
    /// structurally cannot reproduce, because that harness seeds the lists before
    /// the node starts.
    ///
    /// Built **enabled**. `PreconfConfig::default()` has `enabled: false`, which
    /// short-circuits every write on this type — restore would appear to run and
    /// record nothing. Any node that reaches restore has preconf on.
    fn empty_classifier() -> PreconfClassifier {
        PreconfClassifier::from_config(&crate::PreconfConfig {
            enabled: true,
            ..crate::PreconfConfig::default()
        })
    }

    /// A classifier that has finished tracking `hashes`: each was promised,
    /// observed on chain, and has since been buried `SEAL_DEPTH` persisted
    /// blocks deep. That is the only state in which rotation may drop a record,
    /// so it is what the rejournal-loop tests need to set up.
    fn classifier_done_with(hashes: &[TxHash]) -> Arc<PreconfClassifier> {
        let c = empty_classifier();
        for (i, h) in hashes.iter().enumerate() {
            // Distinct (sender, nonce) per hash so the claims do not collide.
            let _ = c.mark_promised(*h, &Address::from([0xEE; 20]), i as u64);
            c.mark_committed(h, LANDED_AT);
        }
        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        for h in hashes {
            assert!(c.is_retention_expired(h), "fixture must actually be releasable");
        }
        Arc::new(c)
    }

    /// A classifier with the watermark already high enough that anything
    /// [`finish_tracking`] marks becomes immediately releasable. For tests that
    /// need to finish a commitment *after* the rotation loop has started.
    fn classifier_with_high_watermark() -> Arc<PreconfClassifier> {
        let c = empty_classifier();
        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        Arc::new(c)
    }

    /// Take `hash` through promise → committed, so the (already high) watermark
    /// makes it releasable. The mid-flight equivalent of the old `mark_sealed`.
    fn finish_tracking(c: &PreconfClassifier, hash: TxHash) {
        // The nonce is derived from the hash so repeated calls do not collide.
        let _ = c.mark_promised(hash, &Address::from([0xEE; 20]), u64::from(hash.0[0]));
        c.mark_committed(&hash, LANDED_AT);
        assert!(c.is_retention_expired(&hash));
    }

    #[tokio::test]
    async fn restore_from_empty_journal_is_noop() {
        let (_dir, j) = fresh_journal().await;
        let pool = StubPool::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_classifier()).await;
        assert!(pool.contains_calls.lock().unwrap().is_empty());
        assert!(pool.add_calls.lock().unwrap().is_empty());
        assert!(fifo.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn restore_injects_missing_txs_and_pushes_fifo() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();

        let pool = StubPool::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_classifier()).await;

        assert_eq!(pool.add_calls.lock().unwrap().len(), 2, "both txs admitted");
        let snapshot = fifo.snapshot().await;
        assert_eq!(snapshot.len(), 2);
    }

    #[tokio::test]
    async fn restore_pushes_fifo_when_pool_already_contains() {
        // Regression guard for J5: pre-fix, when pool.contains returned
        // true, restore's inner branch called `add_envelope` and treated
        // the resulting `Err(AlreadyImported)` as `continue;` — the
        // fifo push was skipped. Post-fix, `add_envelope`'s trait
        // contract treats AlreadyImported as `Ok(recovered)` and
        // restore unconditionally pushes to the fifo.
        let (_dir, j) = fresh_journal().await;
        let e1 = entry(3, 30);
        j.append_promised(&e1).await.unwrap();

        let mut pool = StubPool::new();
        pool.known.insert(e1.hash);
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_classifier()).await;

        assert_eq!(pool.add_calls.lock().unwrap().len(), 1);
        // Core J5 assertion: fifo received the entry.
        assert_eq!(fifo.snapshot().await.len(), 1);
    }

    /// **The ordering invariant of C4.** Every entry must already carry
    /// `Verdict::Promised` **before the first one is offered to the pool**, not
    /// merely before its own admission. Asserted from inside `add_envelope`, i.e.
    /// at exactly the moment the real validator would run.
    ///
    /// Why the stronger form: admitting one tx can cause the pool to validate
    /// another — two journal entries sharing a `(sender, nonce)` are enough, since
    /// admitting the first offers the second to the pool as a replacement — and
    /// validation is what freezes a verdict. Marking each entry just before its
    /// own admission would leave the tail of the set exposed to exactly the
    /// gas-ceiling rejection `Promised` exists to prevent.
    #[tokio::test]
    async fn restore_marks_every_entry_promised_before_admitting_any() {
        /// Pool that, on each tx it is handed, snapshots the verdicts of **all**
        /// journal hashes — including the ones not offered yet.
        struct VerdictSpyPool {
            classifier: Arc<PreconfClassifier>,
            hashes: Vec<TxHash>,
            seen: std::sync::Mutex<Vec<Vec<Option<crate::classifier::Verdict>>>>,
        }
        #[async_trait::async_trait]
        impl RestorePool for VerdictSpyPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((Address::from([seed; 20]), u64::from(seed)))
            }
            async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
                use alloy_consensus::{Signed, TxLegacy};
                use alloy_primitives::Signature;

                let seed = tx_rlp.first().copied().unwrap_or(0);
                let inner = TxLegacy { nonce: u64::from(seed), ..Default::default() };
                let sig = Signature::test_signature();
                let hash = TxHash::from([seed; 32]);
                self.seen
                    .lock()
                    .unwrap()
                    .push(self.hashes.iter().map(|h| self.classifier.verdict(h)).collect());
                Ok(RestoredEnvelope {
                    envelope: TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)),
                    from: Address::from([seed; 20]),
                })
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
        }

        let (_dir, j) = fresh_journal().await;
        // `entry(byte, height)` keys both the journal hash and the stub's
        // fabricated envelope off the same byte, so the spy can look the verdict
        // up by hash.
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        j.append_promised(&entry(3, 12)).await.unwrap();

        let classifier = Arc::new(empty_classifier());
        let pool = VerdictSpyPool {
            classifier: classifier.clone(),
            hashes: (1u8..=3).map(|b| TxHash::from([b; 32])).collect(),
            seen: std::sync::Mutex::new(vec![]),
        };
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &classifier).await;

        let seen = pool.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "every entry must be offered to the pool");
        // The discriminating assertion is on the **first** admission: entries 2
        // and 3 have not been offered yet and must already be `Promised`. Marking
        // each entry just before its own admission satisfies "this one is
        // Promised" but leaves those two `None` — that arrangement fails here.
        assert!(
            seen[0].iter().all(|v| *v == Some(crate::classifier::Verdict::Promised)),
            "all entries must be Promised before the first admission, including \
             those not yet admitted; got {:?}",
            seen[0],
        );
    }

    /// The same ordering invariant for the **slot**: every commitment must own
    /// its `(sender, nonce)` before the first entry is offered to the pool.
    ///
    /// `admit_promised` has only a hash — it runs before the envelope is decoded
    /// — so without an explicit claim the nonce reads as *free* from then until
    /// `add_envelope` drives that entry through the validator. A same-nonce
    /// transaction admitted in that interval takes the slot, and the commitment
    /// loses a nonce its client was already told it had. Nothing can admit in
    /// that interval as the node is wired today (`cli::node` runs restore before
    /// reth's local-tx backup loader, the RPC server and the network), so this
    /// pins the property that the *index* enforces it rather than startup order.
    ///
    /// NB the classifier here is **enabled** with empty allowlists, not
    /// `empty_classifier()` (which is built from a default config, i.e. disabled).
    /// `admit_promised` writes regardless of that flag but every slot API is
    /// gated on it, so a disabled classifier would make this test vacuous.
    #[tokio::test]
    async fn restore_claims_every_slot_before_admitting_any() {
        /// Pool that, on each tx it is handed, snapshots the owner of **all** the
        /// journal entries' slots — including those not offered yet.
        struct SlotSpyPool {
            classifier: Arc<PreconfClassifier>,
            slots: Vec<(Address, u64)>,
            seen: std::sync::Mutex<Vec<Vec<Option<TxHash>>>>,
        }
        #[async_trait::async_trait]
        impl RestorePool for SlotSpyPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((Address::from([seed; 20]), u64::from(seed)))
            }
            async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
                use alloy_consensus::{Signed, TxLegacy};
                use alloy_primitives::Signature;

                let seed = tx_rlp.first().copied().unwrap_or(0);
                self.seen.lock().unwrap().push(
                    self.slots
                        .iter()
                        .map(|(from, nonce)| self.classifier.slot_owner(from, *nonce))
                        .collect(),
                );
                let inner = TxLegacy { nonce: u64::from(seed), ..Default::default() };
                Ok(RestoredEnvelope {
                    envelope: TxEnvelope::Legacy(Signed::new_unchecked(
                        inner,
                        Signature::test_signature(),
                        TxHash::from([seed; 32]),
                    )),
                    from: Address::from([seed; 20]),
                })
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
        }

        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        j.append_promised(&entry(3, 12)).await.unwrap();

        let classifier = Arc::new(PreconfClassifier::new(
            false,
            std::time::Duration::from_secs(3600),
            crate::classifier::DEFAULT_VERDICT_CACHE_CAP,
        ));
        let pool = SlotSpyPool {
            classifier: classifier.clone(),
            slots: (1u8..=3).map(|b| (Address::from([b; 20]), u64::from(b))).collect(),
            seen: std::sync::Mutex::new(vec![]),
        };
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &classifier).await;

        let seen = pool.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "every entry must be offered to the pool");
        // Discriminating on the **first** admission: entries 2 and 3 have not been
        // offered yet. Claiming inside the loop, or leaving the claim to the
        // validator, leaves those two `None` here.
        assert_eq!(
            seen[0],
            (1u8..=3).map(|b| Some(TxHash::from([b; 32]))).collect::<Vec<_>>(),
            "every slot must be claimed before the first admission",
        );
    }

    /// An entry whose transaction is **already on chain** is the commitment
    /// having been kept, not a failure. Two consequences are asserted here:
    ///
    /// * it is not pushed into the fifo (there is nothing left to apply);
    /// * it is marked **sealed**, so the next rotation drops it from the file.
    ///
    /// The second is what stops the entry being immortal: the sealed set lives
    /// in memory and is rebuilt only from canonical notifications, and this
    /// block is already in the past — nothing else will ever mark it again, so
    /// without this every future restart would replay and complain about the
    /// same entry forever.
    #[tokio::test]
    async fn restore_marks_already_on_chain_entries_sealed() {
        struct OnChainPool;
        #[async_trait::async_trait]
        impl RestorePool for OnChainPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((Address::from([seed; 20]), u64::from(seed)))
            }
            async fn add_envelope(&self, _tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
                Err(RestoreSkip::NonceConsumed("nonce too low".into()))
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
        }

        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_classifier();
        restore_preconf_state(&j, &OnChainPool, &landed(), &Arc::new(PreconfTxSet::new(16)), &c)
            .await;

        // Landing starts the retention clock — it does not end tracking. Until
        // the block is buried, a reorg could bring the commitment back and it
        // must still hold its nonce.
        assert!(!c.is_retention_expired(&e.hash), "shallow: still tracked");
        let stats = j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e.clone()], "kept while shallow; stats = {stats:?}");

        // Buried deep enough, the record may go — so the next restart will not
        // see it again.
        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        assert!(c.is_retention_expired(&e.hash));
        let stats = j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty(), "rotation must drop it; stats = {stats:?}");
    }

    /// **The invariant the guard's occupancy check rests on**: nothing ever gets
    /// a fifo entry for a `(sender, nonce)` it does not own.
    ///
    /// Restore is the one place that pushes entries without going through the
    /// pool listener, and the one place that mints `Verdict::Promised` — the
    /// verdict the guard waves past its occupancy check. So it is the only
    /// candidate for producing the violating state, and it needs two journal
    /// records on one `(sender, nonce)` to try.
    ///
    /// Two records like that should not exist (a commitment holds its nonce from
    /// the receipt until it is buried `SEAL_DEPTH` deep, so a same-nonce
    /// replacement can never earn its own receipt). This test does not rely on
    /// that: it hands restore exactly that input and shows the loser still comes
    /// away with no entry, because `push_if_absent` refuses a different hash on
    /// an occupied `(sender, nonce)`.
    #[tokio::test]
    async fn restore_never_leaves_a_fifo_entry_without_its_slot() {
        /// Every entry decodes to the *same* `(sender, nonce)` but a hash taken
        /// from its first rlp byte — the collision the invariant forbids.
        struct SameSlotPool;

        const SHARED_SENDER: Address = Address::new([0xAB; 20]);
        const SHARED_NONCE: u64 = 7;

        #[async_trait::async_trait]
        impl RestorePool for SameSlotPool {
            async fn contains(&self, _hash: &TxHash) -> bool {
                false
            }
            fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
            fn recover_slot(&self, _tx_rlp: &Bytes) -> Option<(Address, u64)> {
                Some((SHARED_SENDER, SHARED_NONCE))
            }
            async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
                use alloy_consensus::{Signed, TxLegacy};
                use alloy_primitives::Signature;
                let byte = tx_rlp.first().copied().unwrap_or(0);
                let inner = TxLegacy { nonce: SHARED_NONCE, ..Default::default() };
                let envelope = TxEnvelope::Legacy(Signed::new_unchecked(
                    inner,
                    Signature::test_signature(),
                    TxHash::from([byte; 32]),
                ));
                Ok(RestoredEnvelope { envelope, from: SHARED_SENDER })
            }
        }

        let (_dir, j) = fresh_journal().await;
        let winner = entry(1, 10);
        let loser = entry(2, 11);
        j.append_promised(&winner).await.unwrap();
        j.append_promised(&loser).await.unwrap();

        let fifo = Arc::new(PreconfTxSet::new(16));
        let c = empty_classifier();
        restore_preconf_state(&j, &SameSlotPool, &landed(), &fifo, &c).await;

        // The pre-pass gives the slot to whoever asks first; the other loses it.
        let owner = c.slot_owner(&SHARED_SENDER, SHARED_NONCE).expect("someone owns the nonce");
        let other = if owner == winner.hash { loser.hash } else { winner.hash };

        // The invariant: every fifo entry owns its `(sender, nonce)`.
        assert!(fifo.contains(&owner).await, "the owner is the one that gets an entry");
        assert!(
            !fifo.contains(&other).await,
            "a transaction that lost the slot must not hold a fifo entry",
        );
    }

    /// A pool that only ever answers "the nonce is gone" — the one error whose
    /// meaning the pool cannot pin down. Reused by the three tests below, which
    /// differ only in what the *chain* then says.
    struct NonceConsumedPool;

    #[async_trait::async_trait]
    impl RestorePool for NonceConsumedPool {
        async fn contains(&self, _hash: &TxHash) -> bool {
            false
        }
        fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
            let seed = tx_rlp.first().copied().unwrap_or(0);
            Some((Address::from([seed; 20]), u64::from(seed)))
        }
        async fn add_envelope(&self, _tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
            Err(RestoreSkip::NonceConsumed("nonce too low".into()))
        }
        fn remove_transactions(&self, _hashes: Vec<TxHash>) {}
    }

    /// **The bug this three-way split exists for.** The sender's nonce is gone,
    /// but a *different* transaction consumed it — so the commitment is broken,
    /// not kept.
    ///
    /// Sealing is the grounds on which rotation forgets an entry, and there is
    /// nothing to forget: the transaction that took the nonce may itself be
    /// reverted, and then this commitment applies again. Until 2026-08-05 this
    /// case was indistinguishable from "kept" — `is_nonce_too_low()` reduces to
    /// `tx.nonce < account.nonce` and never looks at the hash — so the entry was
    /// sealed, dropped at the next rotation, and counted as honoured.
    #[tokio::test]
    async fn a_stolen_nonce_is_not_sealed_and_the_entry_survives_rotation() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        restore_preconf_state(
            &j,
            &NonceConsumedPool,
            &StubChain(OnChain::No),
            &Arc::new(PreconfTxSet::new(16)),
            &empty_classifier(),
        )
        .await;

        j.rotate(|_| true).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(
            remaining,
            vec![e],
            "and must survive rotation, so a later reorg can still \
                                       free its nonce"
        );
    }

    /// Cannot tell ⇒ keep. Folding `Unknown` into "on chain" would reinstate the
    /// silent misreport on a node whose transaction-lookup index is pruned.
    #[tokio::test]
    async fn an_undeterminable_entry_is_not_sealed() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        restore_preconf_state(
            &j,
            &NonceConsumedPool,
            &StubChain(OnChain::Unknown),
            &Arc::new(PreconfTxSet::new(16)),
            &empty_classifier(),
        )
        .await;

        j.rotate(|_| true).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e]);
    }

    /// The counterpart: the chain confirms the hash, so the promise *was* kept
    /// and the record may go — once it is buried deep enough. This is the common
    /// outcome on any restart.
    #[tokio::test]
    async fn a_confirmed_commitment_is_dropped_once_it_is_deep_enough() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_classifier();
        restore_preconf_state(
            &j,
            &NonceConsumedPool,
            &StubChain(OnChain::Yes { height: LANDED_AT }),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await;

        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty());
    }

    /// None of the three pushes into the fifo: the nonce is consumed, so there is
    /// nothing left for the preconf arm to apply either way.
    #[tokio::test]
    async fn a_consumed_nonce_never_reaches_the_fifo() {
        for answer in [OnChain::Yes { height: LANDED_AT }, OnChain::No, OnChain::Unknown] {
            let (_dir, j) = fresh_journal().await;
            j.append_promised(&entry(1, 10)).await.unwrap();
            let fifo = Arc::new(PreconfTxSet::new(16));

            restore_preconf_state(
                &j,
                &NonceConsumedPool,
                &StubChain(answer),
                &fifo,
                &empty_classifier(),
            )
            .await;

            assert!(fifo.snapshot().await.is_empty(), "{answer:?}");
        }
    }

    #[tokio::test]
    async fn restore_skips_entry_on_decode_failure_and_continues() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(4, 40)).await.unwrap();
        j.append_promised(&entry(5, 50)).await.unwrap();

        let mut pool = StubPool::new();
        pool.reject_add = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_classifier()).await;

        // Both entries' add_envelope calls return Err — the function
        // does not panic and walks all entries.
        assert_eq!(pool.add_calls.lock().unwrap().len(), 2);
        assert!(fifo.snapshot().await.is_empty());
    }

    // ── spawn_rejournal_loop ────────────────────────────────────────

    #[tokio::test]
    async fn rejournal_loop_rotates_periodically_and_shuts_down() {
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        j.append_promised(&e_a).await.unwrap();
        let c = classifier_done_with(&[e_a.hash]);

        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // 30ms interval — first tick consumed at start, so the next
        // rotate happens at t ≈ 30ms.
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_millis(30), shutdown_rx);

        // Wait long enough for at least one rotate to fire.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // The sealed entry should have been dropped from the file.
        let (after, _) = j.load().await.unwrap();
        assert!(
            after.is_empty(),
            "rotation must have dropped the sealed entry; instead got {after:?}"
        );

        // Graceful shutdown.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down within timeout")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn size_trigger_rotates_dropping_sealed_without_periodic_tick() {
        // Tiny `max_size` (1 byte) → every append crosses the cap and
        // pings the rotate notify. A huge interval guarantees the
        // periodic ticker cannot rotate within the test window, so any
        // rotation observed is purely size-triggered.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();
        let c = classifier_done_with(&[e1.hash, e2.hash]);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_secs(3600), shutdown_rx);

        // Let the loop consume the pending size-notify and rotate.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (after, _) = j.load().await.unwrap();
        assert_eq!(
            after,
            vec![e3],
            "size-triggered rotation must drop sealed entries and keep the unsealed survivor"
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn open_with_max_size_seeds_counter_from_existing_file() {
        // A pre-existing (unsealed) entry already exceeding the cap must
        // be counted at open, so the very first post-restart append trips
        // the size trigger even though on its own it is tiny.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        {
            let seed = PreconfJournal::open(&path, 0).await.unwrap();
            seed.append_promised(&entry(1, 10)).await.unwrap();
        }
        let existing = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(existing > 0);
        // Reopen with a cap just at the existing size; counter seeded so a
        // notify is armed on the next append.
        let j = PreconfJournal::open(&path, existing).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        // A permit should be pending (size now > cap) — consume it without blocking.
        tokio::time::timeout(Duration::from_millis(50), j.rotate_notify.notified())
            .await
            .expect("size trigger must be armed after reopen seeding");
    }

    #[tokio::test]
    async fn rejournal_loop_shuts_down_promptly_without_rotating() {
        let (_dir, j) = fresh_journal().await;
        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Interval larger than the test waits — only the first
        // (immediately-consumed) tick happens; no interval rotation calls.
        // (A single final rotate on shutdown still runs by design; see
        // `graceful_shutdown_performs_final_rotate` for that behavior.)
        let handle = spawn_rejournal_loop(
            j.clone(),
            empty_classifier().into(),
            Duration::from_secs(60),
            shutdown_rx,
        );

        // Hand the shutdown signal immediately.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn graceful_shutdown_performs_final_rotate() {
        // Regression guard for the graceful-shutdown contract:
        // `run_rejournal_loop` MUST perform one final rotate after the
        // shutdown signal fires, so hashes appended to `sealed` between
        // the last periodic tick and the shutdown are not lost on the
        // on-disk file.
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 100)).await.unwrap();
        let j = Arc::new(j);

        let c = classifier_with_high_watermark();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Interval large — no periodic tick will fire during the test.
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(60), shutdown_rx);

        // Finish tracking AFTER the loop starts but BEFORE shutdown — nothing
        // touches the file until a rotate runs.
        finish_tracking(&c, TxHash::from([1; 32]));

        // Trigger shutdown; the loop's final rotate must drop the sealed entry.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");

        let (after, _) = j.load().await.unwrap();
        assert!(
            after.is_empty(),
            "final rotate on shutdown must have dropped the sealed entry; got {after:?}"
        );
    }

    #[tokio::test]
    async fn run_rejournal_loop_returns_shutdown_output_after_final_rotate() {
        // Anchors the generic-output contract of `run_rejournal_loop`:
        // callers passing a graceful-shutdown guard as `T` need it kept
        // alive across the final-rotate await, then returned so their
        // outer task can drop it explicitly.
        let (_dir, j) = fresh_journal().await;
        let j = Arc::new(j);

        // Sentinel type in place of a `GracefulShutdownGuard`; if the
        // loop forgot to return the signal output, the assertion below
        // wouldn't compile.
        #[derive(Debug, PartialEq)]
        struct Sentinel(u32);

        let shutdown = async { Sentinel(7) };
        let out = run_rejournal_loop(
            j.clone(),
            empty_classifier().into(),
            Duration::from_secs(60),
            shutdown,
        )
        .await;
        assert_eq!(out, Sentinel(7));
    }

    // ── size-trigger rate limit / byte-counter invariants ──────────

    #[tokio::test]
    async fn size_trigger_second_rotation_rate_limited_within_min_gap() {
        // Two size triggers fired within `SIZE_ROTATE_MIN_GAP` (2s) must
        // collapse to a single rotation: the first is honoured, the second
        // dropped. A huge interval keeps the periodic ticker from firing
        // inside the sub-second window, so any rotation is purely
        // size-triggered; `max_size = 1` makes every append trip the cap.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();
        let c = classifier_with_high_watermark();
        finish_tracking(&c, e1.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(3600), shutdown_rx);

        // First (coalesced) size trigger is honoured → released e1 dropped.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone(), e3], "first size trigger drops sealed e1");

        // Seal e2 and fire a *second* trigger well within `min_gap`
        // (~150ms elapsed ≪ 2s). It must be rate-limited — e2 stays on disk.
        finish_tracking(&c, e2.hash);
        j.append_promised(&entry(4, 13)).await.unwrap(); // re-notifies (over cap)
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_second, _) = j.load().await.unwrap();
        assert!(
            after_second.iter().any(|e| e.hash == e2.hash),
            "second trigger within min_gap must be dropped; e2 must survive, got {after_second:?}"
        );

        // (The final rotate on shutdown WILL drop e2 — expected, not asserted.)
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn rotate_does_not_self_retrigger_when_survivors_exceed_cap() {
        // A size-triggered `rotate()` whose survivors still exceed
        // `max_size` must NOT keep the loop rotating on its own: the size
        // trigger fires only from `append_promised`, never from `rotate`.
        // Guards against a future change that re-notifies inside `rotate`
        // (which would spin the full-file rewrite on every wake while the
        // file stays over the cap).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // `max_size = 1` ⇒ even the lone survivor exceeds the cap.
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        let c = classifier_with_high_watermark();
        finish_tracking(&c, e1.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(3600), shutdown_rx);

        // First append-driven trigger drops e1; survivor e2 alone still
        // exceeds `max_size = 1`.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone()], "first rotate drops released e1, keeps e2");

        // Release e2 but issue NO further append. Wait past `min_gap` (2s) so a
        // self-retrigger, if it existed, would be free to fire.
        finish_tracking(&c, e2.hash);
        tokio::time::sleep(Duration::from_millis(2200)).await;

        let (after_wait, _) = j.load().await.unwrap();
        assert_eq!(
            after_wait,
            vec![e2.clone()],
            "no append ⇒ no size trigger; released e2 must survive over the cap, got {after_wait:?}"
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn rotate_resets_size_counter_to_true_on_disk_size() {
        // Approach-A invariant: `size_bytes` tracks the real on-disk size
        // without a stat syscall on the hot path. It must stay equal to the
        // file's true length across both `append_promised` (+= line) and
        // `rotate` (reset to kept-bytes). Drift here silently breaks the
        // size trigger (it would fire late, or never).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // `max_size` is irrelevant to the counter mechanics; 0 keeps the
        // trigger out of the way.
        let j = PreconfJournal::open(&path, 0).await.unwrap();

        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();

        // After appends: counter == on-disk size.
        let on_disk = tokio::fs::metadata(&path).await.unwrap().len();
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            on_disk,
            "counter must equal file size after appends"
        );

        // Drop two released entries; the counter must reset to the kept-bytes
        // total, which equals the rewritten file's true size.
        let dropped = [e1.hash, e3.hash];
        let stats = j.rotate(|h| !dropped.contains(h)).await.unwrap();
        assert_eq!((stats.kept, stats.dropped), (1, 2));

        let on_disk_after = tokio::fs::metadata(&path).await.unwrap().len();
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            on_disk_after,
            "counter must reset to kept-bytes = true file size after rotate"
        );
        assert!(on_disk_after > 0, "the surviving entry keeps the file non-empty");
        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![e2]);
    }

    #[tokio::test]
    async fn rotate_drops_corrupt_lines_and_reports_count() {
        // `rotate()` rewrites the file via `load()`, which skips corrupt
        // lines. The corrupt line must NOT be carried into the new
        // generation, and its count must surface as `bad_lines_skipped`.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let good_a = serde_json::to_string(&entry(7, 70)).unwrap();
        let bad = "{not valid json}";
        let good_b = serde_json::to_string(&entry(8, 80)).unwrap();
        tokio::fs::write(&path, format!("{good_a}\n{bad}\n{good_b}\n")).await.unwrap();

        let j = PreconfJournal::open(&path, 0).await.unwrap();
        let stats = j.rotate(|_| true).await.unwrap();
        assert_eq!(stats.kept, 2, "both good entries survive");
        assert_eq!(stats.dropped, 0, "retain kept everything → nothing dropped");
        assert_eq!(stats.bad_lines_skipped, 1, "one corrupt line reported");

        // The corrupt line is gone from the rewritten file: a second load
        // sees the two good entries and zero bad lines.
        let (after, bad_after) = j.load().await.unwrap();
        assert_eq!(after, vec![entry(7, 70), entry(8, 80)]);
        assert_eq!(bad_after, 0, "corrupt line must not be carried into the new file");
    }

    #[tokio::test]
    async fn size_trigger_fires_at_exact_boundary_not_below() {
        // The trigger condition is `new_size >= max_size` (inclusive). Pin
        // the boundary: an append landing the file *exactly* at `max_size`
        // arms the notify; one byte short does not.
        let line_len = {
            let mut v = serde_json::to_vec(&entry(1, 10)).unwrap();
            v.push(b'\n');
            v.len() as u64
        };

        // Exactly at the cap ⇒ armed.
        {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("preconf.jsonl");
            let j = PreconfJournal::open(&path, line_len).await.unwrap();
            j.append_promised(&entry(1, 10)).await.unwrap();
            tokio::time::timeout(Duration::from_millis(50), j.rotate_notify.notified())
                .await
                .expect("append landing exactly at max_size must arm the size trigger");
        }

        // One byte above the cap ⇒ NOT armed (a single line stays under).
        {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("preconf.jsonl");
            let j = PreconfJournal::open(&path, line_len + 1).await.unwrap();
            j.append_promised(&entry(1, 10)).await.unwrap();
            let armed = tokio::time::timeout(Duration::from_millis(50), j.rotate_notify.notified())
                .await
                .is_ok();
            assert!(!armed, "a file one byte under the cap must NOT arm the size trigger");
        }
    }
}
