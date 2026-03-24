# Spec: Lazy Column Decode (optim-B)

Branch: `research/pg-internals-comparison`
Inspired by PostgreSQL's selective column decoding (heap_deform_tuple with attinmeta).

---

## What to build (not how)

A masked decode variant that skips allocating values for columns not referenced by a
query. Rows are still read in full from disk. Only the decode step is optimized:
unneeded columns have their bytes skipped (pos advanced) without allocating a `String`,
`Vec<u8>`, or any heap value. The result `Vec<Value>` has the same length as before;
skipped slots contain `Value::Null`.

Three call sites change:

1. `codec.rs` — new `decode_row_masked()` function alongside the existing `decode_row()`.
2. `table.rs` — `TableEngine::scan_table()` gains an optional `column_mask` parameter.
3. `executor.rs` — `execute_select_ctx` and `execute_delete_ctx` compute a mask from
   the AST and pass it to `scan_table`.

`decode_row()` and `encode_row()` are not modified. The on-disk format is unchanged.

---

## Inputs / Outputs

### `decode_row_masked`

```rust
pub fn decode_row_masked(
    bytes: &[u8],
    schema: &[DataType],
    mask: &[bool],   // mask[i] == true → decode col i; false → skip
) -> Result<Vec<Value>, DbError>
```

- **Input**: raw encoded row bytes, column type schema, boolean mask.
- **Output**: `Vec<Value>` of length `schema.len()`.
  - `mask[i] == true` and column is non-NULL: decoded value (identical to `decode_row`).
  - `mask[i] == true` and column is NULL (bitmap bit set): `Value::Null` (same as `decode_row`).
  - `mask[i] == false` and column is non-NULL: `pos` advanced by the column's wire size,
    `Value::Null` pushed (no allocation).
  - `mask[i] == false` and column is NULL (bitmap bit set): `pos` not advanced (NULL has
    no wire bytes), `Value::Null` pushed.
- **Errors**:
  - `DbError::TypeMismatch` — `mask.len() != schema.len()`.
  - `DbError::ParseError` — bytes are truncated or structurally invalid (same conditions
    as `decode_row`).

### `scan_table` (updated signature)

```rust
pub fn scan_table(
    storage: &mut dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
    column_mask: Option<&[bool]>,  // None = decode all (backward compat)
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
```

- `None` → calls `decode_row()` for every row (no behavior change for existing callers).
- `Some(mask)` → calls `decode_row_masked()` for every row.
- `mask.len()` must equal `columns.len()`; the contract is enforced inside
  `decode_row_masked`, which returns `DbError::TypeMismatch` if violated.

### Mask computation (executor)

A helper (private to `executor.rs`) computes a `Vec<bool>` of length `n_cols`:

```rust
fn build_column_mask(n_cols: usize, exprs: &[&Expr]) -> Vec<bool>
```

All `Expr::Column { col_idx, .. }` nodes reachable by recursive descent through the
provided expression slices set `mask[col_idx] = true`. All other positions remain
`false`. The function does not walk into subqueries (those are evaluated with their
own row environment and do not reference the outer row's columns by index in the same
scan).

---

## Column wire sizes (for skip logic)

These are the exact sizes used in `decode_row` and must be replicated verbatim in
`decode_row_masked` for fixed-length types:

| `DataType` | Wire bytes | Skip strategy |
|---|---|---|
| `Bool` | 1 | `pos += 1` |
| `Int` | 4 | `pos += 4` |
| `BigInt` | 8 | `pos += 8` |
| `Real` | 8 | `pos += 8` |
| `Decimal` | 17 | `pos += 17` |
| `Date` | 4 | `pos += 4` |
| `Timestamp` | 8 | `pos += 8` |
| `Uuid` | 16 | `pos += 16` |
| `Text` | 3-byte u24 prefix + payload | read u24 via `read_u24(bytes, pos)?`, `pos += 3 + len` |
| `Bytes` | 3-byte u24 prefix + payload | read u24 via `read_u24(bytes, pos)?`, `pos += 3 + len` |

`read_u24` is already defined in `codec.rs` and returns `Result<usize, DbError>`.
For `Text` and `Bytes`, the length prefix MUST be read (to advance `pos` correctly)
but the payload bytes are NOT copied or validated.

---

## Use cases

### 1. Selective projection (happy path)

```sql
SELECT name FROM users WHERE age > 18
```

Schema: `[id: Int, name: Text, age: Int, email: Text, bio: Text, created_at: Timestamp]`
(6 columns, indices 0–5)

Referenced columns: `name` (idx 1, SELECT list), `age` (idx 2, WHERE clause).
Mask: `[false, true, true, false, false, false]`

Result: columns 0, 3, 4, 5 return `Value::Null` — no `String` or `Vec<u8>` allocated
for `email` and `bio`.

### 2. DELETE with WHERE (happy path)

```sql
DELETE FROM t WHERE id = 42
```

Schema: `[id: Int, name: Text, payload: Bytes, score: Real, tags: Text, ts: Timestamp]`
(6 columns)

Referenced columns: `id` (idx 0, WHERE clause only).
Mask: `[true, false, false, false, false, false]`

For 10,000 rows: 5 columns × 10,000 skipped = 50,000 decode operations eliminated.
`name`, `payload`, `tags` are never copied into heap-allocated strings or byte vectors.

### 3. SELECT * — no regression

```sql
SELECT * FROM users
```

Mask: all-true (every index appears in the projection). Detected by
`mask.iter().all(|&b| b)`. `decode_row()` is called directly — zero mask overhead.

### 4. COUNT(*) — all columns skipped

```sql
SELECT COUNT(*) FROM t
```

No `Expr::Column` references anywhere. Mask: all-false.
`decode_row_masked()` advances `pos` for every column without allocating anything.
Only the bitmap bytes are read to distinguish NULL from non-NULL (to advance `pos`
correctly for non-NULL variable-length columns).

### 5. GROUP BY with aggregates

```sql
SELECT age, COUNT(*), AVG(score) FROM users GROUP BY age
```

Referenced columns: `age` (GROUP BY + SELECT), `score` (AVG argument).
Mask: only those two positions set. All other columns skipped.

### 6. Masked column is NULL in the bitmap

Column `i` has `mask[i] == false` AND the null bitmap marks it NULL. Behavior:
`pos` is NOT advanced (NULL has no wire bytes). `Value::Null` is pushed.
This must be correct even when the null bitmap comes from a column with no stored bytes.

---

## Acceptance criteria

- [ ] `decode_row_masked(bytes, schema, mask)` exists in
  `crates/axiomdb-types/src/codec.rs` and is `pub`.
- [ ] `mask.len() != schema.len()` returns `DbError::TypeMismatch` (not a panic).
- [ ] For every fixed-length type (`Bool`, `Int`, `BigInt`, `Real`, `Decimal`, `Date`,
  `Timestamp`, `Uuid`) with `mask[i] == false`: `pos` advances by the exact wire size
  listed in the table above. `ensure_bytes` is still called before advancing (no
  out-of-bounds access on corrupt input).
- [ ] For `Text` and `Bytes` with `mask[i] == false`: `read_u24` is called to read the
  length prefix, `pos` advances by `3 + len`. No `String` or `Vec<u8>` is allocated.
- [ ] For any column where the null bitmap bit is set AND `mask[i] == false`: `pos` is
  NOT advanced and `Value::Null` is pushed (no length prefix read for variable-length
  columns).
- [ ] `decode_row_masked` with an all-true mask produces the same output as `decode_row`
  for every valid input (property tested).
- [ ] `scan_table` signature updated to `column_mask: Option<&[bool]>`. Passing `None`
  produces identical output to the previous behavior.
- [ ] All existing call sites of `scan_table` in `executor.rs` and elsewhere compile
  with `None` passed as the new argument (backward compat — no silent behavior change).
- [ ] `execute_select_ctx` (single-table path) computes a mask from: SELECT list
  expressions, WHERE clause, ORDER BY expressions, GROUP BY expressions, HAVING clause.
  Passes `Some(&mask)` to `scan_table`.
- [ ] `execute_select_ctx` detects all-true mask and calls `decode_row()` directly via
  `None` (or the `all(|&b| b)` check inside `scan_table`).
- [ ] `execute_delete_ctx` (WHERE path) computes a mask from the WHERE clause only and
  passes `Some(&mask)` to `scan_table`.
- [ ] `execute_delete_ctx` (no-WHERE path) is unaffected — it already uses
  `scan_rids_visible` and never calls `scan_table`.
- [ ] All 1165+ existing tests pass without modification.
- [ ] New unit tests in `codec.rs` (under `#[cfg(test)]`) covering:
  - All-false mask on a row with mixed types.
  - All-true mask matches `decode_row` output exactly.
  - Mixed mask (some true, some false) with fixed-length and variable-length columns.
  - Mask with a NULL column that is false (bitmap NULL + mask false → `Value::Null`,
    no pos advance).
  - Mask with a NULL column that is true (bitmap NULL + mask true → `Value::Null`).
  - `mask.len() != schema.len()` returns `DbError::TypeMismatch`.
  - Truncated bytes with `mask[i] == false` for a fixed-length column returns
    `DbError::ParseError` (ensure_bytes still runs).
- [ ] Benchmark `DELETE FROM t WHERE id = X` on 10,000 rows shows fewer allocations
  compared to the pre-mask path (verified via `cargo bench` or allocation counter).

---

## Out of scope

- Changing the on-disk row format. Columns are still encoded in full; only decode is
  optimized.
- Projection pushdown to the storage layer. `HeapChain::scan_visible` still reads every
  row in full from disk.
- Index scan (Phase 5). An index scan eliminates the heap scan entirely for selective
  primary-key lookups; the mask optimization is orthogonal and complements it.
- Modifying `encode_row()`. Encoding always requires all column values.
- Applying the mask to multi-table JOIN paths in `execute_select_with_joins_ctx`. JOIN
  rows are composites of multiple tables; mask computation across joined schemas is
  deferred.
- Applying the mask to `execute_update_ctx`. UPDATE must read every column to re-encode
  the full row.
- Applying the mask to subquery paths (`FromClause::Subquery`). These delegate to the
  non-ctx path.

---

## Dependencies

- `decode_row()` in `axiomdb-types` must remain unchanged. Its signature, behavior, and
  all existing call sites are unaffected.
- `read_u24` must remain accessible within `codec.rs` (currently `fn read_u24` —
  visibility does not need to change; `decode_row_masked` lives in the same module).
- `ensure_bytes` must remain accessible within `codec.rs` (same module, no change
  needed).
- `scan_table` with `None` must be byte-for-byte equivalent to the current behavior so
  that callers that are not yet mask-aware (JOIN path, tests, integration tests) are
  unaffected.
- The semantic analyzer (Phase 4.18) must have resolved all `Expr::Column { col_idx }`
  values before the executor runs. This is already guaranteed for the paths covered by
  this optimization (single-table SELECT and DELETE with WHERE).
