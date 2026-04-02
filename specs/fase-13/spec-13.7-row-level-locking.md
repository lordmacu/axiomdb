# Spec: 13.7 Row-level locking

## What to build (not how)

Replace the current whole-database single-writer execution model with
transaction-scoped row locking for mutating statements so that concurrent
sessions can write at the same time when they touch different rows.

The current server architecture serializes every write through
`Arc<RwLock<Database>>::write()` and a single global `TxnManager` active
transaction slot. This subphase removes that bottleneck for `UPDATE` and
`DELETE` by making row conflicts the unit of contention instead of the
entire database.

The required observable behavior is:

- Different sessions may hold active write transactions simultaneously.
- `UPDATE` and `DELETE` lock only the target rows they will mutate.
- Two transactions mutating different rows must proceed concurrently.
- Two transactions mutating the same row must conflict: one proceeds, the
  other waits until the row lock is released or times out.
- Row locks are held until `COMMIT` or `ROLLBACK`.
- Read-only `SELECT` remains lock-free and is not blocked by row locks.
- After waiting on a conflicting row lock, the statement must re-check row
  visibility and `WHERE` conditions before applying its mutation.

This subphase is the first mandatory step toward MySQL/PostgreSQL-grade
write concurrency. It does not yet include deadlock detection or explicit
locking syntax.

## Inputs / Outputs

- Input: concurrent client sessions connected to the same server process,
  each issuing `BEGIN` / `COMMIT` / `ROLLBACK`, `UPDATE`, and `DELETE`
  statements against the same database.
- Output:
  - non-conflicting `UPDATE` / `DELETE` statements execute concurrently
  - conflicting `UPDATE` / `DELETE` statements wait on the specific row
  - waited statements either continue after the blocking transaction ends
    or fail with `DbError::LockTimeout`
  - row locks are released on commit and rollback
  - reads continue to use MVCC snapshots without waiting for writers
- Errors:
  - `DbError::LockTimeout` when a row lock cannot be acquired within the
    session `lock_timeout`
  - no new SQL-visible error type is required in this subphase

## Use cases

1. Concurrent updates to different rows
   - Session A: `BEGIN; UPDATE accounts SET balance = balance - 10 WHERE id = 1;`
   - Session B: `BEGIN; UPDATE accounts SET balance = balance + 10 WHERE id = 2;`
   - Both statements run concurrently and succeed without waiting on each other.

2. Conflicting updates to the same row
   - Session A: `BEGIN; UPDATE accounts SET balance = balance - 10 WHERE id = 1;`
   - Session B: `BEGIN; UPDATE accounts SET balance = balance + 10 WHERE id = 1;`
   - Session B waits until Session A commits or rolls back, then re-checks the
     row and either applies its update or returns 0 affected rows if the row is
     no longer visible or no longer matches the predicate.

3. Delete/update conflict
   - Session A: `BEGIN; DELETE FROM jobs WHERE id = 42;`
   - Session B: `BEGIN; UPDATE jobs SET status = 'running' WHERE id = 42;`
   - Session B waits on row `(jobs, 42)`. If A commits the delete, B resumes,
     re-checks the row, and reports 0 affected rows.

4. Read query during write contention
   - Session A holds row locks inside `BEGIN`.
   - Session B runs `SELECT * FROM jobs WHERE id = 42`.
   - Session B uses its MVCC snapshot and does not wait on Session A's row locks.

5. Transaction rollback releases locks
   - Session A updates several rows and then `ROLLBACK`s.
   - All row locks owned by A are released immediately.
   - Waiting sessions resume without requiring a server restart or manual cleanup.

## Acceptance criteria

- [ ] Multiple sessions can hold active write transactions simultaneously.
- [ ] The server no longer requires a whole-database exclusive lock for every
      `UPDATE` / `DELETE`.
- [ ] `UPDATE` acquires exclusive row locks on every row it mutates.
- [ ] `DELETE` acquires exclusive row locks on every row it mutates.
- [ ] Row locks are keyed by logical row identity: `(table_id, RecordId)`.
- [ ] Conflicting row-lock acquisition waits only on the conflicting row, not
      on unrelated rows or tables.
- [ ] Row locks are released on `COMMIT`.
- [ ] Row locks are released on `ROLLBACK`.
- [ ] After a wait, the executor re-reads and re-validates the target row
      before mutating it.
- [ ] Read-only statements remain lock-free and do not wait on row locks.
- [ ] Existing `lock_timeout` session setting applies to row-lock waits.
- [ ] Integration tests prove that two sessions can update different rows
      concurrently and remain consistent.
- [ ] Integration tests prove that conflicting updates serialize correctly.
- [ ] Integration tests prove that rollback releases row locks.

## Out of scope

- Deadlock detection and victim selection (`13.8`)
- `SELECT ... FOR UPDATE`, `FOR SHARE`, `NOWAIT`, `SKIP LOCKED` (`13.8b` / `28.2`)
- Gap locks / next-key locks / predicate locking
- Full SSI / serializable conflict graphs
- Advisory locks and table locks
- Multi-process write coordination across independent server processes
- Embedded-mode multi-writer guarantees

## Dependencies

- Phase 7.4 / 7.5 current concurrency baseline (`Arc<RwLock<Database>>`)
- Phase 7.1 snapshot isolation semantics
- Phase 3 transaction/WAL infrastructure
- Phase 5 autocommit + explicit transaction semantics
- Phase 7.8 snapshot registry for safe deferred page reuse

## ⚠️ DEFERRED

- Explicit deadlock detection remains in `13.8`. In `13.7`, cyclic waits are
  broken only by the existing lock timeout path.
- Explicit SQL locking clauses (`FOR UPDATE`, `SKIP LOCKED`, `NOWAIT`) remain
  in `13.8b` / `28.2`.
- Concurrent `INSERT` conflict handling on unique keys is not expanded in this
  subphase beyond existing uniqueness enforcement; `13.7` focuses on removing
  the global writer bottleneck for `UPDATE` / `DELETE`.
