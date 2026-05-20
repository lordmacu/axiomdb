# Spec: appender-v11-complete — production-ready embedded Appender

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 7 v1.1 — lift every v1 limitation so the embedded Appender
works on the same tables SQL `INSERT` works on
Status: implemented

## Context

Attack 7 v1 (`spec-embedded-appender.md`) shipped the Appender as a
"fast-path embedded INSERT API" but rejected five categories of tables
at open time, forcing users to fall back to SQL INSERT for any
production schema:

1. **Clustered (PRIMARY KEY)** tables — most production tables
2. **CHECK** constraints
3. **FOREIGN KEY** constraints
4. **AUTO_INCREMENT / SERIAL** columns
5. **GENERATED ALWAYS** columns

The v1 bench has to use a parallel `bench_users_heap` table (no PK) for
this reason — the canonical `bench_users` (with `PRIMARY KEY (id)`)
errors out. That's a known hole the v1 docs admit.

v1.1 closes the hole: every table that SQL `INSERT` can write, the
Appender can also write — at Appender speed.

## Goal

The Appender accepts and correctly writes rows into any table that the
SQL `INSERT` path accepts, while keeping the v1 performance win
(skipping parse/analyze/dispatch/per-statement scaffolding).

## Non-goals

- **TRIGGERS**: still rejected at open. Statement-level triggers
  require the SQL executor's full statement-trigger machinery; deferred.
- **RETURNING**: still out of scope. Caller queries inserted IDs after
  `finish()` via SQL if needed.
- **ON CONFLICT / ON DUPLICATE KEY UPDATE**: still SQL-only. The
  conflict-resolution path is statement-level and tied to the
  parser/analyzer.
- **Multi-table writes**: still one table per Appender.
- **C FFI**: bindings deferred to a follow-up. v1.1 is Rust-only API
  surface, same as v1.
- **VIRTUAL generated columns**: existing code rejects them in
  `materialize_generated_columns:248`. Out of scope here.
- **DEFAULT-on-omitted-column ergonomics**: the Appender still
  requires all columns to be provided. A nicer API where
  `append_row(&[Value::Default, Value::Int(1)])` triggers DEFAULT
  evaluation is a v1.2 idea, not v1.1.

## Behavior

### Public API — additions and changes

```rust
// crates/axiomdb-embedded/src/appender.rs

impl<'db> Appender<'db> {
    // EXISTING from v1 — UNCHANGED signatures:
    pub fn pending(&self) -> usize;
    pub fn append_row(&mut self, values: &[Value]) -> Result<(), DbError>;
    pub fn append_row_owned(&mut self, values: Vec<Value>) -> Result<(), DbError>;
    pub fn flush(&mut self) -> Result<(), DbError>;
    pub fn finish(self) -> Result<u64, DbError>;
}
```

**No new public methods.** v1.1 lifts the *internal* rejections — the
v1 user-facing API stays byte-identical so existing v1 callers don't
have to change anything.

What changes internally:

1. **`Appender::open` no longer rejects** clustered, CHECK, FK,
   AUTO_INC, or generated columns. Only triggers remain rejected.
2. **`append_row` runs the full per-row pipeline** before pushing into
   the buffer (currently: arity + coerce + NOT NULL; v1.1 adds:
   AUTO_INC assignment, generated-column materialization, text
   constraints, CHECK, FK).
3. **`flush` dispatches to heap or clustered helper** based on
   `table_def.is_clustered()`.

### New helpers exposed from `axiomdb-sql`

Bump `pub(crate)` → `pub` and re-export from `axiomdb_sql` root:

```rust
// from crates/axiomdb-sql/src/executor/insert_helpers.rs
pub fn materialize_generated_columns(
    schema_cols: &[ColumnDef], row_values: &mut [Value],
) -> Result<(), DbError>;

pub fn enforce_text_constraints(
    schema_cols: &[ColumnDef], row_values: &mut [Value],
) -> Result<(), DbError>;

pub fn check_row_constraints_with_cols(
    constraints: &[ConstraintDef], row_values: &[Value],
    table_name: &str, columns: &[ColumnDef],
) -> Result<(), DbError>;
```

Already `pub`: `axiomdb_sql::fk_enforcement::check_fk_child_insert`.

New helper (write a small wrapper, not re-expose the existing
internal):

```rust
// crates/axiomdb-sql/src/table_ctx.rs — new fn alongside insert_rows_batch_with_ctx
impl TableEngine {
    /// Auto-increment helper extracted from insert_heap_ctx.rs:94-123.
    /// Returns the next value for `col_idx` in `table_def`. Uses the
    /// AUTO_INC_SEQ thread-local cache; on miss, scans the heap.
    pub fn next_auto_increment_value(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        schema_cols: &[ColumnDef],
        col_idx: usize,
    ) -> Result<i64, DbError>;
}
```

And for clustered tables, a new batch helper that mirrors the
heap-side `insert_rows_batch_with_ctx`:

```rust
// crates/axiomdb-sql/src/table_ctx.rs — new
impl TableEngine {
    /// Clustered-table analog of insert_rows_batch_with_ctx — accepts
    /// pre-validated/pre-coerced rows and writes them to the clustered
    /// storage (B-Tree of rows keyed by PK) + WAL. Used by the
    /// embedded Appender for clustered tables.
    ///
    /// Assumes the caller already ran constraint/FK/generated/auto_inc.
    pub fn insert_clustered_rows_batch_with_ctx(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        batch: &[Vec<Value>],
    ) -> Result<Vec<RecordId>, DbError>;
}
```

### Semantics

The v1.1 Appender pipeline for one `append_row` call:

1. Arity check (`values.len() == columns.len()`) — same as v1
2. **AUTO_INCREMENT assignment** for any column with `auto_increment =
   true` AND value `Value::Null` — assigns the next value via
   `next_auto_increment_value`, updates the row
3. **Generated-column materialization** for any column with
   `generated_expr.is_some()` — runs `materialize_generated_columns`,
   which evaluates the stored expression and writes the result into
   the row (rejects an explicit non-NULL value for a `GENERATED ALWAYS`
   column, mirroring SQL INSERT)
4. **Type coercion** via `coerce_values_with_ctx` — same as v1
5. **Text constraints** (CHAR padding, VARCHAR length) via
   `enforce_text_constraints` — mutates row in place
6. **NOT NULL** check — same as v1
7. **CHECK constraints** via `check_row_constraints_with_cols`
8. **FK constraints (immediate)** via `check_fk_child_insert` — uses
   the appender's `conn_txn` snapshot + bloom; deferred FKs are
   queued in `ctx.deferred_fk_constraint_ids` (same as SQL)
9. Push the validated row into `self.buffer`. Auto-flush at 1024.

`flush` dispatches:

```rust
let rids = if self.table_def.is_clustered() {
    TableEngine::insert_clustered_rows_batch_with_ctx(...)?
} else {
    TableEngine::insert_rows_batch_with_ctx(...)?
};
// then index_maintenance::insert_into_indexes_with_undo as in v1
```

`finish` is unchanged.

### Error cases

| Input | Expected error |
|-------|----------------|
| `appender("clustered_table")` | now SUCCEEDS (was `NotImplemented`) |
| `appender("table_with_check")` | now SUCCEEDS |
| `appender("table_with_fk")` | now SUCCEEDS |
| `appender("table_with_serial")` | now SUCCEEDS |
| `appender("table_with_generated")` | now SUCCEEDS |
| `appender("table_with_trigger")` | still `NotImplemented` (v2) |
| `append_row` with row violating CHECK | `CheckViolation` (per-row, NOT batch) |
| `append_row` with row referencing missing FK parent | `ForeignKeyViolation` (per-row, but immediate-mode only — deferred FKs queued, validated at commit) |
| `append_row` with explicit value for `GENERATED ALWAYS` non-NULL | `Other("explicit value for generated column")` (mirror SQL) |
| `append_row` with `Value::Null` in `AUTO_INCREMENT` column | SUCCEEDS — value auto-assigned |

## Edge cases

- [ ] AUTO_INC column with caller-supplied non-NULL value → respected
  (no auto-assign), same as SQL `INSERT VALUES (5, ...)`
- [ ] AUTO_INC column with `Value::Null` → next value assigned; cache
  advances; second appender (same Db, after first finish) continues
  the sequence
- [ ] AUTO_INC across multiple flushes inside one appender → values
  monotonic, no gaps from auto-flush
- [ ] Generated STORED column: explicit `DEFAULT`-like behavior is
  `Value::Null` accepted (rebuilt by `materialize_generated_columns`);
  explicit non-NULL → error
- [ ] CHECK violation on row N of a 100-row batch → rows 1..N-1
  pending, append fails, appender remains usable (caller can retry
  without bad row)
- [ ] Deferred FK at append vs immediate FK at append — only immediate
  validated per-row; deferred queued and resolved at `finish()`
  (which calls commit → triggers deferred FK resolution)
- [ ] FK to row inserted in SAME appender batch (parent row appended
  earlier in same flush) — must succeed via snapshot+bloom that sees
  appender's own writes
- [ ] Clustered table: 100k rows append → correct order on disk,
  scan returns sorted
- [ ] Clustered table: PK duplicate → `UniqueViolation` at flush
  (atomic batch rolls back)
- [ ] Clustered table with secondary index: both clustered B-Tree
  AND secondary index updated
- [ ] Mixed: clustered table + AUTO_INC PK + secondary index + CHECK
  on a non-PK column → all enforced

## Performance budget

| Metric | Target |
|---|---:|
| `bench_users` (clustered PK, no constraints) via Appender on Lima | ≥ 150K ops/s (same ballpark as the heap-only v1 number of 204K, with some loss from clustered B-Tree overhead) |
| Regression on v1 heap-only case | ≤ 5% slower than v1 |
| Per-row validation cost (CHECK + FK + AUTO_INC + generated) | should add < 5µs/row when constraints are present |

## Dependencies

Depends on:
- Existing Appender v1 (`crates/axiomdb-embedded/src/appender.rs`)
- All the helpers cited in the "New helpers exposed" section

Blocks:
- Attack 8 (typed builder API): v1.1 finishes the functional surface;
  Attack 8 optimizes within it
- C FFI for Appender: easier once the API is stable v1.1

## Open questions

- [x] Should clustered Appender call `enqueue_clustered_insert_ctx`
  (uses `ctx.clustered_insert_batch` staging) or get a new dedicated
  helper? → **New dedicated helper.** The Appender already owns its
  own batch; reusing the session staging buffer would create
  ordering surprises if SQL INSERT is interleaved (which v1 already
  rejected at open).
- [x] Deferred FK semantics: validate on each `flush` or on `finish`?
  → **On `finish`.** Same as SQL INSERT inside an explicit
  transaction — `commit` triggers `validate_deferred_fks_on_commit`
  (existing machinery). The Appender's `finish` already commits, so
  this falls out for free.
- [x] AUTO_INCREMENT cache invalidation across appenders → already
  handled by the existing `AUTO_INC_SEQ` thread-local invalidation.
- [ ] Should the bench drop `bench_users_heap` once v1.1 supports
  `bench_users` (clustered)?

  Recommendation: keep both — the heap variant is the cleanest
  apples-to-apples vs heap-table workloads; the clustered variant
  matches what real apps use. Report both numbers.

## Done criteria

- [ ] Public API in `crates/axiomdb-embedded/src/lib.rs` is BYTE-
  IDENTICAL to v1 (no new pub fns; only internal lift of rejections)
- [ ] All 5 v1.1 rejection types now succeed (negative tests from v1
  inverted to positive in `integration_appender.rs`)
- [ ] New positive tests cover every "Edge cases" item
- [ ] `bench_users` (clustered) → Appender works; bench scenario
  `insert_appender` now uses `bench_users` (not `bench_users_heap`)
- [ ] `--scenario insert_appender_heap` retained as the heap-only
  variant for comparison
- [ ] `cargo nextest run -p axiomdb-embedded` passes
- [ ] `cargo nextest run --workspace` passes (no regressions in SQL path)
- [ ] `cargo clippy --workspace -- -D warnings` clean on touched crates
- [ ] `cargo fmt --check` clean
- [ ] Performance budget hits ≥ 150K ops/s on clustered
- [ ] No regression > 5% on v1 heap path
- [ ] docs-site embedded.md updated: drop the "heap tables only"
  caveat; mention triggers as the only remaining limitation
- [ ] docs/perf-sqlite-gap.md "Attack 7 v1.1" subsection appended
  with the new clustered numbers
- [ ] Rustdoc on every newly-exposed `axiomdb_sql` helper

## References

- v1 spec — `specs/fase-perf-sqlite-gap/spec-embedded-appender.md`
- v1 plan — `specs/fase-perf-sqlite-gap/plan-embedded-appender.md`
- Heap INSERT pipeline — `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs:160-395`
- Clustered INSERT — `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs:343`
  (`enqueue_clustered_insert_ctx`) and `insert_clustered_ctx.rs:25`
  (`execute_clustered_insert_ctx`)
- Constraint helpers — `crates/axiomdb-sql/src/executor/insert_helpers.rs:240`
  (`materialize_generated_columns`), `:361` (`enforce_text_constraints`),
  `:421` (`check_row_constraints_with_cols`)
- FK enforcement — `crates/axiomdb-sql/src/fk_enforcement.rs:207`
  (`check_fk_child_insert`, already `pub`)
- AUTO_INCREMENT — `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs:94-123`
  (`next_auto_inc_ctx`) — to be extracted into a public helper
- DuckDB Appender — `research/duckdb/src/include/duckdb/main/appender.hpp`
  (the standard we aim at — DuckDB's appender works on any table type)
- SQLite bind+step — `research/sqlite/src/sqliteInt.h` (the speed
  reference)
