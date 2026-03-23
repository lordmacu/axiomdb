# Spec: 4.9a + 4.9c + 4.9d — GROUP BY (Hash) + Aggregate Functions + HAVING

## Scope

This spec covers three interdependent subfases implemented as a single unit:

- **4.9a** — GROUP BY hash-based execution
- **4.9c** — Aggregate functions: `COUNT(*)`, `COUNT(col)`, `SUM`, `MIN`, `MAX`, `AVG`
- **4.9d** — `HAVING` clause: filter groups after aggregation

**4.9b** (sort-based GROUP BY) is deferred — it is an optimization for pre-sorted
data, not new functionality.

---

## What the parser produces

From inspection of `parser/expr.rs`:

```
COUNT(*)      → Expr::Function { name: "count", args: [] }
COUNT(col)    → Expr::Function { name: "count", args: [Expr::Column { col_idx, name }] }
SUM(col)      → Expr::Function { name: "sum",   args: [col_expr] }
MIN(col)      → Expr::Function { name: "min",   args: [col_expr] }
MAX(col)      → Expr::Function { name: "max",   args: [col_expr] }
AVG(col)      → Expr::Function { name: "avg",   args: [col_expr] }
```

---

## Trigger condition

Run the aggregation path when any of the following is true:
- `stmt.group_by` is non-empty, OR
- Any `SelectItem::Expr { expr }` in the SELECT list contains an aggregate
  function call (detected by `contains_aggregate(expr)`), OR
- `stmt.having` is `Some(_)` (implies grouping)

**`contains_aggregate(expr: &Expr) -> bool`:** recursively checks if any
sub-expression is `Expr::Function { name }` where `is_aggregate(name)`.

**`is_aggregate(name: &str) -> bool`:**
```
matches!(name, "count" | "sum" | "min" | "max" | "avg")
```

---

## GROUP BY key hashing

`Value` contains `f64` (`Value::Real`) which does not implement `Hash`. Instead
of using the row codec (which requires `DataType` schema), use a custom
**self-describing byte serialization** for GROUP BY keys:

```
value_to_key_bytes(v: &Value) → Vec<u8>:
  Null         → [0x00]
  Bool(false)  → [0x01, 0x00]
  Bool(true)   → [0x01, 0x01]
  Int(n)       → [0x02, n.to_le_bytes()...]     (4 bytes)
  BigInt(n)    → [0x03, n.to_le_bytes()...]     (8 bytes)
  Real(f)      → [0x04, f.to_bits().to_le_bytes()...]  (8 bytes, bit-exact)
  Decimal(m,s) → [0x05, m.to_le_bytes()..., s]
  Text(s)      → [0x06, (s.len() as u32).to_le_bytes()..., s.as_bytes()...]
  Bytes(b)     → [0x07, (b.len() as u32).to_le_bytes()..., b...]
  Date(d)      → [0x08, d.to_le_bytes()...]
  Timestamp(t) → [0x09, t.to_le_bytes()...]
  Uuid(u)      → [0x0A, u...]                   (16 bytes)
```

**Group key for N GROUP BY expressions:**
```
key_bytes = concat(value_to_key_bytes(v0), value_to_key_bytes(v1), ...)
```

**NULL semantics for grouping:** Two `NULL` values produce the same byte
`[0x00]`, so they form one group — which is the SQL standard behavior for
GROUP BY (unlike equality comparison where `NULL != NULL`).

**Real NaN:** `NaN` values produce distinct bytes via `to_bits()` but NaN is
forbidden in stored values (the row codec rejects them), so NaN keys cannot
appear in practice.

---

## Aggregate accumulators

One accumulator per aggregate expression in the SELECT list and HAVING clause:

```rust
enum AggAccumulator {
    /// COUNT(*) — counts every row regardless of NULLs.
    CountStar { n: u64 },

    /// COUNT(col) — counts only non-NULL values.
    CountCol { n: u64 },

    /// SUM(col) — sum of non-NULL values. None = all values were NULL.
    Sum { acc: Option<Value> },

    /// MIN(col) — minimum non-NULL value.
    Min { acc: Option<Value> },

    /// MAX(col) — maximum non-NULL value.
    Max { acc: Option<Value> },

    /// AVG(col) — running sum + count; final = sum / count.
    /// Uses Decimal arithmetic to avoid float precision loss for INT/BIGINT columns.
    Avg { sum: Value, count: u64 },
}
```

### Accumulator update rules

#### `CountStar` — increment always
```
CountStar.update(_row) → n += 1
```

#### `CountCol { arg_expr }` — increment only for non-NULL
```
CountCol.update(row):
  v = eval(arg_expr, row)
  if !matches!(v, Value::Null): n += 1
```

#### `Sum { arg_expr }` — skip NULL, use coerce_for_op for type promotion
```
Sum.update(row):
  v = eval(arg_expr, row)
  if v == Null: skip
  if acc == None: acc = Some(v)
  else: acc = Some(eval_binary(Add, acc.unwrap(), v)?)
```

#### `Min / Max { arg_expr }` — track extremum, skip NULL
```
Min.update(row):
  v = eval(arg_expr, row)
  if v == Null: skip
  if acc == None: acc = Some(v)
  else if compare(v, acc.unwrap()) == Less: acc = Some(v)

Max: same but keep Greater
```

#### `Avg { arg_expr }` — accumulate sum + count
```
Avg.update(row):
  v = eval(arg_expr, row)
  if v == Null: skip
  sum = eval_binary(Add, sum, v)?
  count += 1
```

### Accumulator finalization

```
CountStar.finalize() → Value::BigInt(n)
CountCol.finalize()  → Value::BigInt(n)
Sum.finalize()       → acc.unwrap_or(Value::Null)
Min.finalize()       → acc.unwrap_or(Value::Null)
Max.finalize()       → acc.unwrap_or(Value::Null)
Avg.finalize():
  if count == 0: Value::Null
  else: eval_binary(Div, sum, Value::BigInt(count))?
        (Integer division produces Int/BigInt; promote to Real for int types)
```

**AVG type rule:**
- `AVG(INT col)` → result is `REAL` (SQL standard)
- `AVG(REAL col)` → `REAL`
- `AVG(DECIMAL col)` → `DECIMAL`

---

## GroupState

One `GroupState` per unique GROUP BY key in the hash map:

```rust
struct GroupState {
    /// Evaluated GROUP BY key values (preserved for projection).
    key_values: Vec<Value>,
    /// One accumulator per aggregate in the query (SELECT + HAVING aggregates).
    accumulators: Vec<AggAccumulator>,
}
```

The `HashMap<Vec<u8>, GroupState>` uses the key bytes described above.

---

## Aggregate index mapping

Before the scan loop, build a list of all aggregate expressions in the query:

```rust
struct AggExpr {
    /// Name of the function, e.g. "sum".
    name: String,
    /// The argument expression (empty for COUNT(*)).
    arg: Option<Expr>,
    /// Position in the `GroupState::accumulators` vector.
    agg_idx: usize,
}
```

Collect all aggregate expressions from:
1. The SELECT list (each `SelectItem::Expr { expr }` that contains an aggregate)
2. The HAVING clause (if present)

**Deduplication:** If the same aggregate appears in both SELECT and HAVING
(e.g., `SELECT COUNT(*) ... HAVING COUNT(*) > 5`), reuse the same accumulator.
Two aggregate expressions are "the same" if they have the same name and the same
argument `col_idx` (or both are COUNT(*)).

---

## Algorithm

```
fn execute_select_grouped(stmt, all_rows_after_join_and_where):

  // 1. Build list of aggregate expressions.
  let agg_exprs: Vec<AggExpr> = collect_agg_exprs(&stmt.columns, &stmt.having);

  // 2. Hash-aggregate loop.
  let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

  for row in all_rows_after_join_and_where:
    // Evaluate GROUP BY key expressions.
    let key_values: Vec<Value> = stmt.group_by.iter()
      .map(|expr| eval(expr, &row))
      .collect::<Result<_, _>>()?;

    // Serialize key.
    let key_bytes: Vec<u8> = key_values.iter().flat_map(value_to_key_bytes).collect();

    // Look up or create GroupState.
    let state = groups.entry(key_bytes).or_insert_with(|| {
      GroupState {
        key_values: key_values.clone(),
        accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
      }
    });
    // Update each accumulator.
    for (acc, agg_expr) in state.accumulators.iter_mut().zip(&agg_exprs) {
      acc.update(&row, &agg_expr)?;
    }

  // Special case: ungrouped aggregate (no GROUP BY).
  // If no rows exist, return one row with empty accumulators (COUNT=0, SUM/MIN/MAX=NULL).
  if stmt.group_by.is_empty() && groups.is_empty() {
    groups.insert(vec![], GroupState::empty(&agg_exprs));
  }

  // 3. Finalize and project.
  let out_cols = build_grouped_column_meta(&stmt.columns, &stmt.group_by, &agg_exprs);
  let mut rows = Vec::new();

  for (_, state) in &groups:
    // Finalize all accumulators.
    let agg_values: Vec<Value> = state.accumulators.iter().map(|a| a.finalize()).collect();

    // HAVING filter.
    if let Some(ref having) = stmt.having:
      let having_result = eval_with_aggs(having, &state.key_values, &agg_values, &agg_exprs)?;
      if !is_truthy(&having_result): continue;

    // Project SELECT list.
    let out_row = project_grouped_row(&stmt.columns, &state.key_values, &agg_values, &agg_exprs)?;
    rows.push(out_row);

  Ok(QueryResult::Rows { columns: out_cols, rows })
```

---

## `eval_with_aggs` — HAVING evaluation

The HAVING expression may contain `Expr::Function` aggregate calls. Standard
`eval()` returns `NotImplemented` for function calls. A wrapper intercepts them:

```rust
fn eval_with_aggs(
    expr: &Expr,
    key_values: &[Value],         // GROUP BY key values (for column refs)
    agg_values: &[Value],         // finalized aggregate values in agg_exprs order
    agg_exprs: &[AggExpr],        // aggregate registry to look up by (name, arg)
) -> Result<Value, DbError>
```

Implementation: recursive tree walk, same as `eval()`, but the `Expr::Function`
arm looks up the aggregate value:
```
Expr::Function { name, args } if is_aggregate(name):
  agg_idx = agg_exprs.iter().position(|ae| ae.matches(name, args))
             .ok_or(/* aggregate not pre-registered */)?
  Ok(agg_values[agg_idx].clone())
Expr::Function { .. }:
  // Non-aggregate function — forward to standard eval
  Err(NotImplemented { feature: "scalar functions — Phase 4.19" })
```

For column references in HAVING (`Expr::Column { col_idx }`): evaluate against
`key_values` (which holds the GROUP BY key columns in declaration order). This
works because the analyzer resolved `col_idx` for GROUP BY columns — they point
into the source row. **Limitation:** HAVING can only reference GROUP BY columns
and aggregate results. Non-GROUP BY column references in HAVING are a semantic
error (caught by the analyzer in Phase 4.18 if ONLY_FULL_GROUP_BY is enforced;
for Phase 4.9 we allow them to evaluate against the first row of the group).

---

## Ungrouped aggregates

`SELECT COUNT(*), SUM(salary) FROM employees` (no GROUP BY):

- Treat the entire table as one group with an empty key `[]`.
- If the table is empty, still emit one output row with `COUNT=0`, `SUM=NULL`, etc.
- This is the SQL standard behavior (a `SELECT COUNT(*) FROM t` on an empty table returns 1 row with count=0, not 0 rows).

---

## Output schema for `SELECT *` with GROUP BY

**Disallowed.** `SELECT * FROM t GROUP BY dept` is invalid because `*` expands
to columns that are not in the GROUP BY clause. Return:
```
Err(DbError::TypeMismatch {
  expected: "column in GROUP BY or aggregate function".into(),
  got: "SELECT * (wildcard) with GROUP BY".into(),
})
```

---

## Column metadata for grouped output

For each SELECT item:
- `GroupByColumn` → `ColumnMeta { name: col.name, data_type: catalog type, nullable: col.nullable }`
- `CountStar` aggregate → `ColumnMeta { name: "count(*)", data_type: DataType::BigInt, nullable: false }`
- `CountCol` aggregate → `ColumnMeta { name: "count(col)", data_type: DataType::BigInt, nullable: false }`
- `Sum(col)` → `ColumnMeta { name: "sum(col)", data_type: same as col, nullable: true }` (NULL if no rows)
- `Min/Max(col)` → same type, nullable: true
- `Avg(col)` → `ColumnMeta { name: "avg(col)", data_type: DataType::Real, nullable: true }`

If a `SelectItem::Expr` has an alias (`expr AS alias`), use that alias as the
name.

---

## Use cases

### 1. Basic GROUP BY + COUNT

```sql
SELECT dept, COUNT(*) FROM employees GROUP BY dept;
```

Groups: `{sales: 10, eng: 5, ...}` → 2 output rows.

### 2. Multiple aggregates

```sql
SELECT dept, COUNT(*), SUM(salary), AVG(salary)
FROM employees
GROUP BY dept;
```

### 3. HAVING filter

```sql
SELECT dept, COUNT(*) FROM employees GROUP BY dept HAVING COUNT(*) > 5;
```

Only departments with more than 5 employees.

### 4. Ungrouped aggregate

```sql
SELECT COUNT(*), MAX(salary) FROM employees;
```

Returns 1 row regardless of table size. If empty: `(0, NULL)`.

### 5. COUNT on empty group for non-NULL

```sql
SELECT dept, COUNT(manager_id) FROM employees GROUP BY dept;
```

If all employees in engineering have NULL manager_id: returns `(eng, 0)`.

### 6. GROUP BY + JOIN

```sql
SELECT u.name, COUNT(o.id)
FROM users u
LEFT JOIN orders o ON u.id = o.user_id
GROUP BY u.id, u.name;
```

GROUP BY after JOIN — operates on the combined row.

---

## Acceptance criteria

- [ ] `GROUP BY col` groups rows by distinct key values
- [ ] `COUNT(*)` counts every row including those with NULLs
- [ ] `COUNT(col)` counts only non-NULL values
- [ ] `SUM(col)` returns NULL when all values in the group are NULL
- [ ] `MIN` / `MAX` skip NULLs; return NULL when all values are NULL
- [ ] `AVG(col)` returns Real for Int columns; NULL for empty groups
- [ ] NULL keys are grouped together (not compared with `=`)
- [ ] Ungrouped aggregate (`SELECT COUNT(*)` without GROUP BY) returns 1 row
- [ ] `SELECT COUNT(*)` on empty table returns `(0)` — not 0 rows
- [ ] `HAVING expr` filters groups after aggregation
- [ ] `HAVING COUNT(*) > N` works correctly
- [ ] GROUP BY + JOIN works on the combined row
- [ ] `SELECT *` with GROUP BY returns `TypeMismatch` error
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- **4.9b** sort-based GROUP BY (optimization for index-sorted data — deferred)
- `COUNT DISTINCT` (deferred — requires a `HashSet<ValueKey>` per group)
- Window functions (Phase 8)
- `ROLLUP` / `CUBE` / `GROUPING SETS` (future)
- Streaming / lazy aggregation (Phase 14)
- Group-by on non-column expressions (e.g., `GROUP BY YEAR(created_at)`) —
  works mechanically but requires scalar function support (Phase 4.19)

---

## Dependencies

- `axiomdb-sql/src/executor.rs` — only file modified
- No new crates, no new dependencies
- `eval()` from `axiomdb-sql/src/eval.rs` — already available, used as-is
- `encode_row` / `decode_row` — NOT used (we use `value_to_key_bytes` instead)
