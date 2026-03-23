# Plan: 4.10 + 4.10b + 4.10c — ORDER BY + LIMIT/OFFSET

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | modify | Remove ORDER BY/LIMIT guards; add sort + pagination |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add ORDER BY / LIMIT tests |

---

## New functions in `executor.rs`

```
compare_sort_values(a, b, direction, nulls) → Ordering
compare_rows_for_sort(a, b, order_items) → Result<Ordering, DbError>
apply_order_by(rows, order_items) → Result<Vec<Row>, DbError>
eval_as_usize(expr) → Result<usize, DbError>
apply_limit_offset(rows, limit, offset) → Result<Vec<Row>, DbError>
```

---

## Algorithm

### Step 1 — Remove ORDER BY and LIMIT guards

In `execute_select`, remove:
```rust
if !stmt.order_by.is_empty() {
    return Err(DbError::NotImplemented { feature: "ORDER BY — Phase 4.10" });
}
if stmt.limit.is_some() {
    return Err(DbError::NotImplemented { feature: "LIMIT — Phase 4.10" });
}
```

### Step 2 — `compare_sort_values`

```rust
fn compare_sort_values(
    a: &Value,
    b: &Value,
    direction: SortOrder,
    nulls: Option<NullsOrder>,
) -> std::cmp::Ordering {
    // Determine whether NULLs sort before or after non-NULLs.
    let nulls_first = match (direction, nulls) {
        (_, Some(NullsOrder::First)) => true,
        (_, Some(NullsOrder::Last))  => false,
        (SortOrder::Asc,  None)      => false,  // AxiomDB default: NULLS LAST for ASC
        (SortOrder::Desc, None)      => true,   // AxiomDB default: NULLS FIRST for DESC
    };

    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => if nulls_first { Less } else { Greater },
        (_, Value::Null) => if nulls_first { Greater } else { Less },
        (a, b) => {
            // Non-NULL comparison: delegate to eval() for type correctness.
            let ord = compare_non_null_for_sort(a, b);
            if direction == SortOrder::Desc { ord.reverse() } else { ord }
        }
    }
}
```

### Step 3 — `compare_non_null_for_sort`

Reuse the comparison logic from the expression evaluator. Since `eval_binary(Lt, ...)` returns a `Value::Bool`, delegate via `eval()`:

```rust
fn compare_non_null_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    // Use eval() for type-correct comparison with coercion.
    // Fallback to Equal on error (type mismatch in ORDER BY → stable non-crash).
    let lt_result = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    let eq_result = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    );
    match (lt_result, eq_result) {
        (Ok(lt), Ok(eq)) => {
            if is_truthy(&lt) { Less }
            else if is_truthy(&eq) { Equal }
            else { Greater }
        }
        _ => Equal  // type mismatch: treat as equal (stable)
    }
}
```

**Note on performance:** This allocates `Expr::Literal` nodes for each comparison.
For Phase 4.10 this is acceptable. Phase 6 (query planner) will build a key
extraction step that avoids per-comparison allocation.

### Step 4 — `compare_rows_for_sort`

```rust
fn compare_rows_for_sort(
    a: &[Value],
    b: &[Value],
    order_items: &[OrderByItem],
) -> Result<std::cmp::Ordering, DbError> {
    for item in order_items {
        let key_a = eval(&item.expr, a)?;
        let key_b = eval(&item.expr, b)?;
        let ord = compare_sort_values(&key_a, &key_b, item.order, item.nulls);
        if ord != std::cmp::Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}
```

### Step 5 — `apply_order_by`

```rust
fn apply_order_by(
    mut rows: Vec<Row>,
    order_items: &[OrderByItem],
) -> Result<Vec<Row>, DbError> {
    // Collect any error during comparison.
    let mut sort_err: Option<DbError> = None;
    rows.sort_by(|a, b| {
        if sort_err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_rows_for_sort(a, b, order_items) {
            Ok(ord) => ord,
            Err(e) => {
                sort_err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }
    Ok(rows)
}
```

### Step 6 — `eval_as_usize` and `apply_limit_offset`

```rust
fn eval_as_usize(expr: &Expr) -> Result<usize, DbError> {
    match eval(expr, &[])? {
        Value::Int(n) if n >= 0    => Ok(n as usize),
        Value::BigInt(n) if n >= 0 => Ok(n as usize),
        Value::Int(_) | Value::BigInt(_) => Err(DbError::TypeMismatch {
            expected: "non-negative integer for LIMIT/OFFSET".into(),
            got: "negative integer".into(),
        }),
        other => Err(DbError::TypeMismatch {
            expected: "integer for LIMIT/OFFSET".into(),
            got: other.variant_name().into(),
        }),
    }
}

fn apply_limit_offset(
    rows: Vec<Row>,
    limit: &Option<Expr>,
    offset: &Option<Expr>,
) -> Result<Vec<Row>, DbError> {
    let offset_n = offset
        .as_ref()
        .map(eval_as_usize)
        .transpose()?
        .unwrap_or(0);
    let limit_n = limit
        .as_ref()
        .map(eval_as_usize)
        .transpose()?;
    Ok(rows
        .into_iter()
        .skip(offset_n)
        .take(limit_n.unwrap_or(usize::MAX))
        .collect())
}
```

### Step 7 — Integration into `execute_select`

**Non-GROUP BY path** — insert after WHERE filter, before projection:

```
// After: combined_rows = post-WHERE source rows
// Before: build_select_column_meta + project_row

// Sort source rows.
if !stmt.order_by.is_empty() {
    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;
}

// Project.
let out_cols = build_select_column_meta(...)?;
let mut rows = combined_rows.iter()
    .map(|v| project_row(&stmt.columns, v))
    .collect::<Result<Vec<_>, _>>()?;

// Paginate projected rows.
rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

Ok(QueryResult::Rows { columns: out_cols, rows })
```

**GROUP BY path** — `execute_select_grouped` now also receives `order_by`,
`limit`, `offset` from `stmt` and applies them before returning:

```
// At end of execute_select_grouped, after projecting rows:
if !stmt.order_by.is_empty() {
    rows = apply_order_by(rows, &stmt.order_by)?;
}
rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;
Ok(QueryResult::Rows { columns: out_cols, rows })
```

**JOIN path** — `execute_select_with_joins` already delegates to
`execute_select_grouped` or returns `QueryResult` directly. Add the same
ORDER BY + LIMIT/OFFSET application before the final `Ok(Rows)`:

```
// After: combined_rows assembled post-WHERE
// Before: build_join_column_meta + project_row

if !stmt.order_by.is_empty() {
    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;
}

let out_cols = build_join_column_meta(...)?;
let mut rows = combined_rows.iter()
    .map(|r| project_row(&stmt.columns, r))
    .collect::<Result<Vec<_>, _>>()?;

rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

Ok(QueryResult::Rows { columns: out_cols, rows })
```

---

## Implementation order

1. Remove ORDER BY and LIMIT guards in `execute_select`. `cargo check`.
2. Implement `compare_sort_values` + `compare_non_null_for_sort`. Unit test.
3. Implement `compare_rows_for_sort`. Unit test.
4. Implement `apply_order_by`. Unit test.
5. Implement `eval_as_usize` + `apply_limit_offset`. Unit test.
6. Integrate into non-GROUP BY path of `execute_select`. Integration test.
7. Integrate into `execute_select_with_joins`. Integration test.
8. Add ORDER BY support in `execute_select_grouped`. Integration test.
9. `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Unit tests in `executor.rs`

```
test_compare_sort_values_nulls_asc_default
  — (Null, Int(5), Asc, None) → Greater  (NULL sorts last for ASC)
  — (Int(5), Null, Asc, None) → Less

test_compare_sort_values_nulls_desc_default
  — (Null, Int(5), Desc, None) → Less  (NULL sorts first for DESC)
  — (Int(5), Null, Desc, None) → Greater

test_compare_sort_values_nulls_first_explicit
  — (Null, Int(5), Asc, Some(First)) → Less

test_compare_sort_values_nulls_last_explicit
  — (Null, Int(5), Desc, Some(Last)) → Greater

test_compare_sort_values_equal_ints
  — (Int(3), Int(3), Asc, None) → Equal

test_compare_sort_values_asc
  — (Int(1), Int(3), Asc, None) → Less

test_compare_sort_values_desc
  — (Int(1), Int(3), Desc, None) → Greater

test_apply_limit
  — 5 rows, LIMIT 3 → 3 rows

test_apply_offset
  — 5 rows, OFFSET 2 → 3 rows (rows 2,3,4)

test_apply_limit_offset
  — 10 rows, LIMIT 3 OFFSET 5 → rows 5,6,7

test_limit_zero
  — LIMIT 0 → 0 rows

test_offset_beyond_end
  — 3 rows, OFFSET 10 → 0 rows

test_negative_limit_error
  — LIMIT -1 → TypeMismatch

test_non_integer_limit_error
  — LIMIT 'abc' → TypeMismatch
```

### Integration tests in `integration_executor.rs`

```
test_order_by_asc
  — INSERT 3 rows unordered; SELECT ... ORDER BY id ASC → ascending

test_order_by_desc
  — same; ORDER BY id DESC → descending

test_order_by_text_column
  — INSERT ('Bob'), ('Alice'), ('Carol'); ORDER BY name → alphabetical

test_order_by_nulls_asc_default
  — INSERT rows with some NULL salaries; ORDER BY salary ASC
  — NULLs appear at the END (NULLS LAST default for ASC)

test_order_by_nulls_desc_default
  — ORDER BY salary DESC → NULLs appear at the START (NULLS FIRST default for DESC)

test_order_by_nulls_first_explicit
  — ORDER BY salary ASC NULLS FIRST → NULLs appear at start

test_order_by_nulls_last_explicit
  — ORDER BY salary DESC NULLS LAST → NULLs appear at end

test_multi_column_order_by
  — INSERT (eng, 90), (eng, 70), (sales, 80)
  — ORDER BY dept ASC, salary DESC → eng rows sorted desc, then sales

test_limit_only
  — 10 rows, LIMIT 3 → 3 rows

test_limit_offset
  — 10 rows, ORDER BY id ASC, LIMIT 3 OFFSET 5 → rows with id 6,7,8

test_limit_zero
  — LIMIT 0 → empty result

test_offset_beyond_table
  — 3 rows, OFFSET 100 → empty result

test_order_by_group_by
  — SELECT dept, COUNT(*) FROM employees GROUP BY dept ORDER BY dept ASC
  — groups in alphabetical order
```

---

## Anti-patterns to avoid

- **DO NOT** sort after projection for non-GROUP BY queries. ORDER BY must
  evaluate against source rows (before projection), otherwise `col_idx` values
  assigned by the analyzer would be out of bounds for the output row.
- **DO NOT** use `sort_unstable_by` if stable sort is needed for equal keys.
  Use `sort_by` to preserve insertion order for ties (SQL requires deterministic
  output for `ORDER BY a, b` when `a = b` and `b = b`).
- **DO NOT** call `eval()` on ORDER BY expressions inside `sort_by` without
  propagating errors. Capture errors via a `sort_err` variable and return after
  the sort.
- **DO NOT** apply LIMIT before ORDER BY. LIMIT is the last step; applying it
  first would return the wrong rows.
- **DO NOT** `unwrap()` anywhere in `src/` code.

---

## Risks

| Risk | Mitigation |
|---|---|
| `sort_by` closure cannot return `Result<>` | Use `sort_err: Option<DbError>` pattern; check after sort |
| ORDER BY `col_idx` out of bounds for GROUP BY + ORDER BY | Check bounds in `compare_rows_for_sort`; return error rather than panic |
| LIMIT `usize::MAX` when `take(usize::MAX)` on large iterators | Iterator laziness handles this correctly — `take(usize::MAX)` collects all remaining elements |
| Alloc-heavy comparisons (Expr::Literal per comparison) | Acceptable for Phase 4.10; profiling in Phase 6 will optimize if needed |
