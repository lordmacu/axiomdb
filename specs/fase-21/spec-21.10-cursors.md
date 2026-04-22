# Spec: 21.10 — SQL cursors

Phase: 21 — Advanced SQL
Task: 21.10 Cursors
Status: implemented 2026-04-21

## Context

Phase 21 still has one stateful SQL gap: transaction-scoped cursors exposed as
top-level SQL statements. The engine already supports long-lived per-session
state in `SessionContext` (warnings, temp schema, transaction state,
savepoints), but every query currently returns all rows immediately and then
discards its execution state.

This task closes the SQL-surface feature tracked in `docs/progreso.md` as
`DECLARE`, `FETCH`, and `CLOSE`. It explicitly does **not** cover MySQL
prepared-statement server-side cursors (`COM_STMT_FETCH`), which remain a wire
protocol feature and a separate problem.

## Goal

Implement transaction-scoped, read-only SQL cursors with `DECLARE`, `FETCH`,
and `CLOSE`, using a materialized result set stored in `SessionContext`.

## Non-goals

- `COM_STMT_FETCH` / MySQL prepared-statement server-side cursors.
- Stored-procedure cursor variables (`OPEN` / `FETCH` / `CLOSE` inside
  procedures).
- Updatable cursors (`FOR UPDATE`, `WHERE CURRENT OF`).
- `WITH HOLD` cursors that survive `COMMIT`.
- Scroll grammar beyond the bounded MVP below (`PRIOR`, `ABSOLUTE`,
  `RELATIVE`, `BACKWARD`, `MOVE`).

## Behavior

### Public API

```rust
pub struct DeclareCursorStmt {
    pub name: String,
    pub query: Box<Stmt>,
}

pub enum FetchCount {
    Next,
    Forward(u64),
    All,
}

pub struct FetchCursorStmt {
    pub name: String,
    pub count: FetchCount,
}

pub enum CloseCursorStmt {
    One(String),
    All,
}

pub enum Stmt {
    // ...
    DeclareCursor(DeclareCursorStmt),
    FetchCursor(FetchCursorStmt),
    CloseCursor(CloseCursorStmt),
}

pub struct SessionCursor {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    pub pos: usize,
}
```

### SQL surface

Accepted grammar in this subphase:

```sql
DECLARE c CURSOR FOR SELECT ...

FETCH NEXT FROM c
FETCH 10 FROM c
FETCH FORWARD 10 FROM c
FETCH ALL FROM c

CLOSE c
CLOSE ALL
```

`FETCH 10 FROM c` is shorthand for `FETCH FORWARD 10 FROM c`.
`FROM` and `IN` are interchangeable in `FETCH`.
`NEXT` is the default when no count is specified.

### Semantics

- `DECLARE name CURSOR FOR <select>` requires an active explicit transaction.
  Outside an explicit transaction it returns `DbError::InvalidValue`.
- The declared query must be a pure row-returning query (`SELECT` or `SetOp`)
  and is executed immediately at `DECLARE` time.
- The full result is materialized into a `SessionCursor { columns, rows, pos }`
  stored in `SessionContext`.
- The materialized rows are the cursor snapshot. Later writes in the same
  transaction do not mutate already-materialized cursor rows.
- Cursor names are case-insensitive for lookup and unique per session.
  Redeclaring an existing cursor name returns `DbError::InvalidValue`.
- `FETCH NEXT` returns one row from the current position and advances by one.
- `FETCH FORWARD n` / `FETCH n` returns up to `n` rows from the current
  position and advances by the number returned.
- `FETCH ALL` returns all remaining rows and moves the cursor to EOF.
- Fetching at EOF returns `QueryResult::empty_rows(cursor.columns.clone())`.
- `CLOSE name` deletes one cursor. `CLOSE ALL` deletes all session cursors.
- `COMMIT`, `ROLLBACK`, connection reset, and change-user cleanup implicitly
  close all SQL cursors.

### Error cases

| Input | Expected error | Message shape |
|-------|----------------|---------------|
| `DECLARE c CURSOR FOR ...` outside explicit transaction | `DbError::InvalidValue` | mentions explicit transaction |
| `DECLARE c CURSOR FOR UPDATE ...` or non-row-returning stmt | `DbError::InvalidValue` | mentions row-returning query |
| duplicate cursor name in same session | `DbError::InvalidValue` | mentions cursor already exists |
| `FETCH ... FROM missing` | `DbError::InvalidValue` | mentions cursor not found |
| `CLOSE missing` | `DbError::InvalidValue` | mentions cursor not found |
| unsupported fetch variant (`FETCH PRIOR`, etc.) | `DbError::NotImplemented` | points to deferred cursor forms |

## Edge cases

- [x] Empty query result: DECLARE succeeds; FETCH returns empty rows with column metadata.
- [x] EOF fetch after exhausting the cursor: still returns empty rows, not error.
- [x] `FETCH ALL` from a fresh cursor returns every row exactly once.
- [x] `FETCH n` with `n = 0` returns empty rows and leaves position unchanged.
- [x] `FETCH n` beyond remaining rows returns only the tail and lands at EOF.
- [x] `CLOSE ALL` is a no-op when no cursors are open.
- [x] Transaction end closes all cursors.
- [x] Session reset / change-user closes all cursors.
- [x] Cursor names are case-insensitive.

## On-disk format

Not applicable. Cursor state is session-local and in-memory only.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `DECLARE` over 10K-row SELECT | within 5% of plain SELECT wall time | within 10% |
| `FETCH NEXT` / `FETCH 100` over materialized rows | O(k) over returned rows | no full re-scan |

## Dependencies

- Depends on existing `SelectStmt` / `SetOp` execution returning
  `QueryResult::Rows`.
- Depends on `SessionContext` for per-session state.
- Blocks `21.23` advanced SQL tests from covering cursors.

## Open questions

- [x] `FETCH n FROM c` accepts only non-negative integer counts in the MVP.
- [x] `DECLARE` accepts `WITH`-prefixed row-returning queries through the same
      analyzed `SELECT` / `SetOp` path.

## Done criteria

- [x] Parser accepts the bounded grammar above and rejects unsupported forms clearly.
- [x] AST and executor support `DECLARE`, `FETCH`, `CLOSE`.
- [x] Cursor state lives in `SessionContext` and is cleaned up on transaction/session boundaries.
- [x] `cargo test -p axiomdb-sql --test integration_cursors` passes.
- [x] `cargo test -p axiomdb-network --test integration_connection_lifecycle` passes.
- [x] `python3 tools/wire-test.py` covers one cursor smoke.
- [x] `cargo clippy -p axiomdb-sql -- -D warnings` passes via `cargo clippy --workspace -- -D warnings`.

## References

- `docs/progreso.md`
- `memory/project_state.md`
- `crates/axiomdb-network/src/mysql/handler.rs` (`COM_STMT_FETCH` remains separate)
- `crates/axiomdb-sql/src/session.rs`
