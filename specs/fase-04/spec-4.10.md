# Spec: 4.10 + 4.10b + 4.10c — ORDER BY + LIMIT/OFFSET

## Scope

Three subfases implemented as a single unit:

- **4.10** — `ORDER BY expr [ASC|DESC]` + `LIMIT n [OFFSET m]`
- **4.10b** — Multi-column ORDER BY with mixed direction: `ORDER BY a ASC, b DESC`
- **4.10c** — `NULLS FIRST` / `NULLS LAST` with correct defaults

**4.10d** (parameterized `LIMIT $1 OFFSET $2`) requires prepared statements — deferred to Phase 5.

---

## SQL pipeline position

ORDER BY runs after all prior stages; LIMIT/OFFSET runs after ORDER BY:

```
scan → join → WHERE → GROUP BY → HAVING → ORDER BY → LIMIT/OFFSET → PROJECT → return
```

**Exception for GROUP BY:** When GROUP BY is present, the rows are projected
inside `execute_select_grouped`. ORDER BY and LIMIT/OFFSET are applied to the
projected output rows returned by that function.

---

## ORDER BY application points

### Without GROUP BY

Order the **source rows** (post-WHERE, pre-projection). Evaluating ORDER BY
expressions against source rows guarantees `col_idx` values (set by the analyzer)
are valid — they point to positions in the original combined row.

```
combined_rows = scan + join + WHERE filter
if !order_by.is_empty():
    sort combined_rows using order_by expressions evaluated against combined_rows
out_cols = build_column_meta(...)
rows = project(combined_rows)     // LIMIT/OFFSET applied here after projection
return Rows
```

### With GROUP BY

`execute_select_grouped` returns projected output rows. ORDER BY is applied to
those projected rows. ORDER BY `col_idx` references must map to output column
positions. This works correctly when ORDER BY references GROUP BY columns that
appear in the SELECT list at predictable positions.

**Limitation (documented):** ORDER BY on aggregate expressions (e.g.,
`ORDER BY SUM(salary)`) after GROUP BY requires the query planner to map
aggregate expressions to output column positions. This is deferred to Phase 6.
For Phase 4.10, ORDER BY on aggregate expressions returns
`DbError::NotImplemented` when `col_idx` would be out of bounds for the output
row.

---

## NULL ordering

SQL does not mandate NULL ordering; the standard leaves it implementation-defined.

**AxiomDB defaults (PostgreSQL-compatible):**
- `ORDER BY col ASC` → `NULLS LAST` (NULLs appear after non-NULL values)
- `ORDER BY col DESC` → `NULLS FIRST` (NULLs appear before non-NULL values)

**Explicit override:** `ORDER BY col ASC NULLS FIRST` or `ORDER BY col DESC NULLS LAST`
overrides the default.

**Rationale:** PostgreSQL's default is more predictable for applications
(NULLs always "outside" the sorted range for the given direction). MySQL's
reversed default is a common source of bugs.

---

## Core comparator

```rust
/// Compare two source rows `a` and `b` using the ORDER BY items.
///
/// Returns `Ordering::Less` if `a` should appear before `b` in the result.
/// Evaluates each ORDER BY expression left-to-right, stopping at the first
/// non-Equal result (stable multi-key sort).
fn compare_rows_for_sort(
    a: &[Value],
    b: &[Value],
    order_items: &[OrderByItem],
) -> Result<std::cmp::Ordering, DbError>
```

### Per-item comparison

```
compare_sort_values(a: &Value, b: &Value, direction: SortOrder, nulls: Option<NullsOrder>)
  → std::cmp::Ordering
```

```
// Determine NULL position for this direction.
nulls_first = match (direction, nulls):
  (_, Some(NullsOrder::First)) => true
  (_, Some(NullsOrder::Last))  => false
  (SortOrder::Asc,  None)      => false   // default: NULLs LAST for ASC
  (SortOrder::Desc, None)      => true    // default: NULLs FIRST for DESC

// Apply NULL ordering.
match (a, b):
  (Value::Null, Value::Null) => Equal
  (Value::Null, _)   => if nulls_first { Less } else { Greater }
  (_, Value::Null)   => if nulls_first { Greater } else { Less }
  (a, b) =>
    // Compare non-NULL values using existing comparison logic.
    let ord = compare_non_null_values(a, b)?;
    // Flip if DESC.
    if direction == SortOrder::Desc { ord.reverse() } else { ord }
```

**`compare_non_null_values`** delegates to the same value comparison used in
`eval_binary(Eq, Lt, ...)` — reuses existing comparison for all Value types.

---

## LIMIT / OFFSET

`stmt.limit: Option<Expr>` and `stmt.offset: Option<Expr>` are evaluated as
scalar expressions against an empty row `&[]` (they are literals or constants,
not column references).

```
limit_n: Option<usize> = stmt.limit
    .as_ref()
    .map(|e| eval_as_usize(e))
    .transpose()?;

offset_n: usize = stmt.offset
    .as_ref()
    .map(|e| eval_as_usize(e))
    .transpose()?
    .unwrap_or(0);
```

```
fn eval_as_usize(expr: &Expr) -> Result<usize, DbError>:
  match eval(expr, &[])?:
    Value::Int(n) if n >= 0  → Ok(n as usize)
    Value::BigInt(n) if n >= 0 → Ok(n as usize)
    Value::Int(n) | Value::BigInt(n) → Err(TypeMismatch { expected: "non-negative integer" })
    other → Err(TypeMismatch { expected: "integer", got: other.variant_name() })
```

Applied after ORDER BY:
```
let rows = rows.into_iter()
    .skip(offset_n)
    .take(limit_n.unwrap_or(usize::MAX))
    .collect::<Vec<_>>();
```

---

## Inputs / Outputs

| Input | Output |
|---|---|
| `stmt.order_by: Vec<OrderByItem>` | rows sorted by the composite key |
| `stmt.limit: Option<Expr>` | at most N rows returned |
| `stmt.offset: Option<Expr>` | first M rows skipped |

`ORDER BY` and `LIMIT/OFFSET` are transparent to `QueryResult::Rows` — the
columns are unchanged; only the rows are reordered and/or truncated.

---

## Use cases

### 1. Single-column ASC

```sql
SELECT name FROM users ORDER BY name ASC;
```

### 2. Single-column DESC with NULLs

```sql
SELECT name, score FROM players ORDER BY score DESC;
-- NULLs appear first (DESC + NULLS FIRST default)
```

### 3. Multi-column mixed direction

```sql
SELECT dept, salary FROM employees ORDER BY dept ASC, salary DESC;
```
Primary key: `dept` ascending. Within same dept: `salary` descending.

### 4. Explicit NULLS LAST for DESC

```sql
SELECT name, score FROM players ORDER BY score DESC NULLS LAST;
-- Override: NULLs appear after all non-NULL scores
```

### 5. LIMIT + OFFSET (pagination)

```sql
SELECT id, name FROM users ORDER BY id ASC LIMIT 10 OFFSET 20;
-- Returns rows 21-30 in ascending ID order
```

### 6. LIMIT without ORDER BY

```sql
SELECT * FROM users LIMIT 5;
-- Returns first 5 rows in undefined scan order (no sort)
```

### 7. ORDER BY + GROUP BY

```sql
SELECT dept, COUNT(*) FROM employees GROUP BY dept ORDER BY dept ASC;
-- Works: dept is a GROUP BY column, col_idx maps correctly
```

---

## Acceptance criteria

- [ ] `ORDER BY col ASC` sorts rows ascending by the column
- [ ] `ORDER BY col DESC` sorts rows descending
- [ ] Multi-column `ORDER BY a, b` uses `a` as primary, `b` as secondary key
- [ ] Mixed direction `ORDER BY a ASC, b DESC` works correctly
- [ ] `ASC` default: NULLs sort last (after all non-NULLs)
- [ ] `DESC` default: NULLs sort first (before all non-NULLs)
- [ ] `NULLS FIRST` explicit: NULLs before non-NULLs regardless of direction
- [ ] `NULLS LAST` explicit: NULLs after non-NULLs regardless of direction
- [ ] `LIMIT n` returns at most n rows
- [ ] `LIMIT n OFFSET m` skips m rows then returns at most n
- [ ] `LIMIT 0` returns 0 rows
- [ ] `OFFSET n` where n >= total rows returns 0 rows
- [ ] Negative LIMIT/OFFSET → `DbError::TypeMismatch`
- [ ] Non-integer LIMIT/OFFSET → `DbError::TypeMismatch`
- [ ] ORDER BY without LIMIT works (no pagination)
- [ ] LIMIT without ORDER BY works (arbitrary row order, no crash)
- [ ] ORDER BY + GROUP BY (column ref): correct
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- Parameterized `LIMIT $1 OFFSET $2` (Phase 5 / 4.10d)
- ORDER BY on aggregate expressions with GROUP BY (Phase 6)
- Stable sort guarantee for equal keys (Rust's `sort_unstable_by` is sufficient;
  stable sort is `sort_by` if needed)
- External sort for large datasets (in-memory only; streaming sort is Phase 14)

---

## Dependencies

- `axiomdb-sql/src/executor.rs` — only file modified
- No new crates, no new dependencies
