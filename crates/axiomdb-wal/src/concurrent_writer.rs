//! ConcurrentWalWriter — thread-safe WAL writer with group commit.
//!
//! ## Design
//!
//! Replaces [`WalWriter`] inside [`TxnManager`] to enable multiple concurrent
//! transactions to append WAL entries without serializing on a single `&mut self`.
//!
//! Inspired by InnoDB's group commit:
//! - **Lock-free LSN reservation**: `next_lsn.fetch_add(n, Relaxed)` — ~10 ns per call.
//! - **Per-transaction scratch buffer**: callers serialize entries into their own
//!   `Vec<u8>` (already exists as `TxnManager::wal_scratch`), then submit the
//!   pre-serialized bytes to the write queue.
//! - **Write queue** (`Mutex<WriteQueue>`): transactions submit entries here.
//!   The lock is held only for a `Vec::push` — microseconds.
//! - **Group commit leader** (`Mutex<WriterState>`): the thread that acquires this
//!   lock drains the entire queue, sorts by base LSN, writes all entries to the
//!   `BufWriter` in one pass, and fsyncs. One fsync covers N transactions.
//!
//! ## Lock ordering (deadlock prevention)
//!
//! ```text
//! RULE: queue_mutex < writer_mutex
//!
//! submit_entry:   acquires queue_mutex only            → safe
//! flush_and_sync: acquires writer_mutex → queue_mutex  → safe (writer > queue)
//!
//! NO function holds writer_mutex, releases it, then acquires queue_mutex
//! while another thread may hold queue_mutex waiting for writer_mutex.
//! ```
//!
//! ## WAL file format
//!
//! Identical to [`WalWriter`] — same 24-byte v2 header, same entry encoding.
//! Recovery logic is unchanged (sequential LSN scan).
//!
//! [`WalWriter`]: crate::writer::WalWriter
//! [`TxnManager`]: crate::txn::TxnManager

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use axiomdb_core::error::{classify_io, DbError};

use crate::entry::WalEntry;
use crate::sync::{sync_wal_data, ResolvedWalSyncMethod, WalSyncMethod};
use crate::writer::{
    read_and_verify_header, round_up, scan_valid_tail, write_header, BUF_CAPACITY, PREALLOC_CHUNK,
    WAL_HEADER_SIZE,
};

// ── Internal structures ───────────────────────────────────────────────────────

/// Serialized WAL entries waiting to be written to disk.
///
/// Entries arrive out of order when multiple transactions submit concurrently.
/// The flush leader sorts by `base_lsn` before writing to ensure the on-disk
/// file always contains entries in LSN order.
struct WriteQueue {
    /// `(base_lsn, serialized_bytes)` per submitted entry or batch.
    ///
    /// `base_lsn` is the LSN of the FIRST entry in `serialized_bytes`.
    /// For single-entry submits: `base_lsn == entry.lsn`.
    /// For batch submits: `base_lsn == reserve_lsns(n)` result.
    entries: Vec<(u64, Vec<u8>)>,
    /// Total bytes across all pending entries — used for pre-allocation.
    total_bytes: usize,
}

impl WriteQueue {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(16),
            total_bytes: 0,
        }
    }

    fn push(&mut self, base_lsn: u64, bytes: Vec<u8>) {
        self.total_bytes += bytes.len();
        self.entries.push((base_lsn, bytes));
    }

    /// Drains all pending entries and returns them sorted by base_lsn.
    fn drain_sorted(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut entries = std::mem::take(&mut self.entries);
        self.total_bytes = 0;
        // Sort ensures on-disk LSN order even when concurrent writers submit
        // out of order (InnoDB applies the same sort in the log buffer copy).
        entries.sort_unstable_by_key(|(lsn, _)| *lsn);
        entries
    }
}

/// Mutable I/O state — only the group commit leader holds this lock.
///
/// Lock is held for the duration of: drain queue + write entries + fsync.
/// Typical hold time: ~3–5 ms (dominated by fsync latency).
struct WriterState {
    writer: BufWriter<File>,
    logical_end: u64,
    reserved_end: u64,
    dml_sync_method: ResolvedWalSyncMethod,
}

impl WriterState {
    /// Extends the file reservation if `required_end` exceeds the current reserved size.
    ///
    /// Pre-allocates in `PREALLOC_CHUNK` increments to avoid per-commit metadata syncs.
    fn ensure_capacity(&mut self, required_end: u64) -> Result<(), DbError> {
        if required_end <= self.reserved_end {
            return Ok(());
        }
        self.writer
            .flush()
            .map_err(|e| classify_io(e, "wal reserve flush"))?;
        let new_reserved_end = round_up(required_end, PREALLOC_CHUNK);
        let file = self.writer.get_mut();
        file.set_len(new_reserved_end)
            .map_err(|e| classify_io(e, "wal reserve grow"))?;
        file.sync_all()
            .map_err(|e| classify_io(e, "wal reserve sync"))?;
        file.seek(SeekFrom::Start(self.logical_end))
            .map_err(|e| classify_io(e, "wal reserve seek"))?;
        self.reserved_end = new_reserved_end;
        Ok(())
    }

    /// Writes all `entries` (already sorted by base_lsn) to the `BufWriter`.
    fn write_entries(&mut self, entries: &[(u64, Vec<u8>)]) -> Result<(), DbError> {
        if entries.is_empty() {
            return Ok(());
        }
        let total_bytes: u64 = entries.iter().map(|(_, b)| b.len() as u64).sum();
        self.ensure_capacity(self.logical_end + total_bytes)?;
        for (_, bytes) in entries {
            self.writer
                .write_all(bytes)
                .map_err(|e| classify_io(e, "wal write entries"))?;
            self.logical_end += bytes.len() as u64;
        }
        Ok(())
    }
}

// ── ConcurrentWalWriter ───────────────────────────────────────────────────────

/// Thread-safe, group-commit WAL writer.
///
/// All public methods take `&self`. Multiple `TxnManager` instances (one per
/// connection) can share one `Arc<ConcurrentWalWriter>`.
///
/// ## Invariants
///
/// - `next_lsn` is strictly monotonically increasing across all callers.
/// - Entries in the WAL file are always in LSN order (guaranteed by sort-before-write).
/// - `flushed_lsn.load(Acquire)` is the highest LSN known to be durable on disk.
///
/// ## Thread safety
///
/// - LSN reservation: lock-free via `AtomicU64::fetch_add`.
/// - Entry submission: `Mutex<WriteQueue>` held for a single `Vec::push` (~1 µs).
/// - Flush (write + fsync): `Mutex<WriterState>` held for the I/O duration (~3–5 ms).
/// - Concurrent writers to different LSRs are fully parallel; the flush serializes
///   them only at the I/O boundary.
pub struct ConcurrentWalWriter {
    /// Next LSN to assign. Lock-free reservation via `fetch_add`.
    next_lsn: AtomicU64,
    /// Highest LSN confirmed durable on disk. Updated by the flush leader.
    flushed_lsn: AtomicU64,
    /// Pending serialized entries, submitted by any thread.
    queue: Mutex<WriteQueue>,
    /// Disk I/O state — only held by the group commit leader.
    writer: Mutex<WriterState>,
}

impl ConcurrentWalWriter {
    // ── Construction ─────────────────────────────────────────────────────────

    /// Creates a new WAL file at `path` (fresh database).
    ///
    /// Fails if the file already exists. Writes the 24-byte v2 header and
    /// pre-allocates the first `PREALLOC_CHUNK` bytes.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let dml_sync_method = WalSyncMethod::Auto.resolve()?;
        let mut file = File::create_new(path)?;
        write_header(&mut file, 0)?;
        let logical_end = WAL_HEADER_SIZE as u64;
        let reserved_end = round_up(logical_end, PREALLOC_CHUNK);
        file.set_len(reserved_end)
            .map_err(|e| classify_io(e, "wal reserve grow"))?;
        file.seek(SeekFrom::Start(logical_end))
            .map_err(|e| classify_io(e, "wal reserve seek"))?;
        file.sync_all()
            .map_err(|e| classify_io(e, "wal create sync"))?;

        Ok(Self {
            next_lsn: AtomicU64::new(1),
            flushed_lsn: AtomicU64::new(0),
            queue: Mutex::new(WriteQueue::new()),
            writer: Mutex::new(WriterState {
                writer: BufWriter::with_capacity(BUF_CAPACITY, file),
                logical_end,
                reserved_end,
                dml_sync_method,
            }),
        })
    }

    /// Opens an existing WAL file to continue writing.
    ///
    /// Verifies the magic header and version. Computes `next_lsn` from the
    /// header's `start_lsn` and the last scanned entry, preserving monotonicity
    /// after WAL rotation.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let dml_sync_method = WalSyncMethod::Auto.resolve()?;
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;

        let start_lsn = read_and_verify_header(&mut file, path)?;
        let tail = scan_valid_tail(&mut file)?;

        let next_lsn = tail.last_lsn.max(start_lsn) + 1;
        file.seek(SeekFrom::Start(tail.logical_end))
            .map_err(|e| classify_io(e, "wal reopen seek logical end"))?;

        Ok(Self {
            next_lsn: AtomicU64::new(next_lsn),
            flushed_lsn: AtomicU64::new(tail.last_lsn),
            queue: Mutex::new(WriteQueue::new()),
            writer: Mutex::new(WriterState {
                writer: BufWriter::with_capacity(BUF_CAPACITY, file),
                logical_end: tail.logical_end,
                reserved_end: tail.reserved_end,
                dml_sync_method,
            }),
        })
    }

    /// Truncates the WAL file to just the v2 header with `start_lsn`.
    ///
    /// Static method — does not require a `ConcurrentWalWriter` instance.
    /// Used by WAL rotation after a successful checkpoint.
    pub fn rotate_file(path: &Path, start_lsn: u64) -> Result<(), DbError> {
        let mut file = OpenOptions::new().write(true).open(path)?;
        let logical_end = WAL_HEADER_SIZE as u64;
        let reserved_end = round_up(logical_end, PREALLOC_CHUNK);
        file.set_len(0)
            .map_err(|e| classify_io(e, "wal rotate truncate"))?;
        write_header(&mut file, start_lsn)?;
        file.set_len(reserved_end)
            .map_err(|e| classify_io(e, "wal rotate reserve"))?;
        file.seek(SeekFrom::Start(logical_end))
            .map_err(|e| classify_io(e, "wal rotate seek"))?;
        file.sync_all()
            .map_err(|e| classify_io(e, "wal rotate sync"))?;
        Ok(())
    }

    // ── LSN reservation ───────────────────────────────────────────────────────

    /// Reserves a single LSN atomically. Lock-free — ~10 ns.
    #[inline]
    pub fn reserve_lsn(&self) -> u64 {
        self.next_lsn.fetch_add(1, Ordering::Relaxed)
    }

    /// Reserves `n` consecutive LSNs atomically and returns the first one.
    ///
    /// Used by batch writers that pre-assign LSNs before serializing entries.
    /// `fetch_add(n, Relaxed)` returns the PREVIOUS value (= first reserved LSN).
    #[inline]
    pub fn reserve_lsns(&self, n: usize) -> u64 {
        self.next_lsn.fetch_add(n as u64, Ordering::Relaxed)
    }

    /// Returns the last assigned LSN (highest LSN ever reserved). `0` if none.
    #[inline]
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// Returns the highest LSN confirmed durable on disk.
    #[inline]
    pub fn flushed_lsn(&self) -> u64 {
        self.flushed_lsn.load(Ordering::Acquire)
    }

    // ── Entry submission ──────────────────────────────────────────────────────

    /// Assigns the next LSN to `entry` and enqueues it.
    ///
    /// Entries are NOT written to disk until [`commit`](Self::commit),
    /// [`commit_data_sync`](Self::commit_data_sync), or
    /// [`flush_no_sync`](Self::flush_no_sync) is called.
    ///
    /// Returns the LSN assigned to the entry.
    pub fn append(&self, entry: &mut WalEntry) -> Result<u64, DbError> {
        let lsn = self.reserve_lsn();
        entry.lsn = lsn;
        let bytes = entry.to_bytes();
        self.enqueue(lsn, bytes);
        Ok(lsn)
    }

    /// Like [`append`] but uses a **caller-provided scratch buffer** to serialize,
    /// avoiding a per-entry heap allocation in the hot path.
    ///
    /// The scratch buffer is cleared before serialization; its capacity is
    /// retained across calls (LMDB-inspired zero-alloc pattern).
    ///
    /// Returns the LSN assigned to the entry.
    pub fn append_with_buf(
        &self,
        entry: &mut WalEntry,
        scratch: &mut Vec<u8>,
    ) -> Result<u64, DbError> {
        let lsn = self.reserve_lsn();
        entry.lsn = lsn;
        scratch.clear();
        entry.serialize_into(scratch);
        self.enqueue(lsn, scratch.to_vec());
        Ok(lsn)
    }

    /// Enqueues a pre-serialized batch of WAL entries (already LSN-stamped).
    ///
    /// `base_lsn` is the LSN of the FIRST entry in `batch`. Must equal the value
    /// returned by the preceding [`reserve_lsns`](Self::reserve_lsns) call for
    /// this batch. Used for ordering when multiple batches arrive concurrently.
    ///
    /// All N entries in the batch are submitted in a single `Mutex` acquisition.
    pub fn write_batch(&self, base_lsn: u64, batch: &[u8]) -> Result<(), DbError> {
        if batch.is_empty() {
            return Ok(());
        }
        self.enqueue(base_lsn, batch.to_vec());
        Ok(())
    }

    // ── Flush / commit ────────────────────────────────────────────────────────

    /// Flushes the buffer to the OS and fsyncs — guarantees on-disk durability.
    ///
    /// Equivalent to [`commit_data_sync`](Self::commit_data_sync).
    pub fn commit(&self) -> Result<(), DbError> {
        self.flush_and_sync(false)
    }

    /// Drains the queue, writes all pending entries, and data-syncs the file.
    ///
    /// Called by the fsync pipeline leader or by immediate-mode commits.
    /// Covers all entries accumulated since the last flush (group commit effect).
    pub fn commit_data_sync(&self) -> Result<(), DbError> {
        self.flush_and_sync(false)
    }

    /// Drains the queue, writes all pending entries, and metadata-syncs the file.
    ///
    /// Used only when header writes or reservation-boundary growth changed WAL
    /// file metadata that subsequent data-sync commits rely on.
    pub fn commit_metadata_sync(&self) -> Result<(), DbError> {
        self.flush_and_sync(true)
    }

    /// Drains the queue, writes all pending entries to the OS page cache, but
    /// does NOT fsync.
    ///
    /// Used for read-only transaction commits (no heap was modified) and for
    /// `WalDurabilityPolicy::Normal`. Not durable across power loss, but
    /// visible to recovery after a process crash (OS cache is shared).
    pub fn flush_no_sync(&self) -> Result<(), DbError> {
        let entries = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.drain_sorted()
        };
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_entries(&entries)?;
        w.writer
            .flush()
            .map_err(|e| classify_io(e, "wal flush no sync"))?;
        if let Some((max_lsn, _)) = entries.last() {
            self.flushed_lsn.fetch_max(*max_lsn, Ordering::Release);
        }
        Ok(())
    }

    /// Flushes the `BufWriter` buffer to the OS without fsync.
    ///
    /// Used in tests to simulate a process crash: flush buffer so entries are
    /// readable by the WAL scanner, then drop without committing.
    pub fn flush_buffer(&self) -> Result<(), DbError> {
        self.flush_no_sync()
    }

    /// Returns the current byte position in the file (header + written entries).
    pub fn file_offset(&self) -> u64 {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .logical_end
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Enqueues a single entry or batch into the write queue.
    ///
    /// `base_lsn` is used for ordering during flush. For single entries,
    /// `base_lsn == entry.lsn`; for batches, `base_lsn == lsn_base` from
    /// `reserve_lsns`.
    #[inline]
    fn enqueue(&self, base_lsn: u64, bytes: Vec<u8>) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push(base_lsn, bytes);
    }

    /// Group commit leader: drain queue → sort → write → flush → fsync.
    ///
    /// Lock ordering: writer_mutex acquired FIRST, then queue_mutex (briefly
    /// for the drain). This is the safe order per the module-level invariant.
    ///
    /// `metadata_sync`: if `true`, calls `sync_all()` (for header writes);
    /// if `false`, calls the steady-state `sync_wal_data()` (fdatasync or
    /// platform equivalent).
    fn flush_and_sync(&self, metadata_sync: bool) -> Result<(), DbError> {
        // Acquire writer lock — become the group commit leader.
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());

        // Drain queue under queue lock (brief — single swap).
        // LOCK ORDER: writer_mutex → queue_mutex (safe per module invariant).
        let entries = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.drain_sorted()
        };

        // Write all pending entries in one BufWriter pass.
        w.write_entries(&entries)?;

        // Flush BufWriter to OS page cache.
        w.writer
            .flush()
            .map_err(|e| classify_io(e, "wal commit flush"))?;

        // Durable sync — covers ALL entries accumulated since last flush
        // (InnoDB group commit: one fsync covers N concurrent transactions).
        if metadata_sync {
            w.writer
                .get_ref()
                .sync_all()
                .map_err(|e| classify_io(e, "wal commit metadata sync"))?;
        } else {
            sync_wal_data(w.writer.get_ref(), w.dml_sync_method)?;
        }

        // Advance flushed_lsn so followers can detect durability without locking.
        if let Some((max_lsn, _)) = entries.last() {
            self.flushed_lsn.fetch_max(*max_lsn, Ordering::Release);
        }

        Ok(())
    }
}

// ── Drop ──────────────────────────────────────────────────────────────────────

impl Drop for ConcurrentWalWriter {
    /// Best-effort flush on drop: drain the write queue into the `BufWriter`
    /// and flush to the OS page cache (no fsync).
    ///
    /// This mirrors the behaviour of the underlying `BufWriter<File>::drop`,
    /// which also only flushes to the OS without guaranteeing durability.
    /// Callers that need durability must call [`commit_data_sync`] explicitly
    /// before dropping.
    ///
    /// The flush is "best-effort": any I/O errors are silently ignored because
    /// `drop` cannot return a `Result`.  In a crash simulation test the data
    /// reaches the OS buffer (recoverable); in production, `commit_data_sync`
    /// has already been called before this path is reached.
    fn drop(&mut self) {
        let _ = self.flush_no_sync();
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{entry::EntryType, reader::WalReader};
    use tempfile::tempdir;

    fn scan_all(path: &std::path::Path) -> Vec<WalEntry> {
        WalReader::open(path)
            .unwrap()
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn make_entry(txn_id: u64, table_id: u32) -> WalEntry {
        WalEntry::new(
            0,
            txn_id,
            EntryType::Insert,
            table_id,
            b"key".to_vec(),
            vec![],
            vec![1, 2, 3],
        )
    }

    // ── Single-threaded correctness ───────────────────────────────────────────

    #[test]
    fn test_create_and_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = ConcurrentWalWriter::create(&path).unwrap();
        assert_eq!(w.current_lsn(), 0);
        drop(w);
        let w2 = ConcurrentWalWriter::open(&path).unwrap();
        assert_eq!(w2.current_lsn(), 0);
    }

    #[test]
    fn test_single_append_and_recover() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = ConcurrentWalWriter::create(&path).unwrap();
        let mut e = make_entry(1, 42);
        let lsn = w.append(&mut e).unwrap();
        assert_eq!(lsn, 1);
        w.commit().unwrap();

        let entries = scan_all(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lsn, 1);
        assert_eq!(entries[0].txn_id, 1);
    }

    #[test]
    fn test_batch_append_in_lsn_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = ConcurrentWalWriter::create(&path).unwrap();

        let mut scratch = Vec::new();
        let n = 5usize;
        let lsn_base = w.reserve_lsns(n);
        scratch.clear();
        for i in 0..n {
            let e = WalEntry::new(
                lsn_base + i as u64,
                1,
                EntryType::Insert,
                99,
                b"k".to_vec(),
                vec![],
                vec![i as u8],
            );
            e.serialize_into(&mut scratch);
        }
        w.write_batch(lsn_base, &scratch).unwrap();
        w.commit().unwrap();

        let entries = scan_all(&path);
        assert_eq!(entries.len(), n);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.lsn, lsn_base + i as u64);
        }
    }

    #[test]
    fn test_append_with_buf_zero_alloc() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = ConcurrentWalWriter::create(&path).unwrap();
        let mut scratch = Vec::with_capacity(64);
        for i in 1u64..=4 {
            let mut e = make_entry(i, 1);
            w.append_with_buf(&mut e, &mut scratch).unwrap();
        }
        w.commit_data_sync().unwrap();

        let entries = scan_all(&path);
        assert_eq!(entries.len(), 4);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.lsn, (i + 1) as u64);
        }
    }

    #[test]
    fn test_open_after_close_resumes_lsn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        {
            let w = ConcurrentWalWriter::create(&path).unwrap();
            let mut e = make_entry(1, 1);
            w.append(&mut e).unwrap();
            let mut e2 = make_entry(2, 1);
            w.append(&mut e2).unwrap();
            w.commit().unwrap();
        }
        let w2 = ConcurrentWalWriter::open(&path).unwrap();
        // next LSN must be > last written (2)
        assert!(w2.reserve_lsn() > 2);
    }

    #[test]
    fn test_rotate_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        {
            let w = ConcurrentWalWriter::create(&path).unwrap();
            let mut e = make_entry(1, 1);
            w.append(&mut e).unwrap();
            w.commit().unwrap();
        }
        ConcurrentWalWriter::rotate_file(&path, 100).unwrap();
        let w2 = ConcurrentWalWriter::open(&path).unwrap();
        // next_lsn = start_lsn (100) + 1 = 101
        assert_eq!(w2.reserve_lsn(), 101);
    }

    #[test]
    fn test_flush_no_sync_visible_to_reader() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = ConcurrentWalWriter::create(&path).unwrap();
        let mut e = make_entry(7, 1);
        w.append(&mut e).unwrap();
        w.flush_no_sync().unwrap();

        let entries = scan_all(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].txn_id, 7);
    }

    // ── Multi-threaded correctness ────────────────────────────────────────────

    #[test]
    fn test_concurrent_appends_all_lsns_present() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = Arc::new(ConcurrentWalWriter::create(&path).unwrap());

        const THREADS: usize = 4;
        const PER_THREAD: usize = 50;

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let w2 = Arc::clone(&w);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let mut e = WalEntry::new(
                        0,
                        (t * 100 + i) as u64,
                        EntryType::Insert,
                        t as u32,
                        b"k".to_vec(),
                        vec![],
                        vec![i as u8],
                    );
                    w2.append(&mut e).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        w.commit().unwrap();

        let entries = scan_all(&path);
        let total = THREADS * PER_THREAD;
        assert_eq!(
            entries.len(),
            total,
            "expected {total} entries, got {}",
            entries.len()
        );

        // All LSNs 1..=total must be present, no duplicates.
        let lsns: HashSet<u64> = entries.iter().map(|e| e.lsn).collect();
        assert_eq!(lsns.len(), total);
        for lsn in 1..=(total as u64) {
            assert!(lsns.contains(&lsn), "missing LSN {lsn}");
        }

        // File must be in LSN order.
        for w in entries.windows(2) {
            assert!(
                w[0].lsn < w[1].lsn,
                "out-of-order: {} >= {}",
                w[0].lsn,
                w[1].lsn
            );
        }
    }

    #[test]
    fn test_concurrent_batches_all_lsns_present() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = Arc::new(ConcurrentWalWriter::create(&path).unwrap());

        const THREADS: usize = 4;
        const BATCH_SIZE: usize = 25;

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let w2 = Arc::clone(&w);
            handles.push(std::thread::spawn(move || {
                let lsn_base = w2.reserve_lsns(BATCH_SIZE);
                let mut scratch = Vec::new();
                for i in 0..BATCH_SIZE {
                    let e = WalEntry::new(
                        lsn_base + i as u64,
                        (t * 100 + i) as u64,
                        EntryType::Insert,
                        t as u32,
                        b"k".to_vec(),
                        vec![],
                        vec![i as u8],
                    );
                    e.serialize_into(&mut scratch);
                }
                w2.write_batch(lsn_base, &scratch).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        w.commit().unwrap();

        let entries = scan_all(&path);
        let total = THREADS * BATCH_SIZE;
        assert_eq!(entries.len(), total);

        let lsns: HashSet<u64> = entries.iter().map(|e| e.lsn).collect();
        assert_eq!(lsns.len(), total);
        for lsn in 1..=(total as u64) {
            assert!(lsns.contains(&lsn), "missing LSN {lsn}");
        }

        for w in entries.windows(2) {
            assert!(
                w[0].lsn < w[1].lsn,
                "out-of-order: {} >= {}",
                w[0].lsn,
                w[1].lsn
            );
        }
    }

    #[test]
    fn test_no_duplicate_lsns_under_contention() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = Arc::new(ConcurrentWalWriter::create(&path).unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 125; // 8 × 125 = 1000 entries

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let w2 = Arc::clone(&w);
            handles.push(std::thread::spawn(move || {
                let mut scratch = Vec::new();
                for i in 0..PER_THREAD {
                    let mut e = WalEntry::new(
                        0,
                        (t * 1000 + i) as u64,
                        EntryType::Insert,
                        1,
                        b"k".to_vec(),
                        vec![],
                        vec![],
                    );
                    w2.append_with_buf(&mut e, &mut scratch).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        w.commit().unwrap();

        let entries = scan_all(&path);
        let lsns: HashSet<u64> = entries.iter().map(|e| e.lsn).collect();
        assert_eq!(lsns.len(), THREADS * PER_THREAD, "duplicate LSNs detected");
    }
}
