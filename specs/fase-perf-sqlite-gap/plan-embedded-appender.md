# Plan: embedded-appender

Phase: perf-sqlite-gap
Task: Attack 7 — `Db::appender(table)` fast-path INSERT API
Spec: specs/fase-perf-sqlite-gap/spec-embedded-appender.md
Status: done

## Summary

Build the Appender bottom-up: type-define the struct and its open path
first (with a failing test for `appender_opens_on_heap_table`), then add
`append_row` (buffer + per-row validation), then `flush` (drain to
`insert_rows_batch_with_ctx`), then `finish` + Drop (commit / rollback),
then guards (clustered/triggers/already-open-txn), then the bench
scenario, then docs.

TDD per step: each step writes the failing test first, then the minimal
implementation. Each step's commit must build and tests-pass.

## Dependencies

Must be done first:
- [x] spec-embedded-appender approved

Blocks (until this plan is done):
- C FFI Appender bindings (follow-up Attack)
- Clustered-table Appender v2
- Python / Node.js Appender bindings

## Affected files

New files:
- `crates/axiomdb-embedded/src/appender.rs` — Appender struct + impl
- `crates/axiomdb-embedded/tests/integration_appender.rs` — full test
  suite

Modified files:
- `crates/axiomdb-embedded/src/lib.rs` — `mod appender;` + re-export
  `Appender`; add `Db::appender(table_name)`
- `benches/comparison/axiomdb_bench/src/main.rs` — new
  `insert_appender` scenario
- `docs/perf-sqlite-gap.md` — Attack 7 results section
- `docs-site/src/user-guide/getting-started.md` (or new
  `embedded/appender.md`) — user-facing doc + example
- `memory/project_sqlite_baseline.md` — Attack 7 results entry

---

## Step 1 — Skeleton + `Db::appender` open path

**Goal:** Get the type to compile, the open path to succeed on a heap
table, and produce an Appender that wraps `&mut Db` + `ConnectionTxn` +
cached schema.
**Files:** `appender.rs` (new), `lib.rs`, `integration_appender.rs` (new)
**Approach:** TDD — failing test for `appender_opens_on_heap_table`,
then minimal impl with no `append_row` yet (deferred to Step 2).

### Test to add

```rust
// crates/axiomdb-embedded/tests/integration_appender.rs

use axiomdb_core::error::DbError;
use axiomdb_embedded::Db;
use tempfile::TempDir;

fn open_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("test.db")).unwrap();
    (dir, db)
}

#[test]
fn appender_opens_on_heap_table() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let app = db.appender("t").unwrap();
    assert_eq!(app.pending(), 0);
}

#[test]
fn appender_open_on_missing_table_returns_table_not_found() {
    let (_dir, mut db) = open_db();
    let err = db.appender("ghost").unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }), "got {err:?}");
}

#[test]
fn appender_open_while_user_txn_open_returns_already_active() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("BEGIN").unwrap();
    let err = db.appender("t").unwrap_err();
    assert!(
        matches!(err, DbError::TransactionAlreadyActive | DbError::Other(_)),
        "got {err:?}"
    );
}
```

### Implementation outline

```rust
// crates/axiomdb-embedded/src/appender.rs

use axiomdb_catalog::ResolvedTable;
use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use axiomdb_wal::ConnectionTxn;

use crate::Db;

pub struct Appender<'db> {
    db: &'db mut Db,
    table_def: axiomdb_catalog::schema::TableDef,
    columns: Vec<axiomdb_catalog::schema::ColumnDef>,
    conn_txn: Option<ConnectionTxn>,
    buffer: Vec<Vec<Value>>,
    rows_inserted: u64,
}

const APPENDER_BATCH_FLUSH: usize = 1024;

impl<'db> Appender<'db> {
    pub(crate) fn open(db: &'db mut Db, table_name: &str) -> Result<Self, DbError> {
        if db.session.conn_txn.is_some() {
            return Err(DbError::Other(
                "Appender requires no active transaction".into(),
            ));
        }
        // Resolve via the same path as SQL INSERT — schema cache aware.
        let resolved = axiomdb_sql::resolve_table_cached(
            &db.storage,
            db.txn.snapshot(),
            &mut db.schema_cache,
            table_name,
        )?;
        let table_def = resolved.table.clone();
        let columns = resolved.columns.clone();
        if table_def.is_clustered() {
            return Err(DbError::Unsupported(
                "Appender on clustered tables is deferred to v2 (use SQL INSERT)".into(),
            ));
        }
        if table_def.has_any_trigger() {
            return Err(DbError::Unsupported(
                "Appender on tables with triggers is deferred (use SQL INSERT)".into(),
            ));
        }
        let mut conn_txn = db.txn.begin()?;
        conn_txn.durability_override = Some(db.session.synchronous().to_wal_policy());
        Ok(Self {
            db,
            table_def,
            columns,
            conn_txn: Some(conn_txn),
            buffer: Vec::with_capacity(APPENDER_BATCH_FLUSH),
            rows_inserted: 0,
        })
    }

    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}

// In lib.rs:
impl Db {
    pub fn appender(&mut self, table_name: &str) -> Result<Appender<'_>, DbError> {
        Appender::open(self, table_name)
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
./tools/vm.sh build -p axiomdb-embedded
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 1 — Appender skeleton + open path

Step 1 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 2 — `append_row` (buffer + per-row validation)

**Goal:** Accumulate rows in the internal buffer. Per-row validation
(arity + coercion + NOT NULL) at append time, NOT deferred to flush.
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — fail tests for buffer accumulation + arity + NOT
NULL + coercion, then add `append_row` implementation that calls
`coerce_values_with_ctx` and pushes onto the buffer.

### Tests to add

```rust
#[test]
fn append_row_accumulates_in_buffer() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())]).unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())]).unwrap();
    assert_eq!(app.pending(), 2);
}

#[test]
fn append_row_wrong_arity_returns_type_mismatch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app.append_row(&[Value::Int(1)]).unwrap_err();
    assert!(matches!(err, DbError::TypeMismatch { .. }), "got {err:?}");
    // Appender remains usable.
    app.append_row(&[Value::Int(1), Value::Text("a".into())]).unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn append_row_not_null_violation_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT NOT NULL, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Null, Value::Text("a".into())])
        .unwrap_err();
    assert!(matches!(err, DbError::NotNullViolation { .. }), "got {err:?}");
    assert_eq!(app.pending(), 0);
}

#[test]
fn append_row_owned_consumes_vec() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let row = vec![Value::Int(1), Value::Text("a".into())];
    app.append_row_owned(row).unwrap();
    assert_eq!(app.pending(), 1);
}
```

### Implementation outline

```rust
impl<'db> Appender<'db> {
    pub fn append_row(&mut self, values: &[Value]) -> Result<(), DbError> {
        if values.len() != self.columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", self.columns.len()),
                got: format!("{} values", values.len()),
            });
        }
        // Coerce + NOT NULL check + cheap CHECK in strict mode. We do NOT
        // call into the full SQL pipeline; we use the shared coercion
        // helper that respects ctx.strict_mode.
        let coerced = axiomdb_sql::coerce_values_with_ctx(
            values.to_vec(),
            &self.columns,
            &mut self.db.session,
            self.buffer.len() + 1, // 1-based row_num for warning text
        )?;
        // NOT NULL check (mirrors SQL INSERT path)
        for (col, v) in self.columns.iter().zip(coerced.iter()) {
            if matches!(v, Value::Null) && !col.is_nullable {
                return Err(DbError::NotNullViolation {
                    column: col.name.clone(),
                });
            }
        }
        self.buffer.push(coerced);
        Ok(())
    }

    pub fn append_row_owned(&mut self, values: Vec<Value>) -> Result<(), DbError> {
        // Same logic; reuses append_row by slicing for now. Optimize later.
        self.append_row(&values)
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 2 — Appender::append_row buffer + validation

Step 2 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 3 — `flush` (write buffered rows to heap + WAL)

**Goal:** Drain the buffer through `TableEngine::insert_rows_batch_with_ctx`
which already handles encode + heap insert + WAL + indexes + bloom.
Atomic per flush — if any row fails the whole batch reverts (existing
batch semantic).
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — fail test for `flush_persists_rows`,
`flush_empty_is_noop`, `flush_keeps_txn_open`.

### Tests to add

```rust
#[test]
fn flush_persists_rows_visible_after_finish() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())]).unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())]).unwrap();
    app.flush().unwrap();
    assert_eq!(app.pending(), 0);
    app.finish().unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn flush_empty_buffer_is_noop() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.flush().unwrap(); // no rows buffered
    app.flush().unwrap();
    app.finish().unwrap();
}

#[test]
fn flush_keeps_appender_usable() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1)]).unwrap();
    app.flush().unwrap();
    app.append_row(&[Value::Int(2)]).unwrap();
    app.finish().unwrap();
    assert_eq!(db.query("SELECT id FROM t ORDER BY id").unwrap().len(), 2);
}
```

### Implementation outline

```rust
impl<'db> Appender<'db> {
    pub fn flush(&mut self) -> Result<(), DbError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let conn_txn = self
            .conn_txn
            .as_mut()
            .expect("appender has active txn while alive");
        let batch = std::mem::take(&mut self.buffer);
        let rids = axiomdb_sql::TableEngine::insert_rows_batch_with_ctx(
            &self.db.storage,
            &self.db.txn,
            &self.table_def,
            &self.columns,
            &mut self.db.session,
            conn_txn,
            &batch,
        )?;
        self.rows_inserted += rids.len() as u64;
        // TODO Step 5: index maintenance (insert_into_indexes_with_undo)
        // For Step 3 we ONLY support tables without secondary indexes.
        // Step 5 wires the index update; for now error if any index exists.
        Ok(())
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 3 — Appender::flush via insert_rows_batch

Step 3 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 4 — `finish` + `Drop` rollback

**Goal:** `finish()` flushes + commits + returns total rows; Drop
without finish rolls back the txn and emits a `tracing::warn!`.
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — `finish_commits_and_returns_count`,
`drop_without_finish_rolls_back`.

### Tests to add

```rust
#[test]
fn finish_commits_and_returns_count() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..5 {
        app.append_row(&[Value::Int(i)]).unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 5);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(5)
    );
}

#[test]
fn drop_without_finish_rolls_back() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(1)]).unwrap();
        app.append_row(&[Value::Int(2)]).unwrap();
        // Drop here without calling finish().
    }
    // Table is empty.
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(0));
    // Subsequent appender + finish works (no leaked txn state).
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(7)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT id FROM t").unwrap()[0][0],
        Value::Int(7)
    );
}
```

### Implementation outline

```rust
impl<'db> Appender<'db> {
    pub fn finish(mut self) -> Result<u64, DbError> {
        self.flush()?;
        let conn_txn = self.conn_txn.take().expect("txn alive");
        // mimic commit_active_txn for the heap-only fast path:
        // commit returns Option<TxnId> for pipeline mode; we don't drive
        // the pipeline from the appender — call commit and treat it as
        // immediate.
        self.db.txn.commit(conn_txn)?;
        self.db
            .txn
            .drain_committed_page_batches(&self.db.storage)?;
        let n = self.rows_inserted;
        Ok(n)
    }
}

impl Drop for Appender<'_> {
    fn drop(&mut self) {
        if let Some(conn_txn) = self.conn_txn.take() {
            tracing::warn!(
                "Appender dropped without finish(); rolling back \
                 {} buffered + {} committed-pending rows",
                self.buffer.len(),
                self.rows_inserted,
            );
            // Best-effort rollback. If this errors, log and move on —
            // a panicking Drop is worse than a logged failure.
            if let Err(e) = self.db.txn.rollback(conn_txn) {
                tracing::error!("Appender Drop rollback failed: {e}");
            }
        }
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 4 — Appender::finish + Drop rollback

Step 4 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 5 — Secondary index maintenance

**Goal:** Wire index updates so the Appender works on tables WITH
secondary indexes (currently `flush` ignores them). Reuses
`insert_into_indexes_with_undo` per row.
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — test `appender_maintains_secondary_index` — write
two rows via Appender on a table with a UNIQUE INDEX, then query through
the index.

### Test to add

```rust
#[test]
fn appender_maintains_secondary_index() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE INDEX idx_v ON t (v)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("alpha".into())]).unwrap();
    app.append_row(&[Value::Int(2), Value::Text("beta".into())]).unwrap();
    app.finish().unwrap();
    // Index lookup must find the row.
    let rows = db.query("SELECT id FROM t WHERE v = 'beta'").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

#[test]
fn appender_unique_index_violation_rolls_back_batch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE UNIQUE INDEX idx_v ON t (v)").unwrap();
    db.run("INSERT INTO t VALUES (1, 'dup')").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(2), Value::Text("ok".into())]).unwrap();
    app.append_row(&[Value::Int(3), Value::Text("dup".into())]).unwrap();
    let err = app.finish().unwrap_err();
    assert!(matches!(err, DbError::DuplicateKey { .. }), "got {err:?}");
    // Pre-existing row 1 still there; new rows rolled back.
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1));
}
```

### Implementation outline

In `flush()`, after the heap insert returns RecordIds:

```rust
let indexes = axiomdb_sql::lookup_table_indexes(&self.db.storage, self.table_def.id)?;
if !indexes.is_empty() {
    for (rid, values) in rids.iter().zip(batch.iter()) {
        axiomdb_sql::insert_into_indexes_with_undo(
            &self.db.storage,
            &self.db.txn,
            conn_txn,
            self.table_def.id,
            &indexes,
            *rid,
            values,
            /*ignore_dup=*/ false,
        )?;
    }
}
```

(Exact function name / signature may differ — wire to whatever the
existing INSERT path uses. The same helper is called from
`executor/insert_heap_ctx.rs:354`.)

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 5 — Appender secondary index maintenance

Step 5 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 6 — Auto-flush at batch size + scale test

**Goal:** When the buffer reaches `APPENDER_BATCH_FLUSH` (1024 rows),
auto-flush so memory stays bounded even for million-row loads.
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — `auto_flush_keeps_buffer_bounded` (append 5000
rows, assert `pending()` stays ≤ 1024 throughout). A second test loads
100k rows and verifies count.

### Test to add

```rust
#[test]
fn auto_flush_keeps_buffer_bounded() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let mut max_pending = 0usize;
    for i in 0..5000 {
        app.append_row(&[Value::Int(i)]).unwrap();
        max_pending = max_pending.max(app.pending());
    }
    assert!(max_pending <= 1024, "max_pending = {max_pending}");
    app.finish().unwrap();
}

#[test]
fn appender_loads_100k_rows() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..100_000i64 {
        app.append_row(&[Value::Int(i as i32), Value::Int((i * 2) as i32)]).unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 100_000);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(100_000)
    );
}
```

### Implementation outline

In `append_row`, after pushing to buffer:

```rust
if self.buffer.len() >= APPENDER_BATCH_FLUSH {
    self.flush()?;
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 6 — auto-flush at 1024 rows

Step 6 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 7 — Bench scenario `insert_appender`

**Goal:** Add a bench scenario that uses the new Appender, so the
speedup is reproducible from `axiomdb_bench --scenario insert_appender`.
**Files:** `benches/comparison/axiomdb_bench/src/main.rs`

### Implementation outline

```rust
"insert_appender" => measure_timed(|| {
    reset(&mut db);
    let t0 = Instant::now();
    let mut app = db.appender("bench_users").unwrap();
    for i in 1..=ac_n {
        let active = i % 2 == 0;
        app.append_row(&[
            Value::Int(i as i32),
            Value::Text(format!("user_{i:06}")),
            Value::Int((18 + (i % 62)) as i32),
            Value::Bool(active),
            Value::Real(100.0 + (i % 1000) as f64 * 0.1),
            Value::Text(format!("u{i}@b.local")),
        ]).unwrap();
    }
    app.finish().unwrap();
    t0.elapsed()
}),
```

Also add to the `run_compare` matrix so it shows in the table.

### Verification

```bash
# Build
./tools/vm.sh build -p axiomdb-bench-comparison --release

# Run
limactl shell axiomdb -- bash -c '$HOME/axiomdb-target/release/axiomdb_bench \
  --scenario insert_appender --rows 5000'
```

Numbers recorded for Step 9.

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 7 — bench scenario insert_appender

Step 7 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 8 — Negative-case tests (clustered, triggers, edge cases)

**Goal:** Cover the rest of the spec's "Edge cases" — clustered table
rejection, trigger rejection, CHECK constraint, FK violation,
generated column conflict, AUTO_INCREMENT auto-assignment, NFC
normalization.
**Files:** `integration_appender.rs`

### Tests to add (one per edge case from spec)

```rust
#[test] fn appender_on_clustered_table_returns_unsupported() { ... }
#[test] fn appender_on_table_with_trigger_returns_unsupported() { ... }
#[test] fn appender_check_violation() { ... }
#[test] fn appender_fk_violation() { ... }
#[test] fn appender_generated_always_rejects_explicit_value() { ... }
#[test] fn appender_auto_increment_assigns_id_on_null() { ... }
#[test] fn appender_nfc_normalizes_text() { ... }
#[test] fn appender_empty_flush_then_finish_is_ok() { ... }
#[test] fn appender_concurrent_select_sees_pre_finish_state() { ... }
#[test] fn appender_honors_set_synchronous_normal() { ... }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 step 8 — Appender edge-case test suite

Step 8 of specs/fase-perf-sqlite-gap/plan-embedded-appender.md
```

---

## Step 9 — Closing: measure, docs, memory, final integration

**Goal:** Run the bench, record the numbers, update docs and memory,
verify against the spec's done criteria.

### Verification against spec

Walk through every item in the spec's "Done criteria":

- [ ] Public API signatures match the spec exactly (review `appender.rs`)
- [ ] Every "Edge cases" item has a test (review test file)
- [ ] `--scenario insert_appender` works and produces JSON output
- [ ] `cargo nextest run -p axiomdb-embedded` passes
- [ ] `cargo clippy -p axiomdb-embedded --tests -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo nextest run --workspace` passes (no regressions)
- [ ] Wire smoke unchanged (Appender doesn't touch wire — no new
      assertions needed)
- [ ] Performance budget: ≥5× over current SQL path on Lima
- [ ] docs-site user-guide page added
- [ ] Rustdoc on every public item

### Files to update

- `docs/perf-sqlite-gap.md` — append "Attack 7 — Embedded Appender"
  section with before/after numbers
- `docs-site/src/user-guide/getting-started.md` — short Appender
  example near the existing Db::run snippet
- `docs-site/src/internals/architecture.md` — note the fast path
- `memory/project_sqlite_baseline.md` — Attack 7 results entry
- `memory/MEMORY.md` — update the baseline entry hook line

### Final commit

```
feat(perf-sqlite-gap): close Attack 7 — embedded Appender shipped

Implements specs/fase-perf-sqlite-gap/spec-embedded-appender.md
Plan: specs/fase-perf-sqlite-gap/plan-embedded-appender.md
Tests: [N new integration tests]
Bench: insert_appender [Y]K ops/s vs SQL INSERT [Z]K ops/s ([R]× faster)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `coerce_values_with_ctx` is not a public function — it's `pub(crate)` in axiomdb-sql | Medium | Step 2 verifies; if not exposed, either add a `pub` re-export or duplicate the small logic into appender.rs |
| `insert_rows_batch_with_ctx` ALSO does index updates internally — making Step 5 a no-op (or even a double-write) | Medium | Read the helper at Step 3 implementation; if it does indexes, Step 5 becomes "test, not implement"; if not, wire it up |
| `commit_active_txn`'s wrapper logic does more than just `txn.commit` (deferred pipeline, locking, page-batch drain) | High | Mirror the relevant subset in `finish()`; reference `exec_with_ctx.rs:43-51` for the canonical commit sequence |
| Performance gain is smaller than projected because the heap-insert path itself has overhead we haven't measured | Medium | Step 9's bench tells us; if <5× we re-evaluate (probably attack auto_inc + generated_cols + constraint hot path next) |
| `resolve_table_cached` is not pub-exported | Low | Re-export from axiomdb-sql lib; trivial fix |
| Drop ordering: if `Db` is dropped while Appender alive — Rust borrow checker prevents this | None | Borrow checker enforces |
| `TableDef::is_clustered`, `has_any_trigger` may not be the actual method names | Low | Verify in Step 1 when reading TableDef; rename references in this plan as needed |

## Rollback plan

If the plan is abandoned mid-way:

1. `git reset --hard <commit before Step 1>` to drop all appender work
2. Leave `appender.rs` orphaned under `crates/axiomdb-embedded/src/` if
   we want to resume — it's behind `mod appender;` which we can remove
   from `lib.rs` to disable
3. Update spec status back to `approved` with a note describing what
   blocked

## Estimated effort

Total: **2 days** (Step-level: 1: 2h, 2: 2h, 3: 3h, 4: 2h, 5: 3h, 6:
1h, 7: 1h, 8: 3h, 9: 2h)

Critical-path risk: Step 3 (the heap-insert wire-up) and Step 5
(secondary indexes) — those depend on functions whose exact signature
and visibility I haven't verified yet. Verify-as-we-go.
