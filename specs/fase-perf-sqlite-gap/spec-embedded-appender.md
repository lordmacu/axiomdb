# Spec: embedded-appender — `Db::appender(table)` fast-path INSERT API

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 7 — embedded fast-path INSERT API that skips the SQL pipeline
(parse + analyze + dispatch + execute_with_ctx scaffolding) and writes
directly to heap + WAL. Analog of DuckDB's Appender and SQLite's
`sqlite3_bind_*` + `sqlite3_step` pattern.

Status: approved

## Context

`docs/perf-sqlite-gap.md` shows that **96.8% of per-row INSERT time is
inside `execute_with_ctx`** (157µs/row on Lima), while the actual per-row
work (`enqueue_clustered_insert_ctx` inner loop) is only **3µs/row**
(prepare_row 1.35µs + batch_push 0.74µs + eval 0.30µs + the rest). That
leaves **~154µs/row of per-statement scaffolding** — parse, analyze,
dispatch, resolve_table, savepoint, column-position lookup, the Stmt
match arm, WAL bookkeeping, etc.

For embedded users (the strategic release target per
`memory/project_embedded_release.md`), most of that scaffolding is dead
weight: the application already has typed Rust values; it doesn't need
SQL parsing or planning. SQLite hits 80K ops/s precisely because its
`sqlite3_bind_*` API skips all of it.

We already have a `PreparedStatement` (Phase 10.8, `lib.rs:379-437`) that
caches the analyzed AST and substitutes Param placeholders, but it still
calls `execute_with_ctx` per row — so it saves the 5µs of parse+analyze
but pays the remaining 152µs of scaffolding. The Appender is the missing
direct path.

## Goal

Expose a Rust API on the embedded `Db` that takes typed `Value`s and a
table name, and writes rows directly to heap + WAL — skipping parse,
analyze, dispatch, and the per-statement scaffolding inside
`execute_with_ctx`.

## Non-goals

- **C FFI surface in v1.** Rust API only; defer FFI bindings to a
  follow-up. The shape of the FFI depends on what real Rust callers want.
- **Clustered tables in v1.** Heap tables only. Clustered tables have
  their own write path (`enqueue_clustered_insert_ctx`) with different
  invariants (batch buffer, leaf-hint, secondary maintenance); add in v2
  once the heap path is proven.
- **`RETURNING`.** The Appender returns row counts, not rows. Callers
  needing inserted IDs should use SQL `INSERT … RETURNING` for now.
- **`ON CONFLICT` / `ON DUPLICATE KEY UPDATE`.** Unique violations
  surface as `DbError::DuplicateKey`; conflict resolution stays in the
  SQL path.
- **Multi-table writes / triggers / wire protocol.** Single table per
  Appender; triggers are deferred (TODO note in code if a table has
  triggers — error out, don't silently bypass).
- **CHECK constraints, FK validation, AUTO_INCREMENT auto-assignment,
  and GENERATED ALWAYS columns** are NOT enforced in v1 (revised
  during implementation — `insert_rows_batch_with_ctx` only does
  encode + heap + WAL, not the per-row constraint helpers in
  `executor/insert_helpers.rs` which are `pub(crate)` to axiomdb-sql).
  Callers MUST provide complete, valid rows. NOT NULL and type
  coercion ARE honored (Step 2 implementation). Honoring the rest is
  a v1.1 follow-up that exposes the SQL helpers or refactors them out
  of the executor module. The Appender errors out on tables that
  declare any of: CHECK, FK, AUTO_INCREMENT, generated columns —
  pointing users to SQL INSERT until v1.1.

## Behavior

### Public API

```rust
// In crates/axiomdb-embedded/src/lib.rs

impl Db {
    /// Open an Appender for high-throughput INSERT into `table_name`.
    ///
    /// The Appender skips the SQL parser/analyzer/dispatcher and writes
    /// typed [`Value`]s directly to the heap. Analogous to DuckDB's
    /// Appender and SQLite's `sqlite3_bind_*`+`sqlite3_step`.
    ///
    /// The Appender holds an active transaction for the duration of its
    /// life. Rows accumulate in an internal batch buffer until
    /// [`Appender::flush`] or [`Appender::finish`] is called.
    ///
    /// # Errors
    /// - `TableNotFound` if `table_name` doesn't exist.
    /// - `Unsupported` if the table is clustered (deferred to v2).
    /// - `Unsupported` if the table has any trigger (deferred).
    /// - I/O errors from txn.begin().
    pub fn appender(&mut self, table_name: &str) -> Result<Appender<'_>, DbError>;
}

/// A fast-path INSERT builder. Created via [`Db::appender`]; consumed
/// by [`Appender::finish`] (commit) or dropped (rollback with warning).
pub struct Appender<'db> { /* opaque */ }

impl<'db> Appender<'db> {
    /// Append one row.
    ///
    /// `values.len()` must equal the table's column count. NULL columns
    /// must be passed explicitly as `Value::Null`. Type coercion uses
    /// the session's current `strict_mode`.
    ///
    /// Errors are returned immediately (no deferred-error model). On
    /// error, the row is NOT added to the batch and subsequent
    /// `append_row` calls remain valid — the caller can retry with a
    /// corrected row.
    ///
    /// # Errors
    /// - `TypeMismatch` if `values.len() != n_columns`.
    /// - `TypeMismatch` if strict coercion fails (mirrors SQL INSERT).
    /// - `NotNullViolation`, `CheckViolation`, `ForeignKey…` as usual.
    pub fn append_row(&mut self, values: &[Value]) -> Result<(), DbError>;

    /// Append a borrowed slice version that avoids the `&[Value]` →
    /// `Vec<Value>` clone for the hot path. The slice is consumed
    /// (drained) into the internal batch.
    pub fn append_row_owned(&mut self, values: Vec<Value>) -> Result<(), DbError>;

    /// Number of rows currently buffered (not yet written to heap).
    pub fn pending(&self) -> usize;

    /// Write buffered rows to heap + WAL, but keep the transaction open
    /// (the Appender can continue appending after flush).
    ///
    /// Useful for very large loads where the caller wants to release
    /// memory periodically without committing.
    ///
    /// # Errors
    /// - I/O errors from heap insert + WAL append.
    /// - Constraint violations from any row in the buffer (atomic batch
    ///   — either all rows write or none).
    pub fn flush(&mut self) -> Result<(), DbError>;

    /// Flush remaining rows, commit the transaction, and consume the
    /// Appender. Returns the total number of rows inserted across all
    /// flushes.
    ///
    /// On error during flush or commit, the transaction is rolled back
    /// and the error returned.
    pub fn finish(self) -> Result<u64, DbError>;
}

// Drop impl: rolls back the transaction and prints a debug-log warning
// if `finish()` wasn't called. Buffered rows are discarded.
impl<'db> Drop for Appender<'db> { /* ... */ }
```

### Semantics

**Lifecycle:**
- `Db::appender` opens an `Appender` that holds a transaction for the
  whole lifetime — like `BEGIN`. Multiple appenders on the same `Db`
  are NOT supported in v1 (would need per-conn-txn handling); attempting
  to open a second returns an error.
- The Appender batches rows in memory until `flush()` writes them or
  `finish()` writes+commits. The batch size is internal (start with
  `1024` rows; tune later).
- `finish()` is the only way to commit. Drop without finish = rollback.

**Type coercion:**
- Honors `ctx.strict_mode` (same as SQL INSERT). Strict mode rejects
  lossy coercion; permissive mode emits warning 1265 and stores the
  coerced value. Warnings stay in the session.

**Transaction:**
- The Appender opens its own transaction via `txn.begin()`. The session's
  `synchronous` setting (Attack 6) is honored — the appender's commit
  pays whatever fsync cost the session was set to.
- If a transaction was already open (`ctx.conn_txn.is_some()`), opening
  an Appender returns `TransactionAlreadyActive`. We don't try to
  participate in user transactions in v1.

**Internal flow:**
1. `Db::appender` looks up `table_def` (cached), validates non-clustered,
   no triggers, opens `ConnectionTxn`, captures resolved column types.
2. `append_row` validates count + coerces values + pushes to internal
   `Vec<Vec<Value>>` buffer. **No** heap I/O until flush.
3. `flush` calls `TableEngine::insert_rows_batch_with_ctx` with the
   buffered rows — this writes heap pages + WAL + secondary indexes +
   bloom updates in one batched pass. Buffer cleared.
4. `finish` calls `flush` + `commit_active_txn` + releases the txn.

### Error cases

| Input | Expected error | Notes |
|-------|----------------|-------|
| `Db::appender("ghost")` | `TableNotFound` | |
| `Db::appender` on clustered table | `Unsupported` | v2 work |
| `Db::appender` on table with triggers | `Unsupported` | defer |
| Second `Db::appender` while one is open | `TransactionAlreadyActive` | |
| `Db::appender` while user has open SQL txn | `TransactionAlreadyActive` | |
| `append_row(&[a, b])` when table has 3 cols | `TypeMismatch` | |
| `append_row` with type that won't coerce (strict mode) | `TypeMismatch` | mirrors SQL INSERT |
| `append_row` violating CHECK | `CheckViolation` | row NOT added to batch |
| `flush` with a unique-violation row | `DuplicateKey` | whole batch rolls back |
| `finish` with I/O error during commit | propagates the I/O error | |
| Drop without finish | nothing returned; rollback + tracing warn | |

## Edge cases

- [ ] Empty buffer at `flush` / `finish` — no-op success.
- [ ] `append_row` with one row, then `finish` — single-row commit works.
- [ ] 100k rows appended in a tight loop — auto-flush at batch size to
  bound memory.
- [ ] `flush` error leaves the Appender in a defined state — buffer
  cleared, transaction still open, subsequent `append_row` works.
- [ ] Schema change between `appender` open and `finish` — appender holds
  the cached `table_def`; if SQL on a different connection ran DDL, the
  appender's writes still complete against the old schema (txn snapshot
  isolation). Caller is responsible for re-opening after DDL.
- [ ] AUTO_INCREMENT column passed as `Value::Null` — same behavior as
  SQL `INSERT VALUES (NULL, …)` — the auto-inc machinery assigns the
  next value. Honored by `insert_rows_batch_with_ctx`.
- [ ] Generated column passed with an explicit value — same behavior as
  SQL INSERT: error if column is `GENERATED ALWAYS` and value is not
  DEFAULT/NULL. Honored by the existing helper.
- [ ] NOT NULL column with `Value::Null` — `NotNullViolation` at append
  time (we eagerly validate per row; don't defer to flush).
- [ ] CHECK constraint failure — same error as SQL.
- [ ] FK violation — same error as SQL.
- [ ] Appender open + concurrent SELECT on same table — works (MVCC
  snapshot for the SELECT, appender's writes invisible until commit).
- [ ] Unicode NFC normalization on TEXT — applied (we go through the same
  encode path).
- [ ] TOAST'd values — applied (`encode_row` handles toasting).
- [ ] `Drop` after partial appends — transaction rolled back, no data
  written.

## On-disk format

**No change.** The Appender reuses `TableEngine::insert_rows_batch_with_ctx`,
which calls `encode_row` + `HeapChain::insert_batch` + `txn.record_insert`
— exactly the same code paths as SQL INSERT. So WAL entries, page
layout, and on-disk format are byte-identical.

## Performance budget

| Metric | Target | Stretch |
|---|---|---|
| Per-row cost (auto-flush at 1024) on `bench_users` schema | ≤ 30µs/row | ≤ 12µs/row (SQLite parity) |
| Throughput on Lima, autocommit equivalent (1 commit / 1024 rows) | ≥ 35K ops/s | ≥ 80K ops/s |
| Throughput vs current SQL `INSERT` autocommit (Lima) | ≥ 5× | ≥ 10× |
| Throughput vs current `PreparedStatement` (Lima) | ≥ 3× | — |

Reference: SQLite 80K ops/s on Lima (`PRAGMA synchronous=NORMAL`).
Current AxiomDB autocommit INSERT: ~4.9K ops/s on Lima (after Attack 6).

The "stretch" target (80K) assumes per-row work stays at the measured
~3µs and the only added cost is the Appender's own dispatch + batching.
Realistic v1 target is ≥ 35K (10× over current SQL path).

## Dependencies

Depends on:
- Existing `TableEngine::insert_rows_batch_with_ctx` (heap path, already
  used by SQL INSERT) — `crates/axiomdb-sql/src/table_ctx.rs:66`
- Existing `txn.begin()` / `commit()` (Attack 6 override honored via
  the session) — `crates/axiomdb-wal/src/txn_begin_commit.rs`
- Existing `resolve_table_cached` for the table lookup
- Existing `coerce_values_with_ctx` for type coercion
- Existing `SessionContext` for strict_mode, warnings, conn_txn

Blocks:
- C FFI Appender (`axiomdb_appender_open/append/finish/free`) — follow-up
- Clustered-table Appender v2 — follow-up
- Trigger support in Appender — follow-up
- Python / Node.js binding updates to expose Appender — follow-up

## Open questions

- [x] Should the Appender flush on a row count or on a byte threshold?
  → **Row count** (1024) in v1. Simpler, and the rows are similar size
  in the embedded use case. Revisit if we see memory issues.
- [x] Should the Appender share a transaction with the user's open SQL
  txn? → **No** in v1. Returns `TransactionAlreadyActive` if a txn is
  open. Avoids subtle semantic surprises around savepoints + appender
  buffer ordering.
- [x] Errors at `flush` — partial commit or all-or-nothing?
  → **All-or-nothing.** `insert_rows_batch_with_ctx` already has this
  semantic via undo on error within the txn.
- [x] What about RETURNING? → Out of scope v1; doc says use SQL INSERT
  RETURNING if needed.
- [ ] **Open**: Drop-without-finish — silent rollback, or panic in
  debug?

  Recommendation: silent rollback + `tracing::warn!` log entry. Panic in
  debug builds is hostile to users who forget to call `finish` in error
  paths.

## Done criteria

- [ ] Public API in `crates/axiomdb-embedded/src/lib.rs` matches the
  signatures above exactly.
- [ ] Unit tests in `crates/axiomdb-embedded/tests/integration_appender.rs`
  cover every "Edge cases" item.
- [ ] One Lima bench scenario `--scenario insert_appender` added to
  `benches/comparison/axiomdb_bench/src/main.rs`; output JSON matches
  the existing scenario format.
- [ ] Bench measurements documented in `docs/perf-sqlite-gap.md`:
  before (SQL path) vs after (Appender) on Lima, with absolute numbers
  and ratio.
- [ ] `cargo nextest run -p axiomdb-embedded` passes.
- [ ] `cargo clippy -p axiomdb-embedded --tests -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] `cargo nextest run --workspace` passes (no regressions in SQL path).
- [ ] Wire smoke unchanged (Appender doesn't touch wire).
- [ ] Performance budget hits the **realistic** v1 target (≥ 5× vs SQL
  path). Documents whether stretch is hit.
- [ ] docs-site: user-guide page added at
  `docs-site/src/user-guide/embedded/appender.md` (or similar) with a
  short example + callout linking to perf gains.
- [ ] Rustdoc on every public item.

## References

- DuckDB Appender — `research/duckdb/src/include/duckdb/main/appender.hpp`
  (BeginRow/Append<T>/EndRow/AppendRow/Flush/Close pattern, auto-flush
  at ~3200 rows)
- SQLite prepared statements — `research/sqlite/src/sqliteInt.h` (bind+
  step lifecycle, statement cache, sqlite3_reset for reuse)
- Existing `PreparedStatement` (Phase 10.8) —
  `crates/axiomdb-embedded/src/lib.rs:379-437` (caches analyzed AST,
  still pays `execute_with_ctx` scaffolding cost — the Appender skips
  that residual cost)
- Existing heap insert path —
  `crates/axiomdb-sql/src/table_ctx.rs:9-62` (`insert_row_with_ctx`)
  and `:66-130` (`insert_rows_batch_with_ctx`, batched encode + WAL +
  HeapChain::insert_batch + zone map tracking)
- INSERT dispatch — `crates/axiomdb-sql/src/executor/exec_dispatch.rs:39`
  → `executor/insert_heap_ctx.rs:1-426` (the SQL-layer scaffolding the
  Appender skips)
- Attack 6 (deferred-fsync) — `specs/fase-perf-sqlite-gap/spec-deferred-fsync.md`
  (the Appender's commit honors `SET synchronous`, same as SQL)
- Roadmap context — `memory/project_embedded_release.md` (this is the
  highest-priority work for the embedded release)
