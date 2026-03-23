# Plan: 4.9a + 4.9c + 4.9d — GROUP BY (Hash) + Aggregate Functions + HAVING

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | modify | Add aggregation path + all helpers |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add GROUP BY / aggregate tests |

One file modified, one file extended. No new crates, no new dependencies.

---

## Module layout inside `executor.rs`

```
// Existing
execute_select (modified: add trigger + dispatch to grouped path)

// New private functions
fn has_aggregates(items: &[SelectItem], having: &Option<Expr>) -> bool
fn contains_aggregate(expr: &Expr) -> bool
fn is_aggregate(name: &str) -> bool

fn execute_select_grouped(stmt, combined_rows, storage, txn)
  └── collect_agg_exprs(items, having) -> Vec<AggExpr>
  └── aggregation loop
  └── finalization + HAVING + projection

fn value_to_key_bytes(v: &Value) -> Vec<u8>
fn group_key_bytes(key_values: &[Value]) -> Vec<u8>

enum AggAccumulator { CountStar, CountCol, Sum, Min, Max, Avg }
impl AggAccumulator { new(agg: &AggExpr), update(row), finalize() }

struct AggExpr { name, arg, agg_idx }
impl AggExpr { matches(name, args) }

struct GroupState { key_values, accumulators }

fn eval_with_aggs(expr, key_values, agg_values, agg_exprs)
fn project_grouped_row(items, key_values, agg_values, agg_exprs)
fn build_grouped_column_meta(items, group_by, agg_exprs, all_cols)
fn finalize_avg(sum: Value, count: u64) -> Value
```

---

## Algorithm

### Step 1 — Trigger detection in `execute_select`

Replace the `GROUP BY` guard:

```
// REMOVE:
if !stmt.group_by.is_empty() {
    return Err(NotImplemented { feature: "GROUP BY — Phase 4.9" });
}

// ADD: after scanning rows (single or joined), check if aggregation needed:
let needs_aggregation = !stmt.group_by.is_empty()
    || has_aggregates(&stmt.columns, &stmt.having);

if needs_aggregation {
    return execute_select_grouped(stmt, combined_rows, ...);
}
// else: existing WHERE filter + projection path
```

For the single-table path: scan first, then check. For the JOIN path:
`execute_select_with_joins` already returns combined rows; check after.

Actually, to keep changes minimal, the grouping is applied AFTER scan+join+WHERE
in a post-processing step. The single-table path and JOIN path both end in
`combined_rows: Vec<Row>` (after WHERE filter). Extract that common part, then:

```
if needs_aggregation:
    execute_select_grouped(stmt, combined_rows, ...)
else:
    project + return Rows
```

This requires extracting "scan + filter" from both paths into a helper. For
implementation simplicity: handle aggregation as a post-join, post-WHERE step.

### Step 2 — `collect_agg_exprs`

Walk SELECT items and HAVING to find all aggregate calls. Return a deduplicated
`Vec<AggExpr>`:

```
fn collect_agg_exprs(items: &[SelectItem], having: &Option<Expr>) -> Vec<AggExpr>:
  let mut result: Vec<AggExpr> = Vec::new();

  fn visit(expr: &Expr, result: &mut Vec<AggExpr>):
    match expr:
      Function { name, args } if is_aggregate(name):
        // Check if already registered.
        let existing = result.iter().position(|ae| ae.name == name && ae.arg_matches(args));
        if existing.is_none():
          result.push(AggExpr { name: name.clone(), arg: args.first().cloned(), agg_idx: result.len() });
      BinaryOp { left, right, .. } => visit(left); visit(right)
      UnaryOp { operand, .. } => visit(operand)
      // ... recurse into other variants as needed
      _ => ()

  for item in items:
    if let Expr { expr, .. } = item: visit(expr, &mut result)
  if let Some(having) = having: visit(having, &mut result)
  result
```

### Step 3 — `value_to_key_bytes`

```rust
fn value_to_key_bytes(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null         => buf.push(0x00),
        Value::Bool(false)  => buf.extend_from_slice(&[0x01, 0x00]),
        Value::Bool(true)   => buf.extend_from_slice(&[0x01, 0x01]),
        Value::Int(n)       => { buf.push(0x02); buf.extend_from_slice(&n.to_le_bytes()); }
        Value::BigInt(n)    => { buf.push(0x03); buf.extend_from_slice(&n.to_le_bytes()); }
        Value::Real(f)      => { buf.push(0x04); buf.extend_from_slice(&f.to_bits().to_le_bytes()); }
        Value::Decimal(m,s) => { buf.push(0x05); buf.extend_from_slice(&m.to_le_bytes()); buf.push(*s); }
        Value::Text(s)      => {
            buf.push(0x06);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b)     => {
            buf.push(0x07);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Date(d)      => { buf.push(0x08); buf.extend_from_slice(&d.to_le_bytes()); }
        Value::Timestamp(t) => { buf.push(0x09); buf.extend_from_slice(&t.to_le_bytes()); }
        Value::Uuid(u)      => { buf.push(0x0A); buf.extend_from_slice(u); }
    }
    buf
}

fn group_key_bytes(key_values: &[Value]) -> Vec<u8> {
    key_values.iter().flat_map(value_to_key_bytes).collect()
}
```

### Step 4 — `AggAccumulator`

```rust
enum AggAccumulator {
    CountStar { n: u64 },
    CountCol  { n: u64 },
    Sum  { acc: Option<Value> },
    Min  { acc: Option<Value> },
    Max  { acc: Option<Value> },
    Avg  { sum: Value, count: u64 },
}

impl AggAccumulator {
    fn new(agg: &AggExpr) -> Self {
        match agg.name.as_str() {
            "count" if agg.arg.is_none() => Self::CountStar { n: 0 },
            "count"                      => Self::CountCol  { n: 0 },
            "sum"                        => Self::Sum  { acc: None },
            "min"                        => Self::Min  { acc: None },
            "max"                        => Self::Max  { acc: None },
            "avg"  => Self::Avg  { sum: Value::Int(0), count: 0 },
            _ => unreachable!("non-aggregate passed to AggAccumulator::new"),
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        match self {
            Self::CountStar { n }  => { *n += 1; }
            Self::CountCol  { n }  => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) { *n += 1; }
            }
            Self::Sum { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None    => v,
                        Some(a) => eval_binary_add(a, v)?,
                    });
                }
            }
            Self::Min { acc } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => if compare_values(&v, &a)? == Ordering::Less { v } else { a },
                    });
                }
            }
            Self::Max { acc } => { /* symmetric to Min, keep Greater */ }
            Self::Avg { sum, count } => {
                let v = eval(agg.arg.as_ref().unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *sum = eval_binary_add(sum.clone(), v)?;
                    *count += 1;
                }
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<Value, DbError> {
        match self {
            Self::CountStar { n }  => Ok(Value::BigInt(n as i64)),
            Self::CountCol  { n }  => Ok(Value::BigInt(n as i64)),
            Self::Sum  { acc }     => Ok(acc.unwrap_or(Value::Null)),
            Self::Min  { acc }     => Ok(acc.unwrap_or(Value::Null)),
            Self::Max  { acc }     => Ok(acc.unwrap_or(Value::Null)),
            Self::Avg  { sum, count } => finalize_avg(sum, count),
        }
    }
}

fn eval_binary_add(a: Value, b: Value) -> Result<Value, DbError> {
    // Use coerce_for_op + arithmetic eval from the expression evaluator.
    use crate::eval::eval_binary_op;  // or replicate inline with coerce_for_op
    let (a, b) = coerce_for_op(a, b)?;
    // Add them.
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.checked_add(y).ok_or(DbError::Overflow)?)),
        (Value::BigInt(x), Value::BigInt(y)) => Ok(Value::BigInt(x.checked_add(y).ok_or(DbError::Overflow)?)),
        (Value::Real(x), Value::Real(y)) => Ok(Value::Real(x + y)),
        (Value::Decimal(m1,s1), Value::Decimal(m2,s2)) => decimal_add(m1,s1,m2,s2),
        _ => unreachable!("coerce_for_op ensures matching types"),
    }
}
```

**Note on `eval_binary_add`:** Rather than duplicating `decimal_arith`, use the
existing `eval(&Expr::BinaryOp { op: Add, left: Literal(a), right: Literal(b) }, &[])`.
This reuses all coercion + overflow logic from eval.rs for free.

**Simplified approach:** build a synthetic `Expr::BinaryOp` and call `eval()`:
```rust
fn eval_binary_add(a: Value, b: Value) -> Result<Value, DbError> {
    use crate::expr::{BinaryOp, Expr};
    let expr = Expr::BinaryOp {
        op: BinaryOp::Add,
        left: Box::new(Expr::Literal(a)),
        right: Box::new(Expr::Literal(b)),
    };
    eval(&expr, &[])
}
```
This is clean and reuses eval's entire numeric type handling.

### Step 5 — `finalize_avg`

```rust
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 {
        return Ok(Value::Null);
    }
    // Always produce Real for AVG (SQL standard).
    let sum_as_real = match sum {
        Value::Int(n)    => Value::Real(n as f64),
        Value::BigInt(n) => Value::Real(n as f64),
        Value::Real(f)   => Value::Real(f),
        Value::Decimal(m, s) => Value::Real(m as f64 * 10f64.powi(-(s as i32))),
        other => return Err(DbError::TypeMismatch {
            expected: "numeric".into(),
            got: other.variant_name().into(),
        }),
    };
    let count_real = Value::Real(count as f64);
    eval(&Expr::BinaryOp {
        op: BinaryOp::Div,
        left: Box::new(Expr::Literal(sum_as_real)),
        right: Box::new(Expr::Literal(count_real)),
    }, &[])
}
```

### Step 6 — `execute_select_grouped`

```rust
fn execute_select_grouped(
    stmt: &SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {

    // Build aggregate registry.
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);

    // One-pass aggregation.
    let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

    for row in &combined_rows {
        let key_values: Vec<Value> = stmt.group_by.iter()
            .map(|e| eval(e, row))
            .collect::<Result<_, _>>()?;
        let key_bytes = group_key_bytes(&key_values);

        let state = groups.entry(key_bytes).or_insert_with(|| GroupState {
            key_values: key_values.clone(),
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });

        for (acc, agg) in state.accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(row, agg)?;
        }
    }

    // Ungrouped aggregate: emit one group with empty key even on empty input.
    if stmt.group_by.is_empty() && groups.is_empty() {
        let state = GroupState {
            key_values: vec![],
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        };
        groups.insert(vec![], state);
    }

    // Build output column metadata.
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Finalize, HAVING filter, project.
    let mut rows = Vec::new();
    for (_, state) in groups {
        let agg_values: Vec<Value> = state.accumulators.into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &state.key_values, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) { continue; }
        }

        let out_row = project_grouped_row(&stmt.columns, &state.key_values, &agg_values, &agg_exprs)?;
        rows.push(out_row);
    }

    Ok(QueryResult::Rows { columns: out_cols, rows })
}
```

### Step 7 — `eval_with_aggs`

Recursive eval, same as `eval()` but with aggregate interception:

```rust
fn eval_with_aggs(
    expr: &Expr,
    key_values: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column { col_idx, .. } => {
            key_values.get(*col_idx).cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds { idx: *col_idx, len: key_values.len() })
        }
        Expr::Function { name, args } if is_aggregate(name) => {
            let idx = agg_exprs.iter().position(|ae| ae.name == *name && ae.arg_matches(args))
                .ok_or_else(|| DbError::Other(
                    format!("aggregate '{name}' not pre-registered — internal error")
                ))?;
            Ok(agg_values[idx].clone())
        }
        Expr::BinaryOp { op: BinaryOp::And, left, right } => {
            // Short-circuit AND (reuse from eval.rs logic inline or just call eval?)
            // Simplification: eval_with_aggs for all sub-exprs
            let l = eval_with_aggs(left, key_values, agg_values, agg_exprs)?;
            match l {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true)  => eval_with_aggs(right, key_values, agg_values, agg_exprs),
                Value::Null => {
                    let r = eval_with_aggs(right, key_values, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(false)) { Value::Bool(false) } else { Value::Null })
                }
                other => Err(DbError::TypeMismatch { expected: "Bool".into(), got: other.variant_name().into() }),
            }
        }
        Expr::BinaryOp { op, left, right } => {
            let l = eval_with_aggs(left, key_values, agg_values, agg_exprs)?;
            let r = eval_with_aggs(right, key_values, agg_values, agg_exprs)?;
            // Delegate to existing eval_binary via synthetic expr.
            eval(&Expr::BinaryOp {
                op: *op,
                left: Box::new(Expr::Literal(l)),
                right: Box::new(Expr::Literal(r)),
            }, &[])
        }
        Expr::UnaryOp { op, operand } => {
            let v = eval_with_aggs(operand, key_values, agg_values, agg_exprs)?;
            eval(&Expr::UnaryOp { op: *op, operand: Box::new(Expr::Literal(v)) }, &[])
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_with_aggs(expr, key_values, agg_values, agg_exprs)?;
            eval(&Expr::IsNull { expr: Box::new(Expr::Literal(v)), negated: *negated }, &[])
        }
        // For other variants: fall through to standard eval against key_values
        other => eval(other, key_values),
    }
}
```

### Step 8 — `project_grouped_row` and `build_grouped_column_meta`

```
project_grouped_row(items, key_values, agg_values, agg_exprs):
  for each SelectItem::Expr { expr, .. }:
    if contains_aggregate(expr):
      eval_with_aggs(expr, key_values, agg_values, agg_exprs)?
    else:
      eval(expr, key_values)?  // column ref → key_values[col_idx]
  SelectItem::Wildcard → TypeMismatch error (not allowed with GROUP BY)
```

---

## Integration with `execute_select`

The cleanest integration point is to extract "scan + WHERE" into a shared path
for both single-table and join queries, then branch:

```
// At the END of execute_select (after combining single-table or join rows):
let combined_rows: Vec<Row> = /* result of scan + WHERE filter */;

let needs_aggregation = !stmt.group_by.is_empty()
    || has_aggregates(&stmt.columns, &stmt.having);

if needs_aggregation {
    return execute_select_grouped(&stmt, combined_rows);
}

// existing projection path
```

This requires refactoring `execute_select` and `execute_select_with_joins` to
both produce a `Vec<Row>` before the projection step. The common structure:

```
execute_select:
  ... (scan/join/filter) → combined_rows: Vec<Row>
  if needs_aggregation: execute_select_grouped(stmt, combined_rows)
  else: project + Rows
```

---

## Implementation order

1. **Add helpers** (`is_aggregate`, `contains_aggregate`, `has_aggregates`,
   `value_to_key_bytes`, `group_key_bytes`). `cargo check`.

2. **`AggExpr` and `AggAccumulator`** with `new`, `update`, `finalize`.
   Unit test: `CountStar` on 3 rows → 3; `Sum(Int)` with one NULL → non-NULL sum.

3. **`eval_binary_add`** and **`finalize_avg`** — delegating to `eval()`.
   Unit test: add Int+BigInt, Real+Real; avg of 3 ints.

4. **`eval_with_aggs`** — HAVING expression evaluator.
   Unit test: `COUNT(*) > 2` with agg_values=[3] → truthy.

5. **`execute_select_grouped`** — full aggregation path.
   `cargo check -p axiomdb-sql`.

6. **Refactor `execute_select`** to extract `combined_rows` and branch on
   `needs_aggregation`.

7. **Integration tests** — see test list below.

8. **Full check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Unit tests in `executor.rs`

```
test_value_to_key_bytes_null           — Null → [0x00]
test_value_to_key_bytes_int            — Int(42) → [0x02, 42, 0, 0, 0]
test_value_to_key_bytes_text           — Text("ab") → [0x06, 2,0,0,0, 97, 98]
test_group_key_bytes_two_values        — concat of two keys is unique
test_group_key_null_equality           — two Nulls produce same bytes

test_agg_count_star_empty              — 0 rows → BigInt(0)
test_agg_count_star_three              — 3 rows → BigInt(3)
test_agg_count_col_with_nulls          — 3 rows, 1 NULL → BigInt(2)
test_agg_sum_all_null                  — all NULLs → Null
test_agg_sum_mixed                     — (1, NULL, 3) → Int(4)
test_agg_min_basic                     — (3,1,2) → Int(1)
test_agg_max_basic                     — (3,1,2) → Int(3)
test_agg_avg_ints                      — (1,2,3) → Real(2.0)
test_agg_avg_empty                     — 0 non-null rows → Null

test_eval_binary_add_int_bigint        — Int(1) + BigInt(2) → BigInt(3)
test_finalize_avg_zero_count           — count=0 → Null
test_finalize_avg_three_ints           — sum=Int(6), count=3 → Real(2.0)
```

### Integration tests in `integration_executor.rs`

```
test_group_by_count_star
  — SELECT dept, COUNT(*) FROM employees GROUP BY dept
  — 3 employees in 'eng', 2 in 'sales' → [(eng,3), (sales,2)]

test_group_by_multiple_aggs
  — SELECT dept, COUNT(*), SUM(salary), AVG(salary) FROM employees GROUP BY dept

test_group_by_null_key_grouped
  — INSERT 2 rows with dept=NULL, 1 with dept='eng'
  — GROUP BY dept → 2 groups: (NULL,2), (eng,1)

test_ungrouped_count_star
  — SELECT COUNT(*) FROM employees → 1 row: (5,)

test_ungrouped_count_empty_table
  — SELECT COUNT(*) FROM empty_t → 1 row: (0,) (not 0 rows)

test_count_col_skips_null
  — SELECT COUNT(manager_id) FROM employees
  — some rows have NULL manager_id → count < total

test_sum_all_null
  — SELECT SUM(salary) FROM employees WHERE dept = 'nonexistent' → NULL

test_min_max
  — SELECT MIN(salary), MAX(salary) FROM employees

test_having_filter
  — SELECT dept, COUNT(*) FROM employees GROUP BY dept HAVING COUNT(*) > 2
  — only groups with count > 2

test_having_with_sum
  — SELECT dept, SUM(salary) FROM employees GROUP BY dept HAVING SUM(salary) > 10000

test_group_by_join
  — SELECT u.name, COUNT(o.id) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.id, u.name

test_select_star_with_group_by_error
  — SELECT * FROM t GROUP BY id → TypeMismatch

test_group_by_order_not_guaranteed
  — GROUP BY results are in HashMap order (not sorted); tests check set equality
```

---

## Anti-patterns to avoid

- **DO NOT** sort or order the HashMap output. GROUP BY result order is undefined
  in SQL without ORDER BY. Tests must check set membership, not position.
- **DO NOT** put aggregation logic inside `eval()`. Aggregates require multiple
  rows — they are fundamentally different from scalar evaluation. Keep them in
  the executor's `AggAccumulator`.
- **DO NOT** call `eval()` with aggregate-containing expressions in the regular
  scan path. Always route through `eval_with_aggs` when the expression may
  contain aggregates.
- **DO NOT** allocate `value_to_key_bytes` on the hot path unnecessarily.
  The Vec allocation per row is acceptable for Phase 4.9; optimization
  (pre-allocated buffers) is deferred to Phase 14.
- **DO NOT** `unwrap()` anywhere in `src/` code.

---

## Risks

| Risk | Mitigation |
|---|---|
| `Real(f64)` NaN in GROUP BY key produces unexpected grouping | `f64::NAN.to_bits() == f64::NAN.to_bits()` is true in Rust (bit equality), so all NaN values would form one group. Since NaN is forbidden in stored values (codec rejects it), this cannot happen in practice. |
| `AggExpr::arg_matches` for deduplication fails for HAVING expressions | Use canonical form: `(name, first_arg_col_idx)` as the identity key. If col_idx matches, the arg is the same. |
| `eval_with_aggs` column lookup: `key_values[col_idx]` — col_idx was assigned relative to the source row, not the group key | The GROUP BY expressions use the same `col_idx` as the source row. After grouping, we evaluate against `key_values` which is `[eval(group_by_expr_0, source_row), ...]`. If group_by_expr is `Expr::Column { col_idx: 3 }`, then `key_values[0]` is `source_row[3]`. The HAVING column references use the SAME col_idx. When HAVING does `Expr::Column { col_idx: 3 }`, we must evaluate it against the ORIGINAL source row OR find the position of col_idx 3 in the group key. Use full `source_row` for HAVING column eval, not `key_values`. Preserve one row per group for this purpose. |
| GROUP BY key deduplication vs ordering | HashMap provides O(1) lookup but unordered output. Tests must use set comparison. |

**Critical invariant for `eval_with_aggs`:**

The GROUP BY expressions evaluate source columns by `col_idx` (position in the
source row). HAVING expressions reference the same columns by the same `col_idx`.
To evaluate `HAVING dept = 'eng'` where `dept` has `col_idx = 2`:
- We need `source_row[2]`, not `key_values[position_of_dept_in_group_by]`.

**Fix:** Store `representative_row: Row` in `GroupState` — one source row from
the group (the first row that matched). Use `representative_row` for column
refs in `eval_with_aggs`, and `agg_values` for aggregate calls.

Updated `GroupState`:
```rust
struct GroupState {
    key_values: Vec<Value>,       // evaluated GROUP BY expressions
    representative_row: Row,      // first source row from this group (for HAVING col refs)
    accumulators: Vec<AggAccumulator>,
}
```

`eval_with_aggs` evaluates `Expr::Column` against `representative_row`, not `key_values`.
