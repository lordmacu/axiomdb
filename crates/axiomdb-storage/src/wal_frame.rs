//! Page-frame WAL log — project B (Option A: SQLite-WAL pure write-ahead REDO).
//!
//! This module is the **isolated foundation** for write-ahead page logging: a frame
//! log we can append page images to, read back (stopping at a torn tail), and index
//! (`page_id` → latest committed frame). It does NOT touch the live read/write path
//! yet — subphase 3 wires `write_page`/`read_page` to it.
//!
//! Format borrows SQLite `wal.c` (file header + per-frame header + page), adapted to
//! our explicit `lsn`:
//! - **txn_id** the transaction that wrote this frame (`0` = non-transactional /
//!   system write). Unlike SQLite's single end-of-txn commit marker (SQLite is
//!   single-writer), AxiomDB is multi-writer so frames from different txns interleave;
//!   each frame self-identifies. Recovery treats a frame as committed iff its `txn_id`
//!   has a `Commit` in the logical WAL (`build_index` takes that predicate).
//! - **salt** copied from the file header into every frame — frames whose salt ≠ the
//!   run's salt are stale (a previous WAL run) and end the valid prefix.
//! - **frame_crc** (crc32c over the frame header sans crc ++ page bytes): a torn /
//!   partially-written frame fails the crc → marks the end of the valid prefix
//!   (append-only ⇒ damage is always at the tail).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use axiomdb_core::error::{classify_io, DbError};

use crate::page::PAGE_SIZE;

const MAGIC: u64 = 0x4158_494f_4d5f_5746; // "AXIOM_WF"

// v2 added the variable-stride commit-marker record (single-fsync commit). A v1 log has
// only full page frames; recovery of a v1 log falls back to the logical-WAL committed
// predicate (see recovery.rs). See specs/fase-redo-recovery/spec-single-fsync-commit.md.
const VERSION: u32 = 2;

/// Sentinel `page_id` marking a **compact commit-marker record** (PostgreSQL's commit
/// record lives in the one WAL; ours lives in the one frame log). A marker is just the
/// 36-byte frame header — no page payload — so its on-disk length is `FRAME_HDR_SIZE`,
/// not `FRAME_SIZE`. Recovery reads markers to learn which `txn_id`s committed without a
/// second (logical-WAL) fsync. `u64::MAX` is never a real page id.
const COMMIT_MARKER: u64 = u64::MAX;

/// File header: magic(8) version(4) page_size(4) salt(8) header_crc(4) _pad(4).
const FILE_HDR_SIZE: u64 = 32;
/// Frame header: page_id(8) lsn(8) txn_id(8) salt(8) frame_crc(4).
const FRAME_HDR_SIZE: usize = 36;
/// crc covers the first 32 header bytes (everything but the crc field) ++ page.
const FRAME_HDR_CRC_PREFIX: usize = 32;
/// One frame on disk: header + a full page image.
const FRAME_SIZE: u64 = FRAME_HDR_SIZE as u64 + PAGE_SIZE as u64;

/// Diagnostic (single-fsync-commit A/B): process-global count of frame-log fsyncs
/// (`sync_to_durable`). A relaxed counter — cheap; read via [`frame_fsyncs`].
pub static FRAME_FSYNCS: AtomicU64 = AtomicU64::new(0);

/// Total frame-log fsyncs since process start (diagnostic).
pub fn frame_fsyncs() -> u64 {
    FRAME_FSYNCS.load(Ordering::Relaxed)
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().expect("8 bytes"))
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().expect("4 bytes"))
}

fn frame_crc(hdr_prefix: &[u8], page: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(hdr_prefix), page)
}

/// A non-volatile reference to one valid frame in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRef {
    pub page_id: u64,
    pub lsn: u64,
    /// Transaction that wrote this frame (`0` = non-transactional / system write).
    pub txn_id: u64,
    /// Byte offset of the frame header in the log file.
    pub offset: u64,
}

/// Result of a variable-stride scan of the frame log's valid prefix
/// ([`FrameLog::scan_records`]).
struct ScannedRecords {
    /// Page frames (salt-matching, crc-verified), in log order.
    pages: Vec<FrameRef>,
    /// Commit markers as `(txn_id, lsn)`, in log order.
    markers: Vec<(u64, u64)>,
    /// Byte offset just past the last valid record (the end of the valid prefix).
    end: u64,
}

/// How [`FrameLog::recycle`] treats the file's already-allocated blocks after a
/// checkpoint has made the main file durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleMode {
    /// Keep the file at its current (high-water) size: bump the salt + reset the
    /// watermarks only. Old frames become stale via salt mismatch and the next append
    /// overwrites them in place — SQLite's PERSIST WAL-reuse (`wal.c` increments the
    /// salt on checkpoint-restart without truncating). The runtime default: it avoids
    /// freeing + re-allocating the file's blocks every checkpoint cycle.
    Reuse,
    /// As [`Reuse`](Self::Reuse), then truncate the file to the header to reclaim disk
    /// (SQLite's TRUNCATE mode). For the shutdown / final checkpoint so the log is small
    /// at rest.
    Truncate,
}

/// Number of shards in the live index (mirrors `BufferPool`'s partitioning).
const WAL_INDEX_SHARDS: usize = 16;

/// Maps each page to its latest frame. Internally **sharded 16-way** (same model as
/// `BufferPool`) so concurrent writers updating different pages never contend on a
/// global lock.
///
/// Two distinct uses: (1) the **live** index, updated on every [`record`](Self::record)
/// as frames are appended in-session (a superset that includes the uncommitted tail);
/// (2) the **rebuilt** index from [`FrameLog::build_index`], which excludes frames
/// whose `txn_id` did not commit (recovery supplies the committed predicate).
#[derive(Debug)]
pub struct WalIndex {
    shards: Box<[Mutex<HashMap<u64, FrameRef>>]>,
    last_commit_lsn: AtomicU64,
}

impl Default for WalIndex {
    fn default() -> Self {
        let shards = (0..WAL_INDEX_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        WalIndex {
            shards,
            last_commit_lsn: AtomicU64::new(0),
        }
    }
}

impl WalIndex {
    #[inline]
    fn shard(&self, page_id: u64) -> &Mutex<HashMap<u64, FrameRef>> {
        &self.shards[(page_id as usize) & (WAL_INDEX_SHARDS - 1)]
    }

    /// Latest recorded frame for `page_id`, if any. Locks only that page's shard.
    /// Returns a copy (the shard guard is released before returning).
    pub fn latest(&self, page_id: u64) -> Option<FrameRef> {
        self.shard(page_id)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&page_id)
            .copied()
    }

    /// Snapshot of every page's recorded frame. Used by REDO recovery to walk all
    /// committed frames on open; not a hot path. Each shard is locked briefly in turn.
    pub fn frames(&self) -> Vec<FrameRef> {
        let mut out = Vec::new();
        for shard in self.shards.iter() {
            let guard = shard.lock().unwrap_or_else(|e| e.into_inner());
            out.extend(guard.values().copied());
        }
        out
    }

    /// Drops every recorded frame. Used by the checkpoint after a full recycle: the
    /// recycled log's offsets are invalid and the pages now live in the main file, so
    /// reads must fall through to it (subphase 6c).
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.last_commit_lsn.store(0, Ordering::Release);
    }

    /// Records `frame` as the latest version of its page (live append path).
    /// Locks only that page's shard.
    pub fn record(&self, frame: FrameRef) {
        self.shard(frame.page_id)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(frame.page_id, frame);
    }

    /// LSN of the last commit frame (0 if the log has no committed txn).
    pub fn last_commit_lsn(&self) -> u64 {
        self.last_commit_lsn.load(Ordering::Acquire)
    }

    /// Sets the last-commit LSN. Used by [`FrameLog::build_index`].
    pub fn set_last_commit_lsn(&self, lsn: u64) {
        self.last_commit_lsn.store(lsn, Ordering::Release);
    }

    /// Number of distinct pages with a recorded frame.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).len())
            .sum()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Append-only page-frame log file. [`append`](Self::append) is **lock-free**: the
/// file offset is reserved with an atomic `fetch_add` and the frame is written with
/// `pwrite` (`write_all_at`), which is thread-safe for disjoint regions — concurrent
/// writers never contend (mirrors `ConcurrentWalWriter`'s atomic-LSN reservation).
pub struct FrameLog {
    file: File,
    /// Run identity, copied into every frame. `AtomicU64` so [`recycle`](Self::recycle)
    /// can swap it through `&self` (under the caller's exclusive checkpoint guard).
    salt: AtomicU64,
    /// Offset where the next frame will be appended. Reserved via `fetch_add`.
    write_offset: AtomicU64,
    /// Contiguous-durable-prefix bookkeeping (subphase 6a). Guards only the completion
    /// fold (~ns) — NEVER held across a `pwrite` or an fsync, so the append hot path and
    /// the fsync stay off this lock.
    sync_state: Mutex<SyncState>,
    /// Notified whenever `contiguous_written` advances or a slot is poisoned, so a
    /// [`sync_to_durable`](Self::sync_to_durable) waiter can re-check its target.
    advanced: Condvar,
    /// Highest fsync'd offset (≤ `contiguous_written`). A crash preserves exactly the
    /// frames in `[FILE_HDR_SIZE, durable)`. Atomic for a lock-free fast-path read.
    durable: AtomicU64,
    /// Serializes/coalesces fsyncs so a multi-ms `sync_data` never blocks append
    /// bookkeeping (the `sync_state` mutex is dropped before the fsync runs under this).
    fsync_leader: Mutex<()>,
    /// On-disk format version of THIS log file (`VERSION` for a freshly created log; the
    /// header's version for an opened one). v1 has no commit markers ⇒ recovery falls back
    /// to the logical-WAL committed predicate; v2+ uses the markers (single-fsync commit).
    version: u32,
}

/// Bookkeeping for the gap-free written prefix under concurrent lock-free appends.
/// Offsets are frame START boundaries; "prefix ends at X" ⇔ every frame with START < X
/// has finished its `pwrite`.
struct SyncState {
    /// Completed records not yet folded into `contiguous_written` (a higher record
    /// finished before a lower one — the out-of-order reorder buffer). Maps each
    /// record's START offset → its on-disk length (`FRAME_SIZE` for a page frame,
    /// `FRAME_HDR_SIZE` for a commit marker) so the gap-free fold advances by the
    /// actual length under the variable-stride layout.
    completed: BTreeMap<u64, u64>,
    /// Gap-free written prefix == the next expected (lowest un-written) START offset.
    contiguous_written: u64,
    /// Lowest START offset whose append `pwrite` failed (a permanent gap), if any.
    poison: Option<u64>,
}

impl SyncState {
    fn new(start: u64) -> Self {
        SyncState {
            completed: BTreeMap::new(),
            contiguous_written: start,
            poison: None,
        }
    }
}

impl FrameLog {
    /// Creates a fresh frame log with a new random salt and a synced file header.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| classify_io(e, "frame log create"))?;
        let salt = fresh_salt();
        let mut hdr = [0u8; FILE_HDR_SIZE as usize];
        hdr[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        hdr[16..24].copy_from_slice(&salt.to_le_bytes());
        let hcrc = crc32c::crc32c(&hdr[0..24]);
        hdr[24..28].copy_from_slice(&hcrc.to_le_bytes());
        file.write_all_at(&hdr, 0)
            .map_err(|e| classify_io(e, "frame log write header"))?;
        file.sync_all()
            .map_err(|e| classify_io(e, "frame log sync header"))?;
        Ok(FrameLog {
            file,
            salt: AtomicU64::new(salt),
            write_offset: AtomicU64::new(FILE_HDR_SIZE),
            sync_state: Mutex::new(SyncState::new(FILE_HDR_SIZE)),
            advanced: Condvar::new(),
            durable: AtomicU64::new(FILE_HDR_SIZE),
            fsync_leader: Mutex::new(()),
            version: VERSION,
        })
    }

    /// Opens an existing frame log: validates the header, loads the salt, and sets
    /// the write offset just past the last VALID frame (any torn tail is overwritten
    /// by the next append).
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| classify_io(e, "frame log open"))?;
        let mut hdr = [0u8; FILE_HDR_SIZE as usize];
        file.read_exact_at(&mut hdr, 0)
            .map_err(|e| classify_io(e, "frame log read header"))?;
        if le_u64(&hdr[0..8]) != MAGIC {
            return Err(DbError::Other("frame log: bad magic".into()));
        }
        // Accept v1 (page frames only) and v2 (adds compact commit markers): the
        // variable-stride reader handles both — a v1 log simply has no markers, so
        // recovery falls back to the logical-WAL committed predicate for it (no
        // regression opening a pre-marker log). Reject only unknown/newer versions.
        let file_version = le_u32(&hdr[8..12]);
        if file_version == 0 || file_version > VERSION {
            return Err(DbError::Other(format!(
                "frame log: unsupported version {file_version}"
            )));
        }
        if le_u32(&hdr[12..16]) as usize != PAGE_SIZE {
            return Err(DbError::Other(format!(
                "frame log: page size {} != {PAGE_SIZE}",
                le_u32(&hdr[12..16])
            )));
        }
        if le_u32(&hdr[24..28]) != crc32c::crc32c(&hdr[0..24]) {
            return Err(DbError::Other("frame log: header checksum mismatch".into()));
        }
        let salt = le_u64(&hdr[16..24]);
        let log = FrameLog {
            file,
            salt: AtomicU64::new(salt),
            write_offset: AtomicU64::new(FILE_HDR_SIZE),
            sync_state: Mutex::new(SyncState::new(FILE_HDR_SIZE)),
            advanced: Condvar::new(),
            durable: AtomicU64::new(FILE_HDR_SIZE),
            fsync_leader: Mutex::new(()),
            version: file_version,
        };
        // The valid prefix on disk is already durable: set every watermark to its end.
        // Use the variable-stride walker's end offset (page frames + compact markers), not
        // `last_frame + FRAME_SIZE` (which would over-count a trailing commit marker).
        let end = log.scan_records()?.end;
        log.write_offset.store(end, Ordering::Relaxed);
        log.sync_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contiguous_written = end;
        log.durable.store(end, Ordering::Relaxed);
        Ok(log)
    }

    /// Appends one page frame and returns its byte offset. Does NOT fsync (call
    /// [`sync`](Self::sync) at the commit boundary).
    pub fn append(
        &self,
        page_id: u64,
        lsn: u64,
        txn_id: u64,
        page: &[u8; PAGE_SIZE],
    ) -> Result<u64, DbError> {
        let mut hdr = [0u8; FRAME_HDR_SIZE];
        hdr[0..8].copy_from_slice(&page_id.to_le_bytes());
        hdr[8..16].copy_from_slice(&lsn.to_le_bytes());
        hdr[16..24].copy_from_slice(&txn_id.to_le_bytes());
        hdr[24..32].copy_from_slice(&self.salt.load(Ordering::Relaxed).to_le_bytes());
        let crc = frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], page);
        hdr[32..36].copy_from_slice(&crc.to_le_bytes());

        // Lock-free: reserve this frame's slot, then pwrite into it. Two writers get
        // disjoint offsets, so the writes never overlap and need no lock.
        let offset = self.write_offset.fetch_add(FRAME_SIZE, Ordering::Relaxed);
        let write = self
            .file
            .write_all_at(&hdr, offset)
            .and_then(|()| self.file.write_all_at(page, offset + FRAME_HDR_SIZE as u64));
        match write {
            Ok(()) => {
                // Publish completion so the contiguous-written prefix can advance.
                self.mark_written(offset, FRAME_SIZE);
                Ok(offset)
            }
            Err(e) => {
                // Poison the slot: a committer waiting past it must error, not deadlock.
                self.mark_poison(offset);
                Err(classify_io(e, "frame log write"))
            }
        }
    }

    /// Appends a **compact commit marker** for `txn_id` (single-fsync commit). The marker
    /// is just the 36-byte frame header (`page_id == COMMIT_MARKER`, the txn, the run salt,
    /// a crc over the header) — no page payload. It MUST be appended AFTER the txn's data
    /// frames so a single [`sync_to_durable`](Self::sync_to_durable) makes data+commit
    /// durable in write-ahead order. Recovery reads markers via
    /// [`committed_txns_from_markers`](Self::committed_txns_from_markers) to learn which
    /// transactions committed, with no second (logical-WAL) fsync. Lock-free like
    /// [`append`](Self::append): reserves a compact slot with `fetch_add(FRAME_HDR_SIZE)`.
    pub fn append_commit_marker(&self, txn_id: u64, lsn: u64) -> Result<u64, DbError> {
        let mut hdr = [0u8; FRAME_HDR_SIZE];
        hdr[0..8].copy_from_slice(&COMMIT_MARKER.to_le_bytes());
        hdr[8..16].copy_from_slice(&lsn.to_le_bytes());
        hdr[16..24].copy_from_slice(&txn_id.to_le_bytes());
        hdr[24..32].copy_from_slice(&self.salt.load(Ordering::Relaxed).to_le_bytes());
        // crc over the header prefix only (no page) — equals frame_crc(prefix, &[]).
        let crc = crc32c::crc32c(&hdr[0..FRAME_HDR_CRC_PREFIX]);
        hdr[32..36].copy_from_slice(&crc.to_le_bytes());

        // Reserve a COMPACT slot (FRAME_HDR_SIZE, not FRAME_SIZE) — variable stride.
        let offset = self
            .write_offset
            .fetch_add(FRAME_HDR_SIZE as u64, Ordering::Relaxed);
        match self.file.write_all_at(&hdr, offset) {
            Ok(()) => {
                self.mark_written(offset, FRAME_HDR_SIZE as u64);
                Ok(offset)
            }
            Err(e) => {
                self.mark_poison(offset);
                Err(classify_io(e, "frame log commit marker write"))
            }
        }
    }

    /// Flushes appended frames durably (fdatasync). Low-level primitive — the commit
    /// boundary uses [`sync_to_durable`](Self::sync_to_durable) instead, which first
    /// waits for a gap-free written prefix.
    pub fn sync(&self) -> Result<(), DbError> {
        self.file
            .sync_data()
            .map_err(|e| classify_io(e, "frame log sync"))
    }

    /// Records that the record at START `offset` with on-disk length `len` finished its
    /// `pwrite`, folding it into the contiguous-written prefix (advancing over any
    /// now-contiguous earlier completions). `len` is `FRAME_SIZE` for a page frame or
    /// `FRAME_HDR_SIZE` for a commit marker — the fold advances by the stored length so
    /// variable-stride records leave no phantom gap.
    fn mark_written(&self, offset: u64, len: u64) {
        {
            let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
            s.completed.insert(offset, len);
            let mut cw = s.contiguous_written;
            while let Some(l) = s.completed.remove(&cw) {
                cw += l;
            }
            s.contiguous_written = cw;
        }
        self.advanced.notify_all();
    }

    /// Records that the frame at START `offset` failed to write — a permanent gap. A
    /// later [`sync_to_durable`](Self::sync_to_durable) targeting beyond it errors rather
    /// than blocking forever on a hole that will never fill.
    fn mark_poison(&self, offset: u64) {
        {
            let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
            s.poison = Some(s.poison.map_or(offset, |p| p.min(offset)));
        }
        self.advanced.notify_all();
    }

    /// Highest offset W such that every frame with START `< W` has finished its `pwrite`
    /// (the gap-free written prefix).
    pub fn contiguous_written_offset(&self) -> u64 {
        self.sync_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contiguous_written
    }

    /// Highest fsync'd offset — a crash preserves exactly the frames in
    /// `[FILE_HDR_SIZE, durable)`.
    pub fn durable_offset(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    /// Commit-boundary durability: make every frame reserved before this call gap-free
    /// and fsync'd. Snapshots the reserved end, waits until the contiguous-written
    /// prefix covers it (so no committed frame is ever left behind a hole on crash),
    /// fsyncs once under the fsync leader, then advances `durable`. Returns once the
    /// reserved end is durable; concurrent callers coalesce on the same fsync.
    ///
    /// Mirrors PostgreSQL `XLogFlush`: `WaitXLogInsertionsToFinish` (the wait below) then
    /// the single write+fsync that advances `LogwrtResult.Flush` (our `durable`).
    ///
    /// # Errors
    /// If a lower frame's `append` failed (a permanent gap below the target), returns
    /// `DbError` instead of blocking forever.
    pub fn sync_to_durable(&self) -> Result<(), DbError> {
        let target = self.write_offset.load(Ordering::Acquire);
        if self.durable.load(Ordering::Acquire) >= target {
            return Ok(());
        }
        // Wait for a gap-free written prefix covering `target` (bookkeeping mutex only).
        {
            let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(p) = s.poison {
                    if p < target {
                        return Err(DbError::Other(
                            "frame log: durable prefix blocked by a failed append".into(),
                        ));
                    }
                }
                if s.contiguous_written >= target {
                    break;
                }
                s = self.advanced.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        }
        // fsync under the leader lock — never the bookkeeping mutex, so appends keep
        // publishing completions while this multi-ms fsync runs. Coalesces syncers.
        let _leader = self.fsync_leader.lock().unwrap_or_else(|e| e.into_inner());
        if self.durable.load(Ordering::Acquire) < target {
            let cw = self
                .sync_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contiguous_written;
            FRAME_FSYNCS.fetch_add(1, Ordering::Relaxed);
            self.file
                .sync_data()
                .map_err(|e| classify_io(e, "frame log sync"))?;
            self.durable.fetch_max(cw, Ordering::Release);
        }
        Ok(())
    }

    /// Recycles the log after a checkpoint: rewrites the header with a FRESH salt, fsyncs,
    /// and resets the offset watermarks. With [`RecycleMode::Truncate`] the file is also
    /// truncated to the header (reclaim disk); with [`RecycleMode::Reuse`] (the runtime
    /// default) the file keeps its size so the next append overwrites the now-stale frames
    /// in place — the fresh salt is what invalidates them ([`scan`](Self::scan) stops at the
    /// first salt mismatch). The caller MUST hold the exclusive checkpoint guard (no
    /// concurrent appends). Does NOT touch any external monotonic LSN counter — the engine's
    /// `frame_lsn` must keep increasing, or the pageLSN redo guard would wrongly skip a frame
    /// appended after the recycle.
    ///
    /// Crash-safe regardless of mode: the checkpoint already applied every committed frame
    /// to the main file and fsync'd it before calling this, so a crash mid-recycle leaves
    /// the main file current and any leftover frames REDO idempotently (pageLSN strict-`>`).
    pub fn recycle(&self, mode: RecycleMode) -> Result<(), DbError> {
        let salt = fresh_salt();
        let mut hdr = [0u8; FILE_HDR_SIZE as usize];
        hdr[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        hdr[16..24].copy_from_slice(&salt.to_le_bytes());
        let hcrc = crc32c::crc32c(&hdr[0..24]);
        hdr[24..28].copy_from_slice(&hcrc.to_le_bytes());
        // Reuse (the runtime default) keeps the file at its high-water size so the next
        // append overwrites the now-stale frames in place — no block free + re-alloc. Only
        // Truncate reclaims disk (shutdown / disk-pressure). Either way the fresh-salt header
        // rewrite below is what invalidates the leftover frames for `scan`.
        if mode == RecycleMode::Truncate {
            self.file
                .set_len(FILE_HDR_SIZE)
                .map_err(|e| classify_io(e, "frame log recycle truncate"))?;
        }
        self.file
            .write_all_at(&hdr, 0)
            .map_err(|e| classify_io(e, "frame log recycle header"))?;
        self.file
            .sync_all()
            .map_err(|e| classify_io(e, "frame log recycle sync"))?;
        self.salt.store(salt, Ordering::Release);
        self.write_offset.store(FILE_HDR_SIZE, Ordering::Release);
        self.durable.store(FILE_HDR_SIZE, Ordering::Release);
        let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
        s.completed.clear();
        s.contiguous_written = FILE_HDR_SIZE;
        s.poison = None;
        Ok(())
    }

    /// Test-only: force the reserved-end (`write_offset`) to simulate in-flight appends
    /// without performing real I/O, so the contiguous-prefix logic can be tested
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn set_write_offset_for_test(&self, offset: u64) {
        self.write_offset.store(offset, Ordering::Relaxed);
    }

    /// Reads the page image of the frame whose header starts at `offset`.
    pub fn read_page_at(&self, offset: u64) -> Result<Box<[u8; PAGE_SIZE]>, DbError> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        self.file
            .read_exact_at(page.as_mut_slice(), offset + FRAME_HDR_SIZE as u64)
            .map_err(|e| classify_io(e, "frame log read page"))?;
        Ok(page)
    }

    /// Reads the frame at `offset` **only if it is still a valid frame for `page_id`**
    /// under the current run salt — one `pread` of header+page that verifies `page_id`,
    /// `salt`, and crc. Returns `None` when the offset is stale: the log was recycled
    /// (salt changed or the offset is now beyond EOF) or it points at a different page.
    /// The read path uses this so a wal-index hit that races a checkpoint's recycle
    /// safely falls back to the (now-current) main file instead of returning wrong bytes.
    pub fn read_page_if_for(
        &self,
        offset: u64,
        page_id: u64,
    ) -> Result<Option<Box<[u8; PAGE_SIZE]>>, DbError> {
        let mut buf = vec![0u8; FRAME_SIZE as usize];
        match self.file.read_exact_at(&mut buf, offset) {
            Ok(()) => {}
            // Beyond EOF ⇒ the log was truncated by a recycle ⇒ stale.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(classify_io(e, "frame log read_if_for")),
        }
        let (hdr, page) = buf.split_at(FRAME_HDR_SIZE);
        if le_u64(&hdr[0..8]) != page_id {
            return Ok(None); // a different page (a recycled offset reused by a new frame)
        }
        if le_u64(&hdr[24..32]) != self.salt.load(Ordering::Relaxed) {
            return Ok(None); // a previous run / post-recycle salt ⇒ stale
        }
        if frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], page) != le_u32(&hdr[32..36]) {
            return Ok(None); // torn ⇒ stale
        }
        let mut out = Box::new([0u8; PAGE_SIZE]);
        out.copy_from_slice(page);
        Ok(Some(out))
    }

    /// Variable-stride walk of the valid prefix. Returns `(page_frames, commit_markers,
    /// end_offset)` where `commit_markers` is `(txn_id, lsn)` per marker and `end_offset`
    /// is the byte offset just past the last valid record. A record is a full **page
    /// frame** (`page_id != COMMIT_MARKER`, length `FRAME_SIZE`) or a compact **commit
    /// marker** (`page_id == COMMIT_MARKER`, length `FRAME_HDR_SIZE`). Stops at the first
    /// stale salt, torn crc, or partial tail (append-only ⇒ damage is at the tail).
    fn scan_records(&self) -> Result<ScannedRecords, DbError> {
        let file_len = self
            .file
            .metadata()
            .map_err(|e| classify_io(e, "frame log metadata"))?
            .len();
        let salt_now = self.salt.load(Ordering::Relaxed);
        let mut frames = Vec::new();
        let mut markers = Vec::new();
        let mut offset = FILE_HDR_SIZE;
        let mut hdr = [0u8; FRAME_HDR_SIZE];
        let mut page = vec![0u8; PAGE_SIZE];
        // Need at least a header to read any record.
        while offset + FRAME_HDR_SIZE as u64 <= file_len {
            self.file
                .read_exact_at(&mut hdr, offset)
                .map_err(|e| classify_io(e, "frame log scan read"))?;
            if le_u64(&hdr[24..32]) != salt_now {
                break; // stale record from a previous run
            }
            let page_id = le_u64(&hdr[0..8]);
            let stored_crc = le_u32(&hdr[32..36]);
            if page_id == COMMIT_MARKER {
                // Compact commit marker: crc covers the header only (no page payload).
                if crc32c::crc32c(&hdr[0..FRAME_HDR_CRC_PREFIX]) != stored_crc {
                    break; // torn marker — end of valid prefix
                }
                markers.push((le_u64(&hdr[16..24]), le_u64(&hdr[8..16]))); // (txn_id, lsn)
                offset += FRAME_HDR_SIZE as u64;
            } else {
                // Full page frame: the page bytes must also be present (else torn tail).
                if offset + FRAME_SIZE > file_len {
                    break;
                }
                self.file
                    .read_exact_at(&mut page, offset + FRAME_HDR_SIZE as u64)
                    .map_err(|e| classify_io(e, "frame log scan read"))?;
                if frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], &page) != stored_crc {
                    break; // torn / corrupt frame — end of valid prefix
                }
                frames.push(FrameRef {
                    page_id,
                    lsn: le_u64(&hdr[8..16]),
                    txn_id: le_u64(&hdr[16..24]),
                    offset,
                });
                offset += FRAME_SIZE;
            }
        }
        Ok(ScannedRecords {
            pages: frames,
            markers,
            end: offset,
        })
    }

    /// Scans the valid prefix for **page frames** (salt-matching, crc-verified, stopping at
    /// the torn tail). Commit markers are skipped here — see
    /// [`committed_txns_from_markers`](Self::committed_txns_from_markers).
    pub fn scan(&self) -> Result<Vec<FrameRef>, DbError> {
        Ok(self.scan_records()?.pages)
    }

    /// The set of `txn_id`s with a durable **commit marker** in the valid prefix — the
    /// single-fsync recovery committed-predicate (frame-only). Empty for a v1 log (no
    /// markers): recovery then falls back to the logical-WAL predicate.
    pub fn committed_txns_from_markers(&self) -> Result<BTreeSet<u64>, DbError> {
        Ok(self
            .scan_records()?
            .markers
            .into_iter()
            .map(|(txn_id, _lsn)| txn_id)
            .collect())
    }

    /// Builds the page → latest-committed-frame index. A frame counts only if
    /// `is_committed(frame.txn_id)` — i.e. its transaction has a `Commit` in the
    /// logical WAL (recovery supplies the predicate). Frames written by in-flight
    /// (uncommitted-at-crash) transactions are skipped, wherever they sit in the log.
    pub fn build_index(&self, is_committed: &dyn Fn(u64) -> bool) -> Result<WalIndex, DbError> {
        let frames = self.scan()?;
        let index = WalIndex::default();
        let mut max_committed_lsn = 0u64;
        for f in &frames {
            if is_committed(f.txn_id) {
                index.record(*f); // latest wins (in-order scan)
                max_committed_lsn = max_committed_lsn.max(f.lsn);
            }
        }
        index.set_last_commit_lsn(max_committed_lsn);
        Ok(index)
    }

    /// The run salt (test/diagnostic).
    pub fn salt(&self) -> u64 {
        self.salt.load(Ordering::Relaxed)
    }

    /// On-disk format version of this log (1 = page frames only, 2 = adds compact commit
    /// markers). Recovery uses this to pick the committed predicate: v2 → frame markers,
    /// v1 → the logical-WAL `Commit` scan (no markers exist to read).
    pub fn format_version(&self) -> u32 {
        self.version
    }
}

fn fresh_salt() -> u64 {
    // Run identity, not cryptographic. Unique per create via the wall clock + a
    // process-local counter so two logs created in the same nanosecond still differ.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(marker: u8) -> [u8; PAGE_SIZE] {
        let mut p = [0u8; PAGE_SIZE];
        p[0] = marker;
        p[PAGE_SIZE - 1] = marker;
        p
    }

    fn tmp(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    #[test]
    fn append_then_read_round_trip() {
        let (_d, path) = tmp("rt.wf");
        let log = FrameLog::create(&path).unwrap();
        let off1 = log.append(2, 1, 0, &page(0xAA)).unwrap();
        let off2 = log.append(3, 2, 7, &page(0xBB)).unwrap();
        assert_ne!(off1, off2);
        assert_eq!(log.read_page_at(off1).unwrap()[0], 0xAA);
        assert_eq!(log.read_page_at(off2).unwrap()[0], 0xBB);
        assert_eq!(log.read_page_at(off2).unwrap()[PAGE_SIZE - 1], 0xBB);
    }

    #[test]
    fn scan_returns_all_valid_frames() {
        let (_d, path) = tmp("scan.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 0, &page(1)).unwrap();
        log.append(2, 2, 5, &page(2)).unwrap();
        let frames = log.scan().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].page_id, 2);
        assert_eq!(frames[1].txn_id, 5);
    }

    #[test]
    fn index_keeps_latest_committed_per_page() {
        let (_d, path) = tmp("idx.wf");
        let log = FrameLog::create(&path).unwrap();
        // txn 7 writes page 2 (v1) + page 3; txn 8 rewrites page 2 (v2). Both commit.
        log.append(2, 1, 7, &page(0x10)).unwrap();
        log.append(3, 2, 7, &page(0x20)).unwrap();
        let off_p2_v2 = log.append(2, 3, 8, &page(0x11)).unwrap();
        let idx = log.build_index(&|_| true).unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.latest(2).unwrap().offset, off_p2_v2); // latest wins
        assert_eq!(idx.latest(2).unwrap().lsn, 3);
        assert_eq!(idx.latest(3).unwrap().lsn, 2);
        assert_eq!(idx.last_commit_lsn(), 3);
    }

    #[test]
    fn frames_returns_every_indexed_page() {
        let (_d, path) = tmp("frames.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 7, &page(0x10)).unwrap();
        log.append(3, 2, 7, &page(0x20)).unwrap();
        log.append(2, 3, 8, &page(0x11)).unwrap(); // page 2 latest = lsn 3
        let idx = log.build_index(&|_| true).unwrap();
        let mut got: Vec<(u64, u64)> = idx.frames().iter().map(|f| (f.page_id, f.lsn)).collect();
        got.sort_unstable();
        assert_eq!(got, vec![(2, 3), (3, 2)]);
    }

    // ── single-fsync commit: compact marker + variable-stride frame log ──────────

    #[test]
    fn commit_marker_round_trips_between_page_frames() {
        let (_d, path) = tmp("marker.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(7, 1, 100, &page(0xA1)).unwrap();
        log.append_commit_marker(100, 2).unwrap();
        log.append(8, 3, 101, &page(0xB2)).unwrap();
        // scan() yields only the two PAGE frames, in order (the marker is variable-stride).
        let frames = log.scan().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].page_id, 7);
        assert_eq!(frames[1].page_id, 8);
        assert_eq!(frames[1].txn_id, 101);
        // the marker is read back as a committed txn.
        assert_eq!(
            log.committed_txns_from_markers().unwrap(),
            BTreeSet::from([100u64])
        );
        // survives a reopen (variable-stride walk from disk).
        drop(log);
        let log2 = FrameLog::open(&path).unwrap();
        assert_eq!(log2.scan().unwrap().len(), 2);
        assert_eq!(
            log2.committed_txns_from_markers().unwrap(),
            BTreeSet::from([100u64])
        );
    }

    #[test]
    fn torn_commit_marker_ends_valid_prefix() {
        let (_d, path) = tmp("torn_marker.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(7, 1, 100, &page(0xA1)).unwrap();
        let marker_off = log.append_commit_marker(100, 2).unwrap();
        log.append(8, 3, 101, &page(0xB2)).unwrap(); // a frame AFTER the marker
        drop(log);
        // Corrupt the marker's crc on disk (header bytes [marker_off+32 .. +36]).
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(&[0xFF; 4], marker_off + 32).unwrap();
        drop(f);
        let log = FrameLog::open(&path).unwrap();
        // scan stops at the torn marker → only the FIRST page frame survives; the frame
        // written after the marker is unreachable.
        let frames = log.scan().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].page_id, 7);
        assert!(log.committed_txns_from_markers().unwrap().is_empty());
    }

    #[test]
    fn durable_prefix_handles_mixed_record_sizes() {
        // The subtle correctness point: the gap-free fold must advance by each record's
        // ACTUAL length, not a fixed FRAME_SIZE. Layout: page @A, marker @B, page @C.
        let (_d, path) = tmp("mixed.wf");
        let log = FrameLog::create(&path).unwrap();
        let a = FILE_HDR_SIZE;
        let b = a + FRAME_SIZE; // after the page frame
        let c = b + FRAME_HDR_SIZE as u64; // after the compact marker
        log.set_write_offset_for_test(c + FRAME_SIZE);
        // complete OUT OF ORDER: C first → prefix stuck at A (gaps at A,B).
        log.mark_written(c, FRAME_SIZE);
        assert_eq!(log.contiguous_written_offset(), a);
        // A done (page len) → prefix advances to B.
        log.mark_written(a, FRAME_SIZE);
        assert_eq!(log.contiguous_written_offset(), b);
        // B done (marker len) → folds B (FRAME_HDR_SIZE) then C (FRAME_SIZE) → end.
        log.mark_written(b, FRAME_HDR_SIZE as u64);
        assert_eq!(log.contiguous_written_offset(), c + FRAME_SIZE);
    }

    #[test]
    fn open_end_offset_is_past_a_trailing_marker() {
        // Regression for the variable-stride open(): a trailing marker advances the write
        // offset by FRAME_HDR_SIZE, not FRAME_SIZE (else the next append leaves a hole).
        let (_d, path) = tmp("trailing_marker.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(7, 1, 100, &page(0xA1)).unwrap();
        let marker_off = log.append_commit_marker(100, 2).unwrap();
        log.sync_to_durable().unwrap();
        drop(log);
        let log2 = FrameLog::open(&path).unwrap();
        let off = log2.append(8, 3, 101, &page(0xB2)).unwrap();
        assert_eq!(off, marker_off + FRAME_HDR_SIZE as u64);
    }

    #[test]
    fn build_index_excludes_uncommitted_txns() {
        let (_d, path) = tmp("uncommitted.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 9, &page(1)).unwrap(); // txn 9 (will be committed)
        log.append(3, 2, 7, &page(2)).unwrap(); // txn 7 (in-flight at crash)
                                                // Recovery's predicate: only txn 9 committed.
        let idx = log.build_index(&|t| t == 9).unwrap();
        assert_eq!(idx.len(), 1, "only the committed txn's page is indexed");
        assert!(idx.latest(2).is_some());
        assert!(idx.latest(3).is_none(), "uncommitted txn's page excluded");
        assert_eq!(idx.last_commit_lsn(), 1);
    }

    #[test]
    fn torn_tail_frame_ends_the_valid_prefix() {
        let (_d, path) = tmp("torn.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 7, &page(1)).unwrap();
        let bad_off = log.append(3, 2, 8, &page(2)).unwrap();
        // Corrupt one byte of the second frame's page on disk.
        log.file
            .write_all_at(&[0xFF], bad_off + FRAME_HDR_SIZE as u64 + 4)
            .unwrap();
        let frames = log.scan().unwrap();
        assert_eq!(frames.len(), 1, "scan stops at the corrupt frame");
        assert_eq!(frames[0].lsn, 1);
    }

    #[test]
    fn reopen_validates_header_and_finds_valid_prefix() {
        let (_d, path) = tmp("reopen.wf");
        let salt = {
            let log = FrameLog::create(&path).unwrap();
            log.append(2, 1, 4, &page(1)).unwrap();
            log.sync().unwrap();
            log.salt()
        };
        let log2 = FrameLog::open(&path).unwrap();
        assert_eq!(log2.salt(), salt);
        assert_eq!(log2.scan().unwrap().len(), 1);
        // An append after reopen lands after the valid prefix.
        let off = log2.append(2, 2, 5, &page(2)).unwrap();
        assert_eq!(off, FILE_HDR_SIZE + FRAME_SIZE);
        assert_eq!(log2.scan().unwrap().len(), 2);
    }

    #[test]
    fn contiguous_watermark_reorders_out_of_order_completions() {
        let (_d, path) = tmp("wm.wf");
        let log = FrameLog::create(&path).unwrap();
        let h = FILE_HDR_SIZE;
        assert_eq!(log.contiguous_written_offset(), h);
        assert_eq!(log.durable_offset(), h);
        // A higher frame completes before a lower one → the prefix must NOT skip the gap.
        log.mark_written(h + FRAME_SIZE, FRAME_SIZE);
        assert_eq!(
            log.contiguous_written_offset(),
            h,
            "gap must not be skipped"
        );
        // Filling the gap folds both in one step.
        log.mark_written(h, FRAME_SIZE);
        assert_eq!(log.contiguous_written_offset(), h + 2 * FRAME_SIZE);
    }

    #[test]
    fn sequential_appends_advance_contiguous_written() {
        let (_d, path) = tmp("wm-seq.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 7, &page(1)).unwrap();
        log.append(3, 2, 7, &page(2)).unwrap();
        assert_eq!(
            log.contiguous_written_offset(),
            FILE_HDR_SIZE + 2 * FRAME_SIZE
        );
    }

    #[test]
    fn sync_to_durable_makes_all_appends_durable_idempotently() {
        let (_d, path) = tmp("sd.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 7, &page(1)).unwrap();
        let end = FILE_HDR_SIZE + FRAME_SIZE;
        log.sync_to_durable().unwrap();
        assert_eq!(log.durable_offset(), end);
        log.sync_to_durable().unwrap(); // idempotent fast-path (durable >= target)
        assert_eq!(log.durable_offset(), end);
    }

    #[test]
    fn sync_to_durable_blocks_until_a_gap_is_filled() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let (_d, path) = tmp("sd-gap.wf");
        let log = Arc::new(FrameLog::create(&path).unwrap());
        let h = FILE_HDR_SIZE;
        // Two frames reserved; only the higher one completed → a gap at h.
        log.set_write_offset_for_test(h + 2 * FRAME_SIZE);
        log.mark_written(h + FRAME_SIZE, FRAME_SIZE);

        let done = Arc::new(AtomicBool::new(false));
        let (l2, d2) = (Arc::clone(&log), Arc::clone(&done));
        let t = std::thread::spawn(move || {
            l2.sync_to_durable().unwrap();
            d2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !done.load(Ordering::SeqCst),
            "sync_to_durable must block while a gap exists below the target"
        );
        log.mark_written(h, FRAME_SIZE); // fill the gap → the waiter proceeds
        t.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(log.durable_offset(), h + 2 * FRAME_SIZE);
    }

    #[test]
    fn sync_to_durable_errors_on_a_poisoned_gap() {
        let (_d, path) = tmp("sd-poison.wf");
        let log = FrameLog::create(&path).unwrap();
        log.set_write_offset_for_test(FILE_HDR_SIZE + FRAME_SIZE);
        log.mark_poison(FILE_HDR_SIZE); // a failed append below the target
        assert!(log.sync_to_durable().is_err());
    }

    #[test]
    fn reopen_inits_watermarks_to_valid_prefix_end() {
        let (_d, path) = tmp("wm-reopen.wf");
        {
            let log = FrameLog::create(&path).unwrap();
            log.append(2, 1, 4, &page(1)).unwrap();
            log.sync().unwrap();
        }
        let log = FrameLog::open(&path).unwrap();
        let end = FILE_HDR_SIZE + FRAME_SIZE;
        assert_eq!(log.contiguous_written_offset(), end);
        assert_eq!(log.durable_offset(), end);
    }

    #[test]
    fn recycle_resets_to_empty_with_a_fresh_salt() {
        let (_d, path) = tmp("recycle.wf");
        let log = FrameLog::create(&path).unwrap();
        let old_salt = log.salt();
        log.append(2, 1, 7, &page(1)).unwrap();
        log.sync_to_durable().unwrap();
        log.recycle(RecycleMode::Reuse).unwrap();
        assert_ne!(
            log.salt(),
            old_salt,
            "fresh salt invalidates any stale tail"
        );
        assert_eq!(log.scan().unwrap().len(), 0, "log is empty after recycle");
        assert_eq!(log.contiguous_written_offset(), FILE_HDR_SIZE);
        assert_eq!(log.durable_offset(), FILE_HDR_SIZE);
        // New appends after a recycle are scannable under the fresh salt.
        log.append(3, 2, 8, &page(2)).unwrap();
        assert_eq!(log.scan().unwrap().len(), 1);
    }

    #[test]
    fn recycle_reuse_keeps_size_truncate_shrinks() {
        let (_d, path) = tmp("reuse_size.wf");
        let log = FrameLog::create(&path).unwrap();
        let salt0 = log.salt();
        for i in 0..4u64 {
            log.append(i + 2, i + 1, 0, &page((i & 0xFF) as u8))
                .unwrap();
        }
        log.sync_to_durable().unwrap();
        let grown = std::fs::metadata(&path).unwrap().len();
        assert!(
            grown > FILE_HDR_SIZE,
            "frames grew the file past the header"
        );

        // Reuse: blocks stay allocated (size preserved); the fresh salt empties scan.
        log.recycle(RecycleMode::Reuse).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            grown,
            "reuse keeps the file at its high-water size"
        );
        assert!(
            log.scan().unwrap().is_empty(),
            "fresh salt ⇒ old frames stale"
        );
        assert_ne!(log.salt(), salt0, "recycle bumps the salt");

        // Truncate: reclaim disk down to the header.
        for i in 0..4u64 {
            log.append(i + 2, i + 1, 0, &page((i & 0xFF) as u8))
                .unwrap();
        }
        log.sync_to_durable().unwrap();
        log.recycle(RecycleMode::Truncate).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            FILE_HDR_SIZE,
            "truncate reclaims the blocks"
        );
        assert!(log.scan().unwrap().is_empty());
    }

    #[test]
    fn recycle_reuse_uses_a_fresh_salt_each_cycle() {
        let (_d, path) = tmp("salt_cycles.wf");
        let log = FrameLog::create(&path).unwrap();
        let s0 = log.salt();
        log.recycle(RecycleMode::Reuse).unwrap();
        let s1 = log.salt();
        log.recycle(RecycleMode::Reuse).unwrap();
        let s2 = log.salt();
        assert_ne!(s0, s1, "first recycle bumps the salt");
        assert_ne!(s1, s2, "second recycle bumps it again");
        assert_ne!(s0, s2, "no salt is reused across cycles");
    }

    #[test]
    fn reuse_roundtrip_scan_sees_only_new_frames() {
        let (_d, path) = tmp("roundtrip.wf");
        let log = FrameLog::create(&path).unwrap();
        for i in 0..6u64 {
            log.append(i + 2, i + 1, 0, &page((i & 0xFF) as u8))
                .unwrap();
        }
        log.sync_to_durable().unwrap();
        let size_before = std::fs::metadata(&path).unwrap().len();

        log.recycle(RecycleMode::Reuse).unwrap();
        assert!(
            log.scan().unwrap().is_empty(),
            "fresh salt empties the scan"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            size_before,
            "reuse keeps the file size"
        );

        // Append FEWER frames than before; scan must stop at the first leftover old frame.
        for i in 0..3u64 {
            log.append(i + 10, i + 100, 0, &page((i & 0xFF) as u8))
                .unwrap();
        }
        let frames = log.scan().unwrap();
        assert_eq!(
            frames.len(),
            3,
            "scan stops at the first leftover old-salt frame"
        );
        assert_eq!(frames[0].page_id, 10);
        assert_eq!(frames[2].lsn, 102);
    }

    #[test]
    fn reuse_does_not_resurrect_stale_frames_across_shrinking_cycles() {
        let (_d, path) = tmp("shrink_cycles.wf");
        let log = FrameLog::create(&path).unwrap();
        // cycle 1: 10 frames (page byte 1), cycle 2: 3 (byte 2), cycle 3: 5 (byte 3).
        for i in 0..10u64 {
            log.append(i + 1, i + 1, 0, &page(1)).unwrap();
        }
        log.recycle(RecycleMode::Reuse).unwrap();
        for i in 0..3u64 {
            log.append(i + 1, i + 1, 0, &page(2)).unwrap();
        }
        log.recycle(RecycleMode::Reuse).unwrap();
        for i in 0..5u64 {
            log.append(i + 1, i + 1, 0, &page(3)).unwrap();
        }
        let frames = log.scan().unwrap();
        assert_eq!(
            frames.len(),
            5,
            "only the current cycle's frames are visible"
        );
        for f in &frames {
            assert_eq!(
                log.read_page_at(f.offset).unwrap()[0],
                3,
                "no frame from cycle 1/2 resurrects"
            );
        }
    }

    #[test]
    fn torn_write_after_reuse_stops_scan_at_crc_not_leftover() {
        use std::os::unix::fs::FileExt;
        let (_d, path) = tmp("torn_reuse.wf");
        let log = FrameLog::create(&path).unwrap();
        for i in 0..5u64 {
            log.append(i + 1, i + 1, 0, &page(1)).unwrap(); // leftovers beyond the new prefix
        }
        log.recycle(RecycleMode::Reuse).unwrap();
        for i in 0..3u64 {
            log.append(i + 1, i + 1, 0, &page(2)).unwrap(); // 3 new (fresh salt)
        }
        // Corrupt the page bytes of new frame index 1 → its crc fails.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let corrupt_off = FILE_HDR_SIZE + FRAME_SIZE + FRAME_HDR_SIZE as u64;
        f.write_all_at(&[0xFF], corrupt_off).unwrap();
        f.sync_all().unwrap();
        let frames = log.scan().unwrap();
        assert_eq!(
            frames.len(),
            1,
            "scan stops at the corrupted frame, ignoring any leftover frame beyond it"
        );
    }

    #[test]
    fn reopen_after_reuse_reads_the_new_prefix() {
        let (_d, path) = tmp("reopen_reuse.wf");
        {
            let log = FrameLog::create(&path).unwrap();
            for i in 0..5u64 {
                log.append(i + 1, i + 1, 0, &page(1)).unwrap();
            }
            log.recycle(RecycleMode::Reuse).unwrap();
            for i in 0..3u64 {
                log.append(i + 10, i + 100, 7, &page(2)).unwrap();
            }
            log.sync_to_durable().unwrap();
        }
        let log2 = FrameLog::open(&path).unwrap();
        let frames = log2.scan().unwrap();
        assert_eq!(
            frames.len(),
            3,
            "reopen reads exactly the post-reuse prefix"
        );
        assert_eq!(frames[0].page_id, 10);
        assert_eq!(
            log2.contiguous_written_offset(),
            FILE_HDR_SIZE + 3 * FRAME_SIZE
        );
        assert_eq!(log2.durable_offset(), FILE_HDR_SIZE + 3 * FRAME_SIZE);
    }

    #[test]
    fn read_page_if_for_verifies_page_and_salt() {
        let (_d, path) = tmp("rif.wf");
        let log = FrameLog::create(&path).unwrap();
        let off = log.append(7, 1, 3, &page(0xAB)).unwrap();
        // Matching page_id under the current salt → Some.
        assert_eq!(log.read_page_if_for(off, 7).unwrap().unwrap()[0], 0xAB);
        // Wrong page_id (a stale offset reused for another page) → None.
        assert!(log.read_page_if_for(off, 8).unwrap().is_none());
        // After a recycle the salt changes → the old offset is stale. Even with Reuse
        // (which keeps the file), read_page_if_for rejects the leftover frame by salt.
        log.recycle(RecycleMode::Reuse).unwrap();
        assert!(log.read_page_if_for(off, 7).unwrap().is_none());
    }

    #[test]
    fn append_is_lock_free_under_concurrency() {
        use std::sync::Arc;
        let (_d, path) = tmp("concurrent.wf");
        let log = Arc::new(FrameLog::create(&path).unwrap());
        let threads = 8u64;
        let per_thread = 50u64;
        let mut handles = Vec::new();
        for t in 0..threads {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                for i in 0..per_thread {
                    let pid = t * per_thread + i;
                    log.append(pid, pid + 1, 0, &page((pid & 0xFF) as u8))
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Every reserved slot was written intact: scan validates each frame's crc.
        let frames = log.scan().unwrap();
        assert_eq!(
            frames.len() as u64,
            threads * per_thread,
            "all frames intact and crc-valid after concurrent appends"
        );
        // Offsets are disjoint (no two writers landed on the same slot).
        let mut offsets: Vec<u64> = frames.iter().map(|f| f.offset).collect();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(
            offsets.len() as u64,
            threads * per_thread,
            "every append reserved a unique offset"
        );
    }

    #[test]
    fn record_is_sharded_under_concurrency() {
        use std::sync::Arc;
        let index = Arc::new(WalIndex::default());
        let threads = 8u64;
        let per_thread = 50u64;
        let mut handles = Vec::new();
        for t in 0..threads {
            let index = Arc::clone(&index);
            handles.push(std::thread::spawn(move || {
                for i in 0..per_thread {
                    let pid = t * 1000 + i; // disjoint page ranges per thread
                    index.record(FrameRef {
                        page_id: pid,
                        lsn: pid + 1,
                        txn_id: 0,
                        offset: pid * 64,
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(index.len() as u64, threads * per_thread);
        for t in 0..threads {
            for i in 0..per_thread {
                let pid = t * 1000 + i;
                assert_eq!(index.latest(pid).unwrap().lsn, pid + 1);
            }
        }
    }
}
