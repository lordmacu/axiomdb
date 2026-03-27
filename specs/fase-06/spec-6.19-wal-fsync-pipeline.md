# Spec: 6.19 — WAL Fsync Pipeline (async leader-based group commit)

## Problem

AxiomDB's `insert_autocommit` throughput is **273 ops/s** vs MariaDB 21K ops/s and
MySQL 11.7K ops/s (77× slower than MariaDB). Root cause: 1 synchronous `sync_all()`
per transaction, even with a single connection.

The existing group commit (3.19) only helps with **concurrent connections**: the
background task sleeps `interval_ms` then fsyncs the batch. With a single sequential
connection (the common benchmark and CLI case), each commit still pays a full fsync
(~3.6ms on APFS SSD) because there's no overlap between the fsync and the next
INSERT arriving.

## Research — how production databases solve this

### MariaDB (InnoDB) — `group_commit_lock` (21K ops/s single connection)

Source: `research/mariadb-server/storage/innobase/log/log0sync.{h,cc}` and
`log/log0log.cc:1155-1207`.

**Two separate locks** (`write_lock` and `flush_lock`), each a `group_commit_lock`:

1. Committer calls `flush_lock.acquire(lsn)`.
2. If `flushed_lsn >= lsn` → returns `EXPIRED` immediately (another thread already
   fsynced past this point). **This is the key for single-connection speed.**
3. If no leader → caller becomes leader (`ACQUIRED`), sets `pending_value`, does the
   fsync, then calls `flush_lock.release(new_flushed_lsn)`.
4. `release()` wakes only waiters whose `lsn <= new_flushed_lsn` (no spurious
   wakeups), and designates one remaining waiter as the next leader.

**Why single-connection is fast:** Between the fsync (~3ms) and the client round-trip
(~0.05ms parse+execute+send OK), the next INSERT arrives while the previous fsync is
still completing. The new commit's `acquire(lsn)` finds the pending fsync covers its
LSN too → `EXPIRED` → zero-cost commit.

**Primitives:** futex on Linux, WaitOnAddress on Windows — kernel-level, near-zero CPU.

### PostgreSQL — `CommitDelay` + `XLogFlush` (single connection: 1 fsync/txn)

Source: `research/postgres/src/backend/access/transam/xlog.c:2860-2886`.

```c
if (CommitDelay > 0 && enableFsync && MinimumActiveBackends(CommitSiblings))
    pg_usleep(CommitDelay);  // sleep before flush to batch
```

- Default: `CommitDelay = 0`, `CommitSiblings = 5` → disabled.
- Only activates when ≥ 5 concurrent backends → **does NOT help single-connection**.
- Each autocommit INSERT still pays 1 fsync via `XLogFlush()` → `issue_xlog_fsync()`.
- PostgreSQL single-connection autocommit ≈ same bottleneck as current AxiomDB.

### DuckDB — per-transaction `Sync()` (embedded, no group commit)

Source: `research/duckdb/src/storage/write_ahead_log.cpp:543-555`.

```cpp
void WriteAheadLog::Flush() {
    writer->Sync();  // fsync per commit
}
```

- `FlushCommit()` called after every transaction commit.
- No explicit group commit or batching.
- DuckDB focuses on OLAP batch inserts, not single-row autocommit throughput.

### SQLite — configurable sync per transaction

Source: `research/sqlite/src/wal.c:2170-2327`, `pager.c:608-617`.

- WAL mode with `PRAGMA synchronous=FULL`: 1 fsync per commit to WAL file.
- WAL mode with `PRAGMA synchronous=NORMAL`: **0 fsyncs** on commit (only on
  checkpoint). This is why SQLite autocommit is fast but not crash-safe per txn.
- No group commit mechanism — single-writer model.

### OceanBase — `BatchLogIOFlushLogTask` (enterprise group commit)

Source: `research/oceanbase/src/logservice/palf/log_io_worker.cpp:66-69, 279-300`.

- Dedicated IO worker thread with configurable `batch_width` × `batch_depth`.
- `reduce_io_task_()` aggregates multiple `LogIOFlushLogTask` items into one batch.
- Single fsync covers the entire batch → 10-100× reduction depending on concurrency.
- Timeout triggers flush even if batch not full.

### Summary table

| Database     | Single-conn autocommit | Mechanism | Fsyncs/txn |
|--------------|------------------------|-----------|------------|
| MariaDB      | 21K ops/s              | Leader-based group_commit_lock (async pipeline) | << 1 (amortized) |
| MySQL 8      | 11.7K ops/s            | Similar (ordered commit + binlog group commit) | << 1 |
| PostgreSQL   | ~300-500 ops/s (default) | CommitDelay (disabled by default) | 1 |
| DuckDB       | ~300 ops/s             | Per-txn Sync() | 1 |
| SQLite WAL   | ~50K ops/s (NORMAL)    | No fsync on commit (NORMAL mode) | 0 |
| OceanBase    | High (batch IO)        | Dedicated IO worker + batch | << 1 |
| **AxiomDB**  | **273 ops/s**          | **Inline sync_all()** | **1** |

**Conclusion:** MariaDB's approach (leader-based async fsync with LSN tracking) is the
only one that achieves high throughput for single-connection autocommit while
maintaining full durability. This is our target.

## What to build

An **async fsync pipeline** in the WAL commit path that allows a committing thread to
overlap its fsync with subsequent transactions arriving on the same or other connections.

### Core concept: leader-follower fsync with LSN tracking

Instead of each transaction calling `sync_all()` inline, the commit path becomes:

1. **Write phase:** Append Commit entry to BufWriter (fast, RAM-only).
2. **Register phase:** Record this transaction's `commit_lsn` in the flush queue.
3. **Check phase:** If `flushed_lsn >= commit_lsn` → return immediately (the fsync
   from another leader already covered this entry).
4. **Leader election:** If no leader is active → become leader. Flush BufWriter to OS,
   then `sync_all()`, then update `flushed_lsn`, then wake all followers whose
   `commit_lsn <= flushed_lsn`.
5. **Follower wait:** If a leader is active → wait (tokio oneshot or Notify) until
   woken by the leader.

This gives us:
- **Single connection, fast client:** INSERT N+1 arrives while fsync of INSERT N is
  running → N+1 piggybacks on N's fsync → ~0 fsyncs for followers → throughput
  limited by parse+execute, not fsync.
- **Multiple connections:** Same batching as current group commit, but leader-based
  instead of timer-based → lower latency.
- **Slow client / idle:** Degrades gracefully to 1 fsync per txn (same as now).

## Inputs / Outputs

### Input
- `WalWriter` with current `BufWriter<File>` and `next_lsn` / `offset` state
- `TxnManager::commit()` calling the new pipeline instead of inline fsync
- `DbConfig` controlling whether the pipeline is enabled

### Output
- `FsyncPipeline` struct managing the leader election and LSN tracking
- Modified `TxnManager::commit()` that returns immediately if another leader's fsync
  covers this transaction
- `CommitFuture` (oneshot receiver) that resolves when the transaction is durable

### Errors
- `DbError::WalFsyncFailed` if the leader's fsync fails → all followers in the batch
  receive the error
- `DbError::DiskFull` propagated from fsync → triggers degraded mode

## Architecture

```
Connection Thread                    FsyncPipeline (shared state)
─────────────────                    ─────────────────────────────
INSERT → WAL append(commit_lsn=42)
  │
  ▼
pipeline.request_fsync(42)
  │
  ├─ flushed_lsn ≥ 42? ──────────► return Ok(()) immediately  [EXPIRED]
  │
  ├─ no active leader? ──────────► become leader               [ACQUIRED]
  │    │                              set pending_lsn = max(queue)
  │    │                              BufWriter::flush()
  │    │                              File::sync_all()
  │    │                              flushed_lsn = synced_lsn
  │    │                              wake followers ≤ flushed_lsn
  │    │                              if more pending → designate next leader
  │    ▼
  │  return Ok(())
  │
  └─ leader active? ─────────────► enqueue(42, oneshot_tx)     [QUEUED]
       │                              wait on oneshot_rx
       ▼
     woken by leader → return Ok(())
```

## Use cases

### 1. Happy path — single connection, fast client (target: 10K+ ops/s)
```
INSERT 1 → commit_lsn=10 → leader → fsync (3ms)
  during those 3ms, client sends INSERT 2:
INSERT 2 → commit_lsn=11 → flushed_lsn=10 < 11 → leader active → QUEUED
  leader finishes fsync → flushed_lsn=11 → wake INSERT 2
INSERT 2 returns immediately → 0ms fsync cost
INSERT 3 → commit_lsn=12 → check flushed_lsn=11 < 12 → no leader → ACQUIRED
  ...pipeline continues
```

### 2. Multiple connections — batching
```
Conn A: INSERT → commit_lsn=50 → ACQUIRED (leader)
Conn B: INSERT → commit_lsn=51 → QUEUED
Conn C: INSERT → commit_lsn=52 → QUEUED
  leader A fsyncs → flushed_lsn=52 → wake B and C
  1 fsync for 3 transactions
```

### 3. Already flushed — immediate return
```
Leader just fsynced to lsn=100
Next INSERT → commit_lsn=99 → flushed_lsn=100 ≥ 99 → EXPIRED → return Ok(())
```

### 4. Fsync failure — all followers error
```
Leader fsyncs → I/O error
  → all QUEUED followers receive Err(WalFsyncFailed)
  → leader returns Err(WalFsyncFailed)
  → degraded mode entered if DiskFull
```

### 5. Idle/slow client — degrades to 1 fsync/txn
```
INSERT 1 → commit_lsn=10 → no leader → ACQUIRED → fsync → done
  ... 500ms gap ...
INSERT 2 → commit_lsn=11 → flushed_lsn=10 < 11 → no leader → ACQUIRED → fsync → done
  Each pays its own fsync — same as current behavior
```

## Acceptance criteria

- [ ] Single-connection `insert_autocommit` with 1000 rows in `local_bench.py` achieves ≥ 5,000 ops/s (vs current 273 ops/s). Target: 10K+ ops/s.
- [ ] Multi-connection (4 connections) autocommit achieves ≥ 15,000 ops/s total.
- [ ] Full durability: every committed transaction survives process crash (same as current inline fsync guarantee).
- [ ] `flushed_lsn` is monotonically increasing and never regresses.
- [ ] Fsync failure propagates to all queued followers as `Err`.
- [ ] DiskFull triggers degraded mode.
- [ ] The existing `group_commit_interval_ms = 0` (disabled) behavior is replaced by the pipeline (always-on, no config needed).
- [ ] The timer-based group commit (3.19) is superseded — the pipeline handles both single-connection and multi-connection cases.
- [ ] `cargo test --workspace` passes with no regressions.
- [ ] WAL crash recovery still works correctly (entries written by BufWriter but not fsynced are replayed on recovery).

## Out of scope

- O_DIRECT / DIO for the WAL file (MariaDB `log_write_through`). Future optimization.
- futex/WaitOnAddress kernel primitives. We use tokio::sync (Notify, oneshot) which is
  sufficient for the async model.
- Binary log / replication group commit (MariaDB's binlog group commit is a separate concern).
- Changing the WAL format or entry types.

## Dependencies

- Phase 3.19 (WAL Group Commit) — will be **superseded** by this. The `CommitCoordinator`
  + background task is replaced by inline leader election in the commit path.
- `WalWriter` flush/sync_all methods — used as-is.
- `TxnManager` commit path — modified to use the pipeline.

## Risks

- **Correctness:** Must ensure `max_committed` advances atomically after fsync, never
  before. A follower must not see `Ok(())` unless its WAL entry is durable on disk.
  → Mitigation: `flushed_lsn` is only updated after `sync_all()` returns `Ok`.

- **Leader crash mid-fsync:** If the leader thread panics after `flush()` but before
  `sync_all()`, followers are stuck. → Mitigation: oneshot channel drops → followers
  receive `Err(RecvError)` → treated as fsync failure.

- **Lock contention:** The pipeline uses a `std::sync::Mutex` (not tokio) for the
  state check + leader election. Must be held only for ~100ns (no I/O inside).
  → Mitigation: critical section is just: read flushed_lsn, check leader, update state.

- **Interaction with WAL rotation:** The `WalRotator` calls checkpoint + rotate which
  also flushes/syncs. Must coordinate with the pipeline to avoid double-fsync or
  lost entries. → Mitigation: rotation acquires the Database lock (already the case),
  pipeline operates under the same lock.
