# Spec: copy-streaming

Phase: 20 — Types + import/export
Task: COPY FROM streaming — O(batch) memory for CSV and JSONL
Status: approved

## Context

`COPY table FROM 'path'` was implemented in Phase 20.5. The current executor
(`copy_from.rs`) parses the entire file into `Vec<Vec<Value>>` before inserting
a single row. A 10 GB CSV of 200 M rows requires ~200 GB of RSS before the first
insert. This subphase replaces that collect-first strategy with a batch-loop that
holds at most `COPY_BATCH_SIZE` rows in memory at any point. The change is
wire-invisible: same SQL syntax, same `Affected { count }` result.

## Goal

Make `COPY table FROM 'path'` process CSV and JSONL files in O(batch_size) memory
regardless of file size, while preserving atomic transaction semantics.

## Non-goals

- Streaming JSON array format (`[{...},{...}]`) — not streamable without a full
  JSON parse tree; document the limitation, suggest JSONL for files > RAM.
- `COPY TO` streaming — COPY TO already buffers at the scan level; no change needed.
- Parallel I/O or background threads — single-threaded streaming is sufficient.
- Changing the SQL syntax or adding new options.
- Client-side COPY (piping data from the client connection) — separate future task.

## Behavior

### Batch size

```rust
const COPY_BATCH_SIZE: usize = 1024;
```

Defined as a module-level constant in `copy_from.rs`. Not user-configurable.

### CSV streaming

1. Open file, build `csv::Reader` with the existing options (delimiter, has_headers, flexible=false).
2. If `use_header`: read the header row once → `columns: Vec<String>`.
3. Enter batch loop:
   - Accumulate up to `COPY_BATCH_SIZE` records from `rdr.records()`.
   - When batch is full **or** EOF reached: call `execute_insert_ctx` with that batch.
   - Accumulate returned `Affected.count`.
4. Return `Affected { count: total, last_insert_id: None }`.

Memory invariant: at most `COPY_BATCH_SIZE` `Vec<Value>` rows alive at any time.

### JSONL streaming (schema-first)

Current implementation does two passes: collect all lines → discover all keys →
remap. The new approach:

1. Resolve the target table's column list **once** from the catalog at the start of
   `execute_copy_from` (reuse `resolve_table_cached` already called by the dispatch
   site, or call it from within `execute_copy_from`).
2. Build `col_index: HashMap<String, usize>` from the table schema (column_name →
   position in schema order). This is the authoritative column list.
3. Enter batch loop over `reader.lines()`:
   - Parse each non-empty line as a JSON object.
   - Map keys through `col_index`. Unknown keys → silently ignored. Missing keys →
     `Value::Null`.
   - Accumulate up to `COPY_BATCH_SIZE` rows, then call `execute_insert_ctx`.
4. Return `Affected { count: total, last_insert_id: None }`.

Memory invariant: at most `COPY_BATCH_SIZE` rows plus the schema `col_index` map.

### JSON array — unchanged (full-load)

`parse_json_file` stays as-is. The error message when a caller tries to load a
very large file will be an OOM at the OS level — no special handling. The docs
note that JSONL is required for files that exceed available RAM.

### Transaction semantics

All batches execute inside the same `conn_txn` that was passed into
`execute_copy_from`. No batch is individually committed. A failure on any batch
(parse error, FK violation, type coercion error) returns `Err(DbError)` to the
caller, which causes the surrounding autocommit or explicit transaction to roll
back — all previously inserted batches are discarded. Identical to PostgreSQL
COPY FROM atomicity.

### Public signatures (internal — no public API changes)

The only signature that changes is `execute_copy_from`'s internal helpers:

```rust
// OLD: parse entire file, return all rows
fn parse_csv_file(...) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError>
fn parse_jsonl_file(...) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError>

// NEW: execute_copy_from drives the loop itself; helpers become private iterators
// (exact shape chosen in /plan-task)
```

No changes to `CopyFromStmt`, `CopyOptions`, `CopyFormat`, or any AST type.
No changes to `execute_insert_ctx` signature.

## Error cases

| Condition | Expected error | Behavior |
|-----------|----------------|----------|
| File not found | `DbError::Io` | fails immediately, before first batch |
| CSV parse error on row N | `DbError::InvalidValue { reason: "COPY FROM: {path}: line {N}: ..." }` | whole COPY rolls back |
| JSONL: line N not valid JSON object | `DbError::InvalidValue { reason: "COPY FROM: {path}: line {N}: JSON parse error: {e}" }` | whole COPY rolls back |
| FK violation in batch M | `DbError` from execute_insert_ctx | whole COPY rolls back |
| Column count mismatch (CSV, no header) | `DbError::InvalidValue` | row-level, whole COPY rolls back |
| Empty file (0 data rows) | `Ok(Affected { count: 0 })` | no batches sent to insert |

## Edge cases

- [ ] Empty CSV (only header, zero data rows) → `Affected { count: 0 }`
- [ ] CSV with exactly `COPY_BATCH_SIZE` rows → one full batch, no partial batch
- [ ] CSV with `COPY_BATCH_SIZE + 1` rows → one full batch + one partial batch of 1
- [ ] JSONL with unknown keys → silently ignored (not an error)
- [ ] JSONL with missing keys → `Value::Null` in those positions
- [ ] JSONL blank lines → skipped, do not count toward row count
- [ ] JSONL line is not an object (array, number, string) → `DbError::InvalidValue`
- [ ] Error on last row of last batch → full rollback including all previous batches
- [ ] Unicode / non-ASCII values in CSV and JSONL → passed through as `Value::Text`

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| COPY FROM CSV, 100 000 rows, 5 cols | < 2 s | < 5 s |
| Peak RSS above baseline | < 50 MB | < 100 MB |

The 50 MB ceiling comes from: `COPY_BATCH_SIZE=1024` × `~5 cols` × `~64 B/Value` ≈ 320 KB
per batch, plus the `execute_insert_ctx` overhead. Real peak will be dominated by
the WAL write buffer, not the row batch.

## Dependencies

- Depends on: Phase 20.5 (`copy_from.rs`, `execute_insert_ctx`) — already in place.
- Blocks: nothing.

## Open questions

None — all resolved during brainstorm.

## Done criteria

- [ ] CSV streaming: `COPY_BATCH_SIZE = 1024` constant defined.
- [ ] CSV streaming: `parse_csv_file` no longer returns `Vec<Vec<Value>>`; rows fed to
      `execute_insert_ctx` in batches within `execute_copy_from`.
- [ ] JSONL streaming: schema-first column resolution; single-pass per-batch loop.
- [ ] JSON array: unchanged; behavior documented.
- [ ] Empty file → `Affected { count: 0 }` (no panic, no error).
- [ ] Error mid-stream → `Err(DbError)` returned (caller rolls back).
- [ ] `cargo nextest run -p axiomdb-sql` passes (all existing COPY tests green).
- [ ] New tests: empty CSV, exact-batch-size CSV, batch+1 CSV, JSONL unknown keys,
      JSONL missing keys, JSONL error mid-stream rollback.
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Wire smoke: 2 new assertions in `tools/wire-test.py` for 20.8.
- [ ] `docs/progreso.md` updated: `20.8 ✅`.

## References

- Implemented code: `crates/axiomdb-sql/src/executor/copy_from.rs`
- Phase 20.5 doc: `docs/fase-20.md` — COPY FROM/TO section
- PostgreSQL COPY atomicity: https://www.postgresql.org/docs/current/sql-copy.html
  ("If an error occurs... all rows inserted up to that point are discarded")
- `csv` crate lazy records: `csv::Reader::records()` returns an iterator — no buffering
