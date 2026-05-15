# Spec: 13.7 SELECT FOR UPDATE / FOR SHARE — Row-level pessimistic locking

Phase: 13 — Advanced PostgreSQL features
Task: Row-level locking via `SELECT ... FOR UPDATE / FOR SHARE [NOWAIT]`
Status: approved

## Context

AxiomDB already has a complete `axiomdb-lock` crate (Phase 40.11) with a sharded
`LockManager`, conflict matrix, FIFO wait queues, deadlock detection (DFS), and both
sync/async acquisition APIs. `ExecutionContext` already carries `lock_mgr:
Option<&LockManager>`. The wire layer already passes `Some(&shared_db.lock_mgr)`.
`SelectStmt.lock_mode: Option<LockMode>` already exists in the AST and is parsed but
ignored. This subphase connects all those pieces: when `lock_mode` is set the executor
acquires physical row locks before returning rows, holding them until COMMIT/ROLLBACK.

## Goal

Parse and execute `SELECT … FOR UPDATE / FOR NO KEY UPDATE / FOR SHARE / FOR KEY SHARE
[NOWAIT]` so that every scanned row is locked for the duration of the transaction,
blocking or failing concurrent writers as specified.

## Non-goals

- `SKIP LOCKED` — deferred to 13.8b (requires partial-scan rollback machinery)
- `FOR UPDATE OF table_list` (scoped locking per table in JOIN) — deferred to 13.8c
- Locking inside subqueries — deferred; only the top-level FROM tables are locked
- Gap locks / next-key locks (InnoDB-style phantom protection) — deferred to Phase 40
- Locking on FDW / foreign tables — returns `NotImplemented`
- Clustered-table `SELECT FOR UPDATE` — deferred (heap tables only for now)

## Behavior

### SQL syntax

```sql
-- Four PG/MySQL-compatible strengths (weakest to strongest):
SELECT … FOR KEY SHARE   [NOWAIT]    -- shared, key columns only; compat alias
SELECT … FOR SHARE       [NOWAIT]    -- shared; MySQL: LOCK IN SHARE MODE
SELECT … FOR NO KEY UPDATE [NOWAIT]  -- exclusive, non-key columns
SELECT … FOR UPDATE      [NOWAIT]    -- exclusive (strongest; MySQL default)

-- MySQL legacy alias (always Block wait policy):
SELECT … LOCK IN SHARE MODE
```

`NOWAIT` is optional after any strength keyword. When omitted the default wait policy
is `Block` (wait up to `lock_timeout_secs`).

### AST changes

Replace the existing `lock_mode: Option<LockMode>` field on `SelectStmt` with a richer
type that captures strength + wait policy:

```rust
/// Phase 13.7 — replaces the old `LockMode` stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    ForKeyShare,      // weakest — FOR KEY SHARE
    ForShare,         // FOR SHARE / LOCK IN SHARE MODE
    ForNoKeyUpdate,   // FOR NO KEY UPDATE
    ForUpdate,        // strongest — FOR UPDATE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitPolicy {
    Block,   // default: wait up to lock_timeout_secs
    NoWait,  // NOWAIT: fail immediately on conflict
    // Skip — deferred to 13.8b
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectLockClause {
    pub strength: LockStrength,
    pub wait_policy: LockWaitPolicy,
}

pub struct SelectStmt {
    // ... existing fields unchanged ...
    // OLD: pub lock_mode: Option<LockMode>,
    /// Phase 13.7 — None when no locking clause present.
    pub lock_clause: Option<SelectLockClause>,
    // ...
}
```

The old `ast::LockMode` enum is **removed**; all match sites migrated to
`SelectLockClause`.

### Lock acquisition semantics

Physical row-level mode mapping:

| `LockStrength`     | Physical mode | Table intention |
|--------------------|---------------|-----------------|
| `ForKeyShare`      | `Shared`      | `IS`            |
| `ForShare`         | `Shared`      | `IS`            |
| `ForNoKeyUpdate`   | `Exclusive`   | `IX`            |
| `ForUpdate`        | `Exclusive`   | `IX`            |

For each table in FROM that is a local heap table:

1. Acquire table-level intention lock: `lock_mgr.acquire_table_lock_sync(txn_id, table_id, IS | IX)`
2. For each `RecordId` returned by `scan_table`: `lock_mgr.acquire_record_lock_sync(txn_id, page_id, slot_id, Shared | Exclusive, flags)`

`flags` carries `LockFlags::REC_NOT_GAP` (no gap lock; row-exact only).

When `wait_policy == NoWait`, pass `LockFlags::REC_NOT_GAP | LockFlags::NOWAIT` — the
lock manager's sync path checks this flag and skips the Condvar wait, returning
`Err(DbError::LockTimeout)` immediately on conflict.

### `LockFlags::NOWAIT` addition to axiomdb-lock

```rust
impl LockFlags {
    /// Phase 13.7 — skip Condvar wait; return LockTimeout immediately on conflict.
    pub const NOWAIT: Self = Self(0x0020);
}
```

The `acquire_record_lock_sync` fast path (no conflict) is unchanged. The slow path
(must wait) checks `flags.contains(LockFlags::NOWAIT)` before entering `Condvar::wait_timeout`;
if set, removes waiter and returns `Err(DbError::LockTimeout)` immediately.

### txn_id acquisition in executor

The executor obtains the transaction ID from `ctx.coord().active_txn_id()`. If `None`
(autocommit path with no active txn), the executor skips locking silently — autocommit
single-statement transactions release as soon as the statement finishes, making locking
meaningless for them.

### Lock release

Locks are released by calling `lock_mgr.release_all_for_txn(txn_id)` in `shared_db.rs`
at the point of COMMIT and ROLLBACK (including implicit rollback on error). This scans
all 80 shards — O(shards) regardless of lock count. Phase 40.12 can optimize to
O(held_locks) using `TxnLockTracker`.

### Error cases

| Situation | Error | Client message |
|---|---|---|
| Lock blocked + timeout | `DbError::LockTimeout` | `Lock wait timeout exceeded` |
| NOWAIT + conflict | `DbError::LockTimeout` | `Lock wait timeout exceeded; try restarting transaction` |
| Deadlock detected | `DbError::DeadlockDetected` | `Deadlock found when trying to get lock` |
| `FOR UPDATE` on FDW table | `DbError::NotImplemented` | `FOR UPDATE is not supported on foreign tables` |
| `FOR UPDATE` on clustered table | `DbError::NotImplemented` | `FOR UPDATE on clustered tables not yet supported` |
| `FOR UPDATE` in set-op branch | `DbError::ParseError` | `FOR UPDATE is not allowed with UNION/INTERSECT/EXCEPT` |
| `FOR UPDATE` in subquery | silently ignored | — |

### Ordering: locks acquired AFTER filter

Locks are acquired only on rows that pass the WHERE clause. Rows filtered out are never
locked. This matches PostgreSQL `nodeLockRows.c` behavior and InnoDB's implementation.

### Multiple locking clauses

`SELECT … FOR UPDATE FOR SHARE` — last clause wins (PG 14+ allows multiple per-table
clauses via `FOR UPDATE OF t1 FOR SHARE OF t2`, but that is deferred).

## Edge cases

- [ ] `FOR UPDATE` with no active transaction (autocommit) — skip locking, return rows normally
- [ ] `FOR UPDATE` on empty table — no rows to lock, returns immediately
- [ ] Two transactions: txn A locks row, txn B tries `FOR UPDATE` → B waits until A commits
- [ ] NOWAIT: txn B with `FOR UPDATE NOWAIT` gets `LockTimeout` immediately
- [ ] `FOR SHARE` + `FOR UPDATE` on same row from different txns: S and X conflict → X waits
- [ ] Two `FOR SHARE` on same row from different txns: S+S compatible → both granted immediately
- [ ] Deadlock: txn A locks row 1 then row 2; txn B locks row 2 then row 1 → `DeadlockDetected`
- [ ] `FOR UPDATE` with `LIMIT` — lock only rows returned after LIMIT, not all scanned rows
- [ ] `FOR UPDATE` with `ORDER BY` — locks rows in ORDER BY result (post-sort, pre-projection rows)
- [ ] `ROLLBACK` after `FOR UPDATE` — all acquired locks released, blocked txn proceeds
- [ ] Lock upgrade: txn holds S on row, requests X on same row → upgrade in place (already supported by `LockManager`)

## Performance budget

| Operation | Target | Acceptable |
|---|---|---|
| `FOR UPDATE` on 100-row table (no contention) | < 2ms overhead vs plain SELECT | < 5ms |
| Lock acquisition per row (fast path, no conflict) | < 1µs | < 5µs |
| Release all locks on COMMIT (80-shard scan) | < 100µs | < 500µs |

The fast path in `acquire_record_lock_sync` is a mutex lock + hashmap lookup + bitmap
set — expected ~200ns per row.

## Dependencies

- Depends on: `axiomdb-lock` (Phase 40.11, already complete)
- Depends on: `ExecutionContext.lock_mgr` (already wired from `SharedDatabase`)
- Blocks: 13.8 (deadlock detection — already in lock manager, just needs wire test)
- Blocks: 13.8b (SKIP LOCKED)

## Done criteria

- [ ] `LockStrength`, `LockWaitPolicy`, `SelectLockClause` defined in `ast.rs`
- [ ] `SelectStmt.lock_mode` replaced with `SelectStmt.lock_clause`; all match sites compile
- [ ] `LockFlags::NOWAIT = 0x0020` added to `axiomdb-lock`; sync slow path checks it
- [ ] Parser: `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`, each with optional `NOWAIT`
- [ ] Parser: `LOCK IN SHARE MODE` maps to `ForShare + Block`
- [ ] Executor (`select_core.rs`, `select_ctx.rs`): table IX/IS + row X/S locks acquired post-filter
- [ ] `shared_db.rs` COMMIT and ROLLBACK paths call `lock_mgr.release_all_for_txn(txn_id)`
- [ ] `FOR UPDATE` on FDW table → `NotImplemented`
- [ ] `FOR UPDATE` on clustered table → `NotImplemented`
- [ ] 14+ integration tests in `tests/integration_select_for_update.rs`:
  - Basic `FOR UPDATE` returns rows and locks them
  - Blocking: second txn waits (thread-based)
  - NOWAIT: second txn fails immediately
  - `FOR SHARE` compatible with another `FOR SHARE`
  - `FOR SHARE` blocked by `FOR UPDATE`
  - ROLLBACK releases locks
  - Deadlock auto-detected
  - `FOR UPDATE` + `LIMIT` locks only returned rows
  - Autocommit: no error, rows returned
  - FDW table → NotImplemented
  - `FOR KEY SHARE` and `FOR NO KEY UPDATE` map to correct physical modes
- [ ] `cargo nextest run -p axiomdb-sql` — all pass
- [ ] `cargo nextest run --workspace` — all pass
- [ ] `cargo clippy --workspace -- -D warnings` — clean
- [ ] Wire smoke: `SELECT … FOR UPDATE` inside explicit `BEGIN/COMMIT` via pymysql
- [ ] `docs-site/src/user-guide/features/transactions.md` updated
- [ ] `docs-site/src/sql-reference/dml.md` updated with locking clause syntax

## References

- PostgreSQL `src/include/nodes/lockoptions.h` — `LockClauseStrength`, `LockWaitPolicy`
- PostgreSQL `src/backend/executor/nodeLockRows.c:161-194` — strength → lockmode mapping
- PostgreSQL `src/backend/parser/gram.y:13601-13660` — locking clause grammar
- InnoDB `lock0lock.cc` — IX before X row lock protocol
- AxiomDB `crates/axiomdb-lock/src/manager.rs` — `acquire_record_lock_sync` (sync API)
- AxiomDB `crates/axiomdb-lock/src/mode.rs` — `LockMode`, `LockFlags`, conflict matrix
- AxiomDB `crates/axiomdb-sql/src/exec_ctx.rs` — `ExecutionContext.lock_mgr`
- AxiomDB `crates/axiomdb-network/src/mysql/shared_db.rs:322,550,593` — lock_mgr already passed
