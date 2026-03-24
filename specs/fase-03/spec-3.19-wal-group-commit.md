# Spec: 3.19 — WAL Group Commit

## What to build (not how)

A `CommitCoordinator` that batches WAL fsyncs across multiple concurrent
connections. Instead of every DML commit paying its own fsync (~1–10ms),
N connections that commit "simultaneously" share a single fsync and all
receive durability confirmation after it completes.

The key behavioral contract:
- A client connection does NOT receive `OK` until the fsync that covers
  its Commit WAL entry has completed successfully.
- The durability guarantee is identical to the current behavior: if the
  process crashes before fsync, the transaction is lost. The difference
  is that one fsync can now cover many transactions instead of one.
- The feature is disabled when `group_commit_interval_ms = 0` (default),
  making it opt-in. Current single-connection behavior is unchanged when
  disabled.

---

## Inputs / Outputs

### New configuration (extends `DbConfig`)
```toml
[wal]
group_commit_interval_ms = 0   # 0 = disabled (default); >0 enables group commit
group_commit_max_batch = 64    # trigger immediate fsync when N connections are waiting
```

- Input: `group_commit_interval_ms: u64` (0 disables), `group_commit_max_batch: usize`
- Both are read-only at startup; changing them requires restart.

### `CommitCoordinator::register_pending(txn_id: TxnId) → oneshot::Receiver<Result<(), DbError>>`
- Input: `txn_id` of the transaction whose Commit WAL entry is already in the BufWriter
- Output: a receiver that resolves to `Ok(())` when the fsync covering this entry
  completes, or `Err(DbError::WalCommitFailed)` if the fsync fails
- Never blocks the caller (returns immediately with a receiver)

### `TxnManager::commit_deferred() → Result<TxnId, DbError>`
- Same as `commit()`, BUT does NOT call `wal.commit()` (no flush, no fsync)
- Writes the Commit WAL entry to the BufWriter (already guaranteed by `wal.append`)
- Does NOT advance `max_committed` (that happens only after fsync confirmation)
- Returns the committed `txn_id` for the caller to pass to `CommitCoordinator`

### `TxnManager::advance_committed(committed: &[TxnId])`
- Advances `max_committed` to `max(committed)`, making the batch visible to future snapshots
- Must be called while holding the `Database` lock, after fsync succeeds

### Errors
- `DbError::WalCommitFailed { source: io::Error }` — new variant; propagated to all
  connections in the batch when fsync fails
- All other existing errors from DML execution are unchanged

---

## Execution flow

### When `group_commit_interval_ms = 0` (disabled — current behavior)
```
Connection A: lock(DB) → DML → txn.commit() [flush+fsync inside] → unlock → OK to client
```
No change to existing code paths.

### When `group_commit_interval_ms > 0` (enabled)

**Per-connection path (inside Database lock):**
```
1. execute_query(sql)       — DML + WAL entries appended to BufWriter
2. txn.commit_deferred()    — Commit entry appended to BufWriter, no fsync
                              max_committed NOT advanced yet
                              returns committed txn_id
3. rx = coordinator.register_pending(txn_id)
4. drop(database_guard)     — release the Database lock ← KEY: BEFORE fsync
5. rx.await                 — wait outside the lock for fsync confirmation
6. match result:
   Ok(())  → send OK to client
   Err(e)  → send error to client (data not durable; warn about potential inconsistency)
```

**CommitCoordinator background task (Tokio task, started at DB open):**
```
loop:
  1. Wait for: timer fires (interval_ms) OR trigger notified (Notify wake)
     OR pending.len() >= max_batch — whichever comes first

  2. Drain pending: take all (txn_id, reply_tx) from the queue

  3. If pending is empty: continue loop (nothing to do)

  4. Acquire Database lock

  5. wal.flush()    — BufWriter → OS page cache
     wal.fsync()    — OS page cache → disk (the ONE fsync for the batch)

  6a. If fsync OK:
        txn.advance_committed(&all_txn_ids)   — max_committed advances
        release lock
        for each reply_tx: reply_tx.send(Ok(()))

  6b. If fsync Err:
        release lock (max_committed NOT advanced)
        for each reply_tx: reply_tx.send(Err(WalCommitFailed))
        log error at ERROR level
        // database is in a degraded state — future commits may also fail
```

**Trigger wake-up rules:**
- `coordinator.register_pending()` calls `trigger.notify_one()` if
  `pending.len() + 1 >= max_batch`. This ensures the background task
  wakes immediately under high load without waiting for the timer.
- The timer fires every `interval_ms` regardless.

---

## Use cases

### 1. Happy path — 16 concurrent INSERT connections
```
t=0ms:  Conn 1–16 each acquire DB lock, INSERT 1 row, write Commit to BufWriter,
        register_pending, release lock.
t=1ms:  CommitCoordinator wakes on timer, drains 16 waiters,
        acquires DB lock, flush+fsync, advance_committed(max), releases lock.
        Notifies all 16 connections → all respond OK simultaneously.
Result: 16 fsyncs → 1 fsync. ~16× throughput improvement on concurrent writes.
```

### 2. Single connection (no batching benefit)
```
t=0ms:  Conn 1 INSERT → register_pending → release lock → await
t=1ms:  CommitCoordinator wakes → 1 waiter → flush+fsync → Conn 1 gets OK
Result: 1 fsync (same as before), latency +1ms (the interval). Throughput unchanged.
```

### 3. fsync failure
```
t=0ms:  Conn 1, 2, 3 register pending.
t=1ms:  CommitCoordinator acquires lock, flush fails (disk full / I/O error).
        Sends Err(WalCommitFailed) to Conn 1, 2, 3.
        max_committed NOT advanced.
        All three connections receive DbError to the client.
Result: No client receives OK. No rows are visible (max_committed not advanced).
        Database logs ERROR. Subsequent commits will also likely fail.
```

### 4. Crash between commit_deferred() and fsync
```
t=0ms:  Conn 1 calls commit_deferred() → Commit entry in BufWriter (not on disk).
        Process crashes.
t=1ms:  Recovery: WalReader finds no Commit entry for txn_id=N.
        Recovery marks txn as in-progress → undo applied.
        Row is gone.
Result: Correct. Client never received OK (it was awaiting rx which never fired).
        No data loss beyond what the WAL guarantees.
```

### 5. Group commit disabled, single INSERT
```
Connection uses current txn.commit() path directly. No change.
```

### 6. Explicit transaction (BEGIN … COMMIT)
```
BEGIN and DML statements execute normally inside the lock.
COMMIT statement calls commit_deferred() + register_pending + release lock + await.
Same behavior as autocommit case — batching applies at the COMMIT boundary.
```

---

## Acceptance criteria

- [ ] With `group_commit_interval_ms = 0` (default): behavior is byte-for-byte
      identical to current — no new code path is active
- [ ] With `group_commit_interval_ms = 1`: N concurrent INSERTs from N connections
      share at most `ceil(N * latency_per_insert / interval_ms)` fsyncs instead of N
- [ ] A client connection does not receive `OK` before the fsync covering its
      Commit entry has completed (verified by crash-between-DML-and-fsync test)
- [ ] After fsync confirmation, the committed rows are immediately visible to
      new snapshots (`max_committed` has been advanced)
- [ ] If fsync fails, ALL connections in the batch receive an error. No connection
      receives `OK`. `max_committed` is not advanced
- [ ] `cargo test --workspace` passes with group commit both enabled and disabled
- [ ] Benchmark `bench_insert_concurrent` shows ≥ 4× throughput improvement
      with 8 concurrent connections vs `group_commit_interval_ms = 0`
- [ ] No `unwrap()` in production code paths added by this feature
- [ ] Single-connection throughput degrades by at most `interval_ms` in latency
      (no regression beyond the configured batch window)

---

## Out of scope

- **Multi-writer DML concurrency**: DML execution is still serialized through the
  global Database Mutex. Group Commit only batches the fsync, not the writes.
  True parallel writers belong to Phase 7 (MVCC full).
- **WAL record per page (3.18)**: different optimization; not a dependency here.
- **WAL batch append (3.17)**: independent; can be combined later.
- **Crash recovery changes**: recovery behavior is unchanged. The CommitCoordinator
  does not affect how the WalReader interprets the log.
- **`innodb_flush_log_at_trx_commit=0` equivalent** (no fsync at all): not
  implemented. The minimum durability guarantee is always "fsync before OK".
- **Per-connection `synchronous_commit` override**: single global config for now.

---

## Dependencies

- Phase 3 WAL (3.1–3.8) must be complete ✅
- `TxnManager`, `WalWriter` in `axiomdb-wal` must be available ✅
- `DbConfig` with `[wal]` section (3.16) must be available ✅
- `tokio` already in use in `axiomdb-network` ✅
- `tokio::sync::{oneshot, Notify, Mutex}` — already available via tokio feature flags

---

## Invariants that must hold after this change

1. **WAL ordering**: `commit_deferred()` appends the Commit entry to the BufWriter
   while the Database lock is held. The BufWriter serializes all entries. The order
   on disk is the same as the order in which locks were acquired.

2. **Durability before visibility**: `max_committed` advances only AFTER fsync
   succeeds and WHILE holding the Database lock. A snapshot taken after a commit
   is always visible iff its Commit entry is durable.

3. **No partial batch visibility**: if fsync fails, the entire batch is rejected.
   No connection in the batch receives OK, and no row from the batch becomes visible.

4. **Background task holds lock minimally**: the CommitCoordinator acquires the
   Database lock ONLY for flush+fsync+advance_committed. It holds it for the
   duration of the fsync call (~1–10ms). This is acceptable given that fsync is the
   bottleneck we're amortizing.

5. **Disabled mode is zero-overhead**: when `group_commit_interval_ms = 0`, no
   background Tokio task is spawned, no channels are created, and the
   `commit_deferred()` path is never called. The existing `commit()` path is
   used unmodified.

---

## ⚠️ DEFERRED

- Per-connection `synchronous_commit` setting (a la PostgreSQL) → Phase 5
  (session state required)
- Adaptive interval tuning (shrink interval under low load, grow under high) →
  post-Phase 7 optimization
- Group Commit + concurrent writers (true parallel DML) → Phase 7
