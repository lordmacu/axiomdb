//! Page-frame WAL log — project B (Option A: SQLite-WAL pure write-ahead REDO).
//!
//! This module is the **isolated foundation** for write-ahead page logging: a frame
//! log we can append page images to, read back (stopping at a torn tail), and index
//! (`page_id` → latest committed frame). It does NOT touch the live read/write path
//! yet — subphase 3 wires `write_page`/`read_page` to it.
//!
//! Format borrows SQLite `wal.c` (file header + per-frame header + page), adapted to
//! our explicit `lsn`:
//! - **commit_marker** (SQLite's `nTruncate` idea): 0 on a non-commit frame; nonzero
//!   (the committing `txn_id`) on the LAST frame of a committed txn.
//! - **salt** copied from the file header into every frame — frames whose salt ≠ the
//!   run's salt are stale (a previous WAL run) and end the valid prefix.
//! - **frame_crc** (crc32c over the frame header sans crc ++ page bytes): a torn /
//!   partially-written frame fails the crc → marks the end of the valid prefix
//!   (append-only ⇒ damage is always at the tail).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use axiomdb_core::error::{classify_io, DbError};

use crate::page::PAGE_SIZE;

const MAGIC: u64 = 0x4158_494f_4d5f_5746; // "AXIOM_WF"
const VERSION: u32 = 1;

/// File header: magic(8) version(4) page_size(4) salt(8) header_crc(4) _pad(4).
const FILE_HDR_SIZE: u64 = 32;
/// Frame header: page_id(8) lsn(8) commit_marker(4) salt(8) frame_crc(4).
const FRAME_HDR_SIZE: usize = 32;
/// crc covers the first 28 header bytes (everything but the crc field) ++ page.
const FRAME_HDR_CRC_PREFIX: usize = 28;
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
    /// 0 = non-commit frame; nonzero = committing txn_id (last frame of the txn).
    pub commit_marker: u32,
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
/// after the last commit marker (recovery).
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
    salt: u64,
    /// Offset where the next frame will be appended. Reserved via `fetch_add`.
    write_offset: AtomicU64,
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
            salt,
            write_offset: AtomicU64::new(FILE_HDR_SIZE),
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
            salt,
            write_offset: AtomicU64::new(FILE_HDR_SIZE),
        };
        // Advance write_offset past the valid prefix.
        let frames = log.scan()?;
        if let Some(last) = frames.last() {
            log.write_offset
                .store(last.offset + FRAME_SIZE, Ordering::Relaxed);
        }
        Ok(log)
    }

    /// Appends one page frame and returns its byte offset. Does NOT fsync (call
    /// [`sync`](Self::sync) at the commit boundary).
    pub fn append(
        &self,
        page_id: u64,
        lsn: u64,
        commit_marker: u32,
        page: &[u8; PAGE_SIZE],
    ) -> Result<u64, DbError> {
        let mut hdr = [0u8; FRAME_HDR_SIZE];
        hdr[0..8].copy_from_slice(&page_id.to_le_bytes());
        hdr[8..16].copy_from_slice(&lsn.to_le_bytes());
        hdr[16..20].copy_from_slice(&commit_marker.to_le_bytes());
        hdr[20..28].copy_from_slice(&self.salt.to_le_bytes());
        let crc = frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], page);
        hdr[28..32].copy_from_slice(&crc.to_le_bytes());

        // Lock-free: reserve this frame's slot, then pwrite into it. Two writers get
        // disjoint offsets, so the writes never overlap and need no lock.
        let offset = self.write_offset.fetch_add(FRAME_SIZE, Ordering::Relaxed);
        self.file
            .write_all_at(&hdr, offset)
            .map_err(|e| classify_io(e, "frame log write header"))?;
        self.file
            .write_all_at(page, offset + FRAME_HDR_SIZE as u64)
            .map_err(|e| classify_io(e, "frame log write page"))?;
        Ok(offset)
    }

    /// Flushes appended frames durably (fdatasync).
    pub fn sync(&self) -> Result<(), DbError> {
        self.file
            .sync_data()
            .map_err(|e| classify_io(e, "frame log sync"))
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
            let salt = le_u64(&hdr[20..28]);
            if salt != self.salt {
                break; // stale frame from a previous run
            }
            let stored_crc = le_u32(&hdr[28..32]);
            if frame_crc(&hdr[0..FRAME_HDR_CRC_PREFIX], page) != stored_crc {
                break; // torn / corrupt frame — end of valid prefix
            }
            frames.push(FrameRef {
                page_id: le_u64(&hdr[0..8]),
                lsn: le_u64(&hdr[8..16]),
                commit_marker: le_u32(&hdr[16..20]),
                offset,
            });
            offset += FRAME_SIZE;
        }
        Ok(frames)
    }

    /// Builds the page → latest-committed-frame index. Frames after the last commit
    /// marker (an unfinished txn's tail) are excluded.
    pub fn build_index(&self) -> Result<WalIndex, DbError> {
        let frames = self.scan()?;
        // Find the last committed frame (highest offset with commit_marker != 0).
        let last_commit_pos = frames.iter().rposition(|f| f.commit_marker != 0);
        let index = WalIndex::default();
        let Some(end) = last_commit_pos else {
            return Ok(index); // nothing committed yet
        };
        for f in &frames[..=end] {
            index.record(*f); // latest wins (in-order scan)
        }
        index.set_last_commit_lsn(frames[end].lsn);
        Ok(index)
    }

    /// The run salt (test/diagnostic).
    pub fn salt(&self) -> u64 {
        self.salt
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
        assert_eq!(frames[1].commit_marker, 5);
    }

    #[test]
    fn index_keeps_latest_committed_per_page() {
        let (_d, path) = tmp("idx.wf");
        let log = FrameLog::create(&path).unwrap();
        // txn1 commits: page 2 (v1) + page 3 (v1), commit on the last frame.
        log.append(2, 1, 0, &page(0x10)).unwrap();
        log.append(3, 2, 11, &page(0x20)).unwrap();
        // txn2 commits: page 2 again (v2).
        let off_p2_v2 = log.append(2, 3, 12, &page(0x11)).unwrap();
        let idx = log.build_index().unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.latest(2).unwrap().offset, off_p2_v2); // latest wins
        assert_eq!(idx.latest(2).unwrap().lsn, 3);
        assert_eq!(idx.latest(3).unwrap().lsn, 2);
        assert_eq!(idx.last_commit_lsn(), 3);
    }

    #[test]
    fn index_excludes_uncommitted_tail() {
        let (_d, path) = tmp("uncommitted.wf");
        let log = FrameLog::create(&path).unwrap();
        log.append(2, 1, 9, &page(1)).unwrap(); // committed txn (marker on its frame)
        log.append(3, 2, 0, &page(2)).unwrap(); // uncommitted: no commit marker after it
        let idx = log.build_index().unwrap();
        assert_eq!(idx.len(), 1, "only the committed page is indexed");
        assert!(idx.latest(2).is_some());
        assert!(idx.latest(3).is_none(), "uncommitted tail page excluded");
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
                        commit_marker: 0,
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
