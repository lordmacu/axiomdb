# Spec: 40.4 — Concurrent WAL Writer

## What to build (not how)

Allow multiple transactions to write WAL entries simultaneously without serializing
on a single BufWriter. The current WalWriter has a single `next_lsn: u64` and a
single `BufWriter<File>` — every WAL append holds exclusive access to both.

The new design separates **entry serialization** (per-transaction, parallel) from
**disk I/O** (single leader, group commit). Multiple transactions prepare their WAL
entries concurrently; one leader flushes all pending entries in a single write+fsync.

## Research findings

### InnoDB (lock-free LSN + group commit leader/follower)
- **Atomic LSN reservation**: `write_lsn_offset` is `Atomic_relaxed<uint64_t>`, threads
  atomically reserve space in the log buffer via CAS — NO lock for position allocation
- **Single shared log buffer** (16MB default, circular): multiple threads write concurrently
  at their reserved offsets — fully parallel data copy
- **Group commit via `group_commit_lock`**: one thread becomes write leader, flushes all
  accumulated data; followers queue callbacks and sleep on per-thread futex
- **Two-phase**: write_lock (write to disk) separate from flush_lock (fsync for durability)
- **Backoff mechanism**: when buffer full, threads set atomic flag and spin rather than block
- **Latency**: ~10-50ns per LSN reservation (atomic op), ~3-5ms per group fsync

### PostgreSQL (spinlock position + insertion locks)
- **Spinlock-protected position**: `insertpos_lck` protects `CurrBytePos` — held ~200ns
- **Fixed insertion lock slots** (8 by default): each backend acquires one slot to prevent
  concurrent segment-boundary crossings — up to 8 parallel inserters
- **Usable byte position abstraction**: counts data bytes only (excludes page headers),
  simplifies reservation arithmetic
- **Background WAL writer**: dedicated process flushes every `WalWriterDelay` ms (default 200ms)
- **Explicit XLogFlush()** for synchronous commit — backend does its own fsync if needed

### Design choice for AxiomDB: InnoDB-inspired hybrid
- **Atomic LSN reservation** (InnoDB) — simpler than PostgreSQL's spinlock, higher throughput
- **Per-transaction scratch buffers** — each transaction serializes its entry locally,
  then submits to shared write queue (avoids contention on shared log buffer)
- **Group commit leader/follower** — existing FsyncPipeline extended for concurrent writers
- **Single write thread** for disk I/O — avoids complexity of concurrent file writes

## Current architecture (what changes)

### WalWriter today
```rust
pub struct WalWriter {
    writer: BufWriter<File>,    // single writer, exclusive access
    next_lsn: u64,              // plain u64 (becomes AtomicU64 in 40.1)
    logical_end: u64,           // position tracking
    reserved_end: u64,          // file space reserved
    dml_sync_method: ResolvedWalSyncMethod,
}
```

### Methods requiring exclusive access today
- `append(&mut self)` — serializes entry + writes to BufWriter
- `append_with_buf(&mut self)` — same with reusable scratch
- `write_batch(&mut self)` — batch write
- `reserve_lsns(&mut self, n)` — reserve N consecutive LSNs
- `commit_data_sync(&mut self)` — flush + fsync

## New structures

### ConcurrentWalWriter

```rust
pub struct ConcurrentWalWriter {
    /// Atomic LSN counter — lock-free reservation for all transactions.
    next_lsn: AtomicU64,

    /// Write queue: transactions submit serialized entries here.
    /// Leader drains the queue and writes to disk in batch.
    write_queue: Mutex<WriteQueue>,

    /// The actual file writer — only accessed by the group commit leader.
    writer: Mutex<BufWriter<File>>,

    /// Flush coordination: leader/follower pattern (extends existing FsyncPipeline).
    flush_state: AtomicU64,  // flushed_lsn — highest LSN durably on disk

    /// File metadata.
    logical_end: AtomicU64,
    reserved_end: AtomicU64,
    dml_sync_method: ResolvedWalSyncMethod,
}
```

### WriteQueue

```rust
struct WriteQueue {
    /// Pending serialized entries waiting to be written to disk.
    /// Each entry: (lsn, serialized_bytes).
    entries: Vec<(u64, Vec<u8>)>,

    /// Total bytes in pending entries (for pre-allocation).
    total_bytes: usize,
}
```

### Per-transaction scratch buffer

Each `ConnectionTxn` (from 40.2) holds its own scratch buffer:
```rust
pub struct ConnectionTxn {
    // ... existing fields ...
    wal_scratch: Vec<u8>,  // reusable per-txn serialization buffer
}
```

## Detailed behavior

### WAL entry append (concurrent, per-transaction)

```
1. Reserve LSN: lsn = next_lsn.fetch_add(1, Relaxed)     → lock-free, ~10ns
2. Serialize entry into txn's scratch buffer               → parallel, no shared state
3. Submit to write_queue:
   a. Acquire write_queue Mutex (brief, ~1µs)
   b. Push (lsn, serialized_bytes) to queue
   c. Release write_queue Mutex
4. Return to caller (entry is "in flight" but not yet on disk)
```

Multiple transactions do steps 1-3 concurrently. No serialization except the brief
Mutex on step 3a.

### Batch WAL append (for INSERT batch, UPDATE batch)

```
1. Reserve N LSNs: lsn_base = next_lsn.fetch_add(n, Relaxed)
2. Serialize all N entries into txn's scratch buffer
3. Submit entire batch to write_queue (single Mutex acquisition)
```

### Group commit (write leader)

```
1. Transaction calls commit() → needs durability guarantee
2. Acquire writer Mutex (becomes leader)
3. Drain write_queue (acquire queue Mutex briefly, swap with empty Vec)
4. Sort drained entries by LSN (ensure sequential order in file)
5. Write all entries to BufWriter in one batch
6. BufWriter.flush() → OS page cache
7. fsync() → durable on disk
8. Update flush_state: flushed_lsn.store(max_lsn_written, Release)
9. Release writer Mutex
10. Wake all followers whose LSN ≤ flushed_lsn
```

### Follower path (transaction waiting for durability)

```
1. Transaction calls commit() → check if flushed_lsn >= my_commit_lsn
2. If yes → already durable (AcquireResult::Expired), return immediately
3. If no → wait for leader to advance flushed_lsn past my commit_lsn
4. Leader wakes followers after fsync
```

### Read-only transactions

No WAL entries needed. No interaction with ConcurrentWalWriter.
Only write BEGIN + COMMIT entries (lightweight, can skip if pure read).

## Concurrency guarantees

| Scenario | Behavior |
|---|---|
| 2 txns append simultaneously | Parallel LSN reservation + parallel serialization; brief queue Mutex |
| 10 txns commit simultaneously | 1 leader writes+fsyncs; 9 followers wait; single fsync covers all 10 |
| Append during fsync | Append succeeds (different Mutex); entries queued for next batch |
| Queue drain during append | Swap-based drain ensures no lost entries (brief Mutex) |
| Crash after append, before fsync | Entries in queue are lost (not durable); recovery uses WAL on disk |

## Use cases

1. **Single client autocommit INSERT:** Same as today — reserve LSN, serialize, write, fsync.
   No overhead from concurrency primitives (Mutex uncontended = ~20ns).

2. **10 concurrent autocommit INSERTs:** Each reserves LSN and serializes in parallel.
   First to commit becomes leader, writes ALL 10 entries, single fsync.
   ~5ms total instead of 10 × 5ms = 50ms.

3. **Long transaction (100 INSERTs in one txn):** Each INSERT appends to queue.
   On COMMIT: leader writes all 100 + any pending from other txns, single fsync.

4. **Mixed workload (writers + readers):** Readers never touch WAL writer.
   Writers only contend on queue Mutex (microseconds).

## Acceptance criteria

- [ ] `next_lsn` is AtomicU64 with `fetch_add` for lock-free LSN reservation
- [ ] Per-transaction scratch buffer for entry serialization (no shared buffer contention)
- [ ] WriteQueue with Mutex: submit entries, leader drains in batch
- [ ] Writer Mutex: only leader holds during disk I/O
- [ ] `flushed_lsn` AtomicU64: followers check durability without lock
- [ ] Group commit: single fsync covers multiple transactions' entries
- [ ] Entry ordering: entries written to file in LSN order (sorted after drain)
- [ ] Batch append: reserve N LSNs atomically, submit N entries in one Mutex acquisition
- [ ] Crash recovery: only entries flushed to disk are recoverable (queue entries lost = correct)
- [ ] All existing WAL tests pass
- [ ] Stress test: 8 threads × 1000 appends → all LSNs present in WAL file, no duplicates
- [ ] No deadlock between writer Mutex and queue Mutex
- [ ] Benchmark: 8 concurrent autocommit INSERTs → ~4-6x throughput vs single writer

## Out of scope

- Lock-free shared log buffer (InnoDB's circular buffer) — too complex for first iteration.
  The queue-based approach achieves the same group commit benefit with simpler code.
- WAL segment rotation under concurrency (extend existing rotation logic)
- Async commit mode (deferred fsync) — already exists via FsyncPipeline

## Dependencies

- 40.1 (Atomic TxnId) — AtomicU64 patterns
- 40.2 (Per-connection txn) — per-transaction scratch buffers
- 40.3 (StorageEngine interior mutability) — &self for storage access during WAL flush
