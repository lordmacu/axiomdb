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

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use axiomdb_core::error::{classify_io, DbError};

use crate::page::PAGE_SIZE;

const MAGIC: u64 = 0x4158_494f_4d5f_5746; // "AXIOM_WF"
const VERSION: u32 = 1;

/// File header: magic(8) version(4) page_size(4) salt(8) header_crc(4) _pad(4).
const FILE_HDR_SIZE: u64 = 32;
/// Frame header: page_id(8) lsn(8) txn_id(8) salt(8) frame_crc(4).
const FRAME_HDR_SIZE: usize = 36;
/// crc covers the first 32 header bytes (everything but the crc field) ++ page.
const FRAME_HDR_CRC_PREFIX: usize = 32;
/// One frame on disk: header + a full page image.
const FRAME_SIZE: u64 = FRAME_HDR_SIZE as u64 + PAGE_SIZE as u64;

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
}

/// Bookkeeping for the gap-free written prefix under concurrent lock-free appends.
/// Offsets are frame START boundaries; "prefix ends at X" ⇔ every frame with START < X
/// has finished its `pwrite`.
struct SyncState {
    /// Completed frame START offsets not yet folded into `contiguous_written` (i.e. a
    /// higher frame finished before a lower one — the out-of-order reorder buffer).
    completed: BTreeSet<u64>,
    /// Gap-free written prefix == the next expected (lowest un-written) START offset.
    contiguous_written: u64,
    /// Lowest START offset whose append `pwrite` failed (a permanent gap), if any.
    poison: Option<u64>,
}

impl SyncState {
    fn new(start: u64) -> Self {
        SyncState {
            completed: BTreeSet::new(),
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
        if le_u32(&hdr[8..12]) != VERSION {
            return Err(DbError::Other(format!(
                "frame log: unsupported version {}",
                le_u32(&hdr[8..12])
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
        };
        // The valid prefix on disk is already durable: set every watermark to its end.
        let frames = log.scan()?;
        let end = frames
            .last()
            .map(|f| f.offset + FRAME_SIZE)
            .unwrap_or(FILE_HDR_SIZE);
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
                self.mark_written(offset);
                Ok(offset)
            }
            Err(e) => {
                // Poison the slot: a committer waiting past it must error, not deadlock.
                self.mark_poison(offset);
                Err(classify_io(e, "frame log write"))
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

    /// Records that the frame at START `offset` finished its `pwrite`, folding it into the
    /// contiguous-written prefix (advancing over any now-contiguous earlier completions).
    fn mark_written(&self, offset: u64) {
        {
            let mut s = self.sync_state.lock().unwrap_or_else(|e| e.into_inner());
            s.completed.insert(offset);
            let mut cw = s.contiguous_written;
            while s.completed.remove(&cw) {
                cw += FRAME_SIZE;
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
            self.file
                .sync_data()
                .map_err(|e| classify_io(e, "frame log sync"))?;
            self.durable.fetch_max(cw, Ordering::Release);
        }
        Ok(())
    }

    /// Recycles the log after a checkpoint: rewrites the header with a FRESH salt,
    /// truncates to the header, fsyncs, and resets the offset watermarks. The caller MUST
    /// hold the exclusive checkpoint guard (no concurrent appends). Does NOT touch any
    /// external monotonic LSN counter — the engine's `frame_lsn` must keep increasing, or
    /// the pageLSN redo guard would wrongly skip a frame appended after the recycle.
    pub fn recycle(&self) -> Result<(), DbError> {
        let salt = fresh_salt();
        let mut hdr = [0u8; FILE_HDR_SIZE as usize];
        hdr[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        hdr[16..24].copy_from_slice(&salt.to_le_bytes());
        let hcrc = crc32c::crc32c(&hdr[0..24]);
        hdr[24..28].copy_from_slice(&hcrc.to_le_bytes());
        self.file
            .set_len(FILE_HDR_SIZE)
            .map_err(|e| classify_io(e, "frame log recycle truncate"))?;
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

    /// Scans the valid prefix: every frame whose salt matches the run AND whose crc
    /// verifies. Stops at the first invalid/partial frame (the torn tail).
    pub fn scan(&self) -> Result<Vec<FrameRef>, DbError> {
        let file_len = self
            .file
            .metadata()
            .map_err(|e| classify_io(e, "frame log metadata"))?
            .len();
        let mut frames = Vec::new();
        let mut offset = FILE_HDR_SIZE;
        let mut buf = vec![0u8; FRAME_SIZE as usize];
        while offset + FRAME_SIZE <= file_len {
            self.file
                .read_exact_at(&mut buf, offset)
                .map_err(|e| classify_io(e, "frame log scan read"))?;
            let (hdr, page) = buf.split_at(FRAME_HDR_SIZE);
            let salt = le_u64(&hdr[24..32]);
            if salt != self.salt.load(Ordering::Relaxed) {
                break; // stale frame from a previous run
            }
            let stored_crc = le_u32(&hdr[32..36]);
            if frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], page) != stored_crc {
                break; // torn / corrupt frame — end of valid prefix
            }
            frames.push(FrameRef {
                page_id: le_u64(&hdr[0..8]),
                lsn: le_u64(&hdr[8..16]),
                txn_id: le_u64(&hdr[16..24]),
                offset,
            });
            offset += FRAME_SIZE;
        }
        Ok(frames)
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
        log.mark_written(h + FRAME_SIZE);
        assert_eq!(
            log.contiguous_written_offset(),
            h,
            "gap must not be skipped"
        );
        // Filling the gap folds both in one step.
        log.mark_written(h);
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
        log.mark_written(h + FRAME_SIZE);

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
        log.mark_written(h); // fill the gap → the waiter proceeds
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
        log.recycle().unwrap();
        assert_ne!(log.salt(), old_salt, "fresh salt invalidates any stale tail");
        assert_eq!(log.scan().unwrap().len(), 0, "log is empty after recycle");
        assert_eq!(log.contiguous_written_offset(), FILE_HDR_SIZE);
        assert_eq!(log.durable_offset(), FILE_HDR_SIZE);
        // New appends after a recycle are scannable under the fresh salt.
        log.append(3, 2, 8, &page(2)).unwrap();
        assert_eq!(log.scan().unwrap().len(), 1);
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
