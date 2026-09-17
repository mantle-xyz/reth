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
//! **No in-memory view of which commitments are still owed lives here.**
//! [`PreconfClassifier`] is the single owner of "this commitment is over",
//! because the only notion this layer could form — *canonical once* — is one a
//! reorg can undo. Rotation receives that decision as the `retain` predicate
//! [`PreconfJournal::rotate`] takes, and the pool listener asks
//! `PreconfClassifier::is_promised` directly. This is the sole statement of that
//! division; the rest of the file assumes it.
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
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxHash};
use parking_lot::Mutex as SyncMutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
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

/// Current wall-clock milliseconds since the Unix epoch, for stamping
/// [`JournalEntry::committed_at_ms`]. Falls back to `0` if the system clock is
/// set before 1970, which is better read as "unset" than worth a panic on a
/// path that is only telemetry.
pub(crate) fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl JournalEntry {
    /// The record for a transaction just executed into the block being built.
    ///
    /// One definition for both writers: preconf commitments are recorded from
    /// the apply, pool transactions from the slice that carries them, and what
    /// a record *is* must not depend on which of the two wrote it.
    pub(crate) fn for_executed(hash: TxHash, tx: &impl Encodable2718, block_height: u64) -> Self {
        Self {
            hash,
            tx_rlp: tx.encoded_2718().into(),
            block_height,
            committed_at_ms: now_unix_ms(),
        }
    }
}

/// How many un-written records the retry buffer holds before the oldest start
/// falling out — thirty blocks' worth.
///
/// Thirty blocks is one rotation interval (the 60s default over 2s blocks),
/// which is as far back as a record can still matter: past it rotation has
/// already dropped the ones no longer owed. Fifteen hundred a block is what
/// 30M of gas holds when it is all simple transfers.
const PENDING_CAPACITY: usize = 30 * 1_500;

/// Backstop on what the retry buffer may hold, in bytes.
///
/// [`PENDING_CAPACITY`] is the limit to reason with — it is expressed in the
/// units the rest of the subsystem uses. It does not bound memory, though: a
/// record carries its transaction's encoding, and at the pool's 128 KiB ceiling
/// per transaction a full buffer would be gigabytes rather than the tens of
/// megabytes ordinary traffic produces.
///
/// This is the fuse for that, not a second budget. Reaching it takes hours of
/// uninterrupted maximum-size transactions with the disk refusing writes
/// throughout; ordinary traffic never comes close, so which limit bites stays
/// predictable.
const PENDING_MAX_BYTES: usize = 1024 * 1024 * 1024;

/// Drop the oldest held records until one of `incoming` bytes fits, returning
/// how many went. `bytes` is the running size of `pending` and is adjusted to
/// match.
///
/// Split out from [`PreconfJournal::buffer`] so the rule can be exercised at a
/// budget a test can afford: [`PENDING_MAX_BYTES`] is a gigabyte, and reaching
/// it for real would mean allocating one.
fn evict_to_fit(
    pending: &mut VecDeque<Vec<u8>>,
    bytes: &mut usize,
    incoming: usize,
    max_count: usize,
    max_bytes: usize,
) -> u64 {
    let mut dropped = 0;
    // `while` for the byte fuse, because one arriving record can be worth many
    // of the ones it displaces. Never evicts down to empty: a record bigger
    // than the whole budget is still worth more held than dropped.
    while pending.len() >= max_count || (*bytes + incoming > max_bytes && !pending.is_empty()) {
        let evicted = pending.pop_front().expect("non-empty");
        *bytes -= evicted.len();
        dropped += 1;
    }
    dropped
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
/// It holds **no** view of which preconf commitments are still owed (see the
/// module docs); that decision reaches rotation through the `retain` predicate
/// [`Self::rotate`] takes.
///
/// It does hold an index of pool transactions announced in a flashblock and not
/// yet seen on chain (see [`crate::unlanded::Unlanded`]) — but the judgement is
/// still the caller's: the chain nonces the sweep runs on are read by the build
/// and handed in. This type never reads the chain.
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
    /// Records a write could not place on disk, waiting for the next one.
    ///
    /// Bounded by [`PENDING_CAPACITY`], and by [`PENDING_MAX_BYTES`] as a fuse;
    /// over either the oldest are dropped, because they are the ones whose block
    /// has most likely sealed already — a record only matters until its block is
    /// canonical.
    ///
    /// Distinct from the eviction `rotate` performs: that one decides which
    /// *durable* records are still owed and is the caller's rule (`retain`).
    /// This one is about records that never reached the disk at all, and is the
    /// price of not blocking the build loop on a failing disk.
    pending: SyncMutex<VecDeque<Vec<u8>>>,
    /// Running size of `pending`, so the byte fuse costs no walk.
    pending_bytes: AtomicUsize,
    /// What this node announced in a flashblock and has not yet seen on chain.
    /// See [`crate::unlanded::Unlanded`] — the journal owns it because every
    /// component that needs it already holds the journal.
    unlanded: crate::unlanded::Unlanded,
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
        let journal = Self {
            path,
            writer: Mutex::new(file),
            max_size,
            size_bytes: AtomicU64::new(init_size),
            rotate_notify: Notify::new(),
            pending: SyncMutex::new(VecDeque::new()),
            pending_bytes: AtomicUsize::new(0),
            unlanded: crate::unlanded::Unlanded::new(),
        };
        // Nothing in memory to seed from the file: recognising a post-restart
        // commitment as ours is `restore_preconf_state`'s job.
        Ok(journal)
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
    ///
    /// A batch of one — the size accounting, the rotation threshold and the
    /// lock discipline have to be identical for both, and writing them twice
    /// is how they stop being.
    pub async fn append_promised(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        self.append_batch(std::slice::from_ref(entry)).await
    }

    /// Record what a slice announced. See [`Unlanded::note_announced`].
    ///
    /// [`Unlanded::note_announced`]: crate::unlanded::Unlanded::note_announced
    pub fn note_announced(&self, height: u64, txs: &[crate::unlanded::Announced]) {
        self.unlanded.note_announced(height, txs);
    }

    /// Record the block this build sealed. See [`Unlanded::note_sealed`].
    ///
    /// [`Unlanded::note_sealed`]: crate::unlanded::Unlanded::note_sealed
    pub fn note_sealed(&self, block: B256) {
        self.unlanded.note_sealed(block);
    }

    /// Whether the chain built on the block this node last sealed.
    /// See [`Unlanded::parent_is_ours`].
    ///
    /// [`Unlanded::parent_is_ours`]: crate::unlanded::Unlanded::parent_is_ours
    pub fn parent_is_ours(&self, parent_hash: B256) -> bool {
        self.unlanded.parent_is_ours(parent_hash)
    }

    /// Drop everything staged. See [`Unlanded::clear`].
    ///
    /// [`Unlanded::clear`]: crate::unlanded::Unlanded::clear
    pub fn clear_unlanded(&self) {
        self.unlanded.clear();
    }

    /// Whether anything is staged. See [`Unlanded::is_empty`].
    ///
    /// [`Unlanded::is_empty`]: crate::unlanded::Unlanded::is_empty
    pub fn unlanded_is_empty(&self) -> bool {
        self.unlanded.is_empty()
    }

    /// Every distinct staged sender. See [`Unlanded::senders`].
    ///
    /// [`Unlanded::senders`]: crate::unlanded::Unlanded::senders
    pub fn unlanded_senders(&self) -> HashSet<Address> {
        self.unlanded.senders()
    }

    /// Take what the chain has not passed. See [`Unlanded::take_unlanded`].
    ///
    /// [`Unlanded::take_unlanded`]: crate::unlanded::Unlanded::take_unlanded
    pub fn take_unlanded(&self, heads: &HashMap<Address, u64>) -> Vec<crate::unlanded::UnlandedTx> {
        self.unlanded.take_unlanded(heads)
    }

    /// Append many records under one lock, with a single write and a single
    /// flush.
    ///
    /// `flush`, not `sync_all`: the bytes leave the runtime's buffer before this
    /// returns, but power-loss durability would cost a per-call fsync.
    ///
    /// The batching is what keeps a slice affordable: it can carry a thousand
    /// transactions, and taking the writer lock once per transaction would hold
    /// up the preconf path that appends through the same lock. An empty batch
    /// does no IO.
    ///
    /// This is the one place the write mechanics live — [`Self::append_promised`]
    /// is a batch of one.
    ///
    /// All-or-nothing on encode: every record is serialised before anything is
    /// written, so a record `serde_json` cannot encode leaves the file
    /// untouched rather than half a batch on disk.
    ///
    /// An **IO** failure is different: the records are kept for the next write
    /// and `Err` is returned for the caller to log, not to act on. Callers must
    /// not resend them: the next write carries them, and a resend would put the
    /// same record on disk twice once one succeeds.
    pub async fn append_batch(&self, entries: &[JournalEntry]) -> Result<(), JournalError> {
        // Encoded up front, and separately from the write: a record `serde_json`
        // cannot encode will not encode on the next attempt either, so it fails
        // outright instead of joining the retry buffer and being carried
        // forever. Nothing is written if any of them fails.
        let mut lines = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut line = serde_json::to_vec(entry).map_err(JournalError::Encode)?;
            line.push(b'\n');
            lines.push(line);
        }
        self.write_lines(lines).await
    }

    /// Write `lines`, preceded by anything an earlier attempt could not place.
    ///
    /// On IO failure everything goes to the retry buffer and the error is
    /// returned for the caller to log. The caller is expected to carry on
    /// regardless: journaling is a side path, and the transactions it describes
    /// are in the block either way — the record only matters if the process
    /// dies before that block is canonical.
    async fn write_lines(&self, mut lines: Vec<Vec<u8>>) -> Result<(), JournalError> {
        if lines.is_empty() && self.pending.lock().is_empty() {
            return Ok(());
        }
        // The writer lock is taken first so a concurrent append cannot slip
        // between draining the buffer and writing it, which would put the older
        // records after the newer ones in the file.
        let mut writer = self.writer.lock().await;
        let mut retried: Vec<Vec<u8>> = {
            let mut pending = self.pending.lock();
            self.pending_bytes.store(0, Ordering::Relaxed);
            pending.drain(..).collect()
        };

        let mut buf = Vec::new();
        if !retried.is_empty() {
            // The write that failed may have stopped mid-line. A newline closes
            // it so this attempt does not fuse onto its tail and cost both
            // records; `load` skips the blank line it leaves behind.
            buf.push(b'\n');
        }
        for line in retried.iter().chain(lines.iter()) {
            buf.extend_from_slice(line);
        }
        let len = buf.len() as u64;

        if let Err(e) = writer.write_all(&buf).await.and(writer.flush().await) {
            drop(writer);
            retried.append(&mut lines);
            self.buffer(retried);
            return Err(JournalError::Io(e));
        }
        let new_size = self.size_bytes.fetch_add(len, Ordering::Relaxed) + len;
        drop(writer);

        metrics::gauge!("preconf.journal.pending_entries").set(0.0);
        metrics::gauge!("preconf.journal.size_bytes").set(new_size as f64);
        if new_size >= self.max_size {
            self.rotate_notify.notify_one();
        }
        Ok(())
    }

    /// Hold `lines` for the next write, dropping the oldest once full.
    fn buffer(&self, lines: Vec<Vec<u8>>) {
        let mut pending = self.pending.lock();
        let mut bytes = self.pending_bytes.load(Ordering::Relaxed);
        let mut dropped = 0u64;
        for line in lines {
            dropped += evict_to_fit(
                &mut pending,
                &mut bytes,
                line.len(),
                PENDING_CAPACITY,
                PENDING_MAX_BYTES,
            );
            bytes += line.len();
            pending.push_back(line);
        }
        self.pending_bytes.store(bytes, Ordering::Relaxed);
        if dropped > 0 {
            metrics::counter!("preconf.journal.dropped_entries_total").increment(dropped);
        }
        metrics::gauge!("preconf.journal.pending_entries").set(pending.len() as f64);
    }

    /// How many records are waiting for a write to succeed.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
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
        Self::parse_entries(BufReader::new(file)).await
    }

    /// Entries from the first `limit` bytes of the file.
    ///
    /// Rotation compacts the file as it stood when the pass began, not as it
    /// stands when the pass ends, so it reads to a byte offset rather than to
    /// end of file. `limit` comes from the size counter, which only ever
    /// advances by whole writes, so the cut cannot land mid-record.
    async fn load_upto(&self, limit: u64) -> Result<(Vec<JournalEntry>, usize), JournalError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(JournalError::Io(e)),
        };
        Self::parse_entries(BufReader::new(file.take(limit))).await
    }

    /// The raw bytes the file has grown by past `from`.
    ///
    /// Copied verbatim rather than parsed and re-encoded: these records arrived
    /// after the compaction pass took its snapshot, so they belong to the
    /// generation being started rather than the one being compacted. Verbatim
    /// also keeps `retain` — a caller-supplied predicate of unknown cost — off
    /// the one stretch of rotation that holds the writer lock.
    ///
    /// A write that failed part-way leaves a partial line, which is carried
    /// across like anything else; the next append closes it with a newline and
    /// [`Self::load`] skips it.
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

    /// Parse newline-delimited entries out of `reader`, skipping the ones that
    /// will not decode and returning how many those were.
    async fn parse_entries<R>(reader: R) -> Result<(Vec<JournalEntry>, usize), JournalError>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
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
    /// The caller is expected to be the rotation loop, not the hot RPC path.
    ///
    /// The read-filter-rewrite pass runs with **no lock held**, so appends
    /// continue into the live file throughout it. Only the tail end takes the
    /// writer lock, and what it does there is bounded by one pass's worth of
    /// appends rather than by the size of the file: splice, rename, re-open.
    /// This matters because a slice is journalled before it is broadcast, so an
    /// append that waits for a multi-megabyte rewrite is a broadcast that
    /// waits for it too.
    pub async fn rotate(
        &self,
        retain: impl Fn(&TxHash) -> bool,
    ) -> Result<RotateStats, JournalError> {
        // Records `preconf.journal.rotate_duration_ms` on every exit path
        // (including the `?` early returns below).
        let _timer = RotateTimer(std::time::Instant::now());

        // How much of the file this pass is answerable for. Read under the
        // writer lock so it cannot be sampled part-way through an append, and
        // taken from the size counter rather than from a `stat` because the
        // counter only advances by whole writes — which is what makes it safe
        // to cut the file here.
        let compacting_upto = {
            let _writer = self.writer.lock().await;
            self.size_bytes.load(Ordering::Relaxed)
        };

        let (entries, bad_before) = self.load_upto(compacting_upto).await?;

        // Which records may go is the caller's decision, not this type's, and it
        // is the *only* rule here: `retain` asks the classifier whether the
        // commitment is still tracked. This type deliberately owns no eviction
        // policy of its own — a second, journal-local rule could drop a record
        // the classifier still holds, which is precisely the divergence the two
        // halves of commitment tracking must not have. Evaluated once per record,
        // so a concurrent update lands in the *next* rotation rather than half of
        // this one.
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

        // From here to the end of the function the writer lock is held, and
        // appends wait. Everything expensive is already done.
        let mut writer = self.writer.lock().await;
        // Declared after the guard so it is dropped before it: what this
        // records is the span an append could have been waiting on, measured
        // from acquiring the lock to just short of releasing it.
        let locked = LockedTimer(std::time::Instant::now());

        // Splice on whatever landed while the pass was running. Without this the
        // swap below would discard those records, and the size counter would
        // describe a file that never existed.
        let carried = self.bytes_after(compacting_upto).await?;
        tmp.write_all(&carried).await?;
        tmp.flush().await?;
        drop(tmp);

        // Atomic swap: rename into place, then re-open the writer against the
        // new inode.
        tokio::fs::rename(&tmp_path, &self.path).await?;
        *writer = OpenOptions::new().create(true).append(true).open(&self.path).await?;
        // Reset the byte counter to the new file's true size while still holding
        // the writer lock, so it stays consistent with any append that
        // serialises before or after this swap.
        let new_size = kept_bytes + carried.len() as u64;
        self.size_bytes.store(new_size, Ordering::Relaxed);
        metrics::gauge!("preconf.journal.size_bytes").set(new_size as f64);

        Ok(RotateStats {
            kept,
            dropped,
            locked_for: locked.0.elapsed(),
            carried_bytes: carried.len() as u64,
            bad_lines_skipped: bad_before,
        })
    }
}

/// Telemetry-friendly summary of a single [`PreconfJournal::rotate`]
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateStats {
    /// Entries the compaction pass carried over — every one of them accepted by
    /// `retain`. Not the whole of the rotated file: see
    /// [`carried_bytes`](Self::carried_bytes).
    pub kept: usize,
    /// Entries left out of the new file — every one of them refused by `retain`.
    pub dropped: usize,
    /// How long the writer lock was held, and so how long an append could have
    /// been waiting on this rotation.
    ///
    /// Reported rather than only measured into
    /// `preconf.journal.rotate_locked_ms` because the metric is emitted from a
    /// drop guard, and nothing that reads the source can tell whether the guard
    /// is still being constructed. As a returned value it is assertable.
    pub locked_for: Duration,
    /// Bytes appended while the pass was running and spliced onto the end of
    /// the rotated file.
    ///
    /// These arrived after the pass took its snapshot, so `retain` was never
    /// asked about them and they are counted in neither `kept` nor `dropped`.
    /// They face the next rotation instead. Reported so `kept` and the rotated
    /// file's line count can be reconciled.
    pub carried_bytes: u64,
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

/// Records `preconf.journal.rotate_locked_ms` on drop: the part of a rotation
/// that holds the writer lock, and so the only part of it an append can wait
/// for. [`RotateTimer`] covers the whole pass, most of which runs unlocked, so
/// it cannot answer that question on its own.
///
/// Fractional milliseconds, unlike the whole-pass timer: this span is expected
/// to be under a millisecond, and at integer resolution "fast" and "did not
/// happen" would both read as zero.
struct LockedTimer(std::time::Instant);

impl Drop for LockedTimer {
    fn drop(&mut self) {
        metrics::histogram!("preconf.journal.rotate_locked_ms")
            .record(self.0.elapsed().as_secs_f64() * 1000.0);
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
    /// [`PreconfClassifier::mark_promised`](crate::PreconfClassifier::mark_promised)
    /// for why that has to happen before `add_envelope`.
    ///
    /// `None` for anything that does not decode or whose signature does not
    /// recover — the entry will fail `add_envelope` for the same reason a moment
    /// later, which is where it gets logged.
    ///
    /// Decodes the same bytes `add_envelope` decodes again: one extra ec-recover
    /// per journal entry, once per process start.
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
/// Separate from [`RestorePool`] because the pool has no way to answer it — see
/// [`RestoreSkip::NonceConsumed`].
pub trait CommitmentChainView: Send + Sync {
    /// Is `hash` on the canonical chain?
    fn commitment_on_chain(&self, hash: &TxHash) -> OnChain;
}

/// Why a journal entry was not re-admitted to the pool.
///
/// Deliberately says only what the **pool** can distinguish. Which of the three
/// things a consumed nonce means — commitment honoured, nonce stolen, or
/// unknowable — is resolved by [`restore_preconf_state`] with a
/// [`CommitmentChainView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSkip {
    /// The sender's account nonce has moved past this transaction — by **some** transaction, not
    /// necessarily this one, so it cannot mean the commitment was kept: `validate_sender_nonce`
    /// compares the *account's* nonce, never the hash, so a different tx yields the same error.
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
/// says. Cold start runs before restore, so without step 1 a restored tx would
/// be classified `Eligible` and re-judged against the *current*
/// `preconf_max_gas_per_tx`: lower that flag, restart, and `add_envelope` starts
/// rejecting commitments a client was already told had succeeded — silently,
/// since restore skips and logs.
///
/// [`Verdict::Promised`] makes the exemption explicit: `admit_and_claim` is
/// get-or-insert, so the verdict installed here survives step 2, and the
/// validator returns a promised transaction straight to its inner validator —
/// ahead of the ceiling and every other preconf gate. The pre-pass loop carries
/// the rest of the argument.
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
    // No prune before the replay pass: the only eviction rule is "the classifier
    // no longer tracks this", and the classifier is empty until the loop below
    // populates it — pruning here would discard every commitment we owe on the
    // very restart meant to honour them. The prune happens once at the end
    // instead, against the records this pass just established.
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

    // Step 1 — a pre-pass: every hash becomes a live commitment owning its nonce before any is
    // admitted. Two reasons it is not folded into the loop below.
    //
    // **The verdict.** `add_envelope` hands the tx to the pool, whose validator classifies
    // whatever is not yet marked. Two journal entries sharing a `(sender, nonce)` are enough to
    // matter: admitting the first would classify the *second* against the current allowlists and
    // put it through the replacement guard instead of treating it as the commitment it is.
    //
    // **The slot.** `mark_promised` both records "a receipt for this went out in a previous
    // process" and claims the `(sender, nonce)`, from what `recover_slot` hands it. Back-filling
    // the claim at validator time would leave the nonce reading free in between, so a same-nonce
    // transaction admitted there would take a nonce the client was already told it had. Startup
    // ordering closes that window today (`cli::node` runs restore before the pool loader, the RPC
    // server and the network); claiming here makes it structural instead.
    for entry in &entries {
        match pool.recover_slot(&entry.tx_rlp) {
            Some((from, nonce)) => {
                if let Err(owner) =
                    classifier.mark_promised(entry.hash, &from, nonce, entry.block_height)
                {
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
            // Undecodable envelope. **No record is written**, deliberately:
            // `recover_slot` shares its first step with `add_envelope`
            // (`recover_raw_transaction::<PoolPooledTx<P>>`), so this entry is
            // about to be rejected there too — it will never enter the pool or
            // get a fifo entry, and a `Promised` record would break the rule that
            // a promise names the `(sender, nonce)` it was issued against.
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
                        // Starting the clock deliberately does not release the
                        // tracking outright. Landing is revocable; if the block
                        // is shallow, a reorg right after startup must still
                        // find the commitment holding its nonce.
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
                        // Deliberately still retained: if the transaction that
                        // took the nonce is itself reverted, this commitment
                        // becomes applicable again, so there is nothing to
                        // forget yet. The cost is that the entry survives
                        // rotation until then, and every restart repeats this
                        // warning — which is the right noise for a promise we
                        // could not keep.
                        warn!(
                            target: "mantle::preconf::journal",
                            hash = ?entry.hash,
                            reason,
                            "restored tx is NOT on chain but its nonce was consumed by another \
                             transaction; whatever was announced about it will not happen"
                        );
                        nonce_taken += 1;
                    }
                    OnChain::Unknown => {
                        // Retained too: an entry we cannot judge must be
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
                // Give the record up here rather than leaving it to expire.
                // Nothing else will ever revisit this hash — it has no fifo
                // entry and will never be observed on chain — so keeping the
                // promise record would pin the nonce and hold the journal line
                // for a commitment this process has already abandoned. The two
                // halves go together or the journal outlives what tracks it.
                classifier.release_unless_committed(&entry.hash);
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

    // `nonce_taken` is the one worth watching: a transaction this node told the
    // outside about, whose nonce something else has since taken. Published as a
    // counter as well, because a log line that only appears at startup is easy
    // to miss.
    //
    // It counts two things that do not weigh the same, and cannot tell them
    // apart. A preconf commitment reaching here means a client holds a
    // synchronous receipt for a transaction that can never land. An ordinary
    // pool transaction reaching here means someone replaced their own pending
    // transaction after a slice had shown it — routine, and the sort of thing
    // that will bury the first case if an alert is hung on this number as it
    // stands.
    //
    // Telling them apart needs the entry to say which it is, and nothing here
    // can work it out: the classifier is empty at this point — this very pass
    // is what fills it — and the record carries no marker. A field would do it,
    // and old files would need no guess, since every record written before pool
    // transactions were journaled is a commitment. Left for the pass that
    // settles the metrics, which is where the alert thresholds get decided and
    // where it will be clear whether this wants a second counter or a label.
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

    // Prune now, against the records this pass just established: entries whose
    // commitment was kept and buried, and the ones given up above, are gone from
    // the classifier and so leave the file here. Doing it after the replay rather
    // than before is what lets the eviction rule be the classifier's alone —
    // before it, every record would read as untracked. Best-effort; the next
    // rotation retries.
    if let Err(e) = journal.rotate(|hash| classifier.is_tracked(hash)).await {
        warn!(
            target: "mantle::preconf::journal",
            ?e,
            "post-restore rotate failed; the file keeps entries the classifier has dropped"
        );
    }
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
/// attempted so entries that `retain` stopped accepting since the last
/// tick leave the on-disk file before the process exits. Callers that
/// hold a reth `GracefulShutdownGuard` must keep it alive across this
/// call so the `TaskManager` waits for the final rotate.
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
    // The one eviction rule: a record may go once the classifier has stopped
    // tracking its commitment. That happens when the commitment was kept and
    // buried `SEAL_DEPTH` persisted blocks deep, when a promise can no longer
    // reach the block it was made for, or when a build gave it up. Everything
    // else — never landed but still reachable, landed but shallow, landed and
    // then reorged out — is still owed and stays in the file.
    let retain = |hash: &TxHash| classifier.is_tracked(hash);
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

    // Final rotate on shutdown — flush records `retain` has stopped accepting
    // since the last tick. `signal_output` is held alive across this await so
    // callers passing a graceful-shutdown guard as `T` keep their runtime's
    // shutdown latch open until the final on-disk write completes. Failures are
    // logged; we do not surface them because the process is going away anyway.
    log_rotate(journal.rotate(&retain).await, "shutdown");

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

    /// A slice's transactions are written together. One write and one flush
    /// rather than one per transaction: a slice can carry a thousand of them,
    /// and the writer lock it takes is the one the RPC path appends through.
    #[tokio::test]
    async fn append_batch_writes_every_entry_in_order() {
        let (_dir, j) = fresh_journal().await;
        let batch = [entry(1, 10), entry(2, 10), entry(3, 10)];

        j.append_batch(&batch).await.unwrap();

        let (loaded, bad) = j.load().await.unwrap();
        assert_eq!(loaded, batch.to_vec());
        assert_eq!(bad, 0);
    }

    // Deliberately untested: that `append_batch` flushes before returning.
    // Removing the `flush` does not turn any test here red — tokio's `File`
    // hands the write to a blocking thread, which has normally finished by the
    // time a second handle reads the path, so such a test passes on timing
    // rather than on the guarantee. `append_promised` has the same gap.

    /// A slice that executed nothing is the common case once the pool is
    /// drained; it must not cost a write.
    #[tokio::test]
    async fn append_batch_of_nothing_writes_nothing() {
        let (_dir, j) = fresh_journal().await;

        j.append_batch(&[]).await.unwrap();

        let (loaded, _) = j.load().await.unwrap();
        assert!(loaded.is_empty());
    }

    /// A journal whose file handle cannot be written to, for exercising the
    /// failure path with a real IO error rather than a stand-in.
    async fn read_only_journal() -> (TempDir, PreconfJournal) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // Create the file, then hold it open read-only: `write_all` on it fails
        // with EBADF, which is as close to a disk refusing a write as a test
        // gets without a filesystem fixture.
        let j = PreconfJournal::open(&path, u64::MAX).await.unwrap();
        let ro = OpenOptions::new().read(true).open(&path).await.unwrap();
        *j.writer.lock().await = ro;
        (dir, j)
    }

    /// A write that fails keeps its records for the next attempt.
    ///
    /// Losing them outright is what the journal exists to prevent, and the
    /// caller has moved on — its cursor advanced the moment it handed them over.
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
        assert_eq!(bad, 0);
        assert_eq!(j.pending_len(), 0, "and the buffer is empty again");
    }

    /// A write that stopped mid-line does not cost the record that follows it.
    ///
    /// `write_all` can fail partway, leaving a line without its newline. The
    /// retry appends behind it, and without something to close the line the two
    /// fuse into one unparseable record — losing the held one as well as the
    /// truncated one.
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

    /// The buffer stops at its capacity, and the records it drops are the ones
    /// offered first — theirs are the blocks most likely sealed by now.
    ///
    /// Offered as one batch rather than one call each: eviction is what is under
    /// test, and forty-five thousand failed writes would be a slow way to reach
    /// it.
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

    /// Fill a buffer through the real eviction rule at a budget a test can
    /// afford, and report what survived.
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

    /// The byte fuse bites when the record count never would.
    ///
    /// The count is the limit to reason with, but it says nothing about memory:
    /// at the pool's 128 KiB per transaction a full buffer is gigabytes.
    #[test]
    fn the_byte_fuse_evicts_where_the_count_limit_would_not() {
        let (pending, dropped) = fill(&[100, 100, 100, 100], 1_000, 300);

        assert_eq!(pending.len(), 3, "only three hundred bytes may be held");
        assert_eq!(dropped, 1);
        assert_eq!(pending.front().expect("non-empty")[0], 1, "and the oldest is what went");
    }

    /// The count still bites first at the sizes ordinary traffic produces —
    /// which is what makes it the limit worth reasoning about.
    #[test]
    fn the_count_limit_bites_first_at_ordinary_sizes() {
        let (pending, dropped) = fill(&[100; 6], 4, 1_000_000);

        assert_eq!(pending.len(), 4);
        assert_eq!(dropped, 2);
    }

    /// A record larger than the whole budget is still held: evicting to empty
    /// and dropping it too would keep nothing at all.
    #[test]
    fn a_record_larger_than_the_budget_is_still_held() {
        let (pending, _) = fill(&[5_000], 1_000, 300);

        assert_eq!(pending.len(), 1);
    }

    /// A batch counts toward the size cap exactly as the same entries appended
    /// one at a time would — otherwise a slicing node would never rotate.
    #[tokio::test]
    async fn append_batch_arms_size_triggered_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        // 1-byte cap: any write at all crosses it.
        let j = Arc::new(PreconfJournal::open(&path, 1).await.unwrap());
        let kept = entry(3, 12);
        j.append_batch(&[entry(1, 10), entry(2, 11), kept.clone()]).await.unwrap();
        let c = classifier_done_with(&[TxHash::from([1; 32]), TxHash::from([2; 32])]);
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
        }
    }

    /// Age is not an eviction rule here. An entry stamped at the epoch is kept
    /// as long as the classifier still tracks its commitment — the journal has
    /// no clock of its own to overrule that with.
    #[tokio::test]
    async fn rotate_keeps_an_ancient_entry_the_classifier_still_tracks() {
        let (_dir, j) = fresh_journal().await;
        let ancient = entry_at(1, 10, 1_000);
        j.append_promised(&ancient).await.unwrap();

        let c = empty_classifier();
        c.mark_promised(ancient.hash, &Address::from([0xEE; 20]), 0, ancient.block_height).unwrap();

        let stats = j.rotate(|h| c.is_tracked(h)).await.unwrap();
        assert_eq!(stats.dropped, 0);
        let (survivors, _) = j.load().await.unwrap();
        assert_eq!(survivors, vec![ancient], "still tracked ⇒ still owed ⇒ kept");
    }

    /// The entry leaves the file exactly when the classifier stops tracking its
    /// commitment — nothing else. When that happens (buried `SEAL_DEPTH` deep, a
    /// promise out of reach, a build giving up) is the classifier's question, and
    /// is pinned by its own `sweep` tests.
    #[tokio::test]
    async fn rotate_drops_an_entry_the_classifier_gave_up() {
        let (_dir, j) = fresh_journal().await;
        let e = entry_at(1, 10, 1_000);
        j.append_promised(&e).await.unwrap();

        let c = empty_classifier();
        c.mark_promised(e.hash, &Address::from([0xEE; 20]), 0, e.block_height).unwrap();
        assert_eq!(j.rotate(|h| c.is_tracked(h)).await.unwrap().dropped, 0);

        assert!(c.release_unless_committed(&e.hash));
        let stats = j.rotate(|h| c.is_tracked(h)).await.unwrap();
        assert_eq!(stats.dropped, 1);
        let (survivors, _) = j.load().await.unwrap();
        assert!(survivors.is_empty(), "the journal follows the classifier");
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
            unlanded: crate::unlanded::Unlanded::new(),
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

    /// Rotation keeps whatever `retain` says to keep. Production passes "the
    /// classifier is still tracking this".
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

    /// An append racing a rotate must not be lost. `retain` accepts everything
    /// here, so any absence is the race, not the retention rule.
    ///
    /// Two failure modes: an entry landing in rotate's snapshot → rename window
    /// can be dropped outright, and even when it survives on disk, a counter
    /// reset to the compaction pass's byte total alone would leave it short of
    /// the real file and silently mistune the size-rotation trigger.
    ///
    /// The appends here race the rotation rather than being placed inside it, so
    /// which of them land in that window varies between runs; the deterministic
    /// placement is
    /// [`an_entry_appended_during_the_compaction_pass_survives_the_swap`].
    #[tokio::test]
    async fn rotate_does_not_lose_concurrent_appends() {
        use std::sync::Arc;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("preconf.jsonl");
        let j = Arc::new(PreconfJournal::open(&path, 0).await.unwrap());

        // Survivors widen rotate's tmp-write window, so the race has something
        // to land in.
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

    /// Hold a rotation open inside its compaction pass.
    ///
    /// `retain` is called once per record being compacted, which makes it the
    /// one place a test can stand in the middle of a rotation and ask what else
    /// can still happen. Returns the journal, a receiver that fires when the
    /// pass has begun, the handle to release it, and the rotation's own handle.
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
                    // Blocks the pass until the test has its answer. A blocking
                    // recv because `retain` is synchronous.
                    release_rx.recv().expect("test releases the pass");
                }
                true
            })
            .await
            .expect("rotate")
        });
        (compacting_rx, release_tx, rotate)
    }

    /// What the locked span reports must be the swap, not the whole rotation —
    /// otherwise `rotate_locked_ms` says appends waited for the rewrite, which
    /// is the number the split was made to bring down.
    ///
    /// The pass is held open for a known stretch and the span is required to be
    /// shorter than that stretch. A timer covering the whole pass would report
    /// at least the hold; the true value is microseconds, so the margin is
    /// three orders of magnitude rather than a threshold to tune.
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

    /// The compaction pass reads to the offset it snapshotted, not to end of
    /// file. Reading further would put records into the rewritten file that the
    /// splice then appends a second time.
    ///
    /// Pinned on `load_upto` directly rather than by racing a rotation: the
    /// window between taking the snapshot and finishing the read has no hook a
    /// test could stand in, so a concurrent append can only land inside it by
    /// luck.
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

    /// Compaction reads and rewrites the whole file — megabytes of it once a
    /// block carries a thousand transactions. An append must not queue behind
    /// that: a slice is journalled before it is broadcast, so an append that
    /// waits is a broadcast that waits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_append_does_not_wait_for_the_compaction_pass() {
        let dir = TempDir::new().unwrap();
        let j = Arc::new(PreconfJournal::open(dir.path().join("preconf.jsonl"), 0).await.unwrap());
        for i in 0..5u8 {
            j.append_promised(&entry(i, u64::from(i))).await.unwrap();
        }

        let (mut compacting, release, rotate) = rotation_held_open(Arc::clone(&j));
        compacting.recv().await.expect("compaction pass began");

        // Generous: the question is whether the append is blocked at all, not
        // how quickly it finishes.
        let appended =
            tokio::time::timeout(Duration::from_secs(5), j.append_promised(&entry(9, 9))).await;

        release.send(()).unwrap();
        rotate.await.unwrap();
        appended.expect("append waited for the compaction pass to finish").unwrap();
    }

    /// The compaction pass owns the file only as far as it had grown when the
    /// pass began. Whatever lands past that point has to be carried into the
    /// rotated file, and counted, or the swap discards it and leaves the size
    /// counter describing a file that no longer exists.
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

        // Timed, not a bare await: an implementation that blocks appends for the
        // duration of the pass would otherwise deadlock the test here rather
        // than fail it.
        let late = entry(9, 9);
        let appended = tokio::time::timeout(Duration::from_secs(5), j.append_promised(&late)).await;

        release.send(()).unwrap();
        rotate.await.unwrap();
        appended.expect("append waited for the compaction pass to finish").unwrap();

        // Exact, not `contains`: the survivors in their original order followed
        // by the one that arrived late. Equality is what rules out the entry
        // being dropped, duplicated, or reordered against the compacted ones.
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
        // Whether recover_slot should fail, i.e. the bytes do not decode.
        undecodable: bool,
    }

    impl StubPool {
        fn new() -> Self {
            Self {
                known: HashSet::new(),
                contains_calls: std::sync::Mutex::new(Vec::new()),
                add_calls: std::sync::Mutex::new(Vec::new()),
                reject_add: false,
                undecodable: false,
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
            if self.undecodable {
                return None;
            }
            let seed = tx_rlp.first().copied().unwrap_or(0);
            Some((Address::from([seed; 20]), u64::from(seed)))
        }
        async fn add_envelope(&self, tx_rlp: &Bytes) -> Result<RestoredEnvelope, RestoreSkip> {
            self.add_calls.lock().unwrap().push(tx_rlp.clone());
            if self.reject_add {
                return Err(RestoreSkip::Rejected("rejected by stub".into()));
            }
            // Undecodable bytes fail here too: the real adapter decodes the same
            // RLP in both methods, so a stub that let one succeed while the other
            // failed would exercise a state production cannot reach.
            if self.undecodable {
                return Err(RestoreSkip::Rejected("does not decode".into()));
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
        move |h| c.is_tracked(h)
    }

    /// Classifier for restore tests, with **empty allowlists on purpose**: cold
    /// start may legitimately have loaded two empty lists (governance allows
    /// nobody) and restore must still bring commitments back. `restart_replay.rs`
    /// structurally cannot reproduce that, since it seeds the lists before the
    /// node starts.
    ///
    /// Built **enabled**, because `PreconfConfig::default()` has
    /// `enabled: false`, which short-circuits every write on this type — restore
    /// would appear to run and record nothing. Any classifier a test hands to
    /// `restore_preconf_state` must be enabled for the same reason.
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
            let _ = c.mark_promised(*h, &Address::from([0xEE; 20]), i as u64, 0);
            c.mark_committed(h, LANDED_AT);
        }
        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        for h in hashes {
            // Rotation keys on the record, so take the step that removes it — the
            // same one `forward` takes when the fifo drops a landed entry. Its
            // `true` doubles as the fixture's sanity check: it refuses while the
            // commitment is still inside its retention period.
            assert!(c.release_unless_committed(h), "fixture must be done with the commitment");
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

    /// Mark `hash` as a commitment this process still owes: promised, never
    /// landed, and promised for a block the chain cannot have buried. Rotation
    /// must keep it — an entry with no record at all would be dropped as an
    /// orphan, so a fixture's survivor has to say so explicitly.
    fn still_owed(c: &PreconfClassifier, hash: TxHash) {
        let _ = c.mark_promised(hash, &Address::from([0xEE; 20]), u64::from(hash.0[0]), u64::MAX);
        assert!(c.is_tracked(&hash));
    }

    /// Take `hash` through promise → committed → released, so the classifier is
    /// done with it — i.e. finish tracking a commitment mid-flight.
    ///
    /// The release is the point: rotation keys on the record, and retention
    /// expiring is not the record disappearing. This takes the same step
    /// production takes when `forward` drops the fifo entry of a landed
    /// commitment, so the journal predicate sees what it would see there.
    fn finish_tracking(c: &PreconfClassifier, hash: TxHash) {
        // The nonce is derived from the hash so repeated calls do not collide.
        let _ = c.mark_promised(hash, &Address::from([0xEE; 20]), u64::from(hash.0[0]), 0);
        c.mark_committed(&hash, LANDED_AT);
        assert!(c.release_unless_committed(&hash), "the watermark must make it releasable");
        assert!(!c.is_tracked(&hash));
    }

    /// What a restart costs, at the volume slicing produces.
    ///
    /// Journaling pool transactions takes the file from a handful of entries per
    /// block to roughly a block's worth, and rotation only clears it once a
    /// minute — so a restart can face tens of thousands of entries, each one
    /// decoded, claimed and offered to the pool before the node serves anything.
    ///
    /// Not an assertion about wall-clock time, which would be a flaky test on
    /// shared CI. It prints, so the number can be read off a run and recorded;
    /// what it *guards* is that restore stays linear — the two volumes differ by
    /// 10x, and anything quadratic in the entry count would show up as 100x.
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

            let pool = StubPool::new();
            let chain = landed();
            let fifo = Arc::new(PreconfTxSet::new(1 << 16));
            let classifier = empty_classifier();

            let started = std::time::Instant::now();
            restore_preconf_state(&journal, &pool, &chain, &fifo, &classifier).await;
            started.elapsed()
        }

        let small = restore_n(1_000).await;
        let large = restore_n(10_000).await;
        println!("restore: 1k entries {small:?}, 10k entries {large:?}");

        // 10x the entries, generously under 40x the time. A quadratic restore
        // would be near 100x and trip this; ordinary scheduling noise will not.
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
    /// at exactly the moment the real validator would run. See the pre-pass in
    /// `restore_preconf_state` for why the stronger form is the one that matters.
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
    /// Without the pre-pass claiming it, the nonce would read as *free* from
    /// restore until `add_envelope` drives that entry through the validator. A
    /// same-nonce transaction admitted in that interval takes the slot, and the
    /// commitment loses a nonce its client was already told it had. Nothing can
    /// admit in that interval as the node is wired today (`cli::node` runs
    /// restore before reth's local-tx backup loader, the RPC server and the
    /// network), so this pins the property that the *index* enforces it rather
    /// than startup order.
    ///
    /// NB the classifier here is **enabled** with empty allowlists, not
    /// `empty_classifier()` (which is built from a default config, i.e. disabled).
    /// `mark_promised` returns early on a disabled classifier and records
    /// nothing at all, which would make this test vacuous.
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
    /// having been kept, not a failure. Landing starts the retention clock
    /// rather than ending tracking: while the block is shallow a reorg could
    /// bring the commitment back, so rotation keeps the record. Once
    /// [`crate::classifier::SEAL_DEPTH`] persisted blocks sit on top, retention
    /// expires and the record may go — which is what stops every future restart
    /// replaying and complaining about the same entry forever.
    ///
    /// That such an entry never reaches the fifo is covered separately, by
    /// `a_consumed_nonce_never_reaches_the_fifo`.
    #[tokio::test]
    async fn restore_starts_the_retention_clock_for_an_already_landed_entry() {
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
        // must still hold its nonce, so the release `forward` would attempt is
        // refused and the record stays.
        assert!(!c.release_unless_committed(&e.hash), "shallow: the record is held");
        let stats = j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e.clone()], "kept while shallow; stats = {stats:?}");

        // Buried deep enough, the record may go — so the next restart will not
        // see it again.
        c.observe_persisted(LANDED_AT + crate::classifier::SEAL_DEPTH);
        assert!(c.release_unless_committed(&e.hash), "buried: the record is released");
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
    /// There is nothing to forget: the transaction that took the nonce may itself
    /// be reverted, and then this commitment applies again. So restore must not
    /// record it as landed, and `retain_tracked` must therefore keep it. Until
    /// 2026-08-05 this case was indistinguishable from "kept" —
    /// `is_nonce_too_low()` reduces to `tx.nonce < account.nonce` and never looks
    /// at the hash — so the entry was dropped at the next rotation and counted as
    /// honoured.
    #[tokio::test]
    async fn a_stolen_nonce_keeps_its_entry_through_rotation() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_classifier();
        restore_preconf_state(
            &j,
            &NonceConsumedPool,
            &StubChain(OnChain::No),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await;

        // `retain_tracked`, not `|_| true`: the entry survives because the
        // classifier still has its record, which is what the old behaviour got
        // wrong — treating the consumed nonce as "kept" dropped the line here.
        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(
            remaining,
            vec![e.clone()],
            "must survive rotation, so a later reorg can still free its nonce"
        );

        // And the record is un-landed, not landed-and-shallow: only an un-landed
        // one can be released outright. A commitment observed on chain would be
        // held by its retention depth instead. How long an un-landed promise is
        // kept is `sweep`'s question, pinned in the classifier's own tests.
        assert!(c.release_unless_committed(&e.hash), "restore must not record it as landed");
    }

    /// Cannot tell ⇒ keep. Folding `Unknown` into "on chain" would reinstate the
    /// silent misreport on a node whose transaction-lookup index is pruned.
    #[tokio::test]
    async fn an_undeterminable_entry_is_retained() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(1, 10);
        j.append_promised(&e).await.unwrap();

        let c = empty_classifier();
        restore_preconf_state(
            &j,
            &NonceConsumedPool,
            &StubChain(OnChain::Unknown),
            &Arc::new(PreconfTxSet::new(16)),
            &c,
        )
        .await;

        j.rotate(retain_tracked(&c)).await.unwrap();
        let (remaining, _) = j.load().await.unwrap();
        assert_eq!(remaining, vec![e.clone()]);
        assert!(c.release_unless_committed(&e.hash), "an unjudgeable entry is not recorded landed");
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
        assert!(c.release_unless_committed(&e.hash), "buried ⇒ the record may go");
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

    /// A pool refusal is a commitment that cannot be honoured. Restore walks
    /// every entry regardless, and gives each refused one up — record released,
    /// so the prune at the end of restore takes its line with it.
    #[tokio::test]
    async fn restore_gives_up_every_entry_the_pool_refuses() {
        let (_dir, j) = fresh_journal().await;
        j.append_promised(&entry(4, 40)).await.unwrap();
        j.append_promised(&entry(5, 50)).await.unwrap();

        let mut pool = StubPool::new();
        pool.reject_add = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        let c = empty_classifier();
        restore_preconf_state(&j, &pool, &landed(), &fifo, &c).await;

        // Both entries' add_envelope calls return Err — the function
        // does not panic and walks all entries.
        assert_eq!(pool.add_calls.lock().unwrap().len(), 2);
        assert!(fifo.snapshot().await.is_empty());
        assert!(!c.is_tracked(&entry(4, 40).hash), "a refused commitment is not kept in memory");
        let (remaining, _) = j.load().await.unwrap();
        assert!(remaining.is_empty(), "nor on disk — the two halves go together");
    }

    /// An entry whose bytes do not decode cannot even be named: the pre-pass gets
    /// no `(sender, nonce)` out of it, so it never becomes a promise record, and
    /// nothing in this process will revisit it. The prune at the end of restore is
    /// what stops it being replayed on every future restart.
    #[tokio::test]
    async fn restore_drops_an_entry_it_cannot_decode() {
        let (_dir, j) = fresh_journal().await;
        let e = entry(6, 60);
        j.append_promised(&e).await.unwrap();

        let mut pool = StubPool::new();
        pool.undecodable = true;
        let fifo = Arc::new(PreconfTxSet::new(16));
        let c = empty_classifier();
        restore_preconf_state(&j, &pool, &landed(), &fifo, &c).await;

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
        let c = classifier_done_with(&[e_a.hash]);

        let j = Arc::new(j);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // 30ms interval — first tick consumed at start, so the next
        // rotate happens at t ≈ 30ms.
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_millis(30), shutdown_rx);

        // Wait long enough for at least one rotate to fire.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // The no-longer-tracked entry should have been dropped from the file.
        let (after, _) = j.load().await.unwrap();
        assert!(
            after.is_empty(),
            "rotation must have dropped the untracked entry; instead got {after:?}"
        );

        // Graceful shutdown.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("loop did not shut down within timeout")
            .expect("loop panicked");
    }

    #[tokio::test]
    async fn size_trigger_rotates_dropping_untracked_without_periodic_tick() {
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
        still_owed(&c, e3.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = spawn_rejournal_loop(j.clone(), c, Duration::from_secs(3600), shutdown_rx);

        // Let the loop consume the pending size-notify and rotate.
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

    #[tokio::test]
    async fn open_with_max_size_seeds_counter_from_existing_file() {
        // A pre-existing entry already exceeding the cap must
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
        // shutdown signal fires, so entries that stopped being tracked
        // between the last periodic tick and the shutdown are still
        // dropped from the on-disk file.
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

        // Trigger shutdown; the loop's final rotate must drop the untracked entry.
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
        still_owed(&c, e2.hash);
        still_owed(&c, e3.hash);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            spawn_rejournal_loop(j.clone(), c.clone(), Duration::from_secs(3600), shutdown_rx);

        // First (coalesced) size trigger is honoured → released e1 dropped.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (after_first, _) = j.load().await.unwrap();
        assert_eq!(after_first, vec![e2.clone(), e3], "first size trigger drops released e1");

        // Release e2 and fire a *second* trigger well within `min_gap`
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
        still_owed(&c, e2.hash);

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

/// Stateful property model for [`PreconfJournal`].
///
/// Replays random `append` / `rotate` sequences against the real journal and an
/// independent reference model, checking after every step that the on-disk file
/// and the `size_bytes` counter agree with it. Guards the retention accounting
/// against changes that leak entries or drift the counter.
///
/// Which records may be dropped is the caller's decision, expressed through
/// `rotate`'s `retain` predicate (production passes "the classifier is still
/// tracking this"), so `Op::Untrack` mutates a tracking set the *test* owns.
/// The model therefore poses the question the interface actually asks: does
/// rotate drop exactly what it was told to, and does the file stay in
/// lock-step. It also pins `abandoned` staying empty while no TTL is set.
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

    /// Fixed 1:1 identity per byte — a signed tx's hash derives from its
    /// content, so a given hash always carries the same entry bytes.
    fn entry_for(byte: u8) -> JournalEntry {
        JournalEntry {
            hash: hash(byte),
            tx_rlp: Bytes::from(vec![byte; 4]),
            block_height: u64::from(byte),
            committed_at_ms: 1_000 + u64::from(byte),
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
                // No TTL is configured, so `retain` is the only drop rule.
                // Unlike the old model, an untracked hash *stays* untracked:
                // the set lives outside the journal, so re-appending the same
                // hash and rotating again drops it again.
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
        // Large cap so appends never auto-arm the size trigger during replay.
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

        /// Any sequence of append / untrack / rotate keeps the on-disk file and
        /// the `size_bytes` counter in lock-step with the reference model — in
        /// particular rotate drops exactly the entries `retain` rejects, and no
        /// more.
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
