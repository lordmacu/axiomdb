# Spec: c-cursor-tier1 — zero-copy cursor C API over materialized results

Phase: perf-sqlite-gap — C read-path parity
Task: Tier 1 (cheap cursor; defers full streaming = Approach B)
Status: approved

## Context

The C read benchmark (`benches/sqlite_vs_axiomdb/bench_read.c`) shows AxiomDB's
per-cell C API at ~2.4× SQLite, vs the packed buffer at ~2.0×. Two costs explain
the gap over SQLite's lazy zero-copy `sqlite3_column_*`:

1. `axiomdb_query` materializes `QueryResult::Rows { rows: Vec<Vec<Value>> }`,
   then **converts every cell again** into `AxiomRows { cells: Vec<Vec<CellValue>> }`
   allocating a `CString` per text cell — a full second pass.
2. The accessors read from that converted structure.

Tier 1 removes the second pass: a cursor that keeps the engine's `Vec<Vec<Value>>`
as-is and returns text **zero-copy** (pointer into the live `Value::Text`). This
is bounded (~1 file), additive (existing `axiomdb_query` untouched), and low-risk.
Memory stays O(n) (full streaming = Approach B is deferred).

## Goal

A SQLite-style cursor C API (`axiomdb_cursor_*`) that materializes once into the
engine's native `Vec<Value>` rows and exposes per-cell accessors with zero-copy
text/blob, closing the C per-cell gap from ~2.4× toward the packed ~2.0× (or
better, since it avoids even the serialization copy).

## Non-goals

- Not streaming / O(1) memory — that is Approach B (deferred). Tier 1 still
  materializes the whole result internally.
- Not lazy per-column decode (SQLite points into the page) — deeper, deferred.
- Not removing `axiomdb_query` / `AxiomRows` / packed (all kept).
- Not exposing the cursor to Python yet (PyO3 already beats sqlite3).

## Behavior

### C FFI (in `crates/axiomdb-embedded/src/lib.rs`, `c-ffi` feature)

```c
typedef struct AxiomCursor AxiomCursor;

// Runs the SELECT, materializes rows once. NULL on error.
AxiomCursor* axiomdb_cursor_open(AxiomDb* db, const char* sql);

// Advances to the next row. Returns 1 if a row is available, 0 at end.
int axiomdb_cursor_step(AxiomCursor* cur);

int          axiomdb_cursor_columns(const AxiomCursor* cur);
const char*  axiomdb_cursor_column_name(const AxiomCursor* cur, int col); // null-terminated
int          axiomdb_cursor_type(AxiomCursor* cur, int col);   // 0..4, current row

int64_t      axiomdb_cursor_int(AxiomCursor* cur, int col);
double       axiomdb_cursor_double(AxiomCursor* cur, int col);
// Zero-copy: pointer into the current row's value; sets *len. Valid until the
// next axiomdb_cursor_step or axiomdb_cursor_close. NOT null-terminated.
const char*  axiomdb_cursor_text(AxiomCursor* cur, int col, size_t* len);
const uint8_t* axiomdb_cursor_blob(AxiomCursor* cur, int col, size_t* len);

void         axiomdb_cursor_close(AxiomCursor* cur);
```

### Semantics

- `axiomdb_cursor_open` runs `Db::run(sql)`; on `QueryResult::Rows` keeps
  `rows: Vec<Vec<Value>>` (NO `CellValue` conversion) + column-name `CString`s;
  on non-rows yields an empty cursor; on error returns NULL.
- `step` starts before the first row (`pos = usize::MAX`); each call advances and
  returns 1 while `pos < n_rows`, else 0.
- Accessors read `rows[pos][col]` of the **current** row.
- Type codes mirror the existing per-cell API: Bool/Int/BigInt/Date/Timestamp→1
  (INT), Real/Decimal→2 (REAL), Text/Json/Jsonb/Uuid/Array/Range/Composite→3
  (TEXT), Bytes→4 (BLOB), Null→0.
- `axiomdb_cursor_text`:
  - `Text`/`Json` → zero-copy pointer into the `String`, `*len = s.len()`.
  - `Uuid`/`Jsonb`/`Array`/`Range`/`Composite` → formatted into a per-cursor
    scratch buffer (valid until the next text/blob access or close).
  - else → NULL, `*len = 0`.
- `axiomdb_cursor_blob`: `Bytes` → zero-copy pointer + len; else NULL/0.
- Lifetime contract (SQLite-compatible): text/blob pointers are valid until the
  next `axiomdb_cursor_step` or `axiomdb_cursor_close`.

### Error cases

| Condition | Result |
|---|---|
| Query error | `axiomdb_cursor_open` returns NULL (see `axiomdb_last_error`) |
| Non-SELECT (DDL/DML) | empty cursor (0 cols, 0 rows) |
| Accessor before first `step` or after end | type 0 / 0 / NULL |
| Type mismatch (e.g. `_int` on text) | 0 / 0.0 / NULL (same as per-cell API) |

## Edge cases

- [ ] step past end returns 0 repeatedly (no panic)
- [ ] empty result → first step returns 0
- [ ] NULL cell → type 0, accessors return 0/NULL
- [ ] empty string / empty blob → valid ptr (or non-crash) + len 0
- [ ] non-ASCII text zero-copy round-trips byte-exact
- [ ] cursor closed exactly once; no use-after-free in the documented contract

## Performance budget

| Operation | Target | Baseline |
|---|---|---|
| C cursor read 10K×6 vs SQLite | ≤ 2.0× (from 2.4×) | per-cell 2.4×, packed 2.0×, sqlite 1.0× |

## Dependencies

- Depends on: `Db::run` → `QueryResult::Rows`, `axiomdb_types::Value`.
- Blocks: nothing (additive). Approach B (streaming) is the deeper follow-up.

## Done criteria

- [ ] `axiomdb_cursor_*` compile under `c-ffi`; exported symbols present
- [ ] Rust unit test: open → step → accessors return correct values incl NULL/text/blob
- [ ] `bench_read.c` gains a cursor path; shows ≤ ~2.0× (improved from 2.4×)
- [ ] `cargo nextest -p axiomdb-embedded` + `clippy` clean
- [ ] Zero-copy text verified byte-exact vs the per-cell path

## References

- C bench: `benches/sqlite_vs_axiomdb/bench_read.c`
- Per-cell + packed FFI: `crates/axiomdb-embedded/src/lib.rs`
- SQLite cursor model: `sqlite3_step` / `sqlite3_column_*` (lifetime contract)
- Deferred deeper work: Approach B (Volcano streaming) — brainstorm this session
