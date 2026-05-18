# Plan: appender-typed-and-ffi

Phase: perf-sqlite-gap
Task: Attack 8 — Appender typed builder + C FFI
Spec: specs/fase-perf-sqlite-gap/spec-appender-typed-and-ffi.md
Status: in-progress

## Summary

Build the Rust typed builder first (Step 1-2), then the C FFI on top
of it (Step 3-4), then the bench (Step 5), then close (Step 6). The
FFI is the heavy lifting — unsafe ptr handling, opaque struct
casting, error-message routing.

TDD per step: red → green → commit.

## Dependencies

- [x] spec approved
- [x] Attack 7 v1.1 landed

Blocks:
- Python binding (`axiomdb-python`)
- Node.js binding (`axiomdb-nodejs`)
- v2 direct-encode optimization

## Affected files

Modified:
- `crates/axiomdb-embedded/src/appender.rs` — add typed builder
  methods + `in_progress_row` field
- `crates/axiomdb-embedded/src/lib.rs` — add C FFI exports
- `crates/axiomdb-embedded/tests/integration_appender.rs` — typed
  builder tests
- `benches/comparison/axiomdb_bench/src/main.rs` — typed builder
  scenario
- `docs/perf-sqlite-gap.md` — Attack 8 subsection
- `docs-site/src/user-guide/embedded.md` — typed builder + C FFI
  examples

New:
- `crates/axiomdb-embedded/tests/integration_appender_ffi.rs` — C FFI
  tests via `extern "C"` raw signatures

---

## Step 1 — Rust typed builder: in_progress_row field + 7 setters

**Goal:** Add per-column setters that push onto an internal
`in_progress_row: Vec<Value>`. No `end_row` yet — Step 2.

### Test (red → green)

```rust
#[test]
fn typed_builder_setters_accumulate_in_progress_row() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (i INT, s TEXT, b BOOL, r REAL)").unwrap();
    let mut app = db.appender("t").unwrap();
    assert_eq!(app.current_row_len(), 0);
    app.append_int(7).unwrap();
    assert_eq!(app.current_row_len(), 1);
    app.append_text("hi").unwrap();
    app.append_bool(true).unwrap();
    app.append_real(3.14).unwrap();
    assert_eq!(app.current_row_len(), 4);
    // No flush/finish — just confirm the buffer behavior. Dropping
    // the appender rolls back.
}
```

### Implementation

```rust
// In Appender struct:
pub(crate) in_progress_row: Vec<Value>,

// In open():
in_progress_row: Vec::with_capacity(columns.len()),

// New methods:
pub fn current_row_len(&self) -> usize { self.in_progress_row.len() }
pub fn append_int(&mut self, v: i32) -> Result<(), DbError> {
    self.in_progress_row.push(Value::Int(v));
    Ok(())
}
// ... and same shape for bigint/bool/real/text/bytes/null
```

### Commit

`feat(perf-sqlite-gap): Attack 8 step 1 — typed builder setters`

---

## Step 2 — `end_row` commits the in-progress row

**Goal:** `end_row` validates arity, drains `in_progress_row` via
`std::mem::take`, calls `append_row_owned` (which runs the full v1.1
pipeline), and clears the in-progress row on either path.

### Tests

```rust
#[test]
fn end_row_commits_and_clears_in_progress() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (i INT, s TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_int(1).unwrap();
    app.append_text("a").unwrap();
    app.end_row().unwrap();
    assert_eq!(app.current_row_len(), 0, "in-progress cleared");
    assert_eq!(app.pending(), 1, "row committed to buffer");
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT i, s FROM t").unwrap()[0][0],
        Value::Int(1)
    );
}

#[test]
fn end_row_arity_mismatch_rejects_and_clears() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (i INT, s TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_int(1).unwrap();
    let err = app.end_row().unwrap_err();  // only 1 value, need 2
    assert!(matches!(err, DbError::TypeMismatch { .. }));
    assert_eq!(app.current_row_len(), 0, "cleared after rejection");
    // Retry succeeds.
    app.append_int(2).unwrap();
    app.append_text("b".into()).unwrap();
    app.end_row().unwrap();
    app.finish().unwrap();
}

#[test]
fn end_row_check_violation_clears_and_keeps_appender_usable() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_int(1).unwrap();
    app.append_int(-5).unwrap();
    let err = app.end_row().unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }));
    assert_eq!(app.current_row_len(), 0);
    // Retry with a valid row.
    app.append_int(2).unwrap();
    app.append_int(30).unwrap();
    app.end_row().unwrap();
    app.finish().unwrap();
}

#[test]
fn end_row_mixed_with_append_row() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (i INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_int(1).unwrap();
    app.end_row().unwrap();
    app.append_row(&[Value::Int(2)]).unwrap();
    app.append_int(3).unwrap();
    app.end_row().unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT i FROM t ORDER BY i").unwrap();
    assert_eq!(rows.len(), 3);
}
```

### Implementation

```rust
pub fn end_row(&mut self) -> Result<(), DbError> {
    let row = std::mem::take(&mut self.in_progress_row);
    // append_row_owned does arity check + full pipeline. On error,
    // in_progress_row is already cleared (take above).
    self.append_row_owned(row)
}
```

That's it. The `Vec::take` clears `in_progress_row` before validation
runs, so error cases naturally leave the appender in a clean state.

### Commit

`feat(perf-sqlite-gap): Attack 8 step 2 — Appender::end_row`

---

## Step 3 — C FFI exports (`AxiomDbAppender`, `axiomdb_appender_*`)

**Goal:** Mirror the Rust typed API in C exports. Opaque
`AxiomDbAppender` struct wraps a `*mut Db` + the Rust `Appender`'s
state (or stores `&'static mut Db` — see implementation note).

### Implementation challenge: lifetimes

The Rust `Appender<'db>` holds `&'db mut Db`. FFI can't carry
lifetimes. Workaround:

```rust
// Heap-owned FFI wrapper that stores raw pointers internally.
pub struct AxiomDbAppender {
    // SAFETY: caller guarantees the Db pointer outlives the appender.
    db: *mut Db,
    // Same fields as Appender<'_> but owned (no lifetime).
    table_def: TableDef,
    columns: Vec<ColumnDef>,
    indexes: Vec<IndexDef>,
    constraints: Vec<ConstraintDef>,
    foreign_keys: Vec<FkDef>,
    auto_inc_col: Option<usize>,
    conn_txn: Option<ConnectionTxn>,
    buffer: Vec<Vec<Value>>,
    in_progress_row: Vec<Value>,
    rows_inserted: u64,
}
```

Alternative: a single internal `Appender<'static>` constructed via
`Box::leak`. Trickier with `Drop`. Use the explicit wrapper — clearer.

Each FFI function dereferences `*mut AxiomDbAppender` and operates on
fields directly, OR reconstructs an `Appender<'_>` borrow for the
duration of the call (cleaner — reuses Rust impl).

Recommended: write a private helper on `AxiomDbAppender` that returns
an ephemeral `Appender<'_>` borrow:

```rust
impl AxiomDbAppender {
    unsafe fn as_appender(&mut self) -> Appender<'_> {
        // SAFETY: db is valid for the lifetime of self.
        Appender {
            db: &mut *self.db,
            // ... move all the fields by reference ...
        }
    }
}
```

This is delicate. Implementation may discover a cleaner approach
(e.g. just inline the typed-setter logic without rebuilding the
borrow). Verify-as-we-go.

### Tests (red → green)

In `tests/integration_appender_ffi.rs`:

```rust
//! C FFI tests — call the extern "C" functions through their raw
//! signatures from safe Rust to exercise the unsafe boundary.

use axiomdb_embedded::{axiomdb_appender_append_int, /* ... */};

#[test]
fn ffi_appender_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = std::ffi::CString::new(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let table = std::ffi::CString::new("t").unwrap();
    unsafe {
        let db = axiomdb_embedded::axiomdb_open(path.as_ptr());
        assert!(!db.is_null());

        let create = std::ffi::CString::new("CREATE TABLE t (i INT, s TEXT)").unwrap();
        assert_eq!(axiomdb_embedded::axiomdb_execute(db, create.as_ptr()), 0);

        let app = axiomdb_embedded::axiomdb_appender_open(db, table.as_ptr());
        assert!(!app.is_null());

        assert_eq!(axiomdb_embedded::axiomdb_appender_append_int(app, 42), 0);
        let s = std::ffi::CString::new("hello").unwrap();
        assert_eq!(axiomdb_embedded::axiomdb_appender_append_text(app, s.as_ptr()), 0);
        assert_eq!(axiomdb_embedded::axiomdb_appender_end_row(app), 0);

        let n = axiomdb_embedded::axiomdb_appender_finish(app);
        assert_eq!(n, 1);

        axiomdb_embedded::axiomdb_close(db);
    }
}

#[test]
fn ffi_appender_null_table_returns_null() { /* ... */ }
#[test]
fn ffi_appender_text_null_ptr_returns_error() { /* ... */ }
#[test]
fn ffi_appender_finish_consumes_pointer() { /* ... */ }
```

### Commit

`feat(perf-sqlite-gap): Attack 8 step 3 — Appender C FFI exports`

---

## Step 4 — Edge cases + tests for both APIs

**Goal:** Cover all 11 "Edge cases" items from the spec.

- typed builder: AUTO_INC via append_null, GENERATED via append_null,
  too-many-values, retry-after-error
- FFI: NULL pointers, non-UTF-8 text, empty bytes, use-after-finish
  is UB (documented, not tested)

### Commit

`feat(perf-sqlite-gap): Attack 8 step 4 — Appender edge cases`

---

## Step 5 — Bench: typed builder + C FFI scenarios

**Goal:** Add `insert_appender_typed` (Rust) and `insert_appender_c`
(C FFI through raw `unsafe`) to the bench. Compare against
`insert_appender` (v1.1 baseline).

### Numbers to capture

- Rust typed builder on heap → should be within 10% of v1.1 heap (190K)
- C FFI on heap → within 20% of Rust typed
- Rust typed on clustered → matches v1.1 clustered (~82K)

### Commit

`feat(perf-sqlite-gap): Attack 8 step 5 — bench typed builder + FFI`

---

## Step 6 — Closing

- workspace nextest
- clippy on touched crates
- fmt
- docs/perf-sqlite-gap.md: Attack 8 subsection
- docs-site/embedded.md: typed builder + C FFI examples
- memory updates
- spec → implemented, plan → done

### Commit

`feat(perf-sqlite-gap): Attack 8 step 6 — close typed builder + C FFI`

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Lifetime juggling for FFI `Appender` is hairy | High | Step 3 will tell; if `as_appender(&mut self) -> Appender<'_>` doesn't typecheck, inline the typed-setter logic directly in FFI fns |
| C FFI `append_text` on non-UTF-8 cstring panics | Low | `CStr::to_str()` returns Result; convert to InvalidValue |
| Test for FFI requires linking; cargo test handles it but the `unsafe extern "C"` calls need careful pointer mgmt | Medium | Use `extern "C"` only where required; rest can be safe Rust calling our own functions |
| Mixing append_row and typed builders has surprising state interactions | Low | Step 2 explicitly tests this; the design uses separate in_progress_row vs buffer |
| FFI perf cost (call overhead per setter) is larger than expected | Medium | Step 5 will tell; if FFI is >2× slower than Rust typed, we may need a "batch row" FFI variant that takes an array of typed values |

## Rollback plan

If abandoned:

1. `git reset --hard <commit before Step 1>` — pure additions; nothing
   in v1.1 breaks
2. Spec status → blocked

## Estimated effort

Total: **2-3 days** (Step 1: 1h, Step 2: 1h, Step 3: 1d (the FFI
lifetime work), Step 4: 0.5d, Step 5: 0.5d, Step 6: 0.5d)

Critical path: Step 3 (FFI lifetime juggling).
