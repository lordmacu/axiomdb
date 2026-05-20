# Plan: appender-v11-complete

Phase: perf-sqlite-gap
Task: Attack 7 v1.1 — production-ready Appender (clustered + constraints)
Spec: specs/fase-perf-sqlite-gap/spec-appender-v11-complete.md
Status: done

## Summary

Build from the inside out: expose helpers in axiomdb-sql first
(Step 1), then add validation phases to the Appender's append_row
pipeline one at a time (Steps 2-5), then add clustered-table support
(Step 6), then update the bench to use the canonical clustered table
(Step 7), then close (Step 8).

TDD per step: each step inverts the v1 negative test into a positive
test, asserts the new behavior, ships the minimal implementation.

## Dependencies

Must be done first:
- [x] spec-appender-v11-complete approved
- [x] Attack 7 v1 fully landed

Blocks (until done):
- Attack 8 (typed builder API)
- C FFI for Appender
- Docker bench validation of clustered numbers

## Affected files

Modified:
- `crates/axiomdb-sql/src/executor/insert_helpers.rs` — bump
  visibility on 3 helpers
- `crates/axiomdb-sql/src/lib.rs` — re-export the 3 helpers + the
  new TableEngine methods
- `crates/axiomdb-sql/src/table_ctx.rs` — add `next_auto_increment_value`
  and `insert_clustered_rows_batch_with_ctx`
- `crates/axiomdb-embedded/src/appender.rs` — lift rejections,
  expand `append_row` pipeline, dispatch `flush` to heap or clustered
- `crates/axiomdb-embedded/tests/integration_appender.rs` — invert
  v1 negatives to positives + edge-case suite
- `benches/comparison/axiomdb_bench/src/main.rs` — drop heap-only
  workaround for `insert_appender` (use `bench_users` clustered);
  add `insert_appender_heap` for the v1 comparison number

New:
- `docs/perf-sqlite-gap.md` — "Attack 7 v1.1" subsection appended
- `docs-site/src/user-guide/embedded.md` — Appender section updated

---

## Step 1 — Expose helpers in axiomdb-sql

**Goal:** Bump `pub(crate)` → `pub` on the three constraint helpers
and add the two new `TableEngine` methods. Nothing wired into the
Appender yet — just make the building blocks available.
**Files:** `insert_helpers.rs`, `table_ctx.rs`, `lib.rs`
**Approach:** No TDD here — this is plumbing. A compile-pass on
axiomdb-sql is sufficient; integration tests come in Step 2+ when the
Appender calls them.

### Implementation outline

```rust
// insert_helpers.rs — change:
pub(crate) fn materialize_generated_columns(...)
//   →
pub fn materialize_generated_columns(...)
// (same for enforce_text_constraints, check_row_constraints_with_cols)

// lib.rs — add to re-exports:
pub use executor::insert_helpers::{
    check_row_constraints_with_cols, enforce_text_constraints,
    materialize_generated_columns,
};

// table_ctx.rs — new fn:
impl TableEngine {
    pub fn next_auto_increment_value(...) -> Result<i64, DbError> {
        // Extract from insert_heap_ctx.rs:94-123, identical logic.
    }
}

// table_ctx.rs — new fn (clustered batch):
impl TableEngine {
    pub fn insert_clustered_rows_batch_with_ctx(...)
        -> Result<Vec<RecordId>, DbError>
    {
        // Mirror insert_rows_batch_with_ctx structure:
        //   for each row: encode → call clustered_table::insert_row
        //   then record_page_writes via txn
        // Implementation reads the existing
        // execute_clustered_insert_ctx body to figure out the right
        // sub-helpers (clustered_table::insert_clustered_row or
        // similar). May discover we need to expose more from
        // axiomdb-storage/clustered_tree.
    }
}
```

### Verification

```bash
./tools/vm.sh build -p axiomdb-sql
./tools/vm.sh test -p axiomdb-sql --tests
# nothing new is exercised yet — sanity that the lib still builds.
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 1 — expose insert helpers in axiomdb-sql

Step 1 of specs/fase-perf-sqlite-gap/plan-appender-v11-complete.md
```

---

## Step 2 — Appender supports AUTO_INCREMENT

**Goal:** Lift the AUTO_INC rejection at open; assign next value
inside `append_row` when the column is AUTO_INC and the user passed
`Value::Null`.
**Files:** `appender.rs`, `integration_appender.rs`
**Approach:** TDD — invert `appender_on_table_with_auto_increment_returns_unsupported`
to a positive test, add 3 more.

### Tests to add (replace v1 negative)

```rust
#[test]
fn appender_assigns_auto_increment_on_null() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT AUTO_INCREMENT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Null, Value::Text("a".into())]).unwrap();
    app.append_row(&[Value::Null, Value::Text("b".into())]).unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
}

#[test]
fn appender_respects_explicit_auto_increment_value() {
    // SQL semantics: explicit value wins, AUTO_INC cache adjusts.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT AUTO_INCREMENT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(100), Value::Text("a".into())]).unwrap();
    app.append_row(&[Value::Null, Value::Text("b".into())]).unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rows[0][0], Value::Int(100));
    assert_eq!(rows[1][0], Value::Int(101)); // next after the explicit max
}
```

### Implementation outline

In `appender.rs`:
- Drop the `auto_increment` rejection in `open()`
- Cache `auto_inc_col_idx: Option<usize>` on the Appender struct
- In `append_row_owned`, before NOT NULL check:
  ```rust
  if let Some(idx) = self.auto_inc_col_idx {
      if matches!(values[idx], Value::Null) {
          let next = TableEngine::next_auto_increment_value(
              &self.db.storage, &self.db.txn,
              self.conn_txn.as_ref().unwrap(),
              &self.table_def, &self.columns, idx,
          )?;
          values[idx] = Value::Int(next as i32); // or BigInt depending on col_type
      }
  }
  ```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 2 — Appender AUTO_INCREMENT support

Step 2 of specs/fase-perf-sqlite-gap/plan-appender-v11-complete.md
```

---

## Step 3 — Appender supports GENERATED columns

**Goal:** Lift the generated rejection; materialize stored generated
columns in `append_row` via `materialize_generated_columns`.
**Approach:** TDD — invert
`appender_on_table_with_generated_column_returns_unsupported` to a
positive test.

### Tests

```rust
#[test]
fn appender_materializes_stored_generated_column_on_null() {
    let (_dir, mut db) = open_db();
    db.run(
        "CREATE TABLE t (id INT, v INT, doubled INT GENERATED ALWAYS AS (v * 2) STORED)",
    ).unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Int(5), Value::Null]).unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT v, doubled FROM t").unwrap();
    assert_eq!(rows[0][1], Value::Int(10));
}

#[test]
fn appender_rejects_explicit_generated_always_value() {
    let (_dir, mut db) = open_db();
    db.run(
        "CREATE TABLE t (id INT, v INT, doubled INT GENERATED ALWAYS AS (v * 2) STORED)",
    ).unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Int(1), Value::Int(5), Value::Int(999)])
        .unwrap_err();
    // The helper rejects explicit values for GENERATED ALWAYS;
    // the exact error type depends on materialize_generated_columns.
    assert!(matches!(err, DbError::Other(_) | DbError::TypeMismatch { .. }));
}
```

### Implementation

In `append_row_owned`, after AUTO_INC, call:
```rust
axiomdb_sql::materialize_generated_columns(&self.columns, &mut values)?;
```

Drop the `generated_expr.is_some()` rejection at open.

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 3 — Appender GENERATED column support
```

---

## Step 4 — Appender supports CHECK + text constraints

**Goal:** Lift CHECK rejection; run `enforce_text_constraints` +
`check_row_constraints_with_cols` per row.
**Approach:** TDD — invert
`appender_on_table_with_check_returns_unsupported` to a positive
test; add violation tests.

### Tests

```rust
#[test]
fn appender_check_constraint_passes() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Int(25)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(1)
    );
}

#[test]
fn appender_check_constraint_violation_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Int(1), Value::Int(-5)])
        .unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "got {err:?}");
    // Appender remains usable.
    app.append_row(&[Value::Int(2), Value::Int(30)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(1)
    );
}

#[test]
fn appender_char_padding_applied() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, code CHAR(5))").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("ab".into())]).unwrap();
    app.finish().unwrap();
    // CHAR(5) right-pads with spaces.
    let rows = db.query("SELECT code FROM t").unwrap();
    let stored = match &rows[0][0] {
        Value::Text(s) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(stored.len(), 5);
}
```

### Implementation

In `append_row_owned`, after generated cols (and before coerce/NOT NULL):
```rust
axiomdb_sql::enforce_text_constraints(&self.columns, &mut values)?;
// later, after coerce + NOT NULL:
axiomdb_sql::check_row_constraints_with_cols(
    &self.constraints, &values,
    &self.table_def.table_name, &self.columns,
)?;
```

The Appender struct gets a `constraints: Vec<ConstraintDef>` field
captured at open (currently `resolved.constraints` is read only to
reject — change to capture).

Drop the CHECK rejection at open.

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 4 — Appender CHECK + text constraints
```

---

## Step 5 — Appender supports FOREIGN KEY constraints

**Goal:** Lift FK rejection; run `check_fk_child_insert` per row for
immediate FKs; defer the others via `ctx.deferred_fk_constraint_ids`.
**Approach:** TDD — positive test + violation.

### Tests

```rust
#[test]
fn appender_fk_to_existing_parent_succeeds() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.run("INSERT INTO parent VALUES (1), (2)").unwrap();
    db.run("CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id))")
        .unwrap();
    let mut app = db.appender("child").unwrap();
    app.append_row(&[Value::Int(10), Value::Int(1)]).unwrap();
    app.append_row(&[Value::Int(11), Value::Int(2)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM child").unwrap()[0][0],
        Value::BigInt(2)
    );
}

#[test]
fn appender_fk_to_missing_parent_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.run("INSERT INTO parent VALUES (1)").unwrap();
    db.run("CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id))")
        .unwrap();
    let mut app = db.appender("child").unwrap();
    let err = app
        .append_row(&[Value::Int(10), Value::Int(999)])
        .unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "got {err:?}"
    );
}
```

### Implementation

In `append_row_owned`, after CHECK:
```rust
if !self.foreign_keys.is_empty() {
    // Split immediate vs deferred per the SQL path.
    let (immediate, deferred) =
        split_child_insert_foreign_keys(&self.foreign_keys);
    if !immediate.is_empty() {
        axiomdb_sql::fk_enforcement::check_fk_child_insert(
            &values, &immediate,
            &self.db.storage, &self.db.txn,
            self.conn_txn.as_ref().unwrap(),
            &self.db.bloom,
        )?;
    }
    // queue deferred — same mechanism as SQL INSERT
    for fk in deferred {
        self.db.session
            .deferred_fk_constraint_ids
            .push(fk.constraint_id);
    }
}
```

If `split_child_insert_foreign_keys` is `pub(crate)`, bump it to
`pub` in Step 1 (revise Step 1).

Drop the FK rejection at open.

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 5 — Appender FK support
```

---

## Step 6 — Appender supports clustered tables

**Goal:** Lift clustered rejection; `flush` dispatches based on
`table_def.is_clustered()`.
**Approach:** TDD — positive test for clustered insert; large-batch
test; PK uniqueness test.

### Tests

```rust
#[test]
fn appender_works_on_clustered_table() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 1..=10 {
        app.append_row(&[Value::Int(i), Value::Text(format!("row{i}"))])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 10);
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0], Value::Int((i + 1) as i32));
    }
}

#[test]
fn appender_clustered_pk_duplicate_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.run("INSERT INTO t VALUES (1, 'a')").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())]).unwrap();
    app.append_row(&[Value::Int(1), Value::Text("dup".into())]).unwrap();
    let err = app.finish().unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation { .. } | DbError::DuplicateKey { .. }));
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1));
}
```

### Implementation

In `appender.rs::flush()`:
```rust
let rids = if self.table_def.is_clustered() {
    TableEngine::insert_clustered_rows_batch_with_ctx(
        &self.db.storage, &self.db.txn,
        &self.table_def, &self.columns,
        &mut self.db.session, conn_txn, &batch,
    )?
} else {
    TableEngine::insert_rows_batch_with_ctx(...)?
};
```

Drop the `is_clustered()` rejection at open. The bulk of Step 1 (the
new `insert_clustered_rows_batch_with_ctx` helper) is wired here.

### Verification

This step is the riskiest — clustered I/O semantics, deferred FK
resolution at commit, AUTO_INC interaction with PK columns. May
discover at impl time that we need additional small helpers in
axiomdb-storage. Verify-as-we-go; revise Step 1 if needed.

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 6 — Appender clustered table support
```

---

## Step 7 — Update bench

**Goal:** `insert_appender` now uses the canonical `bench_users`
(clustered, PK). Keep `insert_appender_heap` as the heap-only
comparison.
**Files:** `benches/comparison/axiomdb_bench/src/main.rs`

### Implementation outline

- Rename current `insert_appender` scenario body → `insert_appender_heap`
- New `insert_appender` body: uses `bench_users` (the existing
  reset() table) instead of `bench_users_heap`
- Register both scenarios in `run_compare` matrix
- Same fix in `run_scenario_timed`

### Verification

```bash
./tools/vm.sh build -p axiomdb-bench-comparison --release
limactl shell axiomdb -- /home/cristian.guest/axiomdb-target/release/axiomdb_bench \
    --scenario insert_appender --rows 5000
# Expect: ≥150K ops/s (per spec performance budget)
```

### Commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 7 — bench uses clustered bench_users
```

---

## Step 8 — Closing

**Goal:** Workspace tests + clippy + fmt + docs + memory + spec/plan
status.

### Verification against spec

Walk every "Done criteria" item:

- [ ] Public API byte-identical to v1
- [ ] All 5 v1 rejections inverted to positives
- [ ] Edge cases all have a test
- [ ] `bench_users` now flows through Appender
- [ ] `--scenario insert_appender_heap` still works (v1 comparison)
- [ ] `cargo nextest run --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean on touched
- [ ] `cargo fmt --check` clean
- [ ] Clustered throughput ≥ 150K ops/s
- [ ] Heap throughput ≤ 5% regression from v1 (204K)
- [ ] docs-site embedded.md — heap-only caveat dropped
- [ ] docs/perf-sqlite-gap.md — "Attack 7 v1.1" subsection
- [ ] Rustdoc on all newly-exposed axiomdb_sql helpers

### Files to update

- `docs/perf-sqlite-gap.md` — append v1.1 results
- `docs-site/src/user-guide/embedded.md` — drop heap-only caveat,
  add triggers-only-remaining note
- `memory/project_sqlite_baseline.md` — v1.1 results entry
- `memory/MEMORY.md` — refresh baseline hook line

### Final commit

```
feat(perf-sqlite-gap): Attack 7 v1.1 step 8 — close production-ready Appender

Implements specs/fase-perf-sqlite-gap/spec-appender-v11-complete.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `insert_clustered_rows_batch_with_ctx` requires more helpers from axiomdb-storage than expected | High | Step 1 will tell; if so, expose them in same step and adjust scope |
| AUTO_INC value type doesn't match column type (Int vs BigInt) | Medium | Check `col.col_type` and emit matching Value variant in Step 2 |
| FK validation hits a snapshot-visibility bug where appender's own writes within the same flush aren't visible to the FK check for a later row in the same batch | Medium | Test explicitly in Step 5; if it breaks, document v1.1 limitation: FK to parent inserted in the same Appender batch is not supported |
| Clustered B-Tree throughput is much lower than heap (e.g. 50K vs 200K) because of split overhead | Low | Spec target of 150K leaves headroom; if missed, document and continue |
| `split_child_insert_foreign_keys` is private | Low | Bump in Step 1 |
| Generated-column materialization order vs AUTO_INC: order matters (AUTO_INC first so generated expr can reference the new id) | Low | The SQL path order (AUTO_INC line 195 → generated line 219) is the model; mirror it exactly |
| Deferred FK at finish() time fails after Appender thinks the batch committed | Medium | The commit machinery validates deferred FK before COMMIT; if any fail, the txn rolls back. The Appender's finish() will propagate the error |

## Rollback plan

If abandoned mid-way:

1. `git reset --hard <commit before Step 1>` to drop all v1.1 work
2. The v1 API is unchanged so no caller breaks
3. Update spec status back to `approved` with a "blocked" note

## Estimated effort

Total: **3-5 days** (Step 1: 1d, Step 2: 0.5d, Step 3: 0.5d, Step 4: 1d,
Step 5: 1d, Step 6: 1.5d, Step 7: 0.5h, Step 8: 0.5d)

Critical path: Step 1 (clustered helper) and Step 6 (clustered
dispatch + FK-in-same-batch interaction).
