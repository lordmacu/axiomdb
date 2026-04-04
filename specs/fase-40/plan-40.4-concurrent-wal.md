# Plan: 40.4 — Concurrent WAL Writer

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-wal/src/concurrent_writer.rs` | **NEW**: ConcurrentWalWriter, WriteQueue |
| `crates/axiomdb-wal/src/writer.rs` | Keep existing WalWriter as fallback; extract shared logic |
| `crates/axiomdb-wal/src/txn.rs` | Use ConcurrentWalWriter; per-txn scratch in ConnectionTxn |
| `crates/axiomdb-wal/src/lib.rs` | Export new module |
| `crates/axiomdb-wal/src/fsync_pipeline.rs` | Extend for multi-writer group commit |
| `crates/axiomdb-wal/src/recovery.rs` | Verify recovery handles concurrent entries (LSN-ordered) |

## Implementation phases

### Phase 1: ConcurrentWalWriter struct
- AtomicU64 for next_lsn, flushed_lsn, logical_end, reserved_end
- Mutex<WriteQueue> for pending entries
- Mutex<BufWriter<File>> for disk I/O (only leader holds)
- Constructor: `ConcurrentWalWriter::open(path, sync_method) → Result`

### Phase 2: Lock-free LSN reservation
- `reserve_lsn(&self) → u64`: single `fetch_add(1, Relaxed)`
- `reserve_lsns(&self, n: usize) → u64`: `fetch_add(n, Relaxed)` returns base

### Phase 3: Submit entry to queue
- `submit_entry(&self, lsn: u64, serialized: Vec<u8>)`
- Acquires Mutex<WriteQueue>, pushes (lsn, bytes), releases
- No disk I/O in this path

### Phase 4: Group commit leader path
- `flush_and_sync(&self) → Result<u64, DbError>`: returns flushed_lsn
  1. Acquire writer Mutex (become leader)
  2. Drain queue (swap with empty Vec under queue Mutex)
  3. Sort by LSN
  4. Write all to BufWriter
  5. flush() + fsync()
  6. Update flushed_lsn (atomic store Release)
  7. Release writer Mutex

### Phase 5: Follower durability check
- `wait_for_flush(&self, target_lsn: u64)`:
  1. Load flushed_lsn (Acquire)
  2. If >= target → return immediately
  3. Else → call flush_and_sync() (become leader) or spin-wait

### Phase 6: Per-transaction scratch buffers
- `ConnectionTxn.wal_scratch: Vec<u8>` reused across appends within one txn
- Avoids allocation per WAL entry
- Cleared on commit/rollback

### Phase 7: Integration with TxnManager
- Replace `self.wal.append_with_buf(...)` calls with:
  1. Reserve LSN
  2. Serialize into txn scratch
  3. Submit to queue
- Replace `self.wal.commit_data_sync()` with:
  1. Submit COMMIT entry
  2. Call flush_and_sync()
  3. Advance max_committed

### Phase 8: Recovery verification
- WAL file has entries in LSN order (guaranteed by sort-before-write)
- Recovery scan_forward works identically (sequential LSN scan)
- No changes to recovery logic needed

## Lock ordering (deadlock prevention)

```
RULE: queue_mutex < writer_mutex

submit_entry:      acquires queue_mutex only             → safe
flush_and_sync:    acquires writer_mutex → queue_mutex   → safe (writer > queue)
reserve_lsn:       acquires nothing (atomic)             → safe

NO function acquires writer_mutex then queue_mutex for a second time → no cycle.
```

## Tests to write

1. **Single-threaded correctness**: append + flush → entries in WAL file
2. **Multi-threaded append**: 4 threads × 100 entries → all 400 entries in WAL, LSN-ordered
3. **Group commit**: 4 threads submit, 1 leader flushes → single fsync covers all
4. **Batch append**: reserve_lsns(100) + submit batch → 100 consecutive LSNs
5. **Recovery after concurrent writes**: crash simulation → recovery finds all flushed entries
6. **Follower fast path**: flushed_lsn already past target → immediate return
7. **Queue drain during append**: concurrent submit + drain → no lost entries

## Anti-patterns to avoid

- DO NOT use a shared log buffer (InnoDB-style circular buffer) — too complex for first
  iteration. Per-txn scratch + queue achieves same group commit without shared memory management.
- DO NOT hold writer Mutex during LSN reservation — defeats concurrency.
- DO NOT sort entries inside the queue Mutex — sort after drain, outside the lock.
- DO NOT use SeqCst ordering on flushed_lsn — Release/Acquire is sufficient.
- DO NOT skip the sort step — concurrent appends may arrive out of LSN order in the queue.

## Risks

- **Queue memory growth**: If leader is slow, queue accumulates. Mitigation: bounded queue
  size with backpressure (block submitters if queue > 10K entries).
- **Sort cost**: Sorting N entries by LSN on each flush. For typical N (10-100 entries per
  flush cycle), this is ~1µs. For extreme N (10K), it's ~100µs — still negligible vs fsync.
- **Writer Mutex contention**: Multiple transactions trying to become leader. First one wins,
  others wait or check flushed_lsn. Expected contention: low (fsync takes ~3-5ms, new
  entries accumulate during that time for next batch).
