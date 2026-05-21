# Spec: python-binding-packed — single-buffer result materialization

Phase: perf-sqlite-gap — Python binding parity
Task: Task 1 of sprint python-binding-perf (ctypes single-buffer)
Status: approved

## Context

The AxiomDB Python binding (`bindings/python/axiomdb.py`) is ~11× slower than
the stdlib `sqlite3` module for result materialization (measured: 78.3 ms vs
7.1 ms for a 10K-row × 6-col `SELECT *`). The cause is **not the engine** — it's
the binding: `query()` makes ~2 ctypes calls **per cell** (`axiomdb_rows_type` +
a typed getter), so a 10K×6 result crosses the FFI boundary ~120,000 times, each
crossing paying ~1.7–2.5 µs of ctypes marshalling overhead.

`sqlite3` is a CPython C extension that builds all Python objects in one C loop
with zero per-cell boundary crossings. We cannot fully match that with ctypes,
but we can collapse the 120K FFI calls into **one** by serializing the entire
result into a single contiguous buffer in Rust and parsing it once in Python.

This is Task 1 (interim win, stays pure-Python ctypes). Task 2 (PyO3 native
extension, true parity ~1.0–1.2×) follows separately.

## Goal

Add a packed-buffer FFI path so the Python binding materializes a full result set
with a single FFI call instead of ~2 per cell, cutting `SELECT *` materialization
from ~11× slower than sqlite3 to ~2–3×.

## Non-goals

- Not achieving full parity — that needs PyO3 (Task 2). Target here is ~2–3×.
- Not changing the engine or `AxiomRows` / existing per-cell accessors (kept for
  backward compatibility and the C API).
- Not touching the `execute()` / Appender paths (already fast — no per-cell FFI).
- Not adding DB-API 2.0 cursor semantics yet (Task 2 scope).

## Behavior

### New C FFI (in `crates/axiomdb-embedded/src/lib.rs`, `c-ffi` feature)

```rust
/// Executes a SELECT and serializes the entire result set into one contiguous
/// heap buffer. Returns the buffer pointer and writes its length to `out_len`.
/// Returns NULL on error (use `axiomdb_last_error`). Free with
/// `axiomdb_packed_free(ptr, len)`.
///
/// # Safety
/// `db` from `axiomdb_open`; `sql` non-null UTF-8; `out_len` non-null.
#[no_mangle]
pub unsafe extern "C" fn axiomdb_query_packed(
    db: *mut Db,
    sql: *const c_char,
    out_len: *mut usize,
) -> *mut u8;

/// Frees a buffer returned by `axiomdb_query_packed`.
///
/// # Safety
/// `ptr`/`len` must be exactly as returned by `axiomdb_query_packed`.
#[no_mangle]
pub unsafe extern "C" fn axiomdb_packed_free(ptr: *mut u8, len: usize);
```

### Buffer format (little-endian)

```
Header:
  u32   magic         = 0x41584D31  ("AXM1")
  u32   n_cols
  u64   n_rows
  Column names section (n_cols entries):
    u32 name_len, name_bytes (UTF-8)

Cells section (row-major, n_rows × n_cols cells):
  Each cell:
    u8  tag            0=NULL, 1=INT(i64), 2=REAL(f64), 3=TEXT, 4=BLOB
    payload by tag:
      NULL → (none)
      INT  → i64 (8 bytes)
      REAL → f64 (8 bytes)
      TEXT → u32 len + len bytes (UTF-8)
      BLOB → u32 len + len bytes
```

Type mapping mirrors the existing `CellValue::type_code()` so behavior is
identical to the per-cell path (Bool/Int/BigInt/Date→INT; Real→REAL;
Text/Json→TEXT; Bytes→BLOB; Null→NULL).

### Python API (in `bindings/python/axiomdb.py`)

```python
class AxiomDB:
    def query(self, sql: str) -> list[dict]:
        """Unchanged signature — returns list[dict]. Now uses the packed
        buffer internally (one FFI call), then builds dicts in Python."""

    def query_tuples(self, sql: str) -> list[tuple]:
        """Fast path — returns list[tuple] (sqlite3-compatible shape).
        Skips dict construction; fastest materialization."""

    def query_with_columns(self, sql: str) -> tuple[list[str], list[tuple]]:
        """Returns (column_names, rows-as-tuples)."""
```

### Semantics

- `query_tuples()` parses the packed buffer once and returns `list[tuple]`,
  matching `sqlite3.Cursor.fetchall()` shape and the bench's SQLite harness.
- `query()` keeps its `list[dict]` contract (column name → value) for backward
  compatibility, built from the same packed buffer (one FFI call + Python loop).
- Both free the buffer via `axiomdb_packed_free` in a `finally`.
- Empty result (DDL/DML/zero rows): returns `[]`.
- The legacy per-cell accessors remain for the C API and any external callers.

### Error cases

| Condition | Result |
|---|---|
| Query error (bad SQL, missing table) | NULL buffer → raise `AxiomDBError(last_error)` |
| `out_len` null (C misuse) | returns NULL |
| Non-SELECT (DDL/DML) | empty buffer (0 rows, 0 cols) → `[]` |

## Edge cases

- [ ] Zero rows — header with n_rows=0, no cells section
- [ ] NULL cells — tag 0, no payload
- [ ] Empty string / empty blob — len=0, no bytes
- [ ] Non-ASCII UTF-8 text — round-trips byte-exact, decoded in Python
- [ ] Large blob — u32 length (4 GB cap, acceptable)
- [ ] Mixed types within a column (NULL among ints) — per-cell tag handles it
- [ ] Buffer freed exactly once even if Python parse raises (`finally`)

## Performance budget

| Operation | Target |
|---|---|
| `query_tuples` 10K×6 vs sqlite3 fetchall | ≤ 3× (from 11×) |
| `query` (dict) 10K×6 vs sqlite3 | ≤ 4× (dict build overhead) |
| FFI calls per query | 2 (query_packed + packed_free), was ~120K |

Reference: sqlite3 fetchall 10K×6 ≈ 7.1 ms on this host.

## Dependencies

- Depends on: existing `Db::run` + `QueryResult::Rows`, `value_to_cell`/`CellValue`.
- Blocks: Task 2 (PyO3) is independent but shares the benchmark harness.

## Open questions

*(all resolved)*

- [x] Return shape? → `query_tuples` (tuples, fast) + `query` (dicts, compat).
- [x] How to free? → explicit `axiomdb_packed_free(ptr, len)` with Box<[u8]>.
- [x] Format endianness? → little-endian (x86/arm native, no swap).

## Done criteria

- [ ] `axiomdb_query_packed` + `axiomdb_packed_free` compile under `c-ffi`
- [ ] Buffer format round-trips all types (INT/REAL/TEXT/BLOB/NULL) byte-exact
- [ ] `query_tuples()` returns same data as legacy `query()` (values match)
- [ ] `query()` keeps `list[dict]` contract (regression-free)
- [ ] Rust unit test: pack a known result, assert byte layout
- [ ] Python correctness check vs sqlite3 (same rows/values)
- [ ] Bench shows `select` 10K×6 improves from ~11× to ≤3× slower than sqlite3
- [ ] `cargo nextest -p axiomdb-embedded` + `clippy` clean

## References

- Current binding hot loop: `bindings/python/axiomdb.py:227-249`
- C FFI + AxiomRows: `crates/axiomdb-embedded/src/lib.rs:782-1082`
- CPython reference: `Modules/_sqlite/cursor.c` `_pysqlite_fetch_one_row`
- Research findings: this session (PyO3 ~1.0-1.2×, ctypes-buffer ~2-3×)
- Bench harness: `benches/sqlite_vs_axiomdb/bench.py`
