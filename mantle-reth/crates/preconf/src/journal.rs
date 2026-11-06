//! Append-only journal of preconf commitments for restart-safe operation.
//!
//! A successful preconf receipt promises the client that the transaction will
//! land. If the node dies before it does, the restart must put the transaction
//! back in the queue; [`PreconfJournal`] is what it reads to do so.
//!
//! The file is **JSON Lines**: one [`JournalEntry`] per line, append only.
//! Human-readable for incident triage, at the cost of a few bytes per record.
//!
//! **This layer decides nothing about which records are still owed.**
//! [`Commitments`] owns that. Rotation takes it as the `retain` predicate of
//! [`PreconfJournal::rotate`], and restore asks `Commitments::is_tracked`. A
//! second, journal-local rule could drop a record the registry still holds.
//!
//! **Written in execution order.** An ordinary pool transaction is held in
//! memory as it executes ([`PreconfJournal::hold`], as [`ClaimKind::Announced`])
//! and written by the next write: a commitment's ([`PreconfJournal::append_promised`]),
//! a slice boundary's, or the next build's before it replays anything
//! ([`PreconfJournal::flush`]). A restart replays the file in order, so a
//! commitment that depends on a transaction executed before it finds that
//! transaction first. A line survives rotation while the registry tracks its
//! hash.
//!
//! Besides the write and rotation API, the module provides the startup helper
//! [`restore_preconf_state`] and the background loop [`spawn_rejournal_loop`].

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash};
use parking_lot::Mutex as SyncMutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use crate::{ClaimKind, Commitments, PreconfTxSet, types::PushResult};

/// One persisted record: a preconf commitment, or an ordinary pool transaction
/// a slice carried. Holds what a restart needs to put the transaction back in
/// the queue and to recognise it on chain.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Transaction hash — primary key.
    pub hash: TxHash,
    /// EIP-2718 encoded transaction, re-injected into the queue on restart.
    pub tx_rlp: Bytes,
    /// L2 block height the transaction was executed into. Restore passes it to
    /// `Commitments` as the promised height, which `Commitments::sweep`
    /// measures the record's retention from.
    pub block_height: u64,
    /// Wall-clock ms when the record was made, for correlating with logs.
    pub committed_at_ms: u64,
    /// Which claim this node made: a commitment from the apply, or an
    /// announcement of a pool transaction from a slice. Restore re-establishes
    /// each record as its kind; it cannot infer the kind itself, because the
    /// registry is empty until restore fills it.
    ///
    /// Absent from files written before the field existed; those records read
    /// back as commitments, including the pool transactions among them.
    #[serde(default = "promised_by_default")]
    pub kind: ClaimKind,
}

/// Default [`JournalEntry::kind`] for a record written before the field
/// existed. See that field.
const fn promised_by_default() -> ClaimKind {
    ClaimKind::Promised
}

/// Current wall-clock milliseconds since the Unix epoch, for
/// [`JournalEntry::committed_at_ms`]. `0` if the clock reads before 1970: the
/// field is telemetry only, not worth a panic.
pub(crate) fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl JournalEntry {
    /// The record for a transaction just executed into the block being built.
    /// Both writers use it; only `kind` differs.
    pub(crate) fn for_executed(
        hash: TxHash,
        tx: &impl Encodable2718,
        block_height: u64,
        kind: ClaimKind,
    ) -> Self {
        Self {
            hash,
            tx_rlp: tx.encoded_2718().into(),
            block_height,
            committed_at_ms: now_unix_ms(),
            kind,
        }
    }
}

/// How many unwritten records the buffer holds before the oldest are
/// dropped: thirty blocks of 1,500 simple transfers (30M gas each).
///
/// Thirty blocks is one rotation interval (60s default, 2s blocks). A record
/// older than that is no longer needed: its block has most likely sealed.
const PENDING_CAPACITY: usize = 30 * 1_500;

/// Byte limit on the buffer, as a fuse against memory blow-up.
///
/// [`PENDING_CAPACITY`] is the limit that normally applies. It does not bound
/// memory: at the pool's 128 KiB per-transaction ceiling a full buffer would be
/// gigabytes. Ordinary traffic stays far below this fuse.
const PENDING_MAX_BYTES: usize = 1024 * 1024 * 1024;

/// Drop the oldest held records until `incoming` bytes fit, returning how many
/// were dropped. `bytes` is the running size of `pending`, kept in step.
///
/// Separate from [`PreconfJournal::buffer`] so tests can use a small budget
/// instead of allocating [`PENDING_MAX_BYTES`].
fn evict_to_fit(
    pending: &mut VecDeque<Vec<u8>>,
    bytes: &mut usize,
    incoming: usize,
    max_count: usize,
    max_bytes: usize,
) -> u64 {
    let mut dropped = 0;
    // A loop because one large record can displace many small ones. Never
    // empties the buffer: a record bigger than the whole budget is still kept.
    while pending.len() >= max_count || (*bytes + incoming > max_bytes && !pending.is_empty()) {
        let evicted = pending.pop_front().expect("non-empty");
        *bytes -= evicted.len();
        dropped += 1;
    }
    dropped
}

/// Errors from journal IO.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Underlying filesystem / IO error.
    #[error("journal IO: {0}")]
    Io(#[from] io::Error),
    /// A record `serde_json` could not encode; retrying cannot fix it. Decode
    /// failures have no variant: a bad line is skipped and counted.
    #[error("journal serialisation: {0}")]
    Encode(#[source] serde_json::Error),
}

/// On-disk append-only journal (see the module docs).
///
/// Shared behind an `Arc` by its writers and the rotation loop; all writes go
/// through one async `Mutex` around the file handle. Holds no view of which
/// records are still owed.
#[derive(Debug)]
pub struct PreconfJournal {
    /// Path to the journal file. Rotation writes a sibling tmp file and renames
    /// it over this path.
    path: PathBuf,
    /// Append handle. A `Mutex` because `write_all` takes `&mut self`.
    writer: Mutex<File>,
    /// File size in bytes at which a size-triggered rotation is requested.
    /// Positive whenever the journal is enabled (see [`crate::PreconfConfig`]).
    max_size: u64,
    /// Running on-disk byte count, used to arm size-triggered rotation without a
    /// `stat` on the hot path. Advanced by `write_lines` on success, re-read from
    /// the file after a failed write (which may have placed part of its bytes),
    /// and reset by `rotate`. Rotation's own cut point does not come from here.
    size_bytes: AtomicU64,
    /// Signalled by `write_lines` once `size_bytes` reaches `max_size`, waking
    /// [`run_rejournal_loop`]. `notify_one` coalesces a burst into one wake.
    rotate_notify: Notify,
    /// Records not yet in the file, oldest first, carried by the next write:
    /// those [`Self::hold`] was given, and those a failed write could not place.
    /// One queue, so every write keeps execution order without reasoning about
    /// which kind came first.
    ///
    /// Bounded by [`PENDING_CAPACITY`], with [`PENDING_MAX_BYTES`] as a fuse;
    /// past either the oldest are dropped, as their blocks are the likeliest to
    /// have sealed. This is how a failing disk avoids blocking the build loop.
    pending: SyncMutex<VecDeque<Vec<u8>>>,
    /// Running size of `pending` in bytes.
    pending_bytes: AtomicUsize,
    /// How many records at the front of `pending` are there because a write
    /// failed. Held records are not counted: `preconf.journal.pending_entries`
    /// reports this, and is meant to be zero unless the disk is failing.
    unplaced: AtomicUsize,
    /// The last write failed and may have stopped mid-line, so the next one
    /// opens with a newline to close it. Read and written under `writer`.
    torn: AtomicBool,
}

impl PreconfJournal {
    /// Open (or create) the journal file at `path` in append mode, leaving any
    /// existing contents in place for [`Self::load`].
    ///
    /// `size_bytes` starts at the existing file's size, so `max_size` also
    /// counts records that survived from before a restart.
    pub async fn open(path: impl AsRef<Path>, max_size: u64) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            // The operator may have created it already.
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).await?;
        let init_size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        metrics::gauge!("preconf.journal.size_bytes").set(init_size as f64);
        let journal = Self {
            path,
            writer: Mutex::new(file),
            max_size,
            size_bytes: AtomicU64::new(init_size),
            rotate_notify: Notify::new(),
            pending: SyncMutex::new(VecDeque::new()),
            pending_bytes: AtomicUsize::new(0),
            unplaced: AtomicUsize::new(0),
            torn: AtomicBool::new(false),
        };
        // Nothing to seed in memory: rebuilding state from the file is
        // `restore_preconf_state`'s job.
        Ok(journal)
    }

    /// Path the journal is bound to. Test-only; production reads the field.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep `entry` in memory for the next write, which writes it ahead of its
    /// own records. Does no IO.
    ///
    /// For ordinary pool transactions: they are written when something else
    /// is — a commitment, a slice boundary, the start of the next build — so a
    /// commitment that executed after one finds it ahead of itself in the file.
    pub fn hold(&self, entry: &JournalEntry) {
        match serde_json::to_vec(entry) {
            Ok(mut line) => {
                line.push(b'\n');
                self.buffer(vec![line]);
            }
            Err(e) => warn!(
                target: "mantle::preconf::journal",
                hash = ?entry.hash, %e,
                "journal record could not be encoded; not held"
            ),
        }
    }

    /// Write whatever is held. Nothing held, no IO.
    pub async fn flush(&self) -> Result<(), JournalError> {
        self.write_lines(Vec::new()).await
    }

    /// Append one commitment: [`Self::append_batch`] with a batch of one.
    pub async fn append_promised(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        self.append_batch(std::slice::from_ref(entry)).await
    }

    /// Append many records with one lock, one write and one flush. Batching
    /// keeps a slice of a thousand transactions from holding the lock a
    /// thousand times. An empty batch does no IO.
    ///
    /// `flush`, not fsync: the bytes reach the OS before this returns, but are
    /// not guaranteed to survive a power loss.
    ///
    /// Every record is encoded before anything is written, so an encode error
    /// writes nothing. On an IO error the records are kept for the next write
    /// and `Err` is returned for logging only. Callers must not resend: the
    /// next write already carries them.
    pub async fn append_batch(&self, entries: &[JournalEntry]) -> Result<(), JournalError> {
        // A record that fails to encode will fail again, so it is not buffered
        // for retry.
        let mut lines = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut line = serde_json::to_vec(entry).map_err(JournalError::Encode)?;
            line.push(b'\n');
            lines.push(line);
        }
        self.write_lines(lines).await
    }

    /// Write `lines`, preceded by everything held: records given to
    /// [`Self::hold`] and records an earlier write could not place.
    ///
    /// On IO failure everything goes back to the buffer and the error is
    /// returned. Callers carry on regardless: the transactions are in the block
    /// either way, and the record only matters if the process dies before that
    /// block is canonical.
    async fn write_lines(&self, mut lines: Vec<Vec<u8>>) -> Result<(), JournalError> {
        if lines.is_empty() && self.pending.lock().is_empty() {
            return Ok(());
        }
        // Lock first, so no append slips in between draining the buffer and
        // writing it and lands ahead of the older records.
        let mut writer = self.writer.lock().await;
        let mut retried: Vec<Vec<u8>> = {
            let mut pending = self.pending.lock();
            self.pending_bytes.store(0, Ordering::Relaxed);
            pending.drain(..).collect()
        };

        let mut buf = Vec::new();
        if self.torn.load(Ordering::Relaxed) {
            // The failed write may have stopped mid-line. A newline closes that
            // line so this write does not fuse onto it; `load` counts it as one
            // corrupt entry.
            buf.push(b'\n');
        }
        for line in retried.iter().chain(lines.iter()) {
            buf.extend_from_slice(line);
        }
        let len = buf.len() as u64;

        if let Err(e) = writer.write_all(&buf).await.and(writer.flush().await) {
            // Buffered while still holding the lock, to keep the order. Anything
            // held since the drain above is newer, so it stays behind these.
            retried.append(&mut lines);
            // Everything this write drained is unplaced now, including what was
            // only held; set before buffering so an eviction there counts down.
            self.unplaced.store(retried.len(), Ordering::Relaxed);
            self.torn.store(true, Ordering::Relaxed);
            self.buffer_front(retried);
            self.report_pending();
            // The write may have placed part of `buf` before failing, so the
            // counter no longer matches the file. Re-read the true length;
            // if even that fails, rotation does not depend on the counter.
            if let Ok(meta) = writer.metadata().await {
                self.size_bytes.store(meta.len(), Ordering::Relaxed);
            }
            return Err(JournalError::Io(e));
        }
        let new_size = self.size_bytes.fetch_add(len, Ordering::Relaxed) + len;
        // Reset under the lock so a concurrent failing append cannot be overwritten.
        self.torn.store(false, Ordering::Relaxed);
        self.unplaced.store(0, Ordering::Relaxed);
        self.report_pending();
        drop(writer);

        metrics::gauge!("preconf.journal.size_bytes").set(new_size as f64);
        if new_size >= self.max_size {
            self.rotate_notify.notify_one();
        }
        Ok(())
    }

    /// Add `lines` behind everything already held, dropping the oldest once
    /// full.
    fn buffer(&self, lines: Vec<Vec<u8>>) {
        let mut pending = self.pending.lock();
        for line in lines {
            self.push_evicting(&mut pending, line);
        }
        drop(pending);
        self.report_pending();
    }

    /// Put a failed write's `lines` back ahead of anything held since it
    /// drained the buffer: they executed first. Called under `writer`.
    fn buffer_front(&self, lines: Vec<Vec<u8>>) {
        let mut pending = self.pending.lock();
        let newer: Vec<Vec<u8>> = pending.drain(..).collect();
        self.pending_bytes.store(0, Ordering::Relaxed);
        for line in lines.into_iter().chain(newer) {
            self.push_evicting(&mut pending, line);
        }
    }

    /// Push `line` at the back, first dropping the oldest records if it would
    /// not fit. A dropped record that a failed write left is no longer unplaced.
    fn push_evicting(&self, pending: &mut VecDeque<Vec<u8>>, line: Vec<u8>) {
        let mut bytes = self.pending_bytes.load(Ordering::Relaxed);
        let dropped =
            evict_to_fit(pending, &mut bytes, line.len(), PENDING_CAPACITY, PENDING_MAX_BYTES);
        bytes += line.len();
        pending.push_back(line);
        self.pending_bytes.store(bytes, Ordering::Relaxed);
        if dropped > 0 {
            metrics::counter!("preconf.journal.dropped_entries_total").increment(dropped);
            let _ = self.unplaced.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(dropped as usize))
            });
        }
    }

    /// `pending_entries` counts only what a failed write left; `buffered_entries`
    /// counts everything not yet in the file.
    fn report_pending(&self) {
        metrics::gauge!("preconf.journal.pending_entries")
            .set(self.unplaced.load(Ordering::Relaxed) as f64);
        metrics::gauge!("preconf.journal.buffered_entries").set(self.pending.lock().len() as f64);
    }

    /// How many records are waiting for a write to succeed. Test-only; in
    /// production this is the `preconf.journal.pending_entries` gauge.
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    /// How many held records a failed write left. Test-only; in production this
    /// is the `preconf.journal.pending_entries` gauge.
    #[cfg(test)]
    pub fn unplaced_len(&self) -> usize {
        self.unplaced.load(Ordering::Relaxed)
    }

    /// Read every entry that parses, skipping and counting the lines that do
    /// not. A missing file reads as empty. Returns the entries and the count of
    /// skipped lines.
    pub async fn load(&self) -> Result<(Vec<JournalEntry>, usize), JournalError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            // The normal first start.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(JournalError::Io(e)),
        };
        Self::parse_entries(BufReader::new(file)).await
    }

    /// Entries from the first `limit` bytes of the file: the part a rotation
    /// compacts. `limit` is the file length taken under the writer lock, so no
    /// append is in progress at the cut. The cut can still follow the partial
    /// line of a failed write; that line is skipped as corrupt, and its records
    /// are rewritten whole from the buffer.
    async fn load_upto(&self, limit: u64) -> Result<(Vec<JournalEntry>, usize), JournalError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(JournalError::Io(e)),
        };
        Self::parse_entries(BufReader::new(file.take(limit))).await
    }

    /// The raw bytes appended past `from`: records that arrived while a
    /// rotation was compacting. Copied verbatim rather than parsed, so `retain`
    /// is not evaluated while the writer lock is held; a partial line among
    /// them is carried too, and skipped by the next [`Self::load`].
    async fn bytes_after(&self, from: u64) -> Result<Vec<u8>, JournalError> {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(JournalError::Io(e)),
        };
        file.seek(io::SeekFrom::Start(from)).await?;
        let mut out = Vec::new();
        file.read_to_end(&mut out).await?;
        Ok(out)
    }

    /// Parse newline-delimited entries, skipping and counting the ones that do
    /// not decode. Lines are read as bytes, so a non-UTF-8 line is one bad
    /// record rather than a failed read; only a real IO error fails.
    async fn parse_entries<R>(mut reader: R) -> Result<(Vec<JournalEntry>, usize), JournalError>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut out = Vec::new();
        let mut bad = 0usize;
        let mut line_no = 0usize;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf).await? == 0 {
                break;
            }
            line_no += 1;
            let mut line = buf.as_slice();
            if let Some(rest) = line.strip_suffix(b"\n") {
                line = rest;
            }
            if let Some(rest) = line.strip_suffix(b"\r") {
                line = rest;
            }
            match serde_json::from_slice::<JournalEntry>(line) {
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

    /// Rewrite the file keeping only the entries `retain` accepts, then rename
    /// the rewrite over the live file. Called by the rotation loop, never on the
    /// hot path.
    ///
    /// The rename is atomic against a process crash. Against power loss, the
    /// rewrite is fsynced before the rename and the directory after it, so the
    /// new file is never seen without its contents. (Appends themselves are
    /// only flushed; see [`Self::append_batch`].)
    ///
    /// The read-filter-rewrite pass holds no lock, so appends continue
    /// meanwhile. The writer lock is taken only briefly at the start, to read
    /// the file length, and for the tail: splice the records appended during
    /// the pass, fsync, rename. The tail costs those appends plus one fsync,
    /// not a pass over the file — which matters because a slice is journaled
    /// before it is broadcast, so a waiting append delays the broadcast.
    pub async fn rotate(
        &self,
        retain: impl Fn(&TxHash) -> bool,
    ) -> Result<RotateStats, JournalError> {
        // Records `preconf.journal.rotate_duration_ms` on every exit path.
        let _timer = RotateTimer(std::time::Instant::now());

        // The part of the file this pass compacts: its length, read under the
        // writer lock so no append is half-done. Not `size_bytes`, which a
        // failed write can leave short of the true end.
        let compacting_upto = {
            let writer = self.writer.lock().await;
            writer.metadata().await?.len()
        };

        let (entries, bad_before) = self.load_upto(compacting_upto).await?;

        // Evaluated once per record; a registry change mid-pass takes effect in
        // the next rotation.
        let mut kept = 0usize;
        let mut dropped = 0usize;
        let mut kept_bytes = 0u64;
        let tmp_path = tmp_path_for(&self.path);

        let mut tmp =
            OpenOptions::new().create(true).truncate(true).write(true).open(&tmp_path).await?;
        for entry in &entries {
            if !retain(&entry.hash) {
                dropped += 1;
                continue;
            }
            let mut line = serde_json::to_vec(entry).map_err(JournalError::Encode)?;
            line.push(b'\n');
            tmp.write_all(&line).await?;
            kept_bytes += line.len() as u64;
            kept += 1;
        }
        // Most of the fsync cost, paid before taking the lock.
        tmp.flush().await?;
        tmp.sync_data().await?;

        // Appends wait from here until the guard is released.
        let mut writer = self.writer.lock().await;
        // Declared after the guard so it drops first: it measures how long an
        // append could have waited.
        let locked = LockedTimer(std::time::Instant::now());

        // Splice on the records appended during the pass, which the swap would
        // otherwise lose.
        let carried = self.bytes_after(compacting_upto).await?;
        tmp.write_all(&carried).await?;
        tmp.flush().await?;
        // Durable before the rename: on some filesystems a rename can survive a
        // power loss that its file's contents do not, leaving an empty journal.
        // `sync_data` still syncs the file size, which is all a reader needs.
        tmp.sync_data().await?;
        drop(tmp);

        // Opened before the rename, so nothing that can fail comes after it: a
        // handle that fails to open after the swap would leave appends going
        // to the replaced file, which no longer has a name.
        let new_writer = OpenOptions::new().append(true).open(&tmp_path).await?;
        tokio::fs::rename(&tmp_path, &self.path).await?;
        *writer = new_writer;
        // Reset under the lock, so no append sees the old count.
        let new_size = kept_bytes + carried.len() as u64;
        self.size_bytes.store(new_size, Ordering::Relaxed);
        metrics::gauge!("preconf.journal.size_bytes").set(new_size as f64);
        let locked_for = locked.0.elapsed();
        drop(locked);
        drop(writer);

        // Makes the rename itself durable. Outside the lock, and only logged on
        // failure: the swap has happened and appends are already going to the
        // new file, so there is nothing to undo.
        if let Err(e) = sync_parent_dir(&self.path).await {
            warn!(
                target: "mantle::preconf::journal",
                ?e,
                "journal directory fsync after rotation failed"
            );
        }

        Ok(RotateStats {
            kept,
            dropped,
            locked_for,
            carried_bytes: carried.len() as u64,
            bad_lines_skipped: bad_before,
        })
    }
}

/// fsync the directory holding `path`, so a rename inside it survives a power
/// loss.
async fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(parent).await?.sync_all().await
}

/// Summary of one [`PreconfJournal::rotate`], for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateStats {
    /// Entries the compaction pass kept, i.e. accepted by `retain`. Excludes
    /// [`carried_bytes`](Self::carried_bytes).
    pub kept: usize,
    /// Entries the compaction pass dropped, i.e. refused by `retain`.
    pub dropped: usize,
    /// How long the writer lock was held, i.e. how long an append could have
    /// waited. Also recorded as `preconf.journal.rotate_locked_ms`; returned
    /// so tests can assert on it.
    pub locked_for: Duration,
    /// Bytes appended during the pass and spliced on unfiltered. Not counted in
    /// `kept` or `dropped`; the next rotation filters them.
    pub carried_bytes: u64,
    /// Corrupt lines the pass skipped, and so left out of the new file.
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

/// Records `preconf.journal.rotate_locked_ms` on drop: the tail of a rotation
/// that holds the writer lock, the part an append can wait for. In fractional
/// milliseconds, to keep sub-millisecond resolution.
struct LockedTimer(std::time::Instant);

impl Drop for LockedTimer {
    fn drop(&mut self) {
        metrics::histogram!("preconf.journal.rotate_locked_ms")
            .record(self.0.elapsed().as_secs_f64() * 1000.0);
    }
}

// ─── Decoding the journal back into transactions ────────────────────────────

/// Turns journal bytes back into something the queue can hold.
///
/// Production uses [`RestoreDirect`](crate::RestoreDirect); tests use a stub.
/// A trait so this crate needs no dependency on `reth-transaction-pool`.
#[async_trait::async_trait]
pub trait RestoreSource: Send + Sync {
    /// Decode and sender-recover `tx_rlp` into the form the queue holds.
    ///
    /// `Err` only for what the bytes themselves get wrong: they do not decode,
    /// or decode to a type the preconf path does not carry. Whether the
    /// commitment is still owed is decided by [`restore_preconf_state`].
    async fn recover_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip>;

    /// Recover just the `(sender, nonce)` of `tx_rlp`, so restore can claim
    /// every slot before admitting any entry (see
    /// [`Commitments::mark_promised`](crate::Commitments::mark_promised)).
    ///
    /// `None` if the bytes do not decode or the signature does not recover;
    /// `recover_envelope` then fails on the same entry. Costs one extra
    /// signature recovery per entry, once per process start.
    fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)>;
}

/// Whether a commitment is on the canonical chain, as far as this node can
/// tell.
///
/// Three-valued on purpose: [`Self::Unknown`] is a **fact about our knowledge**,
/// not a failure to be folded into either answer. As `No` it would raise a
/// false "commitment lost" alarm for every honoured commitment on a node with
/// a pruned index; as `Yes` it would report a commitment kept without evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnChain {
    /// Found on the canonical chain, in the block at `height`. Restore measures
    /// the record's retention from that height.
    Yes {
        /// Canonical block number the transaction was found in.
        height: u64,
    },
    /// Not on the canonical chain, and the lookup index is intact, so the miss
    /// can be trusted.
    No,
    /// Cannot be determined: the transaction-lookup index has been pruned, or
    /// the query itself failed.
    Unknown,
}

/// The chain lookups [`restore_preconf_state`] needs to tell "this commitment
/// landed" from "another transaction took its nonce".
pub trait CommitmentChainView: Send + Sync {
    /// Is `hash` on the canonical chain?
    fn commitment_on_chain(&self, hash: &TxHash) -> OnChain;

    /// `sender`'s account nonce on the canonical chain, or `None` if it
    /// cannot be read. Restore asks this first; only when the nonce has moved
    /// past the entry does it pay for [`Self::commitment_on_chain`].
    fn account_nonce(&self, sender: &Address) -> Option<u64>;
}

/// Why a journal entry could not be turned back into a transaction. Covers
/// only what the bytes get wrong; whether a commitment is still owed is
/// decided by [`restore_preconf_state`] with a [`CommitmentChainView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSkip {
    /// Corrupt bytes, or a type that cannot be preconfirmed. The commitment is
    /// lost.
    Rejected(String),
}

/// Output of a successful [`RestoreSource::recover_envelope`]: the envelope to
/// push into the fifo, and its sender.
#[derive(Debug)]
pub struct RestoredEnvelope {
    /// Decoded, sender-recovered transaction envelope.
    pub envelope: TxEnvelope,
    /// Sender address recovered from the signature.
    pub from: Address,
}

// ─── Startup restore ────────────────────────────────────────────────────────
/// Is the commitment's nonce still unused on chain?
///
/// `true` also when the account cannot be read: a transient storage error is
/// no evidence the nonce was taken. If the entry turns out to be stale, the
/// build's forward sweep drops it.
fn nonce_still_free<C: CommitmentChainView>(chain: &C, rec: &RestoredEnvelope) -> bool {
    match chain.account_nonce(&rec.from) {
        Some(on_chain) => alloy_consensus::Transaction::nonce(&rec.envelope) >= on_chain,
        None => true,
    }
}

/// Rebuild the in-memory preconf state from the journal at startup.
///
/// 1. Re-establish a registry record for **every** entry first, as the kind it was written with
///    (`mark_promised` for a commitment, `record_announced` for a pool transaction). This claims
///    each entry's `(sender, nonce)` before any entry is admitted.
/// 2. Decode each entry via [`RestoreSource::recover_envelope`], then ask the chain whether it is
///    still owed: the sender's nonce first, and only if that has moved on, whether this transaction
///    is the one that took it.
/// 3. Push each still-owed entry into the fifo as
///    [`PreconfSource::Replay`](crate::types::PreconfSource::Replay), which the dispatch layer's
///    deadline and gas-budget gates let through.
///
/// No eligibility check: nothing here reads `PreconfConfig`, so a commitment
/// already acknowledged comes back whatever the current policy says.
///
/// A bad record (corrupt bytes, a type the preconf path does not carry) is
/// logged and skipped. Failing to read the file at all is returned as `Err`,
/// and the caller must not start: nothing restored would be tracked, so the
/// first rotation would delete every commitment still in the file.
pub async fn restore_preconf_state<P: RestoreSource, C: CommitmentChainView>(
    journal: &PreconfJournal,
    source: &P,
    chain: &C,
    fifo: &Arc<PreconfTxSet>,
    claims: &Commitments,
) -> Result<(), JournalError> {
    // No rotation before this pass: the registry is empty until the pass fills
    // it, so every record would read as untracked and be deleted. The rotation
    // runs at the end instead.
    let (entries, bad_lines) = match journal.load().await {
        Ok(v) => v,
        Err(e) => {
            // Starting anyway would lose every commitment in the file: none of
            // them would enter `Commitments`, so the first rotation would delete
            // them all. A missing file (`Ok`, the first start) and a corrupt
            // line (skipped) do not reach here; only a real IO failure does.
            metrics::counter!("preconf.journal.restore_load_failed_total").increment(1);
            error!(
                target: "mantle::preconf::journal",
                ?e,
                "journal load failed; refusing to start rather than lose its commitments"
            );
            return Err(e);
        }
    };
    // Counted as well as logged: a file quietly shedding lines is otherwise
    // invisible.
    metrics::counter!("preconf.journal.restore_bad_lines_total").increment(bad_lines as u64);
    info!(
        target: "mantle::preconf::journal",
        count = entries.len(),
        bad_lines,
        "preconf journal load"
    );

    // Step 1, as its own pass so every `(sender, nonce)` is claimed before any
    // entry is admitted:
    //
    // - Two entries can share a `(sender, nonce)`. Admitting the first before the second is
    //   recorded would send the second through the replacement guard instead of treating it as the
    //   commitment it is.
    // - Claiming later, at validation, would leave the nonce free in between for a same-nonce
    //   transaction to take. Startup order (`cli::node` restores before the pool loader, RPC and
    //   network start) closes that window today; claiming here does not depend on it.
    for entry in &entries {
        match source.recover_slot(&entry.tx_rlp) {
            Some((from, nonce)) => {
                // Re-established as the kind it was written with. Reading a pool
                // transaction back as `Promised` would report a sender's own
                // replacement as a broken commitment. Both kinds claim the slot
                // and are kept for the same depth.
                let claimed = match entry.kind {
                    ClaimKind::Promised => {
                        claims.mark_promised(entry.hash, &from, nonce, entry.block_height)
                    }
                    ClaimKind::Announced => {
                        claims.record_announced(entry.hash, &from, nonce, entry.block_height)
                    }
                };
                if let Err(owner) = claimed {
                    // An earlier entry in the file owns the nonce, so this one
                    // loses it; not seized (see `mark_promised`). If it reaches
                    // the push below, the push is refused and counted as a
                    // conflict.
                    warn!(
                        target: "mantle::preconf::journal",
                        hash = ?entry.hash,
                        ?owner,
                        "a same-nonce tx already owns this slot; commitment may not be honoured"
                    );
                }
            }
            // Undecodable, so no record is made: a record must name the
            // `(sender, nonce)` it claims. `recover_envelope` rejects the same
            // entry below. Reaching this means corruption that survived JSON
            // parsing, or a transaction type this binary no longer supports.
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
    let mut conflicts = 0usize;
    let mut honored = 0usize;
    let mut nonce_taken = 0usize;
    let mut promise_nonce_taken = 0usize;
    let mut unknown = 0usize;
    let mut rejected = 0usize;

    for entry in entries {
        let recovered = match source.recover_envelope(&entry.tx_rlp).await {
            // Nonce not yet used: still owed, back into the queue.
            Ok(rec) if nonce_still_free(chain, &rec) => rec,
            Ok(rec) => {
                let reason = format!(
                    "sender {:?} has moved past nonce {}",
                    rec.from,
                    alloy_consensus::Transaction::nonce(&rec.envelope),
                );
                // The nonce is used; ask whether by this transaction.
                match chain.commitment_on_chain(&entry.hash) {
                    OnChain::Yes { height } => {
                        // Kept before the restart. Measure retention from the
                        // block it landed in, which a reorg could still undo
                        // (see `Commitments::mark_committed`).
                        claims.mark_committed(&entry.hash, height);
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
                        // Another transaction took the nonce, so this one can
                        // never land: its hash is bound to that nonce.
                        //
                        // The step-1 record stays, with no fifo entry, until
                        // `Commitments::sweep` releases it at `SEAL_DEPTH`
                        // persisted blocks past its promised height; rotation
                        // then drops the line. Nothing re-injects it if the
                        // other transaction is reverted.
                        warn!(
                            target: "mantle::preconf::journal",
                            hash = ?entry.hash,
                            reason,
                            "restored tx is NOT on chain but its nonce was consumed by another \
                             transaction; whatever was announced about it will not happen"
                        );
                        nonce_taken += 1;
                        if entry.kind == ClaimKind::Promised {
                            promise_nonce_taken += 1;
                        }
                    }
                    OnChain::Unknown => {
                        // Cannot judge, so the record is kept like the `No`
                        // arm's. Usually means the transaction-lookup index was
                        // pruned.
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
                // Released now rather than left for `sweep`: nothing can land
                // it, and the record would hold the sender's nonce until then.
                claims.release_unless_committed(&entry.hash);
                warn!(
                    target: "mantle::preconf::journal",
                    hash = ?entry.hash,
                    reason,
                    "restored tx could not be decoded; commitment cannot be honoured"
                );
                rejected += 1;
                continue;
            }
        };

        // The earlier entry in the file keeps a contested `(sender, nonce)`: it
        // was pushed first, so a later one is refused with `ConflictActive`.
        // `AlreadyExists` is a duplicate line of an entry already restored.
        match fifo
            .push_if_absent(
                Arc::new(recovered.envelope),
                recovered.from,
                crate::types::PreconfSource::Replay,
            )
            .await
        {
            PushResult::Inserted => restored += 1,
            PushResult::AlreadyExists => {}
            PushResult::ConflictActive(_) => conflicts += 1,
        }
    }

    // Entries whose nonce another transaction took. `restore_nonce_taken`
    // counts both kinds; `restore_promise_nonce_taken` counts commitments only
    // — a client holds a receipt for a transaction that can never land — and is
    // the one to alert on. For a pool transaction this is usually the sender
    // replacing their own, which is routine.
    metrics::counter!("preconf.journal.restore_nonce_taken").increment(nonce_taken as u64);
    metrics::counter!("preconf.journal.restore_promise_nonce_taken")
        .increment(promise_nonce_taken as u64);
    metrics::counter!("preconf.journal.restore_unknown").increment(unknown as u64);
    info!(
        target: "mantle::preconf::journal",
        restored,
        conflicts,
        honored,
        nonce_taken,
        promise_nonce_taken,
        unknown,
        rejected,
        "preconf restore complete"
    );

    // Rotate against the records this pass established, dropping the ones it
    // released or found buried. Failure is only logged; the next rotation
    // retries.
    if let Err(e) = journal.rotate(|hash| claims.is_tracked(hash)).await {
        warn!(
            target: "mantle::preconf::journal",
            ?e,
            "post-restore rotate failed; the file keeps entries the registry has dropped"
        );
    }
    Ok(())
}

// ─── Background rotation loop ───────────────────────────────────────────────

/// Rotate the journal every `interval`, and when a write signals the size cap,
/// until `shutdown` resolves; then rotate once more and return.
///
/// A failed rotation is logged and retried on the next tick. Shutdown is only
/// observed between rotations, never in the middle of one. The first tick is
/// skipped, so a node does not rotate a near-empty file right after boot.
///
/// The final rotation removes records the registry dropped since the last
/// tick. Callers passing a reth `GracefulShutdownGuard` as the shutdown output
/// keep it alive until that rotation completes.
pub async fn run_rejournal_loop<F, T>(
    journal: Arc<PreconfJournal>,
    claims: Arc<Commitments>,
    interval: Duration,
    shutdown: F,
) -> T
where
    F: Future<Output = T>,
{
    // The one eviction rule: drop a record once the registry stops tracking it.
    let retain = |hash: &TxHash| claims.is_tracked(hash);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the immediate first tick.
    ticker.tick().await;
    // Size-triggered rotations at most once per `min_gap`, so a file whose
    // survivors alone exceed the cap does not rotate on every append.
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
            // Shutdown first, so no extra rotation runs after the signal.
            biased;
            output = &mut shutdown => {
                debug!(target: "mantle::preconf::journal", "rotation loop shutting down");
                break output;
            }
            // Ignored within `min_gap` of the last rotation; the next append
            // signals again, and the ticker is the backstop.
            _ = journal.rotate_notify.notified() => {
                let now = Instant::now();
                if last_rotate.is_none_or(|t| now.duration_since(t) >= min_gap) {
                    log_rotate(journal.rotate(&retain).await, "size");
                    last_rotate = Some(now);
                }
            }
            _ = ticker.tick() => {
                log_rotate(journal.rotate(&retain).await, "tick");
                last_rotate = Some(Instant::now());
            }
        }
    };

    // Final rotation. `signal_output` is held across it, so a shutdown guard
    // passed as `T` stays alive until the write completes. Failure is only
    // logged: the process is exiting.
    log_rotate(journal.rotate(&retain).await, "shutdown");

    signal_output
}

/// Minimum gap between two size-triggered rotations, capped at the periodic
/// interval: one L2 slot (2s), so a burst of triggers rotates at most once a
/// slot.
const SIZE_ROTATE_MIN_GAP: Duration = Duration::from_secs(2);

/// Log a rotation outcome, for all three triggers (tick, size, shutdown).
fn log_rotate(result: Result<RotateStats, JournalError>, reason: &'static str) {
    match result {
        Ok(stats) => {
            debug!(
                target: "mantle::preconf::journal",
                reason,
                kept = stats.kept,
                dropped = stats.dropped,
                locked_ms = stats.locked_for.as_secs_f64() * 1000.0,
                carried_bytes = stats.carried_bytes,
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

/// Spawn [`run_rejournal_loop`] on the ambient runtime, stopped by
/// `shutdown_rx`. For tests and callers with their own runtime; production
/// runs [`run_rejournal_loop`] under
/// `TaskExecutor::spawn_critical_with_graceful_shutdown_signal`.
pub fn spawn_rejournal_loop(
    journal: Arc<PreconfJournal>,
    claims: Arc<Commitments>,
    interval: Duration,
    shutdown_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // A dropped sender also counts as shutdown.
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        let () = run_rejournal_loop(journal, claims, interval, shutdown).await;
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
            kind: ClaimKind::Promised,
        }
    }

    /// The record a slice writes for an ordinary pool transaction it carried.
    fn announced_entry(byte: u8, height: u64) -> JournalEntry {
        JournalEntry { kind: ClaimKind::Announced, ..entry(byte, height) }
    }

    /// A record written before `kind` existed still parses, and reads back as a
    /// commitment, so a broken one is still reported as broken.
    #[test]
    fn a_record_written_before_the_kind_field_reads_back_as_a_commitment() {
        let legacy = serde_json::json!({
            "hash": TxHash::from([1; 32]),
            "tx_rlp": Bytes::from(vec![1u8; 4]),
            "block_height": 10,
            "committed_at_ms": 1_001,
        });

        let parsed: JournalEntry =
            serde_json::from_value(legacy).expect("a record from before the field still parses");

        assert_eq!(parsed.kind, ClaimKind::Promised);
    }

    async fn fresh_journal() -> (TempDir, PreconfJournal) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        (dir, j)
    }

    /// A batch is written in order, as one write.
    #[tokio::test]
    async fn append_batch_writes_every_entry_in_order() {
        let (_dir, j) = fresh_journal().await;
        let batch = [entry(1, 10), entry(2, 10), entry(3, 10)];

        j.append_batch(&batch).await.unwrap();

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, batch.to_vec());
        assert_eq!(bad, 0);
    }

    // Not tested: that `append_batch` flushes before returning. tokio's `File`
    // writes on a blocking thread that has normally finished before a second
    // handle reads the path, so such a test would pass even without the flush.

    /// An empty batch, common once the pool is drained, does no IO.
    #[tokio::test]
    async fn append_batch_of_nothing_writes_nothing() {
        let (_dir, j) = fresh_journal().await;

        j.append_batch(&[]).await.unwrap();

        let (loaded, _) = j.load().await.unwrap();
        assert!(loaded.is_empty());
    }

    /// A held record is not written by holding it.
    #[tokio::test]
    async fn holding_a_record_writes_nothing() {
        let (_dir, j) = fresh_journal().await;

        j.hold(&announced_entry(1, 10));

        let (loaded, _) = j.load().await.unwrap();
        assert!(loaded.is_empty());
    }

    /// The next write carries held records ahead of its own, in the order they
    /// were held — which is execution order.
    #[tokio::test]
    async fn held_records_go_ahead_of_the_next_commitment() {
        let (_dir, j) = fresh_journal().await;
        let (first, second, commitment) =
            (announced_entry(1, 10), announced_entry(2, 10), entry(3, 10));

        j.hold(&first);
        j.hold(&second);
        j.append_promised(&commitment).await.unwrap();

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, vec![first, second, commitment]);
        assert_eq!(bad, 0, "held records add no separator line");
    }

    /// `flush` writes what is held and nothing else.
    #[tokio::test]
    async fn flush_writes_what_is_held() {
        let (_dir, j) = fresh_journal().await;
        let held = announced_entry(1, 10);

        j.hold(&held);
        j.flush().await.unwrap();

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, vec![held]);
        assert_eq!(bad, 0);
        assert_eq!(j.pending_len(), 0);
    }

    /// Held records are not unplaced records: only a failed write makes one.
    #[tokio::test]
    async fn held_records_do_not_count_as_unplaced() {
        let (_dir, j) = read_only_journal().await;

        j.hold(&announced_entry(1, 10));
        assert_eq!(j.unplaced_len(), 0, "nothing has failed yet");

        j.append_promised(&entry(2, 10)).await.expect_err("the handle is read-only");
        assert_eq!(j.unplaced_len(), 2, "the failed write takes the held record with it");
    }

    /// Only the first write after a failure opens with a separator: a held
    /// record waiting for a later write is not a sign of a torn line.
    #[tokio::test]
    async fn a_separator_follows_a_failure_and_nothing_else() {
        let (dir, j) = read_only_journal().await;
        j.append_promised(&entry(1, 10)).await.expect_err("the handle is read-only");
        let writable =
            OpenOptions::new().append(true).open(dir.path().join("preconf.jsonl")).await.unwrap();
        *j.writer.lock().await = writable;

        j.append_promised(&entry(2, 10)).await.expect("the disk is back");
        j.hold(&announced_entry(3, 10));
        j.append_promised(&entry(4, 10)).await.unwrap();

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.hash).collect::<Vec<_>>(),
            [1, 2, 3, 4].map(|b| entry(b, 10).hash)
        );
        assert_eq!(bad, 1, "one separator, for the one failure");
    }

    /// A journal whose writes fail with a real IO error.
    async fn read_only_journal() -> (TempDir, PreconfJournal) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // A read-only handle: `write_all` fails with EBADF.
        let j = PreconfJournal::open(&path, u64::MAX).await.unwrap();
        let ro = OpenOptions::new().read(true).open(&path).await.unwrap();
        *j.writer.lock().await = ro;
        (dir, j)
    }

    /// A failed write keeps its records for the next attempt; the caller has
    /// already moved on and will not resend them.
    #[tokio::test]
    async fn a_failed_write_keeps_its_records_for_the_next_attempt() {
        let (_dir, j) = read_only_journal().await;

        j.append_batch(&[entry(1, 10), entry(2, 10)]).await.expect_err("the handle is read-only");

        assert_eq!(j.pending_len(), 2, "both records must be held for a retry");
        let (loaded, _) = j.load().await.unwrap();
        assert!(loaded.is_empty(), "and none of them reached the file");
    }

    /// The next successful write carries the held records with it, oldest first.
    #[tokio::test]
    async fn the_next_write_carries_what_the_failed_one_held() {
        let (dir, j) = read_only_journal().await;
        j.append_batch(&[entry(1, 10)]).await.expect_err("the handle is read-only");

        // The disk comes back.
        let writable =
            OpenOptions::new().append(true).open(dir.path().join("preconf.jsonl")).await.unwrap();
        *j.writer.lock().await = writable;

        j.append_batch(&[entry(2, 10)]).await.expect("the handle writes again");

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.hash).collect::<Vec<_>>(),
            vec![entry(1, 10).hash, entry(2, 10).hash],
            "the held record goes first, keeping the file in execution order",
        );
        assert_eq!(bad, 1, "the failed write leaves a counted recovery separator");
        assert_eq!(j.pending_len(), 0, "and the buffer is empty again");
    }

    /// A retry after a write that stopped mid-line does not fuse onto that line;
    /// only the truncated line is lost.
    #[tokio::test]
    async fn a_retry_does_not_fuse_onto_a_half_written_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // A line that stops mid-record, exactly as an interrupted write leaves it.
        std::fs::write(&path, br#"{"hash":"0x12"#).unwrap();

        let j = PreconfJournal::open(&path, u64::MAX).await.unwrap();
        // Hold a record back, the way a failed write does.
        let ro = OpenOptions::new().read(true).open(&path).await.unwrap();
        *j.writer.lock().await = ro;
        let held = entry(1, 10);
        j.append_batch(std::slice::from_ref(&held)).await.expect_err("the handle is read-only");
        assert_eq!(j.pending_len(), 1);

        // The disk comes back and the retry lands.
        let writable = OpenOptions::new().append(true).open(&path).await.unwrap();
        *j.writer.lock().await = writable;
        let fresh = entry(2, 10);
        j.append_batch(std::slice::from_ref(&fresh)).await.expect("writes again");

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.hash).collect::<Vec<_>>(),
            vec![held.hash, fresh.hash],
            "the truncated line must cost only itself",
        );
        assert_eq!(bad, 1, "and it is still counted as the corrupt line it is");
    }

    /// A full buffer drops the records offered first. Offered as one batch, so
    /// the test does not need tens of thousands of failed writes.
    #[tokio::test]
    async fn a_full_buffer_drops_its_oldest_records() {
        let (_dir, j) = read_only_journal().await;
        let overflow = 3usize;
        let offered: Vec<JournalEntry> = (0..PENDING_CAPACITY + overflow)
            .map(|i| JournalEntry {
                hash: TxHash::from(alloy_primitives::U256::from(i as u64).to_be_bytes()),
                ..entry(0, 10)
            })
            .collect();

        j.append_batch(&offered).await.expect_err("the handle is read-only");

        assert_eq!(j.pending_len(), PENDING_CAPACITY, "the buffer stops at its capacity");
        let oldest_line = j.pending.lock().front().cloned().expect("buffer is not empty");
        let oldest: JournalEntry = serde_json::from_slice(oldest_line.trim_ascii_end()).unwrap();
        assert_eq!(
            oldest.hash, offered[overflow].hash,
            "the records dropped are the ones offered first",
        );
    }

    /// Run the real eviction rule at a small budget; return what survived and
    /// how many were dropped.
    fn fill(sizes: &[usize], max_count: usize, max_bytes: usize) -> (VecDeque<Vec<u8>>, u64) {
        let mut pending = VecDeque::new();
        let mut bytes = 0usize;
        let mut dropped = 0u64;
        for (i, &len) in sizes.iter().enumerate() {
            let line = vec![i as u8; len];
            dropped += evict_to_fit(&mut pending, &mut bytes, line.len(), max_count, max_bytes);
            bytes += line.len();
            pending.push_back(line);
        }
        (pending, dropped)
    }

    /// The byte fuse evicts where the count limit would not.
    #[test]
    fn the_byte_fuse_evicts_where_the_count_limit_would_not() {
        let (pending, dropped) = fill(&[100, 100, 100, 100], 1_000, 300);

        assert_eq!(pending.len(), 3, "only three hundred bytes may be held");
        assert_eq!(dropped, 1);
        assert_eq!(pending.front().expect("non-empty")[0], 1, "and the oldest is what went");
    }

    /// At ordinary record sizes the count limit applies first.
    #[test]
    fn the_count_limit_bites_first_at_ordinary_sizes() {
        let (pending, dropped) = fill(&[100; 6], 4, 1_000_000);

        assert_eq!(pending.len(), 4);
        assert_eq!(dropped, 2);
    }

    /// A record larger than the whole budget is still held.
    #[test]
    fn a_record_larger_than_the_budget_is_still_held() {
        let (pending, _) = fill(&[5_000], 1_000, 300);

        assert_eq!(pending.len(), 1);
    }

    /// A batch counts toward the size cap like single appends, so a slicing
    /// node still rotates.
    #[tokio::test]
    async fn append_batch_arms_size_triggered_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // 1-byte cap: any write at all crosses it.
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let kept = entry(3, 12);
        j.append_batch(&[entry(1, 10), entry(2, 11), kept.clone()]).await.unwrap();
        let c = commitments_done_with(&[TxHash::from([1; 32]), TxHash::from([2; 32])]);
        still_owed(&c, kept.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_secs(3600), shutdown_rx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (after, _) = j.load().await.unwrap();
        assert_eq!(after, vec![kept], "the batch must have pinged the rotate notify");

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    /// A `JournalEntry` with an explicit commit timestamp.
    fn entry_at(byte: u8, height: u64, committed_at_ms: u64) -> JournalEntry {
        JournalEntry {
            hash: TxHash::from([byte; 32]),
            tx_rlp: Bytes::from(vec![byte; 4]),
            block_height: height,
            committed_at_ms,
            kind: ClaimKind::Promised,
        }
    }

    /// Age is not an eviction rule: an entry stamped at the epoch is kept while
    /// `Commitments` tracks it.
    #[tokio::test]
    async fn rotate_keeps_an_ancient_entry_commitments_still_tracks() {
        let (_dir, j) = fresh_journal().await;
        let ancient = entry_at(1, 10, 1_000);
        j.append_promised(&ancient).await.unwrap();

        let c = empty_commitments();
        c.mark_promised(ancient.hash, &Address::from([0xEE; 20]), 0, ancient.block_height).unwrap();

        let stats = j.rotate(|h| c.is_tracked(h)).await.unwrap();
        assert_eq!(stats.dropped, 0);
        let (survivors, _) = j.load().await.unwrap();
        assert_eq!(survivors, vec![ancient], "still tracked ⇒ still owed ⇒ kept");
    }

    /// An entry leaves the file when `Commitments` stops tracking it, and not
    /// before. When that happens is covered by the `sweep` tests.
    #[tokio::test]
    async fn rotate_drops_an_entry_commitments_gave_up() {
        let (_dir, j) = fresh_journal().await;
        let e = entry_at(1, 10, 1_000);
        j.append_promised(&e).await.unwrap();

        let c = empty_commitments();
        c.mark_promised(e.hash, &Address::from([0xEE; 20]), 0, e.block_height).unwrap();
        assert_eq!(j.rotate(|h| c.is_tracked(h)).await.unwrap().dropped, 0);

        assert!(c.release_unless_committed(&e.hash));
        let stats = j.rotate(|h| c.is_tracked(h)).await.unwrap();
        assert_eq!(stats.dropped, 1);
        let (survivors, _) = j.load().await.unwrap();
        assert!(survivors.is_empty(), "the journal follows `Commitments`");
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
            pending: SyncMutex::new(VecDeque::new()),
            pending_bytes: AtomicUsize::new(0),
            unplaced: AtomicUsize::new(0),
            torn: AtomicBool::new(false),
        };
        let (loaded, bad) = j.load().await.unwrap();
        assert!(loaded.is_empty());
        assert_eq!(bad, 0);
    }

    #[tokio::test]
    async fn load_skips_corrupt_lines_and_reports_count() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // Two good lines around one corrupt line.
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
    async fn load_counts_blank_lines_without_losing_valid_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let good = serde_json::to_string(&entry(3, 30)).unwrap();
        // Two blank lines before the good one and one after.
        tokio::fs::write(&path, format!("\n\n{good}\n\n")).await.unwrap();
        let j = PreconfJournal::open(&path, 0).await.unwrap();
        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, vec![entry(3, 30)]);
        assert_eq!(bad, 3, "blank lines are evidence of an incomplete journal write");
    }

    /// Rotation keeps exactly what `retain` accepts.
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

    /// A predicate that keeps everything drops nothing.
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

    /// Appends racing a rotation are neither lost nor left out of `size_bytes`.
    /// `retain` keeps everything, so any loss is the race.
    ///
    /// Which appends land inside the rotation varies between runs; the
    /// deterministic version is
    /// [`an_entry_appended_during_the_compaction_pass_survives_the_swap`].
    #[tokio::test]
    async fn rotate_does_not_lose_concurrent_appends() {
        use std::sync::Arc;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 0).await.unwrap());

        // Existing records lengthen the rotation, giving the race room.
        for i in 0..5u8 {
            j.append_promised(&entry(i, u64::from(i))).await.unwrap();
        }

        let n: u8 = 30;
        let rot = {
            let j = j.clone();
            tokio::spawn(async move { j.rotate(|_| true).await.unwrap() })
        };
        let mut appends = Vec::new();
        for i in 5..n {
            let j = j.clone();
            appends.push(tokio::spawn(async move {
                j.append_promised(&entry(i, u64::from(i))).await.unwrap()
            }));
        }
        rot.await.unwrap();
        for a in appends {
            a.await.unwrap();
        }

        let (after, _) = j.load().await.unwrap();
        let on_disk: HashSet<TxHash> = after.iter().map(|e| e.hash).collect();
        for i in 0..n {
            assert!(
                on_disk.contains(&TxHash::from([i; 32])),
                "entry {i} lost across concurrent rotate"
            );
        }

        let on_disk_bytes = tokio::fs::metadata(&path).await.unwrap().len();
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            on_disk_bytes,
            "size_bytes counter drifted from true on-disk size across concurrent rotate"
        );
    }

    /// Hold a rotation open inside its compaction pass, by blocking in the
    /// first `retain` call. Returns a receiver that fires once the pass has
    /// begun, the sender that releases it, and the rotation's handle.
    fn rotation_held_open(
        j: Arc<PreconfJournal>,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<RotateStats>,
    ) {
        let (compacting_tx, compacting_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let rotate = tokio::spawn(async move {
            let first = std::sync::atomic::AtomicBool::new(true);
            j.rotate(move |_| {
                if first.swap(false, Ordering::Relaxed) {
                    compacting_tx.send(()).expect("test is listening");
                    // Blocking recv: `retain` is synchronous.
                    release_rx.recv().expect("test releases the pass");
                }
                true
            })
            .await
            .expect("rotate")
        });
        (compacting_rx, release_tx, rotate)
    }

    /// `locked_for` covers only the locked tail, not the compaction pass. The
    /// pass is held open for 300ms; the tail (a small splice and one fsync)
    /// takes far less.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_locked_span_excludes_the_compaction_pass() {
        let dir = TempDir::new().unwrap();
        let j = Arc::new(PreconfJournal::open(dir.path().join("preconf.jsonl"), 0).await.unwrap());
        for i in 0..5u8 {
            j.append_promised(&entry(i, u64::from(i))).await.unwrap();
        }

        let (mut compacting, release, rotate) = rotation_held_open(Arc::clone(&j));
        compacting.recv().await.expect("compaction pass began");

        let held = Duration::from_millis(300);
        tokio::time::sleep(held).await;
        release.send(()).unwrap();
        let stats = rotate.await.unwrap();

        assert!(
            stats.locked_for < held,
            "locked span {:?} covers the compaction pass, which was held open for {held:?}",
            stats.locked_for,
        );
    }

    /// `load_upto` reads only to its limit, or the splice would append records
    /// a second time. Tested directly because a race cannot reliably land an
    /// append inside that window.
    #[tokio::test]
    async fn the_compaction_pass_reads_only_as_far_as_its_snapshot() {
        let (_dir, j) = fresh_journal().await;
        let compacted = entry(1, 10);
        j.append_promised(&compacted).await.unwrap();

        let snapshot = j.size_bytes.load(Ordering::Relaxed);
        let latecomer = entry(2, 11);
        j.append_promised(&latecomer).await.unwrap();

        let (seen, bad) = j.load_upto(snapshot).await.unwrap();

        assert_eq!(bad, 0);
        assert_eq!(seen, vec![compacted], "read past the snapshot into {latecomer:?}");
    }

    /// An append does not wait for the compaction pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_append_does_not_wait_for_the_compaction_pass() {
        let dir = TempDir::new().unwrap();
        let j = Arc::new(PreconfJournal::open(dir.path().join("preconf.jsonl"), 0).await.unwrap());
        for i in 0..5u8 {
            j.append_promised(&entry(i, u64::from(i))).await.unwrap();
        }

        let (mut compacting, release, rotate) = rotation_held_open(Arc::clone(&j));
        compacting.recv().await.expect("compaction pass began");

        // Generous: this checks the append is not blocked, not its speed.
        let appended =
            tokio::time::timeout(Duration::from_secs(5), j.append_promised(&entry(9, 9))).await;

        release.send(()).unwrap();
        rotate.await.unwrap();
        appended.expect("append waited for the compaction pass to finish").unwrap();
    }

    /// A record appended during the compaction pass is carried into the rotated
    /// file and counted in `size_bytes`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_entry_appended_during_the_compaction_pass_survives_the_swap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 0).await.unwrap());
        for i in 0..5u8 {
            j.append_promised(&entry(i, u64::from(i))).await.unwrap();
        }

        let (mut compacting, release, rotate) = rotation_held_open(Arc::clone(&j));
        compacting.recv().await.expect("compaction pass began");

        // Timed, so an implementation that blocks appends fails instead of
        // deadlocking.
        let late = entry(9, 9);
        let appended = tokio::time::timeout(Duration::from_secs(5), j.append_promised(&late)).await;

        release.send(()).unwrap();
        rotate.await.unwrap();
        appended.expect("append waited for the compaction pass to finish").unwrap();

        // Exact equality: rules out the late entry being dropped, duplicated or
        // reordered.
        let mut expected: Vec<JournalEntry> = (0..5u8).map(|i| entry(i, u64::from(i))).collect();
        expected.push(late);

        let (after, bad) = j.load().await.unwrap();
        assert_eq!(bad, 0);
        assert_eq!(after, expected, "rotated file is not survivors-then-latecomer");
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            tokio::fs::metadata(&path).await.unwrap().len(),
            "size counter does not describe the rotated file"
        );
    }

    /// After a rotation, appends land in the rotated file.
    #[tokio::test]
    async fn rotate_then_append_writes_to_new_file_handle() {
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

    /// A non-UTF-8 byte costs one corrupt line, not the whole read: a failed
    /// read would stop the node starting, and make every rotation fail.
    #[tokio::test]
    async fn a_line_that_is_not_utf8_costs_only_itself() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let mut file = serde_json::to_vec(&entry(7, 70)).unwrap();
        file.extend_from_slice(b"\n\xff\xfe{}\n");
        file.extend_from_slice(&serde_json::to_vec(&entry(8, 80)).unwrap());
        file.push(b'\n');
        tokio::fs::write(&path, file).await.unwrap();
        let j = PreconfJournal::open(&path, u64::MAX).await.unwrap();

        let (loaded, bad) = j.load().await.expect("a bad byte does not fail the read");
        assert_eq!(loaded, vec![entry(7, 70), entry(8, 80)]);
        assert_eq!(bad, 1);

        let stats = j.rotate(|_| true).await.expect("nor does it fail rotation");
        assert_eq!(stats.bad_lines_skipped, 1);
        let (after, bad_after) = j.load().await.unwrap();
        assert_eq!(after, vec![entry(7, 70), entry(8, 80)]);
        assert_eq!(bad_after, 0, "rotation leaves the bad line behind");
    }

    /// A write that fails part-way leaves bytes `size_bytes` never counted.
    /// Rotation must still cut at the real end of the file, or it splits and
    /// loses the record written after the failure.
    ///
    /// The partial write is staged by hand (a fragment appended directly, its
    /// record put back in the buffer, the torn flag set); no portable way stops
    /// a real write half-way.
    #[tokio::test]
    async fn rotation_after_a_partial_write_keeps_every_record() {
        let (_dir, j) = fresh_journal().await;
        let before = entry(1, 10);
        j.append_promised(&before).await.unwrap();

        let failed = entry(2, 10);
        let failed_line = serde_json::to_vec(&failed).unwrap();
        {
            use std::io::Write;
            let mut raw = std::fs::OpenOptions::new().append(true).open(&j.path).unwrap();
            raw.write_all(&failed_line[..failed_line.len() / 2]).unwrap();
        }
        let mut held = failed_line;
        held.push(b'\n');
        j.buffer(vec![held]);
        j.torn.store(true, Ordering::Relaxed);

        let after = entry(3, 10);
        j.append_promised(&after).await.expect("the disk is back");
        j.rotate(|_| true).await.unwrap();

        let (loaded, _) = j.load().await.unwrap();
        assert_eq!(
            loaded.iter().map(|e| e.hash).collect::<Vec<_>>(),
            vec![before.hash, failed.hash, after.hash],
        );
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            std::fs::metadata(&j.path).unwrap().len(),
            "and the counter matches the rotated file",
        );
    }

    /// After a rotation the writer and the path name the same file, so later
    /// appends land where a restart reads.
    #[cfg(unix)]
    #[tokio::test]
    async fn rotation_leaves_the_writer_on_the_file_at_the_path() {
        use std::os::unix::fs::MetadataExt;

        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.rotate(|_| true).await.unwrap();

        let writer_ino = j.writer.lock().await.metadata().await.unwrap().ino();
        let path_ino = std::fs::metadata(&j.path).unwrap().ino();
        assert_eq!(writer_ino, path_ino);
        assert!(!tmp_path_for(&j.path).exists(), "the tmp file was renamed into place");
    }

    /// An unreadable journal makes restore return the error, not an empty
    /// "successful" restore.
    #[tokio::test]
    async fn restore_returns_the_error_when_the_journal_cannot_be_read() {
        let (dir, j) = fresh_journal().await;
        let path = dir.path().join("preconf.jsonl");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let fifo = Arc::new(PreconfTxSet::new(16));

        let result =
            restore_preconf_state(&j, &StubSource::new(), &landed(), &fifo, &empty_commitments())
                .await;

        assert!(matches!(result, Err(JournalError::Io(_))), "got {result:?}");
    }

    // ── restore_preconf_state ──────────────────────────────────────

    /// Stub [`RestoreSource`]: records every `recover_envelope` call and
    /// fabricates an envelope from the first byte of the bytes it is given.
    struct StubSource {
        /// Bytes passed to `recover_envelope`, in order.
        add_calls: std::sync::Mutex<Vec<Bytes>>,
        /// `recover_envelope` returns `Err`.
        reject_add: bool,
        /// The bytes do not decode: `recover_slot` returns `None` too.
        undecodable: bool,
    }

    impl StubSource {
        fn new() -> Self {
            Self {
                add_calls: std::sync::Mutex::new(Vec::new()),
                reject_add: false,
                undecodable: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl RestoreSource for StubSource {
        /// Must return the `(from, nonce)` `recover_envelope` fabricates, or
        /// restore would claim one slot and push another.
        fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
            if self.undecodable {
                return None;
            }
            let seed = tx_rlp.first().copied().unwrap_or(0);
            Some((Address::from([seed; 20]), u64::from(seed)))
        }
        async fn recover_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
            self.add_calls.lock().unwrap().push(tx_rlp.clone());
            if self.reject_add {
                return Err(RestoreSkip::Rejected("rejected by stub".into()));
            }
            // Fails here as in `recover_slot`, as the real adapter would.
            if self.undecodable {
                return Err(RestoreSkip::Rejected("does not decode".into()));
            }
            // A dummy legacy transaction; restore only uses `envelope` and
            // `from` for the fifo push.
            use alloy_consensus::{Signed, TxLegacy};
            use alloy_primitives::Signature;
            let nonce = u64::from(tx_rlp.first().copied().unwrap_or(0));
            let inner = TxLegacy { nonce, ..Default::default() };
            let sig = Signature::test_signature();
            // Hash and sender derived from the first byte.
            let hash_byte = tx_rlp.first().copied().unwrap_or(0);
            let hash = TxHash::from([hash_byte; 32]);
            let envelope = TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash));
            let from = Address::from([hash_byte; 20]);
            Ok(RestoredEnvelope { envelope, from })
        }
    }

    /// Scripted chain view: the same `OnChain` answer for every hash, and
    /// `.1` as every sender's nonce (`None`: unreadable, so every entry's nonce
    /// counts as still free).
    struct StubChain(OnChain, Option<u64>);

    impl CommitmentChainView for StubChain {
        fn account_nonce(&self, _sender: &Address) -> Option<u64> {
            self.1
        }

        fn commitment_on_chain(&self, _hash: &TxHash) -> OnChain {
            self.0
        }
    }

    /// The chain view for tests that do not care whether an entry landed.
    fn landed() -> StubChain {
        StubChain(OnChain::Yes { height: LANDED_AT }, None)
    }

    /// Height `landed()` reports; only its distance to the persisted watermark
    /// matters.
    const LANDED_AT: u64 = 10;

    /// The production rotation predicate.
    fn retain_tracked(c: &Commitments) -> impl Fn(&TxHash) -> bool + '_ {
        move |h| c.is_tracked(h)
    }

    /// An **enabled** `Commitments` with **empty allowlists**.
    ///
    /// Enabled, because a disabled one (the `PreconfConfig` default) records
    /// nothing and restore would appear to do nothing. Empty allowlists,
    /// because restore must bring commitments back even when governance
    /// currently allows nobody; `restart_replay.rs` cannot cover that, as it
    /// seeds the lists before the node starts.
    fn empty_commitments() -> Commitments {
        Commitments::from_config(&crate::PreconfConfig {
            enabled: true,
            ..crate::PreconfConfig::default()
        })
    }

    /// A `Commitments` that has finished tracking `hashes`: promised, landed,
    /// buried `SEAL_DEPTH` deep and released, so rotation may drop them.
    fn commitments_done_with(hashes: &[TxHash]) -> Arc<Commitments> {
        let c = empty_commitments();
        for (i, h) in hashes.iter().enumerate() {
            // Distinct (sender, nonce) per hash so the claims do not collide.
            let _ = c.mark_promised(*h, &Address::from([0xEE; 20]), i as u64, 0);
            c.mark_committed(h, LANDED_AT);
        }
        c.observe_persisted(LANDED_AT + crate::commitments::SEAL_DEPTH);
        for h in hashes {
            // Stands in for `Commitments::sweep` releasing a buried record.
            // Returns false while still within retention, which the assert
            // catches.
            assert!(c.release_unless_committed(h), "fixture must be done with the commitment");
        }
        Arc::new(c)
    }

    /// A `Commitments` whose watermark already makes anything
    /// [`finish_tracking`] marks releasable, for finishing a commitment after
    /// the rotation loop has started.
    fn commitments_with_high_watermark() -> Arc<Commitments> {
        let c = empty_commitments();
        c.observe_persisted(LANDED_AT + crate::commitments::SEAL_DEPTH);
        Arc::new(c)
    }

    /// Mark `hash` as still owed: promised for a block that cannot be buried
    /// yet, so rotation keeps it. (An entry with no record would be dropped.)
    fn still_owed(c: &Commitments, hash: TxHash) {
        let _ = c.mark_promised(hash, &Address::from([0xEE; 20]), u64::from(hash.0[0]), u64::MAX);
        assert!(c.is_tracked(&hash));
    }

    /// Take `hash` through promised → landed → released, so `Commitments` is
    /// done with it. The release stands in for `Commitments::sweep`.
    fn finish_tracking(c: &Commitments, hash: TxHash) {
        // The nonce is derived from the hash so repeated calls do not collide.
        let _ = c.mark_promised(hash, &Address::from([0xEE; 20]), u64::from(hash.0[0]), 0);
        c.mark_committed(&hash, LANDED_AT);
        assert!(c.release_unless_committed(&hash), "the watermark must make it releasable");
        assert!(!c.is_tracked(&hash));
    }

    /// Restore stays linear in the entry count. With slicing, a restart can
    /// face tens of thousands of entries.
    ///
    /// Asserts on the ratio between 1k and 10k entries, not on wall-clock
    /// time, which would be flaky on shared CI: linear is ~10x, quadratic ~100x.
    /// The timings are printed for reference.
    #[tokio::test]
    async fn restore_cost_grows_with_the_entry_count_not_faster() {
        async fn restore_n(n: usize) -> std::time::Duration {
            let (_dir, journal) = fresh_journal().await;
            let entries: Vec<_> = (0..n)
                .map(|i| {
                    let mut e = entry(0, 10);
                    // Distinct hashes; the restore pre-pass keys on them.
                    e.hash = TxHash::from(alloy_primitives::U256::from(i as u64).to_be_bytes());
                    e
                })
                .collect();
            journal.append_batch(&entries).await.unwrap();

            let pool = StubSource::new();
            let chain = landed();
            let fifo = Arc::new(PreconfTxSet::new(1 << 16));
            let claims = empty_commitments();

            let started = std::time::Instant::now();
            restore_preconf_state(&journal, &pool, &chain, &fifo, &claims).await.expect("restore");
            started.elapsed()
        }

        let small = restore_n(1_000).await;
        let large = restore_n(10_000).await;
        println!("restore: 1k entries {small:?}, 10k entries {large:?}");

        // Under 40x: quadratic would be near 100x; scheduling noise stays well
        // below 40x.
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);
        assert!(
            ratio < 40.0,
            "restore should stay roughly linear in the entry count; 1k took {small:?}, \
             10k took {large:?} ({ratio:.1}x)",
        );
    }

    #[tokio::test]
    async fn restore_from_empty_journal_is_noop() {
        let (_dir, j) = fresh_journal().await;
        let pool = StubSource::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_commitments())
            .await
            .expect("restore");
        assert!(pool.add_calls.lock().unwrap().is_empty());
        assert!(fifo.snapshot().await.is_empty());
    }

    /// Restore re-establishes each record as the kind it was written with. A
    /// pool transaction read back as `Promised` would make a sender's own
    /// replacement look like a broken commitment.
    #[tokio::test]
    async fn restore_keeps_the_kind_each_record_was_written_with() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_batch(&[announced_entry(2, 10)]).await.unwrap();

        let claims = Arc::new(empty_commitments());
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &StubSource::new(), &landed(), &fifo, &claims)
            .await
            .expect("restore");

        assert!(
            claims.was_promised(&TxHash::from([1; 32])),
            "a commitment has to come back as one",
        );
        assert!(
            !claims.was_promised(&TxHash::from([2; 32])),
            "a pool transaction a slice carried must not come back as a receipt nobody can repair",
        );
    }

    #[tokio::test]
    async fn restore_injects_missing_txs_and_pushes_fifo() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();

        let pool = StubSource::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_commitments())
            .await
            .expect("restore");

        assert_eq!(pool.add_calls.lock().unwrap().len(), 2, "both txs admitted");
        let snapshot = fifo.snapshot().await;
        assert_eq!(snapshot.len(), 2);
    }

    #[tokio::test]
    async fn restore_pushes_single_entry_to_fifo() {
        let (_dir, j) = fresh_journal().await;
        let e1 = entry(3, 30);
        j.append_promised(&e1).await.unwrap();

        let pool = StubSource::new();
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &empty_commitments())
            .await
            .expect("restore");

        assert_eq!(pool.add_calls.lock().unwrap().len(), 1);
        assert_eq!(fifo.snapshot().await.len(), 1);
    }

    /// Every entry has a record **before the first one reaches
    /// `recover_envelope`**, not merely before its own.
    #[tokio::test]
    async fn restore_marks_every_entry_promised_before_admitting_any() {
        /// On each call, snapshots whether every journal hash is tracked,
        /// including those not decoded yet.
        struct RecordSpySource {
            claims: Arc<Commitments>,
            hashes: Vec<TxHash>,
            seen: std::sync::Mutex<Vec<Vec<bool>>>,
        }
        #[async_trait::async_trait]
        impl RestoreSource for RecordSpySource {
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((Address::from([seed; 20]), u64::from(seed)))
            }
            async fn recover_envelope(
                &self,
                tx_rlp: &Bytes,
            ) -> Result<RestoredEnvelope, RestoreSkip> {
                use alloy_consensus::{Signed, TxLegacy};
                use alloy_primitives::Signature;

                let seed = tx_rlp.first().copied().unwrap_or(0);
                let inner = TxLegacy { nonce: u64::from(seed), ..Default::default() };
                let sig = Signature::test_signature();
                let hash = TxHash::from([seed; 32]);
                self.seen
                    .lock()
                    .unwrap()
                    .push(self.hashes.iter().map(|h| self.claims.is_tracked(h)).collect());
                Ok(RestoredEnvelope {
                    envelope: TxEnvelope::Legacy(Signed::new_unchecked(inner, sig, hash)),
                    from: Address::from([seed; 20]),
                })
            }
        }

        let (_dir, j) = fresh_journal().await;
        // `entry(byte, ..)` and the spy derive the hash from the same byte.
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        j.append_promised(&entry(3, 12)).await.unwrap();

        let claims = Arc::new(empty_commitments());
        let pool = RecordSpySource {
            claims: claims.clone(),
            hashes: (1u8..=3).map(|b| TxHash::from([b; 32])).collect(),
            seen: std::sync::Mutex::new(vec![]),
        };
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &claims).await.expect("restore");

        let seen = pool.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "every entry must be offered to the pool");
        // At the first call, entries 2 and 3 must already be tracked. Recording
        // each entry just before its own decode would leave them `false`.
        assert!(
            seen[0].iter().all(|promised| *promised),
            "all entries must be Promised before the first admission, including \
             those not yet admitted; got {:?}",
            seen[0],
        );
    }

    /// Every entry's `(sender, nonce)` is claimed before the first entry reaches
    /// `recover_envelope`, so no same-nonce transaction can take it in between.
    /// Startup order prevents that today; this checks restore does not depend
    /// on it.
    #[tokio::test]
    async fn restore_claims_every_slot_before_admitting_any() {
        /// On each call, snapshots the owner of every entry's slot, including
        /// those not decoded yet.
        struct SlotSpySource {
            claims: Arc<Commitments>,
            slots: Vec<(Address, u64)>,
            seen: std::sync::Mutex<Vec<Vec<Option<TxHash>>>>,
        }
        #[async_trait::async_trait]
        impl RestoreSource for SlotSpySource {
            fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
                let seed = tx_rlp.first().copied().unwrap_or(0);
                Some((Address::from([seed; 20]), u64::from(seed)))
            }
            async fn recover_envelope(
                &self,
                tx_rlp: &Bytes,
            ) -> Result<RestoredEnvelope, RestoreSkip> {
                use alloy_consensus::{Signed, TxLegacy};
                use alloy_primitives::Signature;

                let seed = tx_rlp.first().copied().unwrap_or(0);
                self.seen.lock().unwrap().push(
                    self.slots
                        .iter()
                        .map(|(from, nonce)| self.claims.slot_owner(from, *nonce))
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
        }

        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 10)).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        j.append_promised(&entry(3, 12)).await.unwrap();

        // Must be enabled, or `mark_promised` records nothing and the test
        // proves nothing.
        let claims =
            Arc::new(Commitments::enabled(crate::commitments::DEFAULT_COMMITMENT_CACHE_CAP));
        let pool = SlotSpySource {
            claims: claims.clone(),
            slots: (1u8..=3).map(|b| (Address::from([b; 20]), u64::from(b))).collect(),
            seen: std::sync::Mutex::new(vec![]),
        };
        let fifo = Arc::new(PreconfTxSet::new(16));
        restore_preconf_state(&j, &pool, &landed(), &fifo, &claims).await.expect("restore");

        let seen = pool.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "every entry must be offered to the pool");
        // At the first call, entries 2 and 3 must already be claimed.
        // Claiming inside the decode loop would leave them `None`.
        assert_eq!(
            seen[0],
            (1u8..=3).map(|b| Some(TxHash::from([b; 32]))).collect::<Vec<_>>(),
            "every slot must be claimed before the first admission",
        );
    }

    /// An entry already on chain is a kept commitment: its line stays while the
    /// landing block is shallow and can go once it is buried. (That it never
    /// reaches the fifo is `a_consumed_nonce_never_reaches_the_fifo`.)
    #[tokio::test]
    async fn restore_starts_the_retention_clock_for_an_already_landed_entry() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_commitments();
        // Nonce gone as well as `Yes`: an on-chain transaction has used its
        // nonce, and the used nonce is what makes restore look up the hash.
        restore_preconf_state(
            &j,
            &OnChainSource,
            &StubChain(OnChain::Yes { height: LANDED_AT }, NONCE_IS_GONE),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await
        .expect("restore");

        // Shallow: the record, and so the line, is kept.
        assert!(!c.release_unless_committed(&e.hash), "shallow: the record is held");
        let stats = j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e.clone()], "kept while shallow; stats = {stats:?}");

        // Buried: the record may go, and rotation drops the line.
        c.observe_persisted(LANDED_AT + crate::commitments::SEAL_DEPTH);
        assert!(c.release_unless_committed(&e.hash), "buried: the record is released");
        let stats = j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty(), "rotation must drop it; stats = {stats:?}");
    }

    /// Restore never leaves a fifo entry for a `(sender, nonce)` it does not own.
    ///
    /// Restore pushes entries without going through admission, so it is the
    /// one place that could break this. Two journal records on one
    /// `(sender, nonce)` should not occur, but given them, only the slot's
    /// owner gets an entry: `push_if_absent` refuses a different hash on an
    /// occupied slot.
    #[tokio::test]
    async fn restore_never_leaves_a_fifo_entry_without_its_slot() {
        /// Every entry decodes to the same `(sender, nonce)`, with a hash from
        /// its first byte.
        struct SameSlotSource;

        const SHARED_SENDER: Address = Address::new([0xAB; 20]);
        const SHARED_NONCE: u64 = 7;

        #[async_trait::async_trait]
        impl RestoreSource for SameSlotSource {
            fn recover_slot(&self, _tx_rlp: &Bytes) -> Option<(Address, u64)> {
                Some((SHARED_SENDER, SHARED_NONCE))
            }
            async fn recover_envelope(
                &self,
                tx_rlp: &Bytes,
            ) -> Result<RestoredEnvelope, RestoreSkip> {
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
        let c = empty_commitments();
        restore_preconf_state(&j, &SameSlotSource, &landed(), &fifo, &c).await.expect("restore");

        // The first entry claims the slot; the other loses it.
        let owner = c.slot_owner(&SHARED_SENDER, SHARED_NONCE).expect("someone owns the nonce");
        let other = if owner == winner.hash { loser.hash } else { winner.hash };

        assert!(fifo.contains(&owner).await, "the owner is the one that gets an entry");
        assert!(
            !fifo.contains(&other).await,
            "a transaction that lost the slot must not hold a fifo entry",
        );
    }

    /// A sender nonce past every journaled entry, so the tests below all take
    /// the "nonce used" path and differ only in the `OnChain` answer.
    const NONCE_IS_GONE: Option<u64> = Some(u64::MAX);

    /// A decoder that always succeeds; the outcome is up to the chain view.
    struct OnChainSource;

    #[async_trait::async_trait]
    impl RestoreSource for OnChainSource {
        fn recover_slot(&self, tx_rlp: &Bytes) -> Option<(Address, u64)> {
            let seed = tx_rlp.first().copied().unwrap_or(0);
            Some((Address::from([seed; 20]), u64::from(seed)))
        }
        async fn recover_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
            use alloy_consensus::{Signed, TxLegacy};
            use alloy_primitives::Signature;
            let seed = tx_rlp.first().copied().unwrap_or(0);
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
    }

    /// Nonce used by a *different* transaction: the commitment is broken, not
    /// kept. Restore must not record it as landed, and its line survives
    /// rotation until `Commitments::sweep` releases the record. A nonce-only
    /// check would read this as kept and drop the line at once.
    #[tokio::test]
    async fn a_stolen_nonce_keeps_its_entry_through_rotation() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_commitments();
        restore_preconf_state(
            &j,
            &OnChainSource,
            &StubChain(OnChain::No, NONCE_IS_GONE),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await
        .expect("restore");

        // `retain_tracked`, not `|_| true`: the line survives because the
        // record does.
        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(
            remaining,
            vec![e.clone()],
            "must survive rotation, so a later reorg can still free its nonce"
        );

        // Not landed: a landed record could not be released while shallow.
        assert!(c.release_unless_committed(&e.hash), "restore must not record it as landed");
    }

    /// `Unknown` keeps the entry; read as "on chain" it would hide a broken
    /// commitment on a node with a pruned lookup index.
    #[tokio::test]
    async fn an_undeterminable_entry_is_retained() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_commitments();
        restore_preconf_state(
            &j,
            &OnChainSource,
            &StubChain(OnChain::Unknown, NONCE_IS_GONE),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await
        .expect("restore");

        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e.clone()]);
        assert!(c.release_unless_committed(&e.hash), "an unjudgeable entry is not recorded landed");
    }

    /// The common case: the chain confirms the hash, so the commitment was kept
    /// and the line can go once the landing block is buried.
    #[tokio::test]
    async fn a_confirmed_commitment_is_dropped_once_it_is_deep_enough() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_commitments();
        restore_preconf_state(
            &j,
            &OnChainSource,
            &StubChain(OnChain::Yes { height: LANDED_AT }, NONCE_IS_GONE),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await
        .expect("restore");

        c.observe_persisted(LANDED_AT + crate::commitments::SEAL_DEPTH);
        assert!(c.release_unless_committed(&e.hash), "buried ⇒ the record may go");
        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty());
    }

    /// With the nonce used, no `OnChain` answer puts the entry in the fifo.
    #[tokio::test]
    async fn a_consumed_nonce_never_reaches_the_fifo() {
        for answer in [OnChain::Yes { height: LANDED_AT }, OnChain::No, OnChain::Unknown] {
            let (_dir, j) = fresh_journal().await;
            j.append_promised(&entry(1, 10)).await.unwrap();
            let fifo = Arc::new(PreconfTxSet::new(16));

            restore_preconf_state(
                &j,
                &OnChainSource,
                &StubChain(answer, NONCE_IS_GONE),
                &fifo,
                &empty_commitments(),
            )
            .await
            .expect("restore");

            assert!(fifo.snapshot().await.is_empty(), "{answer:?}");
        }
    }

    /// Restore walks every entry even when `recover_envelope` rejects them all,
    /// releases each rejected record, and the closing rotation drops its line.
    #[tokio::test]
    async fn restore_gives_up_every_entry_the_pool_refuses() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(4, 40)).await.unwrap();
        j.append_promised(&entry(5, 50)).await.unwrap();

        let mut pool = StubSource::new();
        pool.reject_add = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        let c = empty_commitments();
        restore_preconf_state(&j, &pool, &landed(), &fifo, &c).await.expect("restore");

        // Both calls return `Err`; restore still visits every entry.
        assert_eq!(pool.add_calls.lock().unwrap().len(), 2);
        assert!(fifo.snapshot().await.is_empty());
        assert!(!c.is_tracked(&entry(4, 40).hash), "a refused commitment is not kept in memory");
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty(), "nor on disk — the two halves go together");
    }

    /// An undecodable entry gets no record (there is no `(sender, nonce)` to
    /// claim), so the closing rotation drops its line instead of every restart
    /// replaying it.
    #[tokio::test]
    async fn restore_drops_an_entry_it_cannot_decode() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(6, 60);
        j.append_promised(&e).await.unwrap();

        let mut pool = StubSource::new();
        pool.undecodable = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        let c = empty_commitments();
        restore_preconf_state(&j, &pool, &landed(), &fifo, &c).await.expect("restore");

        assert!(!c.is_tracked(&e.hash), "an entry that cannot be named holds no record");
        assert!(fifo.snapshot().await.is_empty());
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty(), "and it leaves the file rather than replaying forever");
    }

    // ── spawn_rejournal_loop ────────────────────────────────────────

    #[tokio::test]
    async fn rejournal_loop_rotates_periodically_and_shuts_down() {
        let (_dir, j) = fresh_journal().await;
        let e_a = entry(1, 10);
        j.append_promised(&e_a).await.unwrap();
        let c = commitments_done_with(&[e_a.hash]);

        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // The first tick is skipped, so the first rotation is at ~30ms.
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_millis(30), shutdown_rx);

        tokio::time::sleep(Duration::from_millis(80)).await;

        let (after, _) = j.load().await.unwrap();
        assert!(
            after.is_empty(),
            "rotation must have dropped the untracked entry; instead got {after:?}"
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down within timeout")
            .expect("loop panicked");
    }

    /// With a 1-byte cap every append signals a rotation; the hour-long
    /// interval rules out the ticker, so the rotation seen is size-triggered.
    #[tokio::test]
    async fn size_trigger_rotates_dropping_untracked_without_periodic_tick() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();
        let c = commitments_done_with(&[e1.hash, e2.hash]);
        still_owed(&c, e3.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_secs(3600), shutdown_rx);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let (after, _) = j.load().await.unwrap();
        assert_eq!(
            after,
            vec![e3],
            "size-triggered rotation must drop untracked entries and keep the still-owed survivor"
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    /// `open` counts an existing file toward the cap, so the first append after
    /// a restart can already trigger a rotation.
    #[tokio::test]
    async fn open_with_max_size_seeds_counter_from_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        {
            let seed = PreconfJournal::open(&path, 0).await.unwrap();
            seed.append_promised(&entry(1, 10)).await.unwrap();
        }
        let existing = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(existing > 0);
        // Cap at the existing size: one more append crosses it.
        let j = PreconfJournal::open(&path, existing).await.unwrap();
        j.append_promised(&entry(2, 11)).await.unwrap();
        tokio::time::timeout(Duration::from_millis(50), j.rotate_notify.notified())
            .await
            .expect("size trigger must be armed after reopen seeding");
    }

    #[tokio::test]
    async fn rejournal_loop_shuts_down_promptly_without_rotating() {
        let (_dir, j) = fresh_journal().await;
        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // No interval rotation within the test; the final rotation on shutdown
        // still runs (see `graceful_shutdown_performs_final_rotate`).
        let handle = spawn_rejournal_loop(
            j.clone(),
            empty_commitments().into(),
            Duration::from_secs(60),
            shutdown_rx,
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    /// The final rotation on shutdown drops entries released since the last
    /// tick.
    #[tokio::test]
    async fn graceful_shutdown_performs_final_rotate() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(1, 100)).await.unwrap();
        let j = Arc::new(j);

        let c = commitments_with_high_watermark();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // No periodic tick fires during the test.
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(60), shutdown_rx);

        // Released after the loop starts, so only the final rotation can drop
        // it.
        finish_tracking(&c, TxHash::from([1; 32]));

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");

        let (after, _) = j.load().await.unwrap();
        assert!(
            after.is_empty(),
            "final rotate on shutdown must have dropped the untracked entry; got {after:?}"
        );
    }

    /// `run_rejournal_loop` returns the shutdown future's output, so a caller's
    /// shutdown guard lives until the final rotation is done.
    #[tokio::test]
    async fn run_rejournal_loop_returns_shutdown_output_after_final_rotate() {
        let (_dir, j) = fresh_journal().await;
        let j = Arc::new(j);

        // Stands in for a `GracefulShutdownGuard`.
        #[derive(Debug, PartialEq)]
        struct Sentinel(u32);

        let shutdown = async { Sentinel(7) };
        let out = run_rejournal_loop(
            j.clone(),
            empty_commitments().into(),
            Duration::from_secs(60),
            shutdown,
        )
        .await;
        assert_eq!(out, Sentinel(7));
    }

    // ── size-trigger rate limit / byte-counter invariants ──────────

    /// Two size triggers within `SIZE_ROTATE_MIN_GAP` (2s) rotate once.
    #[tokio::test]
    async fn size_trigger_second_rotation_rate_limited_within_min_gap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();
        let c = commitments_with_high_watermark();
        finish_tracking(&c, e1.hash);
        still_owed(&c, e2.hash);
        still_owed(&c, e3.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(3600), shutdown_rx);

        // The first size trigger rotates, dropping released e1.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone(), e3], "first size trigger drops released e1");

        // A second trigger ~150ms later is ignored, so released e2 stays.
        finish_tracking(&c, e2.hash);
        j.append_promised(&entry(4, 13)).await.unwrap(); // signals again
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_second, _) = j.load().await.unwrap();
        assert!(
            after_second.iter().any(|e| e.hash == e2.hash),
            "second trigger within min_gap must be dropped; e2 must survive, got {after_second:?}"
        );

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down")
            .expect("loop panicked");
    }

    /// A rotation whose survivors still exceed the cap does not trigger
    /// another: only appends signal, never `rotate` itself.
    #[tokio::test]
    async fn rotate_does_not_self_retrigger_when_survivors_exceed_cap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // 1-byte cap: even one survivor exceeds it.
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        let c = commitments_with_high_watermark();
        finish_tracking(&c, e1.hash);
        still_owed(&c, e2.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(3600), shutdown_rx);

        // The first trigger drops e1; e2 alone still exceeds the cap.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone()], "first rotate drops released e1, keeps e2");

        // Release e2 without appending, and wait past `min_gap` (2s).
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

    /// `size_bytes` equals the file length after appends and after a rotation;
    /// if it drifts, the size trigger fires late or never.
    #[tokio::test]
    async fn rotate_resets_size_counter_to_true_on_disk_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // A 0 cap keeps the trigger out of the way.
        let j = PreconfJournal::open(&path, 0).await.unwrap();

        let e1 = entry(1, 10);
        let e2 = entry(2, 11);
        let e3 = entry(3, 12);
        j.append_promised(&e1).await.unwrap();
        j.append_promised(&e2).await.unwrap();
        j.append_promised(&e3).await.unwrap();

        let on_disk = tokio::fs::metadata(&path).await.unwrap().len();
        assert_eq!(
            j.size_bytes.load(Ordering::Relaxed),
            on_disk,
            "counter must equal file size after appends"
        );

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

    /// Rotation leaves corrupt lines out of the new file and reports them in
    /// `bad_lines_skipped`.
    #[tokio::test]
    async fn rotate_drops_corrupt_lines_and_reports_count() {
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

        let (after, bad_after) = j.load().await.unwrap();
        assert_eq!(after, vec![entry(7, 70), entry(8, 80)]);
        assert_eq!(bad_after, 0, "corrupt line must not be carried into the new file");
    }

    /// The size trigger is inclusive (`new_size >= max_size`): a file exactly at
    /// the cap signals, one byte under does not.
    #[tokio::test]
    async fn size_trigger_fires_at_exact_boundary_not_below() {
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

        // Cap one byte above the line: the file stays one byte under it.
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

/// Property test: random append / untrack / rotate sequences against the real
/// journal and a reference model. After every step the file's contents and
/// `size_bytes` must match the model. The tracking set (`Op::Untrack`) is
/// owned by the test, as `retain` is the caller's rule.
#[cfg(test)]
mod proptest_journal_model {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    /// Small identity space so untrack/rotate collisions are frequent.
    const IDS: u8 = 6;

    fn hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    /// One fixed entry per byte, as a real hash always carries the same
    /// transaction.
    fn entry_for(byte: u8) -> JournalEntry {
        JournalEntry {
            hash: hash(byte),
            tx_rlp: Bytes::from(vec![byte; 4]),
            block_height: u64::from(byte),
            committed_at_ms: 1_000 + u64::from(byte),
            kind: ClaimKind::Promised,
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Append(u8),
        /// The caller stops tracking these hashes, so the next `rotate` drops
        /// them.
        Untrack(Vec<u8>),
        Rotate,
    }

    fn byte() -> impl Strategy<Value = u8> {
        0..IDS
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            byte().prop_map(Op::Append),
            prop::collection::vec(byte(), 0..4).prop_map(Op::Untrack),
            Just(Op::Rotate),
        ]
    }

    /// Reference model. `file` mirrors the on-disk line sequence (by identity
    /// byte); `untracked` mirrors what the caller's `retain` will reject.
    #[derive(Default)]
    struct Model {
        file: Vec<u8>,
        untracked: BTreeSet<u8>,
    }

    impl Model {
        fn apply(&mut self, op: &Op) {
            match op {
                Op::Append(b) => self.file.push(*b),
                Op::Untrack(bytes) => self.untracked.extend(bytes.iter().copied()),
                // `retain` is the only drop rule. An untracked hash stays
                // untracked, so re-appending it and rotating drops it again.
                Op::Rotate => {
                    let untracked = self.untracked.clone();
                    self.file.retain(|b| !untracked.contains(b));
                }
            }
        }
    }

    async fn fresh() -> (TempDir, PreconfJournal) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // Huge cap, so appends never signal a rotation during replay.
        let j = PreconfJournal::open(&path, u64::MAX).await.unwrap();
        (dir, j)
    }

    async fn run_and_check(ops: &[Op]) {
        let (dir, j) = fresh().await;
        let path = dir.path().join("preconf.jsonl");
        let mut model = Model::default();

        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Append(b) => j.append_promised(&entry_for(*b)).await.unwrap(),
                Op::Untrack(_) => {}
                Op::Rotate => {
                    // Stats computed from the pre-rotate model.
                    let before = model.file.len();
                    let kept_expected =
                        model.file.iter().filter(|b| !model.untracked.contains(b)).count();
                    let untracked = model.untracked.clone();
                    let stats =
                        j.rotate(|h| !untracked.iter().any(|b| hash(*b) == *h)).await.unwrap();

                    assert_eq!(stats.kept, kept_expected, "step {i}: rotate kept mismatch");
                    assert_eq!(
                        stats.dropped,
                        before - kept_expected,
                        "step {i}: rotate dropped mismatch"
                    );
                }
            }
            model.apply(op);

            // (A) On-disk contents match the model's file, in order.
            let (loaded, bad) = j.load().await.unwrap();
            assert_eq!(bad, 0, "step {i}: unexpected corrupt lines");
            let expected: Vec<JournalEntry> = model.file.iter().map(|b| entry_for(*b)).collect();
            assert_eq!(loaded, expected, "step {i}: journal file diverged after {op:?}");

            // (B) `size_bytes` equals the true on-disk file size.
            let disk = tokio::fs::metadata(&path).await.unwrap().len();
            assert_eq!(
                j.size_bytes.load(Ordering::Relaxed),
                disk,
                "step {i}: size_bytes drifted from disk after {op:?}"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Any append / untrack / rotate sequence keeps the file and
        /// `size_bytes` in step with the model; rotate drops exactly what
        /// `retain` rejects.
        #[test]
        fn journal_matches_reference_model(ops in prop::collection::vec(op(), 1..24)) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_and_check(&ops));
        }
    }
}
