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
//! In-memory state tracked alongside the file:
//!
//! - **sealed set** — hashes that the canonical-state handler has reported as included in a sealed
//!   block. Used by the rotation step to drop already-on-chain entries from the next file
//!   generation, and by the pool listener to detect reorg reinjects (a hash in the sealed set that
//!   reappears via pool re-admission was previously promised — see
//!   `pool_ext::preconf_pool_listener`).
//!
//! The journal exposes `append_promised` / `load` / `mark_sealed` /
//! `contains` / `rotate` for the durability path, plus the startup
//! helper [`restore_preconf_state`] and the background rotation loop
//! [`spawn_rejournal_loop`].

use std::{
    collections::HashSet,
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
use tracing::{debug, info, warn};

use crate::PreconfTxSet;

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
/// shared between the RPC handler (writer) and the canonical-state
/// handler (sealed-set updater). All writes serialise through an async
/// `Mutex` around the file handle; reads of the sealed set are
/// independent and lock-free at the data-structure level (the inner
/// `HashSet` is guarded by its own short-lived lock).
#[derive(Debug)]
pub struct PreconfJournal {
    /// Path to the journal file. Stored for rotation, which writes a
    /// sibling tmp file and atomically renames into place.
    path: PathBuf,
    /// Append handle protected by a `Mutex` because the trait
    /// `tokio::io::AsyncWriteExt::write_all` takes `&mut self`.
    writer: Mutex<File>,
    /// In-memory set of hashes whose commitments have been observed
    /// on a sealed block. Updated by the canonical-state handler;
    /// consulted by rotation to skip "already on chain" entries.
    sealed: Mutex<HashSet<TxHash>>,
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
}

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
        Ok(Self {
            path,
            writer: Mutex::new(file),
            sealed: Mutex::new(HashSet::new()),
            max_size,
            size_bytes: AtomicU64::new(init_size),
            rotate_notify: Notify::new(),
        })
    }

    /// Path the journal is bound to. Stable for the lifetime of the
    /// instance.
    pub fn path(&self) -> &Path {
        &self.path
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

    /// Mark a transaction as observed in a sealed block. Idempotent —
    /// a second call with the same hash is a no-op.
    pub async fn mark_sealed(&self, hash: TxHash) {
        self.sealed.lock().await.insert(hash);
    }

    /// `true` when `hash` has been seen on chain. The canonical-state
    /// handler uses this to drive the `reorg_drift_total` metric:
    /// reverted blocks whose txs are tracked here indicate operator
    /// alert territory.
    pub async fn contains(&self, hash: &TxHash) -> bool {
        self.sealed.lock().await.contains(hash)
    }

    /// Number of sealed hashes currently tracked. Test / telemetry helper.
    pub async fn sealed_len(&self) -> usize {
        self.sealed.lock().await.len()
    }

    /// Rewrite the journal file dropping every entry whose `hash` is
    /// in the sealed set, then atomically swap it for the live file.
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
    pub async fn rotate(&self) -> Result<RotateStats, JournalError> {
        // Records `preconf.journal.rotate_duration_ms` on every exit path
        // (including the `?` early returns below).
        let _timer = RotateTimer(std::time::Instant::now());

        let (entries, bad_before) = self.load().await?;
        // Snapshot the sealed set once here. Any `mark_sealed` firing
        // during the rewrite is intentionally not observed by this
        // rotation — its hash will land in the sealed set for the
        // *next* tick, which prevents a hash from being both dropped
        // by this rotate AND missing from the sealed set on the same
        // pass.
        let sealed_snapshot: HashSet<TxHash> = self.sealed.lock().await.clone();
        metrics::gauge!("preconf.journal.sealed_len").set(sealed_snapshot.len() as f64);

        let mut kept = 0usize;
        let mut dropped = 0usize;
        let mut kept_bytes = 0u64;
        let tmp_path = tmp_path_for(&self.path);

        {
            let mut tmp =
                OpenOptions::new().create(true).truncate(true).write(true).open(&tmp_path).await?;
            for entry in &entries {
                if sealed_snapshot.contains(&entry.hash) {
                    dropped += 1;
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

        // Atomic swap. Hold the writer lock so any concurrent
        // `append_promised` waits until the new file is in place,
        // then re-opens against the new inode.
        let mut writer = self.writer.lock().await;
        tokio::fs::rename(&tmp_path, &self.path).await?;
        *writer = OpenOptions::new().create(true).append(true).open(&self.path).await?;
        // Reset the byte counter to the new file's true size while still
        // holding the writer lock, so it stays consistent with any
        // `append_promised` that serialises before/after this swap.
        self.size_bytes.store(kept_bytes, Ordering::Relaxed);
        metrics::gauge!("preconf.journal.size_bytes").set(kept_bytes as f64);

        // Sealed entries that just left the file are also redundant
        // in memory now: rotate is the point at which a commitment
        // can stop being tracked entirely.
        if dropped > 0 {
            let mut sealed = self.sealed.lock().await;
            for entry in &entries {
                if sealed_snapshot.contains(&entry.hash) {
                    sealed.remove(&entry.hash);
                }
            }
        }

        Ok(RotateStats { kept, dropped, bad_lines_skipped: bad_before })
    }
}

/// Telemetry-friendly summary of a single [`PreconfJournal::rotate`]
/// invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
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
/// via [`PoolAdapter`] (next phase, when the cli crate wires journal in);
/// tests inject a stub that records `contains` / `add_envelope` calls
/// without standing up the full reth pool.
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
    async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, String>;

    /// Synchronously remove transactions from the pool by hash. Used
    /// by [`PreconfTxSet`](crate::PreconfTxSet)'s pool-eviction
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
/// For each entry, in order:
///
/// 1. Decode + attempt to admit into the pool via [`RestorePool::add_envelope`]. The trait treats
///    `AlreadyImported` as success — reth's own local-tx backup may have restored the same tx from
///    disk before this call, and either outcome yields the recovered envelope needed for the fifo
///    push.
/// 2. Push the recovered envelope into the fifo with
///    [`PreconfSource::Replay`](crate::types::PreconfSource::Replay) so the dispatch layer's
///    deadline / gas-budget gates bypass the tx (SLA: "receipt returned → tx must land").
///
/// Non-recoverable failures (corrupt tx bytes, pool refusal for reasons
/// other than `AlreadyImported`) are logged and skipped — best-effort
/// restore, never block startup.
pub async fn restore_preconf_state<P: RestorePool>(
    journal: &PreconfJournal,
    pool: &P,
    fifo: &Arc<PreconfTxSet>,
) {
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

    let mut restored = 0usize;
    let mut decode_failures = 0usize;

    for entry in entries {
        let recovered = match pool.add_envelope(&entry.tx_rlp).await {
            Ok(rec) => rec,
            Err(reason) => {
                warn!(
                    target: "mantle::preconf::journal",
                    hash = ?entry.hash,
                    reason,
                    "pool rejected restored tx; skipping fifo push"
                );
                decode_failures += 1;
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

    info!(
        target: "mantle::preconf::journal",
        restored,
        decode_failures,
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
    interval: Duration,
    shutdown: F,
) -> T
where
    F: Future<Output = T>,
{
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
                    log_rotate(journal.rotate().await, "size");
                    last_rotate = Some(now);
                }
            }
            _ = ticker.tick() => {
                log_rotate(journal.rotate().await, "tick");
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
    log_rotate(journal.rotate().await, "shutdown");

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
fn log_rotate(result: Result<RotateStats, JournalError>, reason: &'static str) {
    match result {
        Ok(stats) => debug!(
            target: "mantle::preconf::journal",
            reason,
            kept = stats.kept,
            dropped = stats.dropped,
            bad = stats.bad_lines_skipped,
            "journal rotation"
        ),
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
        let () = run_rejournal_loop(journal, interval, shutdown).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
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
            sealed: Mutex::new(HashSet::new()),
            max_size: 0,
            size_bytes: AtomicU64::new(0),
            rotate_notify: Notify::new(),
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

    #[tokio::test]
    async fn mark_sealed_is_idempotent() {
        let (_dir, j) = fresh_journal().await;
        let h = TxHash::from([5; 32]);
        j.mark_sealed(h).await;
        j.mark_sealed(h).await;
        j.mark_sealed(h).await;
        assert!(j.contains(&h).await);
        assert_eq!(j.sealed_len().await, 1);
    }

    #[tokio::test]
    async fn contains_is_false_for_unknown_hash() {
        let (_dir, j) = fresh_journal().await;
        assert!(!j.contains(&TxHash::from([9; 32])).await);
    }

    #[tokio::test]
    async fn rotate_drops_sealed_keeps_unsealed() {
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        let e_b = entry(2, 11);
        let e_c = entry(3, 12);
        j.append_promised(&e_a).await.unwrap();
        j.append_promised(&e_b).await.unwrap();
        j.append_promised(&e_c).await.unwrap();
        j.mark_sealed(e_b.hash).await;

        let stats = j.rotate().await.unwrap();
        assert_eq!(stats.kept, 2);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.bad_lines_skipped, 0);

        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![e_a, e_c]);

        // Sealed entry is removed from in-memory set too — it has no
        // further utility once it's out of the file.
        assert!(!j.contains(&e_b.hash).await);
    }

    #[tokio::test]
    async fn rotate_then_append_writes_to_new_file_handle() {
        // Verify the writer is re-opened against the new inode after
        // rotation — a subsequent append must land in the rotated file.
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        j.append_promised(&e_a).await.unwrap();
        j.mark_sealed(e_a.hash).await;
        j.rotate().await.unwrap();

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
        async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, String> {
            self.add_calls.lock().unwrap().push(tx_rlp.clone());
            if self.reject_add {
                return Err("rejected by stub".into());
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

    #[tokio::test]
    async fn restore_from_empty_journal_is_noop() {
        let (_dir, j) = fresh_journal().await;
        let pool = StubPool::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &fifo).await;
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
        restore_preconf_state(&j, &pool, &fifo).await;

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
        restore_preconf_state(&j, &pool, &fifo).await;

        assert_eq!(pool.add_calls.lock().unwrap().len(), 1);
        // Core J5 assertion: fifo received the entry.
        assert_eq!(fifo.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn restore_skips_entry_on_decode_failure_and_continues() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(4, 40)).await.unwrap();
        j.append_promised(&entry(5, 50)).await.unwrap();

        let mut pool = StubPool::new();
        pool.reject_add = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &fifo).await;

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
        j.mark_sealed(e_a.hash).await;

        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // 30ms interval — first tick consumed at start, so the next
        // rotate happens at t ≈ 30ms.
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_millis(30), shutdown_rx);

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
        j.mark_sealed(e1.hash).await;
        j.mark_sealed(e2.hash).await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_secs(3600), shutdown_rx);

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
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_secs(60), shutdown_rx);

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

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Interval large — no periodic tick will fire during the test.
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_secs(60), shutdown_rx);

        // Mark sealed AFTER the loop starts but BEFORE shutdown — the
        // sealed hash lives only in memory until a rotate flushes it.
        j.mark_sealed(TxHash::from([1; 32])).await;

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
        let out = run_rejournal_loop(j.clone(), Duration::from_secs(60), shutdown).await;
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
        j.mark_sealed(e1.hash).await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_secs(3600), shutdown_rx);

        // First (coalesced) size trigger is honoured → sealed e1 dropped.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone(), e3], "first size trigger drops sealed e1");

        // Seal e2 and fire a *second* trigger well within `min_gap`
        // (~150ms elapsed ≪ 2s). It must be rate-limited — e2 stays on disk.
        j.mark_sealed(e2.hash).await;
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
        j.mark_sealed(e1.hash).await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), Duration::from_secs(3600), shutdown_rx);

        // First append-driven trigger drops e1; survivor e2 alone still
        // exceeds `max_size = 1`.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone()], "first rotate drops sealed e1, keeps e2");

        // Seal e2 but issue NO further append. Wait past `min_gap` (2s) so a
        // self-retrigger, if it existed, would be free to fire.
        j.mark_sealed(e2.hash).await;
        tokio::time::sleep(Duration::from_millis(2200)).await;

        let (after_wait, _) = j.load().await.unwrap();
        assert_eq!(
            after_wait,
            vec![e2.clone()],
            "no append ⇒ no size trigger; sealed e2 must survive over the cap, got {after_wait:?}"
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

        // Drop two sealed entries; the counter must reset to the kept-bytes
        // total, which equals the rewritten file's true size.
        j.mark_sealed(e1.hash).await;
        j.mark_sealed(e3.hash).await;
        let stats = j.rotate().await.unwrap();
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
        let stats = j.rotate().await.unwrap();
        assert_eq!(stats.kept, 2, "both good entries survive");
        assert_eq!(stats.dropped, 0, "nothing sealed → nothing dropped");
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
