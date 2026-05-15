# Spec: 13.8b — SKIP LOCKED

Phase: 13 — Advanced PostgreSQL
Task: SKIP LOCKED pessimistic-locking wait policy
Status: implemented

## Context

Phase 13.7 delivered `SELECT ... FOR UPDATE / FOR SHARE [NOWAIT]` with row-level
locks managed by `axiomdb-lock`. The `LockWaitPolicy` enum currently has two
variants: `Block` (wait up to `lock_timeout`) and `NoWait` (fail immediately).

`SKIP LOCKED` is the third policy: instead of blocking or erroring, the query
**silently omits** any row whose lock cannot be acquired and returns only the
rows that were successfully locked. This is the canonical pattern for
multi-worker job queues where each worker should claim a distinct row.

## Goal

Add `LockWaitPolicy::SkipLocked` so that `SELECT ... FOR UPDATE SKIP LOCKED`
(and equivalent `FOR SHARE / FOR NO KEY UPDATE / FOR KEY SHARE SKIP LOCKED`)
returns only the rows that could be exclusively locked, applying LIMIT *after*
filtering out locked rows.

## Non-goals

- `SKIP LOCKED` on clustered tables — same restriction as `FOR UPDATE`; returns
  `NotImplemented` (deferred).
- `SKIP LOCKED` on foreign tables (HTTP FDW) — same restriction; returns
  `NotImplemented`.
- Gap locks or next-key locks — out of scope for the current lock manager.
- Changing how `Block` or `NoWait` work — no behavioral change to existing modes.

## Behavior

### Public API additions

#### `axiomdb-lock` crate

```rust
impl LockManager {
    /// Try to acquire a record lock without blocking.
    /// Returns `Ok(true)`  — lock granted.
    /// Returns `Ok(false)` — conflicting lock held by another txn (skip this row).
    /// Returns `Err(_)`    — internal error (deadlock abort or storage error).
    ///
    /// Never enqueues a waiter; never blocks.
    pub fn try_acquire_record_lock_sync(
        &self,
        txn_id: TxnId,
        page_id: u64,
        slot_id: u16,
        mode: LockMode,
    ) -> Result<bool, DbError>;
}
```

#### `axiomdb-sql` AST

```rust
pub enum LockWaitPolicy {
    Block,
    NoWait,
    SkipLocked,   // new variant
}
```

#### Parser

```
FOR UPDATE SKIP LOCKED
FOR NO KEY UPDATE SKIP LOCKED
FOR SHARE SKIP LOCKED
FOR KEY SHARE SKIP LOCKED
```

`SKIP` is not a reserved keyword — parsed via `eat_ident_ci(p, "SKIP")` followed
by `eat_ident_ci(p, "LOCKED")`.  `NOWAIT` check runs first; if `SKIP LOCKED`
is present, `NOWAIT` is absent and vice versa.

### Executor pipeline for SKIP LOCKED

When `lc.wait_policy == LockWaitPolicy::SkipLocked` the lock block in
`select_ctx.rs` restructures the pipeline:

```
Standard pipeline (Block / NoWait):
  WHERE → ORDER BY → LIMIT → lock all → project → return

SkipLocked pipeline:
  WHERE → ORDER BY → [no LIMIT yet] → try-lock each row →
  keep only granted rows → LIMIT/OFFSET → project → return
```

Concretely:

1. **WHERE** — collect `rid_pairs: Vec<(RecordId, Row)>` (same as today).
2. **ORDER BY** — sort `rid_pairs` (same as today).
3. **Skip LIMIT** — do NOT truncate yet.
4. **Try-lock loop** — for each `(rid, row)` in order:
   - Call `lm.try_acquire_record_lock_sync(txn_id, rid.page_id, rid.slot_id, row_mode)`.
   - If `Ok(true)` → keep in `locked_pairs`.
   - If `Ok(false)` → discard silently (SKIP).
   - If `Err(e)` → propagate immediately.
5. **LIMIT/OFFSET** — apply to `locked_pairs`.
6. **Project** — same projection path as today.
7. **Early return** — same as today.

Table-intention lock (`acquire_table_lock_sync`) is still acquired before the
loop (IS for share modes, IX for update modes). This ensures the table itself
is not dropped while iterating.

When there is no active `conn_txn` or no `lock_manager` (autocommit path),
locking is silently skipped and all rows are returned (same as `Block`/`NoWait`).

### Semantics of `try_acquire_record_lock_sync`

- **Same-txn upgrade**: if the calling txn already holds a compatible or
  stronger lock on the slot, return `Ok(true)` immediately (same fast path
  as `acquire_record_lock_sync`).
- **No conflict**: if no conflicting granted lock exists and the wait queue is
  empty, grant the lock and return `Ok(true)`.
- **Conflict**: if a conflicting lock is held by another txn, return `Ok(false)`
  without enqueueing a waiter, without touching the wait-for graph, and without
  running deadlock detection. Never blocks.
- **Internal error**: only `Err` on logic errors (e.g. mutex poisoning) — in
  practice never returns `Err` for normal conflicts.

### Error cases

| Situation | Result |
|---|---|
| `SKIP LOCKED` on foreign table | `DbError::NotImplemented { "FOR UPDATE is not supported on foreign tables" }` |
| `SKIP LOCKED` on clustered table | `DbError::NotImplemented { "FOR UPDATE on clustered tables not yet supported" }` |
| `SKIP LOCKED` with GROUP BY / aggregates | falls through to normal pipeline (locking skipped, full result returned) — same as today for `FOR UPDATE` + GROUP BY |
| Lock manager mutex poisoned | `Err(DbError::Other(...))` |

## Edge cases

- [ ] All rows locked → returns 0 rows (empty result, no error)
- [ ] No rows match WHERE → returns 0 rows (no lock attempts)
- [ ] `LIMIT 1 SKIP LOCKED` with first N rows locked → returns the first
  unlocked row (correct job-queue behavior)
- [ ] Same txn already holds a lock on a row → `try_acquire` returns `Ok(true)`,
  row is included (idempotent re-lock)
- [ ] `OFFSET m SKIP LOCKED` — offset applied after skip-lock filtering, not before
- [ ] `FOR SHARE SKIP LOCKED` — shared mode; two txns both holding FOR SHARE on
  same row is compatible → both see the row
- [ ] `SKIP LOCKED` in autocommit — no active txn → all rows returned, no locks acquired
- [ ] `SKIP LOCKED` with `ORDER BY` — rows skipped in order; deterministic which
  rows are returned if locks are stable

## Performance budget

| Operation | Target |
|---|---|
| `try_acquire_record_lock_sync` (no conflict) | ≤ 2 µs (same as granted fast path) |
| `try_acquire_record_lock_sync` (conflict, skip) | ≤ 2 µs (no blocking, no enqueue) |
| `SELECT ... LIMIT 1 SKIP LOCKED` on 1000-row table | ≤ 5 ms end-to-end |

## Dependencies

- Depends on: `specs/fase-13/spec-13.7-select-for-update.md` (implemented ✅)
- Blocks: nothing — this is the last subphase of Phase 13

## Open questions

None — all resolved in brainstorm.

## Done criteria

- [ ] `LockManager::try_acquire_record_lock_sync` implemented in `axiomdb-lock`
- [ ] `LockWaitPolicy::SkipLocked` variant in AST
- [ ] Parser: `FOR UPDATE SKIP LOCKED` (and all four strength variants) parse correctly
- [ ] Executor: LIMIT applied *after* skip-lock filtering
- [ ] `LIMIT 1 SKIP LOCKED` with a pre-locked row returns the next available row (not 0 rows)
- [ ] All rows locked → 0 rows returned, no error
- [ ] `FOR SHARE SKIP LOCKED` — two sessions both see the row (shared compatibility)
- [ ] `SKIP LOCKED` in autocommit → all rows returned, no error
- [ ] `OFFSET` + `SKIP LOCKED` applies offset after filtering
- [ ] `cargo nextest run -p axiomdb-lock` passes
- [ ] `cargo nextest run -p axiomdb-sql` passes (including new integration tests)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke test: `528 + N` assertions passing
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated (SKIP LOCKED in FOR UPDATE table)
- [ ] `docs-site/src/user-guide/features/transactions.md` SKIP LOCKED limitation removed

## References

- Implemented predecessor: `specs/fase-13/spec-13.7-select-for-update.md`
- PostgreSQL: `src/backend/executor/nodeLockRows.c` — `TM_WouldBlock → goto lnext`
- PostgreSQL: `src/include/nodes/lockoptions.h` — `LockWaitSkip` enum variant
- MySQL 8.0: `SELECT ... FOR UPDATE SKIP LOCKED` (added 8.0.1)
