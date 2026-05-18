# Spec: appender-typed-and-ffi — typed builder API + C FFI for Appender

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 8 — per-column typed builder API on `Appender` plus C FFI
exports so Python / Node.js / Swift / C++ embedded callers can drive
the Appender without going through Rust
Status: approved

## Context

After Attack 7 v1.1, the Appender works on every table the SQL `INSERT`
path works on (except triggers) and hits **82-128K ops/s on clustered
+ 191K on heap** in Lima. The remaining limitations are:

1. **Rust API is `Vec<Value>`-based** — callers either build a `Vec`
   per row or pay an internal clone via `append_row(&[Value])`. The
   `Vec` and the per-`Value` enum dispatch are real per-row overhead.
2. **Rust-only surface** — no Python / Node.js / Swift / C++ caller
   can use the Appender today. Embedded targets (per
   `memory/project_embedded_release.md`) include Python and Node.js
   bindings; without C FFI the fast path is invisible to them.

Both gaps share the same root cause: the API shape doesn't match how
typed callers want to build rows. DuckDB solved this 1:1 with its
Appender's `BeginRow / Append<T> / EndRow` pattern
(`research/duckdb/src/include/duckdb/main/appender.hpp`). SQLite did
the same in its `sqlite3_bind_<type>` family. We adopt the same shape.

## Goal

Expose a typed, per-column builder API on `Appender` (Rust) and
corresponding C FFI exports so:

- **Rust callers** can write `app.append_int(1)?; app.append_text("a")?;
  app.end_row()?;` and skip the `Vec<Value>` allocation on the hot
  path.
- **C/Python/Node.js callers** can write the same pattern via
  `axiomdb_appender_append_int(app, 1); axiomdb_appender_append_text(app, "a");
  axiomdb_appender_end_row(app);`.

## Non-goals

- **Zero-allocation encoding**. v1 of the typed builder still
  collects values into a `Vec<Value>` per row, then delegates to the
  existing `append_row_owned` pipeline. The API shape is what
  matters; eliminating the intermediate `Vec` is a follow-up that
  needs row-codec refactoring.
- **Python / Node.js bindings themselves**. We ship the C FFI; the
  binding code (PyO3 / napi-rs) is a follow-up.
- **Streaming text / blob append**. SQLite has
  `sqlite3_bind_text` with a length and a destructor; we accept
  null-terminated C strings only in v1. Long-data streaming
  (`SQLITE_BIND_VARNUM`-style) is out of scope.
- **Type checking at append time**. If the caller calls `append_text`
  for an `INT` column, the error surfaces at `end_row()` (coercion
  fails) or `flush()`. Same model as the existing Rust API.
- **Reusable appender across tables**. One Appender per table, same as
  v1.
- **Concurrent appenders**. Same as v1 — at most one Appender per `Db`.

## Behavior

### Public Rust API — additions

```rust
// crates/axiomdb-embedded/src/appender.rs

impl<'db> Appender<'db> {
    // EXISTING from v1/v1.1 — UNCHANGED:
    pub fn pending(&self) -> usize;
    pub fn append_row(&mut self, values: &[Value]) -> Result<(), DbError>;
    pub fn append_row_owned(&mut self, values: Vec<Value>) -> Result<(), DbError>;
    pub fn flush(&mut self) -> Result<(), DbError>;
    pub fn finish(self) -> Result<u64, DbError>;

    // NEW (Attack 8) — per-column typed builders:

    /// Append an `INT` / `INTEGER` value to the current row.
    /// Maps to `Value::Int(v)`. Use [`append_bigint`] for `BIGINT`.
    pub fn append_int(&mut self, v: i32) -> Result<(), DbError>;

    /// Append a `BIGINT` value to the current row.
    pub fn append_bigint(&mut self, v: i64) -> Result<(), DbError>;

    /// Append a `BOOL` value to the current row.
    pub fn append_bool(&mut self, v: bool) -> Result<(), DbError>;

    /// Append a `REAL` / `DOUBLE` value to the current row.
    pub fn append_real(&mut self, v: f64) -> Result<(), DbError>;

    /// Append a `TEXT` value to the current row.
    /// The string is copied (we own a `String` internally).
    pub fn append_text(&mut self, v: &str) -> Result<(), DbError>;

    /// Append a `BYTES` value to the current row.
    pub fn append_bytes(&mut self, v: &[u8]) -> Result<(), DbError>;

    /// Append a NULL value to the current row.
    pub fn append_null(&mut self) -> Result<(), DbError>;

    /// Commit the in-progress row to the appender buffer. The number of
    /// values appended since the previous `end_row()` must equal the
    /// table's column count, else returns `TypeMismatch`.
    ///
    /// After `end_row()` succeeds, the row goes through the full
    /// validation pipeline (AUTO_INC, GENERATED, text constraints,
    /// coercion, NOT NULL, CHECK, FK). If any step fails the row is
    /// rejected and the in-progress row buffer is cleared (caller can
    /// start a new row immediately).
    pub fn end_row(&mut self) -> Result<(), DbError>;

    /// Number of values appended in the in-progress row (not yet
    /// committed via `end_row`).
    pub fn current_row_len(&self) -> usize;
}
```

Mixing `append_row(&[...])` / `append_row_owned(vec![])` with the
typed builders is allowed; the typed builders' in-progress row is
independent of the `append_row` path. Calling `end_row()` when
`current_row_len() == 0` is an error (TypeMismatch).

### Public C FFI — additions

```c
// crates/axiomdb-embedded/include/axiomdb.h (or in lib.rs Rust)

typedef struct AxiomDbAppender AxiomDbAppender;

// Open an appender for `table_name`. Returns NULL on error; use
// axiomdb_last_error(db) to read the message.
AxiomDbAppender* axiomdb_appender_open(AxiomDb* db, const char* table_name);

// Per-column appenders. Each returns 0 on success, -1 on error.
// On error the in-progress row is cleared and axiomdb_last_error(db)
// holds the message.
int axiomdb_appender_append_int(AxiomDbAppender* app, int32_t v);
int axiomdb_appender_append_bigint(AxiomDbAppender* app, int64_t v);
int axiomdb_appender_append_bool(AxiomDbAppender* app, int32_t v);  // 0 = false, !=0 = true
int axiomdb_appender_append_real(AxiomDbAppender* app, double v);
int axiomdb_appender_append_text(AxiomDbAppender* app, const char* v);  // UTF-8 NUL-term
int axiomdb_appender_append_bytes(AxiomDbAppender* app, const uint8_t* data, size_t len);
int axiomdb_appender_append_null(AxiomDbAppender* app);

// Commit the in-progress row. Returns 0 on success, -1 on error.
int axiomdb_appender_end_row(AxiomDbAppender* app);

// Flush buffered rows to heap+WAL. Keeps the txn open.
int axiomdb_appender_flush(AxiomDbAppender* app);

// Flush + commit + free the appender. Returns the rows-inserted
// count on success, -1 on error. The appender pointer is invalid
// after this call (do not free again).
int64_t axiomdb_appender_finish(AxiomDbAppender* app);

// Discard the appender and rollback its txn without committing.
// Used for early-abort paths. The appender pointer is invalid after.
void axiomdb_appender_free(AxiomDbAppender* app);
```

### Semantics

**Lifecycle (Rust + C):**

- `appender_open` → returns an Appender (owned/heap on C side, borrow-
  scoped on Rust side). Holds a transaction.
- Caller does `(append_<type>... × N_cols) → end_row()` per row.
- Caller does `flush()` to release memory; `finish()` to commit and
  consume.
- C side: `axiomdb_appender_finish` consumes the appender (the
  underlying Rust `finish(self)` takes ownership). Caller MUST NOT
  use the pointer after.
- C side: `axiomdb_appender_free` rolls back and frees (analog of
  Rust `Drop`).

**Error propagation:**

- Rust returns `DbError` as before.
- C returns 0/-1 (or NULL); the actual error message is set on the
  Db's `error_msg` (already-existing mechanism, exposed via
  `axiomdb_last_error(db)`). So C callers do:
  ```c
  if (axiomdb_appender_append_int(app, 1) != 0) {
      const char* err = axiomdb_last_error(db);
      // ...
  }
  ```

**Internal flow:**

Each `append_<type>` pushes onto `self.in_progress_row: Vec<Value>`.
`end_row()` checks arity, then calls `self.append_row_owned(row)`
which runs the existing v1.1 pipeline (AUTO_INC → generated → text →
coerce → NOT NULL → CHECK → FK → push to buffer → maybe auto-flush).

### Error cases

| Operation | Bad input | Result |
|-----------|-----------|--------|
| `append_int` etc | (no input) | always pushes; no validation per call |
| `end_row` | `current_row_len() != n_columns` | `TypeMismatch` |
| `end_row` | row fails CHECK / FK / NOT NULL / coerce | the pipeline error (existing semantics) |
| `axiomdb_appender_*` | NULL appender pointer | -1 (or NULL) |
| `axiomdb_appender_append_text` | NULL or non-UTF-8 cstring | -1 + `InvalidValue` |
| `axiomdb_appender_finish` | NULL pointer | -1 |
| Calling `axiomdb_appender_<anything>` after `finish` | use-after-free | undefined (C semantics — caller's responsibility) |

## Edge cases

- [ ] `end_row` with current_row_len = 0 → TypeMismatch (arity mismatch)
- [ ] `end_row` with current_row_len < n_columns → TypeMismatch
- [ ] `end_row` with current_row_len > n_columns → TypeMismatch (too many)
- [ ] `end_row` failure (e.g. CHECK violation) → in-progress row cleared;
  caller can immediately start a new row via `append_<type>`
- [ ] Mixing `append_row(&[])` and typed builders on the same Appender → works;
  buffers are independent
- [ ] Typed AUTO_INCREMENT column: `append_null()` triggers auto-assign
  (mirrors `append_row(&[Value::Null, ...])`)
- [ ] Typed GENERATED column: `append_null()` is the only acceptable input;
  any concrete `append_int(99)` → `InvalidValue` at `end_row`
- [ ] FFI: text with embedded NUL byte — caller responsible (we read
  via `CStr::from_ptr` which stops at the first NUL)
- [ ] FFI: empty bytes (`len = 0`) → `Value::Bytes(vec![])` accepted
- [ ] FFI: data pointer NULL with len > 0 → -1 (InvalidValue)
- [ ] FFI: appender pointer mismatch — using a freed appender = UB,
  same as any C pointer; documented in safety section

## On-disk format

**No change.** Both APIs route through the same `append_row_owned` →
`flush` → `insert_rows_batch_with_ctx` / `insert_clustered_rows_batch_with_ctx`
path established in v1.1. WAL entries, page layout, row codec all
identical.

## Performance budget

| Metric | Target |
|---|---:|
| Rust typed builder on heap `bench_users_heap` | within 10% of v1.1 heap (190K → ≥170K ops/s) — typed builder shouldn't slow us down |
| C FFI on heap `bench_users_heap` | within 20% of Rust typed (calling through FFI adds boundary cost) |
| Clustered numbers | unchanged from v1.1 (~82-128K) — the B-Tree split is the bottleneck, not the API shape |

This Attack does NOT target the perf gap to SQLite; it targets the
reach/usability gap. Perf parity vs v1.1 is the budget.

## Dependencies

Depends on:
- Attack 7 v1.1 Appender (`crates/axiomdb-embedded/src/appender.rs`)
- Existing C FFI scaffolding in `crates/axiomdb-embedded/src/lib.rs`
  (`AxiomDb`, `AxiomRows`, `axiomdb_last_error`)

Blocks:
- Python binding (`axiomdb-python`) — out of scope here, follow-up
- Node.js binding (`axiomdb-nodejs`) — out of scope here, follow-up
- Direct-encode optimization (zero `Vec<Value>` alloc) — follow-up

## Open questions

- [x] Should `end_row` take ownership of the in-progress row, or copy
  it? → **Take ownership** via `std::mem::take` — zero allocation
  beyond what the v1.1 path already does.
- [x] Should C FFI `axiomdb_appender_open` take a string for the
  table NAME or use a `*const c_char` like other functions? →
  `*const c_char` (UTF-8 NUL-term) — matches all other FFI functions.
- [x] Should typed builders accept by position (index) or be
  positional only via call order? → **Call order only** (no
  per-call column index argument). Matches DuckDB and SQLite
  semantics. Simpler, faster, less error-prone (you can't pass the
  wrong column index because there isn't one).
- [ ] Should the C FFI Appender be a true opaque struct (separate
  type) or just an alias for the Rust `Appender`? Recommendation:
  separate opaque struct `AxiomDbAppender` to insulate from Rust
  ABI changes.

## Done criteria

- [ ] All 8 new Rust methods on `Appender` (7 typed setters + `end_row`
  + `current_row_len`) match the signatures above exactly
- [ ] All 11 new C FFI exports (8 type-specific append + open +
  end_row + flush + finish + free) follow the existing `axiomdb_*`
  naming convention
- [ ] Rust integration tests in `integration_appender.rs`:
  - typed builder happy path on heap
  - typed builder happy path on clustered (PK)
  - end_row arity mismatch
  - end_row CHECK violation (in-progress row cleared, retry works)
  - mixing append_row + typed builders
- [ ] C FFI integration tests via a small `extern "C"` Rust test that
  uses the C functions through their raw signatures
- [ ] `cargo nextest run -p axiomdb-embedded` passes
- [ ] `cargo nextest run --workspace` passes (no regressions)
- [ ] `cargo clippy -p axiomdb-embedded -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Heap typed-builder throughput ≥ 170K ops/s on Lima
- [ ] C FFI heap throughput ≥ 140K ops/s on Lima (within 20% of Rust)
- [ ] docs-site embedded.md: typed builder example added (Rust and C)
- [ ] docs/perf-sqlite-gap.md "Attack 8" subsection appended
- [ ] memory/project_sqlite_baseline.md: Attack 8 entry
- [ ] Rustdoc on every new public method

## References

- DuckDB Appender — `research/duckdb/src/include/duckdb/main/appender.hpp`
  (`BeginRow / Append<T> / EndRow / AppendRow / Flush / Close`).
  Templated `Append<T>` is the C++ surface; the C export is per-type
  (`duckdb_append_int32`, `duckdb_append_varchar`, etc.).
- SQLite bind — `research/sqlite/src/sqliteInt.h` (the `sqlite3_bind_*`
  family + `sqlite3_step` + `sqlite3_reset` lifecycle). Per-column
  positional, type-specific.
- Existing FFI surface — `crates/axiomdb-embedded/src/lib.rs:725-1019`
  (`axiomdb_open`, `axiomdb_execute`, `axiomdb_query`, `axiomdb_rows_*`,
  `axiomdb_last_error`).
- Attack 7 v1 spec — `specs/fase-perf-sqlite-gap/spec-embedded-appender.md`
- Attack 7 v1.1 spec — `specs/fase-perf-sqlite-gap/spec-appender-v11-complete.md`
- Roadmap — `memory/project_embedded_release.md` (Python / Node.js
  bindings are explicit release targets — this Attack unblocks them)
