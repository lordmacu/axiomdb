# Plan: 39.21 — Aggregate Hash Execution

## Files to create/modify

- `crates/axiomdb-sql/Cargo.toml` — add `hashbrown = "0.15"`
- `crates/axiomdb-sql/src/executor/aggregate.rs` — all changes (single file)
- `crates/axiomdb-sql/tests/integration_aggregate_hash.rs` — new integration tests

## Algorithm / Data structure

### Root cause: `agg_add` and `agg_compare` go through `eval()`

```
// CURRENT — called 50K times for AVG:
fn agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    eval(                                    // full interpreter dispatch
        &Expr::BinaryOp {
            left: Box::new(Expr::Literal(a)),  // heap alloc
            right: Box::new(Expr::Literal(b)), // heap alloc
        },
        &[],
    )
}
// 2 Box allocations + eval tree traversal per accumulator update
```

Fix: match directly on the value variants. No AST nodes, no allocations.

### Root cause 2: key serialization for every row

```
// CURRENT — for GROUP BY age (INT), 50K times:
key_buf.extend_from_slice(&value_to_session_key_bytes(&Value::Int(42)));
// → Vec<u8> with tag byte + 4 bytes, then HashMap lookup via &[u8]
```

Fix: `GroupTablePrimitive` uses `i64` directly as hash key, zero serialization.

### Root cause 3: `representative_row: row.clone()` for each new group

For `SELECT age, COUNT(*), AVG(score) GROUP BY age` — 62 groups × 6 columns
cloned unnecessarily. Fix: pre-compute which column indices are needed and store
only those, using a sparse `Vec<Value>` at finalization.

---

### New data structures

```rust
// ── Replaces GroupState ────────────────────────────────────────────────────────

struct GroupEntry {
    /// Values of the GROUP BY expressions (for output row construction).
    key_values: Vec<Value>,
    /// Values of non-aggregate column references in SELECT / HAVING.
    /// Indexed parallel to `non_agg_col_indices` computed before the scan loop.
    non_agg_col_values: Vec<Value>,
    /// One accumulator per AggExpr in the query.
    accumulators: Vec<AggAccumulator>,
}

// ── GroupTablePrimitive — INT/BIGINT single-column GROUP BY ───────────────────

struct GroupTablePrimitive {
    /// key: i64 GROUP BY value → group index into `entries`
    /// hashbrown::HashMap memoizes the u64 hash internally in its raw table,
    /// so we get hash memoization (DataFusion technique) for free.
    map: hashbrown::HashMap<i64, usize>,
    /// NULL key gets its own slot (SQL: NULLs are equal in GROUP BY).
    null_group: Option<usize>,
    entries: Vec<GroupEntry>,
}

// ── GroupTableGeneric — all other cases ───────────────────────────────────────

struct GroupTableGeneric {
    /// key: serialized Vec<u8> → group index
    /// hashbrown replaces std::HashMap; 20–40% faster probing on realistic
    /// workloads due to SIMD-accelerated linear probing (Robin Hood).
    map: hashbrown::HashMap<Vec<u8>, usize>,
    entries: Vec<GroupEntry>,
}

// ── Dispatch enum (avoids trait object overhead) ──────────────────────────────

enum GroupTableKind {
    Primitive(GroupTablePrimitive),
    Generic(GroupTableGeneric),
}
```

### `value_agg_add` — direct arithmetic (no eval, no alloc)

```rust
fn value_agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) =>
            x.checked_add(y).map(Value::Int)
             .ok_or_else(|| DbError::Overflow { op: "SUM".into() }),
        (Value::BigInt(x), Value::BigInt(y)) =>
            x.checked_add(y).map(Value::BigInt)
             .ok_or_else(|| DbError::Overflow { op: "SUM".into() }),
        (Value::Real(x), Value::Real(y)) => Ok(Value::Real(x + y)),
        // Cross-type promotions (widening to the larger type)
        (Value::Int(x), Value::BigInt(y)) | (Value::BigInt(y), Value::Int(x)) =>
            (x as i64).checked_add(y).map(Value::BigInt)
                      .ok_or_else(|| DbError::Overflow { op: "SUM".into() }),
        (Value::Int(x), Value::Real(y)) | (Value::Real(y), Value::Int(x)) =>
            Ok(Value::Real(x as f64 + y)),
        (Value::BigInt(x), Value::Real(y)) | (Value::Real(y), Value::BigInt(x)) =>
            Ok(Value::Real(x as f64 + y)),
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) if s1 == s2 =>
            m1.checked_add(m2).map(|m| Value::Decimal(m, s1))
              .ok_or_else(|| DbError::Overflow { op: "SUM".into() }),
        (a, b) => Err(DbError::TypeMismatch {
            expected: format!("numeric, got {} and {}", a.variant_name(), b.variant_name()),
            got: "incompatible types in SUM/AVG".into(),
        }),
    }
}
```

### `finalize_avg` — direct division (no eval, no alloc)

```rust
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 { return Ok(Value::Null); }
    let f: f64 = match sum {
        Value::Int(n)         => n as f64,
        Value::BigInt(n)      => n as f64,
        Value::Real(f)        => f,
        Value::Decimal(m, s)  => m as f64 * 10f64.powi(-(s as i32)),
        other => return Err(DbError::TypeMismatch {
            expected: "numeric".into(),
            got: other.variant_name().into(),
        }),
    };
    Ok(Value::Real(f / count as f64))
}
```

### Non-agg column index pre-computation

```rust
/// Collects column indices (col_idx values) referenced by non-aggregate
/// expressions in the SELECT list and HAVING clause. Deduplicated.
///
/// These are the only column values needed from each source row at
/// group-finalization time. All other column values can be discarded.
fn compute_non_agg_col_indices(stmt: &SelectStmt) -> Vec<usize> {
    let mut idxs: Vec<usize> = Vec::new();
    for item in &stmt.columns {
        if let SelectItem::Expr { expr, .. } = item {
            if !contains_aggregate(expr) {
                collect_col_idxs(expr, &mut idxs);
            }
        }
    }
    if let Some(having) = &stmt.having {
        // Walk HAVING, collecting col_idxs that are outside aggregate calls
        collect_non_agg_col_idxs_in_having(having, &mut idxs);
    }
    idxs.sort_unstable();
    idxs.dedup();
    idxs
}
```

### Updated hot loop in `execute_select_grouped_hash`

```
Before scan loop:
  1. non_agg_col_indices = compute_non_agg_col_indices(&stmt)
  2. row_len = combined_rows.first().map(|r| r.len()).unwrap_or(0)
  3. Determine GroupTableKind:
     - If group_by is single Expr::Column with INT or BIGINT type → Primitive
     - Otherwise → Generic
  4. key_buf: Vec<u8> (reused, only for Generic path)
  5. key_values_buf: Vec<Value> (reused across rows)

Per-row hot loop:
  1. Evaluate GROUP BY expressions → key_values_buf (fast-path: col_idx direct)
  2. Look up group in table (Primitive: key = i64, no alloc;
     Generic: key = serialized Vec<u8>, clone only on new group)
  3. On NEW group:
     a. Extract non_agg_col_values from row (only indices in non_agg_col_indices)
     b. Insert GroupEntry into table.entries
  4. Update accumulators:
     - CountStar: += 1
     - CountCol: col read + null check
     - Sum: value_agg_add (no eval, no Box)
     - Min: compare_values_null_last (already exists, direct match)
     - Max: compare_values_null_last reversed
     - Avg: value_agg_add for sum field, += 1 for count

Finalization loop (unchanged signature for eval_with_aggs / project_grouped_row):
  1. Build virtual_row: vec![Value::Null; row_len]
     Fill in: virtual_row[idx] = non_agg_col_values[i] for each stored index
  2. Finalize accumulators → agg_values
  3. HAVING filter via eval_with_aggs(&virtual_row, ...)
  4. Project via project_grouped_row(&virtual_row, ...)
```

### `GroupTableKind` selection logic

```
Single GROUP BY column that is Expr::Column { col_idx } AND:
  - We can determine the column type as INT or BIGINT

→ GroupTablePrimitive (extract row[col_idx] as i64 or None for NULL)

Otherwise → GroupTableGeneric
```

Note: type information at this point in the executor comes from `combined_rows`
(we can inspect the actual value type of the first non-null row for that column).
This avoids needing schema information at execution time.

---

## Implementation phases

1. **Add `hashbrown` dependency** to `crates/axiomdb-sql/Cargo.toml`.
   Verify it compiles: `cargo check -p axiomdb-sql`.

2. **Implement `value_agg_add`** (new function, no existing code changed).
   Write unit tests for all numeric type combinations.
   Verify: no call to `eval()`, no `Box::new`.

3. **Fix `finalize_avg`** — remove the final `eval()` call; use direct f64 division.

4. **Update `AggAccumulator::update` for Sum and Avg** — replace `agg_add(...)` calls
   with `value_agg_add(...)`.
   Update `Min` and `Max` to use `compare_values_null_last` instead of `agg_compare`.
   Run: `cargo test -p axiomdb-sql`.

5. **Implement `GroupEntry`** — add struct, replace `GroupState` usages.
   Keep `GroupState` temporarily (or remove in the same step).

6. **Implement `GroupTablePrimitive`** with `get_or_insert(key: Option<i64>, ...)` and
   `drain(self) -> impl Iterator<Item = GroupEntry>`.
   Unit test: insert 62 groups, check counts are correct.

7. **Implement `GroupTableGeneric`** with `hashbrown::HashMap<Vec<u8>, usize>`.
   Keeps existing serialization (`value_to_session_key_bytes`), drops `std::HashMap`.

8. **Add `compute_non_agg_col_indices`** and the virtual_row construction helper.

9. **Rewrite `execute_select_grouped_hash`** to use `GroupTableKind`, `GroupEntry`,
   `non_agg_col_indices`, and virtual_row. Remove `GroupState`, remove old
   `HashMap<Vec<u8>, GroupState>`. Keep `eval_with_aggs` and `project_grouped_row`
   signatures unchanged.

10. **Run full test suite**: `cargo test -p axiomdb-sql`. Fix any failures.

11. **Run benchmark**: `python3 benches/comparison/local_bench.py --scenario aggregate --rows 50000`.
    Must reach ≤ 15ms. If not, profile with `cargo flamegraph` or `cargo samply`.

12. **Run regression benchmarks**: `python3 benches/comparison/local_bench.py --scenario all --rows 50000 --table`.
    Verify `insert_multi_values`, `select`, `update` have no regression > 5%.

## Tests to write

**Unit tests** (in `aggregate.rs` or inline `#[cfg(test)]`):
- `value_agg_add`: all type combinations (Int+Int, BigInt+BigInt, Real+Real,
  Int+Real, BigInt+Real, Int+BigInt, Decimal+Decimal same scale,
  overflow → Err, mismatched types → Err)
- `finalize_avg`: count=0 → Null, Int sum, BigInt sum, Real sum, Decimal sum
- `GroupTablePrimitive`: 62 groups, NULL group, group reuse
- `GroupTableGeneric`: TEXT key, multi-column composite key

**Integration tests** (`tests/integration_aggregate_hash.rs`):
- `SELECT age, COUNT(*), AVG(score) FROM t GROUP BY age` → correct results
- `SELECT COUNT(*) FROM empty_table` → `[(0)]`
- `SELECT MIN(score), MAX(score) FROM t` → correct min/max, no GROUP BY
- `SELECT dept, SUM(salary) FROM t GROUP BY dept HAVING SUM(salary) > 1000`
- `GROUP BY NULL` (column with all-NULL values) → one NULL group
- `AVG` with all-NULL column → `NULL`
- `SUM` over INT that causes overflow → `DbError::Overflow`
- `SELECT name, age, COUNT(*) FROM t GROUP BY age` — non-agg non-key column
  in SELECT (exercises `non_agg_col_indices` path)
- Mixed types in SUM: insert rows with Int and Real in same column (exercises
  cross-type promotion in `value_agg_add`)

## Anti-patterns to avoid

- **DO NOT** call `eval()` inside `value_agg_add` or `value_agg_compare`. The entire
  point is to avoid the expression interpreter in the hot path.
- **DO NOT** use `Box::new(Expr::Literal(...))` anywhere in the accumulator update
  path. Search for this pattern after implementation.
- **DO NOT** introduce dynamic dispatch (`dyn GroupTable`). Use the `GroupTableKind`
  enum for zero-cost dispatch.
- **DO NOT** change `eval_with_aggs` or `project_grouped_row` function signatures.
  The virtual_row adapter preserves the existing interface contract.
- **DO NOT** touch the sorted aggregation path (`execute_select_grouped_sorted`).
  It is correct and not in scope.
- **DO NOT** serialize a column value to `Vec<u8>` on the `GroupTablePrimitive` path.
  The entire purpose of that path is zero-serialization.
- **DO NOT** remove `value_to_session_key_bytes` or `value_to_key_bytes` — they are
  still needed by `GroupTableGeneric` and other callers.

## Risks

| Risk | Mitigation |
|---|---|
| `GroupTableKind` type detection wrong: first non-null row is `Real` but table is `INT` due to coercion | Use the actual `Value` variant in the first row, not schema type. If first value is not `Int`/`BigInt`, fall back to Generic without error. |
| `non_agg_col_indices` misses a column needed by a complex HAVING expression | `collect_non_agg_col_idxs_in_having` must recursively walk all non-aggregate sub-expressions in HAVING. Add integration test for complex HAVING. |
| `virtual_row` construction wrong length when `combined_rows` is empty (ungrouped aggregate) | Guard: if `combined_rows.is_empty()`, `row_len = 0`; ungrouped aggregate with no rows still emits one output row (existing behavior preserved). |
| `value_agg_add` overflow behavior differs from old `eval()`-based behavior | Old `eval()` uses wrapping for Int; new code uses `checked_add` → `DbError::Overflow`. New behavior is more correct. Regression tests verify SUM results. |
| hashbrown version conflict with workspace deps | Add `hashbrown = "0.15"` exactly. If workspace already pulls a different version transitively, use `[dependencies.hashbrown] workspace = true` after checking `Cargo.lock`. |
