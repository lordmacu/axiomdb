# Spec: 21.11 — Query hints

Phase: 21 — Advanced SQL
Task: 21.11 Query hints
Status: completed

## Context

Phase 21 still lists SQL query hints as a pending compatibility gap:

- `/*+ INDEX(...) */`
- `/*+ HASH_JOIN */`
- `/*+ PARALLEL(...) */`

The important cut is narrower than some old docs suggest. MySQL-style SELECT
modifiers such as `HIGH_PRIORITY`, `STRAIGHT_JOIN`, `SQL_SMALL_RESULT`,
`SQL_BIG_RESULT`, and `SQL_BUFFER_RESULT` are already parsed and consumed in
`parser/dml.rs`, with integration coverage in
`tests/integration_g6_functions.rs`. Those modifiers are **not** the missing
work for `21.11`.

The real missing surface is comment-based optimizer hints. Today the lexer
skips regular `/* ... */` block comments entirely, so `/*+ ... */` cannot
reach the parser or planner. On the execution side, AxiomDB already has:

- adaptive hash-vs-nested-loop join selection in `executor/joins.rs`
- index selection in `planner_select.rs`
- internal parallel table-scan support in `table_scan.rs`

This subphase adds a bounded SQL hint surface that can steer those existing
paths without introducing a full MariaDB/MySQL optimizer-hint framework.

## Goal

Implement bounded SQL query hints for `SELECT` statements using optimizer
comments, with real execution impact for index choice and hash-join choice.

## Non-goals

- A full MariaDB-style hint engine with query-block names, conflict precedence,
  global/session hint state, QB_NAME, JOIN_ORDER, NO_* variants, or dozens of
  hint families.
- Hint support on `UPDATE`, `DELETE`, `INSERT`, `MERGE`, or DDL in this
  subphase.
- A guaranteed parallel executor contract. `PARALLEL(n)` is accepted as an
  advisory hint only.
- Hint persistence in the catalog.
- Hints inside regular comments anywhere in the statement; only the supported
  `SELECT /*+ ... */` placement is in scope.

## SQL surface

Accepted forms in this subphase:

```sql
SELECT /*+ HASH_JOIN */ ...
SELECT /*+ PARALLEL(4) */ ...
SELECT /*+ INDEX(users idx_users_email) */ ...
SELECT /*+ INDEX(u idx_users_email) HASH_JOIN PARALLEL(8) */ ...
```

Supported placement:

```sql
SELECT /*+ ... */ <select list> FROM ...
```

Not supported in this subphase:

```sql
SELECT col /*+ ... */ FROM t
UPDATE /*+ ... */ t SET ...
SELECT /*+ NO_HASH_JOIN */ ...
SELECT /*+ INDEX_MERGE(...) */ ...
SELECT /*+ QB_NAME(main) */ ...
```

## Public API / AST

```rust
pub enum SelectHint {
    Index { table: String, index: String },
    HashJoin,
    Parallel { workers: usize },
}

pub struct SelectStmt {
    // ...
    pub hints: Vec<SelectHint>,
}
```

Parser contract:

- `SelectStmt.hints.is_empty()` means no optimizer hints were supplied.
- Hints are preserved in source order.
- Unsupported hint names are rejected with a parse error in this bounded MVP,
  rather than silently ignored.

## Semantics

### General

- Hints are advisory planner/executor directives attached to one `SELECT`.
- Hints never change query results; they only affect access-path / join-path
  selection or are accepted as advisory metadata.
- Hints are case-insensitive by name.
- Duplicate identical hints are tolerated.
- Conflicting hints are rejected when this MVP cannot reconcile them
  deterministically.

### `INDEX(table index_name)`

- Applies to one base table reference in the current SELECT.
- `table` may match the table name or its alias.
- `index_name` must resolve to an existing index on that table.
- Planner effect:
  - when planning that table's scan, prefer the named index if the query
    predicate is compatible with it
  - if the named index is incompatible with the query predicate, fall back to
    the existing planner path instead of forcing a broken plan
- Error cases:
  - unknown table/alias in the hint → analysis/planning error
  - unknown index name for the matched table → analysis/planning error

### `HASH_JOIN`

- Applies to the current SELECT's join execution.
- Requests hash join whenever the join condition is an equijoin and the join
  type is already supported by the existing hash-join implementation.
- If the join is not hash-joinable (for example non-equijoin), execution falls
  back to the current nested-loop path.
- This hint does not add new hash-join capabilities; it only overrides the
  size-threshold heuristic when the existing hash path is legal.

### `PARALLEL(n)`

- `n` must be a positive integer.
- Parsed and stored as a SELECT hint.
- Advisory only in this subphase:
  - accepted through parser/analyzer/planner/executor
  - may be surfaced in `EXPLAIN`
  - does not guarantee `n` workers or any parallel plan shape
- The hint must not fail a query solely because the executor cannot honor it.

## Edge cases

- [ ] `SELECT /*+ HASH_JOIN */ ...` on a non-equijoin still succeeds and uses
      the existing non-hash path.
- [ ] `SELECT /*+ INDEX(t idx) */ ...` on a query with no indexable predicate
      still succeeds and falls back cleanly.
- [ ] Alias matching works: `FROM users u` pairs with `INDEX(u idx_users_email)`.
- [ ] Unknown table alias in `INDEX(...)` errors deterministically.
- [ ] Unknown index name in `INDEX(...)` errors deterministically.
- [ ] `PARALLEL(0)` is rejected.
- [ ] Duplicate `HASH_JOIN` hints do not change semantics.
- [ ] Unsupported hints such as `NO_HASH_JOIN` are rejected with a clear parse
      error instead of being silently discarded.
- [ ] Existing ordinary block comments continue to be skipped normally.

## Planner / executor behavior

### Hint capture

Because the lexer currently skips block comments, `/*+ ... */` must be
captured before ordinary comment stripping loses the contents. The resulting
hint payload is then parsed and attached to `SelectStmt`.

Compatibility rule:

- regular `/* ... */` comments remain skipped
- MySQL version comments `/*! ... */` keep their existing behavior
- optimizer hints `/*+ ... */` are the only new comment form preserved for SQL
  statement analysis

### Planner interaction

- `INDEX(...)` only constrains the target table's candidate index choice.
- Existing partial-index and expression-index compatibility checks still apply.
- If the requested index is incompatible with the query predicate, the planner
  must not produce a malformed lookup/range plan just to satisfy the hint.

### Join interaction

- `HASH_JOIN` bypasses the current row-count threshold in `executor/joins.rs`
  when `detect_equijoin(...)` already proves the join is hash-compatible.
- Join-type legality remains unchanged.

### `EXPLAIN`

- `EXPLAIN` output should make the effect visible enough to verify:
  - hinted index selection is visible via the chosen index
  - hinted hash join is visible via the chosen join path
  - advisory parallel hint may be surfaced textually if helpful

## Acceptance criteria

1. [ ] `SELECT /*+ INDEX(t idx_t_a) */ * FROM t WHERE a = 1` parses and can
       steer the planner to the named index when compatible.
2. [ ] `SELECT /*+ HASH_JOIN */ * FROM t JOIN u ON t.id = u.t_id` parses and
       uses the hash-join path even below the normal size threshold.
3. [ ] `SELECT /*+ PARALLEL(4) */ * FROM t` parses, executes, and does not
       error even if no parallel plan is chosen.
4. [ ] Unsupported hints are rejected cleanly with a parse error.
5. [ ] Unknown table/index references in `INDEX(...)` fail deterministically.
6. [ ] Existing non-hint block comments still behave exactly as before.
7. [ ] Existing SELECT modifiers (`HIGH_PRIORITY`, `STRAIGHT_JOIN`, etc.)
       continue to parse unchanged.
8. [ ] `EXPLAIN` coverage proves the hinted access path / join path was chosen.
9. [ ] `python3 tools/wire-test.py` gains at least one `21.11` smoke.

## Out of scope follow-ups

- `NO_HASH_JOIN`, `NO_INDEX`, `INDEX_MERGE`, `MRR`, `BKA`, `SEMIJOIN`,
  `JOIN_ORDER`, `JOIN_PREFIX`, `QB_NAME`, and table-level `USE/FORCE/IGNORE INDEX`.
- Hint support for DML statements other than `SELECT`.
- Strong `PARALLEL(n)` worker-count guarantees or a full parallel plan model.
- Rich conflict resolution between multiple incompatible hints.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| Hint parsing overhead on a hinted SELECT | negligible vs existing parse cost | no measurable regression on non-hinted queries |
| `HASH_JOIN` on eligible small equijoins | follows current hash-join runtime | no correctness regression |
| `INDEX(...)` on compatible lookup/range queries | same as normal index path | no malformed plan / forced full error |

## Dependencies

- `crates/axiomdb-sql/src/lexer.rs` — comment handling
- `crates/axiomdb-sql/src/parser/dml.rs` — SELECT parsing
- `crates/axiomdb-sql/src/ast.rs` — hint storage
- `crates/axiomdb-sql/src/planner_select.rs` / `planner_ctx.rs` — index choice
- `crates/axiomdb-sql/src/executor/joins.rs` — adaptive join selection
- `crates/axiomdb-sql/src/table_scan.rs` — optional parallel-scan plumbing
- `tools/wire-test.py`

## Open questions

- [x] `21.11` is about optimizer comments, not legacy SELECT modifiers.
- [x] `PARALLEL(n)` is advisory only in this cut.
- [x] Unsupported hints are rejected, not silently ignored, to keep the MVP
      deterministic and testable.

## Done criteria

- [ ] `SELECT /*+ ... */` hints are captured before block-comment skipping.
- [ ] `SelectStmt` carries parsed hints.
- [ ] `INDEX(table index)` can steer planner index selection when compatible.
- [ ] `HASH_JOIN` can override the size threshold for eligible equijoins.
- [ ] `PARALLEL(n)` parses and flows through execution without breaking queries.
- [ ] `EXPLAIN` coverage proves hinted behavior.
- [ ] `cargo test -p axiomdb-sql --test integration_query_hints` passes.
- [ ] `python3 tools/wire-test.py` passes with `21.11` smoke coverage.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace -- -D warnings` passes.

## References

- `docs/progreso.md`
- `memory/project_state.md`
- `db.md`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/src/lexer.rs`
- `crates/axiomdb-sql/src/planner_select.rs`
- `crates/axiomdb-sql/src/executor/joins.rs`
- `crates/axiomdb-sql/src/table_scan.rs`
- `research/mariadb-server/sql/opt_hints_parser.cc`
- `research/mariadb-server/sql/opt_hints.h`
