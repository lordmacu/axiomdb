# Plan: 40.11 — Executor Refactoring

## Overview

Wire `LockManager` into the executor so that UPDATE/DELETE acquire row X-locks
before modifying rows, and INSERT/SELECT acquire table-level intention locks.
Also bridge the async LockManager into the sync executor.

## Key design decision: sync → async executor bridge

### Problem

The `LockManager` API is fully async (`tokio::sync::Notify` + `tokio::time::timeout`).
The executor is fully sync (`fn execute_with_ctx`, not `async fn`).

```
handler.rs (async)  →  SharedDatabase::execute_query (sync)  →  execute_with_ctx (sync)
                                                                  ↳ INSERT / UPDATE / DELETE (sync)
                                                                    ↳ lock_mgr.acquire_record_lock() ← ASYNC!
```

### Research: how InnoDB and PostgreSQL solve this

**InnoDB** (`research/mariadb/storage/innobase/lock/lock0lock.cc`):
the lock system is 100% synchronous. `lock_rec_lock()` blocks the current thread
via `os_event_wait()` (futex on Linux). One thread per connection → blocking is cheap.

**PostgreSQL** (`research/postgresql/src/backend/storage/lmgr/proc.c`):
also synchronous — `ProcSleep()` puts the backend to sleep on a `sem_wait()`.
One process per connection → blocking is fine.

Both systems use the process/thread-per-connection model where blocking is cheap.
AxiomDB uses tokio (M:N async runtime), so blocking a tokio worker thread is
unacceptable — it would stall all connections multiplexed on that worker.

### Chosen approach: dual sync/async LockManager API

Add synchronous lock methods that use `std::sync::Condvar` instead of
`tokio::sync::Notify`. The executor stays sync (avoiding an invasive async
transformation of ~100 functions). The sync lock method:

1. **Fast path**: immediate grant (no waiting) — same logic, zero async overhead.
2. **Slow path**: `Condvar::wait_timeout()` — blocks the OS thread.

The fast path is the common case for OLTP workloads (different rows → no conflict).
The slow path triggers only on actual row contention.

**Why blocking is acceptable for now:** `SharedDatabase::execute_query()` runs
synchronously inside a tokio task. The lock wait is bounded by `lock_wait_timeout`
(default 50s, typically < 1ms for row locks). Phase 40.12 can wrap contended paths
in `tokio::task::spawn_blocking()` if benchmarks show tokio worker starvation.

**Alternative rejected — making the executor async:** Would require changing ~100
function signatures, adding `.await` at ~200 call sites, propagating `Send` bounds,
and rewriting all tests. Far too invasive for a lock integration phase.

## Files to create/modify

### New files

| File | Purpose |
|------|---------|
| `crates/axiomdb-lock/src/sync_api.rs` | `SyncNotify` (Condvar-based), `acquire_record_lock_sync()`, `acquire_table_lock_sync()` |

### Modified files

| File | What changes |
|------|-------------|
| `crates/axiomdb-lock/src/manager.rs` | Factor shared grant/conflict logic into `try_grant_or_enqueue()` internal helper; both async and sync entry points delegate to it |
| `crates/axiomdb-lock/src/lib.rs` | Export sync API |
| `crates/axiomdb-sql/src/exec_ctx.rs` | Add `lock_mgr: Option<&'a LockManager>` field + `lock_manager()` accessor |
| `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` | Pass `lock_mgr` to `ExecutionContext::new()` |
| `crates/axiomdb-sql/src/executor/exec_entry.rs` | Pass `lock_mgr` to `ExecutionContext::new()` |
| `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` | Add IX(table) lock before insert |
| `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` | Add IX(table) lock before clustered insert |
| `crates/axiomdb-sql/src/executor/update_ctx.rs` | Add IX(table) lock at entry |
| `crates/axiomdb-sql/src/executor/update_candidates.rs` | Add X(row) per candidate BEFORE modification; re-verify after WaitGranted |
| `crates/axiomdb-sql/src/executor/update_entry.rs` | Wire lock through to update helpers |
| `crates/axiomdb-sql/src/executor/update_fused_range.rs` | X(row) lock in fused-range path |
| `crates/axiomdb-sql/src/executor/update_clustered.rs` | X(row) lock in clustered UPDATE |
| `crates/axiomdb-sql/src/executor/delete.rs` | IX(table) + X(row) per candidate BEFORE delete-mark; re-verify after WaitGranted |
| `crates/axiomdb-network/src/mysql/shared_db.rs` | Add `lock_mgr: LockManager` field; pass to executor |
| `crates/axiomdb-embedded/src/lib.rs` | Pass `None` for lock_mgr (no locking in embedded mode) |

## Algorithm / Data structure

### Dual-mode lock manager internals

```
                  ┌─────────────────────────┐
                  │   try_grant_or_enqueue() │  ← shared logic
                  │   (shard mutex only)     │
                  └──────────┬──────────────┘
                             │
              ┌──────────────┴──────────────┐
              │                             │
    ┌─────────▼──────────┐       ┌──────────▼─────────┐
    │  async entry point │       │  sync entry point  │
    │  tokio::Notify     │       │  Condvar::wait     │
    │  tokio::timeout    │       │  Condvar::notify   │
    └────────────────────┘       └────────────────────┘
```

The shared `try_grant_or_enqueue()` returns one of:
- `Granted` — fast path, no waiting needed
- `MustWait(WaitHandle)` — slow path, caller parks with their mechanism

`WaitHandle` is generic over the notification mechanism:
- Async: `Arc<tokio::sync::Notify>`
- Sync: `Arc<SyncNotify>` where `SyncNotify = (Mutex<bool>, Condvar)`

### SyncNotify implementation

```rust
struct SyncNotify {
    pair: (Mutex<bool>, Condvar),
}

impl SyncNotify {
    fn new() -> Self {
        Self { pair: (Mutex::new(false), Condvar::new()) }
    }

    fn notify(&self) {
        let (lock, cvar) = &self.pair;
        let mut granted = lock.lock().unwrap();
        *granted = true;
        cvar.notify_one();
    }

    fn wait_timeout(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &self.pair;
        let mut granted = lock.lock().unwrap();
        while !*granted {
            let result = cvar.wait_timeout(granted, timeout).unwrap();
            granted = result.0;
            if result.1.timed_out() && !*granted {
                return false; // timeout
            }
        }
        true // notified
    }
}
```

### Lock acquisition points per DML (InnoDB model)

Based on research of InnoDB's `row0ins.cc` (lines 3774-3905) and `row0upd.cc`
(lines 2600-2620):

```
INSERT (InnoDB: row_ins_step → lock_table(IX) → btr_cur_optimistic_insert):
  1. IX(table)                    — table_lock_sync(txn_id, table_id, IX)
  2. Insert row                   — no explicit row lock (row doesn't exist yet)
  (Gap locks deferred — requires B-tree next-key locking integration)

UPDATE (InnoDB: row_upd_step → lock_table(IX) → lock_clust_rec_modify_check_and_lock(X)):
  1. IX(table)                    — table_lock_sync(txn_id, table_id, IX)
  2. Collect candidate rows       — MVCC scan (no locks)
  3. For each candidate:
     a. X(row)                    — record_lock_sync(txn_id, page_id, slot_id, X)
     b. Re-verify if WaitGranted  — re-read row, re-check visibility + WHERE
     c. Apply modification        — update row in page
     d. Update secondary indexes

DELETE (InnoDB: row_upd_del_mark_clust_rec → same lock path as UPDATE):
  1. IX(table)                    — table_lock_sync(txn_id, table_id, IX)
  2. Collect candidate rows       — MVCC scan (no locks)
  3. For each candidate:
     a. X(row)                    — record_lock_sync(txn_id, page_id, slot_id, X)
     b. Re-verify if WaitGranted  — re-read row header visibility
     c. Delete-mark row           — set txn_id_deleted

SELECT (plain — InnoDB consistent read, no locks):
  1. IS(table) (optional)         — table_lock_sync(txn_id, table_id, IS)
  2. MVCC scan                    — no row locks
```

### Re-verification after lock wait (InnoDB pessimistic re-check / PostgreSQL EvalPlanQual)

When `acquire_record_lock_sync()` returns `WaitGranted`, the row was potentially
modified by the transaction that previously held the lock. Re-verification required:

```rust
if result == LockResult::WaitGranted {
    // Re-read the row (page may have been modified by the releasing txn).
    let page = storage.read_page(rid.page_id)?;
    let hdr = read_row_header(&page, rid.slot_id);

    // 1. Visibility: is it still visible to our snapshot?
    if !is_visible(hdr, &snap) {
        continue; // Row deleted/superseded — skip.
    }

    // 2. WHERE re-evaluation (UPDATE/DELETE only).
    let row_data = decode_row(&page, rid.slot_id, &col_types);
    if !evaluate_where(&where_expr, &row_data, &schema_cols) {
        continue; // Row no longer matches — skip.
    }
}
```

### ExecutionContext extension

```rust
pub struct ExecutionContext<'a> {
    storage: &'a dyn StorageEngine,
    coord: &'a TxnManager,
    bloom: &'a BloomRegistry,
    lock_mgr: Option<&'a LockManager>,   // NEW — None for embedded/test mode
}
```

`Option` allows backward compatibility: embedded mode and unit tests that don't
need locking pass `None`. DML lock calls are gated:

```rust
if let Some(lm) = exec_ctx.lock_manager() {
    lm.acquire_table_lock_sync(conn_txn.txn_id, table_id, LockMode::IntentionExclusive, ...)?;
}
```

## Implementation phases

### Phase A: Sync lock API (crates/axiomdb-lock)

1. Define `SyncNotify` struct (Condvar-based wait/notify).
2. Factor shared grant/conflict logic out of `acquire_record_lock()` into
   `try_grant_or_enqueue_record()` that returns `Granted | MustWait(notify_handle)`.
3. Implement `acquire_record_lock_sync()` using `SyncNotify` for the wait path.
4. Implement `acquire_table_lock_sync()` using `SyncNotify`.
5. Add `release_all_locks()` (already exists, verify it's sync-compatible).
6. Ensure `promote_waiters()` calls both `tokio::Notify::notify_one()` and
   `SyncNotify::notify()` depending on waiter type (enum dispatch).
7. Unit tests: same scenarios as async, called from `#[test]` (not `#[tokio::test]`).

**Verifiable:** `cargo test -p axiomdb-lock` passes.

### Phase B: ExecutionContext + LockManager field (crates/axiomdb-sql)

1. Add `lock_mgr: Option<&'a LockManager>` to `ExecutionContext`.
2. Update `ExecutionContext::new()` signature (4th arg: `lock_mgr`).
3. Add accessor `pub fn lock_manager(&self) -> Option<&'a LockManager>`.
4. Update all `ExecutionContext::new()` call sites (~3 places).
5. No behavioral change — all lock_mgr = None initially.

**Verifiable:** `cargo test --workspace` passes unchanged.

### Phase C: Table-level intention locks in DML entry points

1. INSERT: IX(table) after table resolution, before any row write.
2. UPDATE: IX(table) after table resolution, before candidate collection.
3. DELETE: IX(table) after table resolution, before candidate collection.
4. SELECT: IS(table) — optional, add only if needed for DDL coordination later.

**Verifiable:** `cargo test --workspace` passes. Lock counters increment.

### Phase D: Row-level X-locks for UPDATE and DELETE

1. UPDATE: X(row) per candidate BEFORE modification in `update_candidates.rs`.
   - Standard path (heap UPDATE).
   - Fused-range path (`update_fused_range.rs`).
   - Clustered path (`update_clustered.rs`).
   - Re-verify after WaitGranted.
2. DELETE: X(row) per candidate BEFORE delete-mark in `delete.rs`.
   - Heap DELETE path.
   - Clustered DELETE path.
   - Bulk-empty path (table X-lock instead of per-row).
   - Re-verify after WaitGranted.

**Verifiable:** `cargo test --workspace` passes. Concurrent UPDATE on same row serialized.

### Phase E: Lock release on transaction end

1. In `exec_with_ctx.rs` commit/rollback paths: call
   `lock_mgr.release_all_locks(txn_id)` after WAL commit/rollback.
2. Wire through `SharedDatabase` commit/rollback in `shared_db.rs`.
3. Verify that `promote_waiters()` correctly unblocks sync waiters.

**Verifiable:** Two-txn test: first commits → second unblocks and completes.

### Phase F: SharedDatabase integration + wire tests

1. Add `lock_mgr: LockManager` field to `SharedDatabase`.
2. Initialize in `open_with_config()`.
3. Pass `Some(&self.lock_mgr)` in `execute_query()`/`execute_stmt()`.
4. Wire protocol smoke test: concurrent INSERT + concurrent UPDATE.

**Verifiable:** `tools/wire-test.py` passes with concurrent assertions.

## Tests to write

### Unit tests (crates/axiomdb-lock)
- `test_sync_record_lock_immediate_grant` — no conflict → Granted
- `test_sync_record_lock_wait_then_grant` — X holds, second waits, first releases → WaitGranted
- `test_sync_record_lock_timeout` — lock held → timeout error
- `test_sync_deadlock_detection` — A→B→A cycle detected synchronously
- `test_sync_table_lock_ix_compatible` — two IX same table → both granted
- `test_sync_mixed_async_sync_waiters` — async waiter + sync waiter on same resource

### Integration tests (crates/axiomdb-network)
- `test_concurrent_insert_no_lock_conflict` — two connections INSERT different rows → no blocking
- `test_concurrent_update_same_row_serialized` — connection B waits for A's commit
- `test_update_after_delete_re_verify` — UPDATE on deleted row → skip (re-verify)
- `test_deadlock_detected_and_reported` — A↔B deadlock → one gets error 1213

### Wire protocol smoke test (tools/wire-test.py)
- Two concurrent connections both INSERT (spec acceptance criterion)
- UPDATE lock wait → second succeeds after first commits

### Bench (crates/axiomdb-lock/benches/)
- `bench_sync_uncontended_lock` — fast path latency (target: < 200ns)
- `bench_sync_contended_4_threads` — 4 threads × 10K locks on 100 rows

## Anti-patterns to avoid

- **DO NOT** make the executor async — too invasive for this phase
- **DO NOT** use `tokio::runtime::Handle::block_on()` inside tokio — panics
- **DO NOT** acquire row locks after modification (violates lock-before-modify)
- **DO NOT** hold row locks across transaction boundaries — release on commit/rollback
- **DO NOT** acquire IX/IS table locks inside tight loops — once per statement
- **DO NOT** skip re-verification after `WaitGranted` — row may have changed
- **DO NOT** remove the `Option<>` from lock_mgr — embedded mode needs None

## Risks

| Risk | Mitigation |
|------|-----------|
| Sync lock blocks tokio worker under heavy contention | Bounded by `lock_wait_timeout`. Phase 40.12 adds `spawn_blocking()`. |
| Deadlock detection in sync path | Same Brent's algorithm, runs before parking (synchronous detection). |
| Re-verification logic incorrect | Unit test each case: invisible row, WHERE mismatch, unchanged row. |
| Condvar wakeup missed (spurious) | `Mutex<bool>` guard pattern with while-loop check (standard idiom). |
| Performance regression from lock overhead | Fast path is O(1) shard lookup + bitmap. Bench in 40.12. |
| SyncNotify + tokio Notify coexistence | Waiter enum dispatches to correct notify type. Integration test with mixed waiters. |
