# Plan: 21.10 — SQL cursors

Phase: 21 — Advanced SQL
Task: 21.10 Cursors
Spec: specs/fase-21/spec-21.10-cursors.md
Status: completed 2026-04-21

## Summary

Implement `21.10` as a bounded SQL-cursor MVP in four layers. First add the
statement grammar and AST nodes for `DECLARE`, `FETCH`, and `CLOSE`. Then add
session-local cursor storage plus lifecycle cleanup hooks. After that wire the
executor to materialize cursor queries at `DECLARE` time and slice rows on
`FETCH`. Finish with SQL and wire regression coverage plus the usual closeout
docs/memory updates.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.10-cursors.md` approved.
- [x] Existing transaction-boundary behavior in `SessionContext` / network
      lifecycle remains green after 21.8.

Blocks:
- [x] `21.23` advanced SQL cursor coverage still depends on this feature and is now unblocked.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_cursors.rs` — end-to-end SQL cursor coverage.
- `crates/axiomdb-sql/src/executor/cursor.rs` — cursor execution helpers.

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — new cursor statement structs/variants.
- `crates/axiomdb-sql/src/lib.rs` — re-export new cursor AST types.
- `crates/axiomdb-sql/src/parser/mod.rs` — statement-level dispatch.
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — analyze declared cursor queries.
- `crates/axiomdb-sql/src/session.rs` — session-local cursor map + cleanup helpers.
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — route cursor statements.
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — EXPLAIN dispatch arms.
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` — transaction boundary cleanup.
- `crates/axiomdb-sql/src/plan_deps.rs` — declared query dependency tracking.
- `crates/axiomdb-network/src/mysql/shared_db.rs` — read-only statement classification.
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — cursor cleanup assertions.
- `tools/wire-test.py` — 21.10 cursor smoke.
- `docs/progreso.md`, `memory/project_state.md`, `docs/fase-21.md`,
  `memory/architecture.md`, `memory/lessons.md` — subphase closeout.

## Step 1 — Grammar and AST

**Goal:** parse the bounded cursor grammar and represent it explicitly in the AST.
**Files:** `ast.rs`, `lexer.rs`, `parser/mod.rs`.
**Approach:** TDD — parser-first tests for success and rejection cases.

### Tests to add

```rust
#[test]
fn parse_declare_cursor_for_select() { ... }

#[test]
fn parse_fetch_forward_count() { ... }

#[test]
fn parse_close_all() { ... }
```

### Implementation outline

- Add `Stmt::{DeclareCursor, FetchCursor, CloseCursor}` plus helper structs.
- Add lexer tokens for `DECLARE`, `CURSOR`, `FORWARD`, and `CLOSE` if missing.
- Extend `parse_stmt()` with statement-level cursor branches.
- Keep unsupported fetch variants (`PRIOR`, `ABSOLUTE`, etc.) either unparsed
  or parsed-and-rejected consistently per spec.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Session cursor state and lifecycle

**Goal:** store materialized cursors in `SessionContext` and clean them up safely.
**Files:** `session.rs`, `exec_with_ctx.rs`, optionally network lifecycle hooks.
**Approach:** add a small `SessionCursor` struct and helper methods instead of
scattering `HashMap` mutations through executor call sites.

### Tests to add

```rust
#[test]
fn close_all_cursors_noop_when_empty() { ... }

#[test]
fn commit_clears_session_cursors() { ... }
```

### Implementation outline

- Add `HashMap<String, SessionCursor>` to `SessionContext`.
- Normalize names on insert/lookup (ASCII lowercase).
- Add helpers: `declare_cursor`, `fetch_cursor`, `close_cursor`, `close_all_cursors`.
- Clear cursor state on `COMMIT` / `ROLLBACK`, and expose one cleanup helper
  reusable by network reset/change-user/disconnect paths.

### Verification

```bash
cargo test -p axiomdb-sql
cargo test -p axiomdb-network --test integration_connection_lifecycle
```

## Step 3 — Executor semantics

**Goal:** implement `DECLARE`, `FETCH`, and `CLOSE`.
**Files:** new `executor/cursor.rs`, `exec_dispatch.rs`, `exec_explain.rs`.
**Approach:** materialize once at `DECLARE`, slice rows on `FETCH`.

### Tests to add

```rust
#[test]
fn declare_requires_explicit_transaction() { ... }

#[test]
fn fetch_next_and_fetch_all_advance_cursor() { ... }

#[test]
fn fetch_missing_cursor_errors() { ... }
```

### Implementation outline

- `DECLARE`: require `ctx.in_explicit_txn && ctx.conn_txn.is_some()`, execute the
  inner query via existing SELECT dispatch, require `QueryResult::Rows`, store it.
- `FETCH NEXT`: return one row and increment `pos`.
- `FETCH FORWARD n` / `FETCH n`: return `rows[pos .. pos+n]`.
- `FETCH ALL`: return `rows[pos ..]`.
- Empty fetches use `QueryResult::empty_rows(columns.clone())`.
- `CLOSE`: remove one or all cursors and return `QueryResult::Empty`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_cursors
cargo clippy -p axiomdb-sql -- -D warnings
```

## Step 4 — Wire-visible regression and closeout

**Goal:** verify cursor behavior through the MySQL protocol and close the subphase.
**Files:** `tools/wire-test.py`, docs/memory closeout files.

### Tests to add

```python
# tools/wire-test.py
cur.execute("BEGIN")
cur.execute("DECLARE c CURSOR FOR SELECT ...")
cur.execute("FETCH 2 FROM c")
assert cur.fetchall() == ...
cur.execute("CLOSE c")
cur.execute("COMMIT")
```

### Verification

```bash
cargo fmt --check
cargo test -p axiomdb-sql --test integration_cursors
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Cursor semantics drift into MySQL `COM_STMT_FETCH` scope | medium | Keep spec/parser limited to SQL statements only; leave wire command unsupported |
| Transaction cleanup misses one lifecycle edge | medium | Centralize cleanup helper in `SessionContext` and reuse it from network hooks |
| Large cursor materialization increases memory cost | medium | Explicitly document materialized MVP and keep streaming/hold cursors out of scope |
| Unsupported fetch variants create parser ambiguity with 21.19 `FETCH FIRST` | low | Keep cursor parsing at statement level; 21.19 remains inside SELECT tail parsing |

## Estimated effort

Total: high

- Step 1: 45-60 min
- Step 2: 45-60 min
- Step 3: 1.5-2 h
- Step 4: 45-60 min
