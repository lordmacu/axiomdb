# Spec: 4.8 — JOIN (Nested Loop)

## What to build (not how)

Extend `execute_select` to support INNER JOIN, LEFT JOIN, RIGHT JOIN, and
CROSS JOIN using a nested loop strategy. Multiple JOINs in the same query
(`FROM a JOIN b ON ... JOIN c ON ...`) must be supported.

The analyzer (Phase 4.18) already builds the combined row layout and resolves
all `col_idx` values for JOINs. The executor simply needs to construct the
combined row `[left_values || right_values]` correctly and apply the join
condition against it.

---

## Combined row layout (already handled by analyzer)

For `FROM users u JOIN orders o ON u.id = o.user_id`:

```
Table      col_offset  Columns
users      0           id=0  name=1  age=2  email=3
orders     4           id=4  user_id=5  total=6  status=7

Combined row: [u.id, u.name, u.age, u.email, o.id, o.user_id, o.total, o.status]
```

The `col_idx` in every `Expr::Column` in the analyzed statement already points
to the correct position. The executor only needs to build the combined row
correctly; expressions evaluate against it without any additional mapping.

---

## Inputs / Outputs

### `execute_select` — updated contract

When `stmt.joins` is non-empty, the executor:

1. Resolves all tables: the FROM table and every JOIN table.
2. Scans each table once under the current transaction snapshot.
3. Builds the combined rows using nested-loop join (progressive for multiple joins).
4. Applies the WHERE clause against the combined rows.
5. Projects the SELECT list.
6. Returns `QueryResult::Rows`.

FULL OUTER JOIN (`JoinType::Full`) is not in scope — return
`DbError::NotImplemented { feature: "FULL OUTER JOIN — Phase 4.8+" }`.

### Combined row

For a query with N joined tables, the combined row has:
`col_count(table_0) + col_count(table_1) + ... + col_count(table_N)` values.

The `col_idx` assigned by the analyzer is the offset within this combined row.

---

## Join semantics per type

### INNER JOIN

Emit a combined row for every `(left_row, right_row)` pair where the ON
condition evaluates to truthy:

```
for each left_row in left:
  for each right_row in right:
    combined = left_row ++ right_row
    if is_truthy(eval(ON_expr, &combined)):
      emit combined
```

### CROSS JOIN

Emit all `(left_row, right_row)` combinations with no condition check.
`CROSS JOIN` in SQL has no `ON` clause; the parser produces
`JoinCondition::On(Expr::Literal(Value::Bool(true)))` or an equivalent.

```
for each left_row in left:
  for each right_row in right:
    emit left_row ++ right_row
```

### LEFT JOIN

Emit every left row at least once. If no right row matches the ON condition,
emit the left row with NULLs padded to the right:

```
for each left_row in left:
  matched = false
  for each right_row in right:
    combined = left_row ++ right_row
    if is_truthy(eval(ON_expr, &combined)):
      emit combined
      matched = true
  if !matched:
    emit left_row ++ [Null; right_col_count]
```

### RIGHT JOIN

Emit every right row at least once. If no left row matches, emit NULLs on the
left side:

```
matched_right = vec![false; right_rows.len()]
for each left_row in left:
  for (i, right_row) in right.enumerate():
    combined = left_row ++ right_row
    if is_truthy(eval(ON_expr, &combined)):
      emit combined
      matched_right[i] = true

for (i, right_row) in right.enumerate():
  if !matched_right[i]:
    emit [Null; left_col_count] ++ right_row
```

### FULL OUTER JOIN

```
return Err(DbError::NotImplemented {
    feature: "FULL OUTER JOIN — Phase 4.8+".into(),
});
```

---

## `JoinCondition::Using` handling

`USING(col1, col2)` is equivalent to `ON left.col1 = right.col1 AND left.col2 = right.col2`.

At execution time, for each column name in `Using(names)`:
1. Find its `col_idx` in the **left side** of the combined row by scanning the
   left table's `ColumnDef` list and adding the left table's col_offset.
2. Find its `col_idx` in the **right side** by scanning the right table's
   `ColumnDef` list and adding the right table's col_offset.
3. Evaluate `combined[left_idx] == combined[right_idx]` using `eval_binary(Eq, ...)`.
4. AND all equality results together; the result determines whether the row passes.

If a column named in USING does not exist in one of the tables, return
`DbError::ColumnNotFound`.

---

## Multiple JOINs — progressive accumulation

For `FROM a JOIN b ON ... JOIN c ON ...`:

```
stage = scan(a)                 → Vec<Row> with a's columns
stage = apply_join(stage, scan(b), INNER, ON_ab, a_cols, b_cols)
         → Vec<Row> with a's + b's columns
stage = apply_join(stage, scan(c), LEFT, ON_ac, (a+b)_cols, c_cols)
         → Vec<Row> with a's + b's + c's columns
```

Each call to `apply_join` takes the accumulated combined rows as `left_rows`
and a freshly-scanned right table as `right_rows`. The `left_col_count`
grows with each join (it is the total number of columns accumulated so far).

**Key property:** Because the analyzer already computed `col_idx` for the
final combined row, no remapping is needed at any stage. The combined row
after all joins is exactly what the expressions expect.

---

## Column metadata for `SELECT *` with JOINs

For `SELECT * FROM users u JOIN orders o ON ...`:

The output columns are the union of all table columns in FROM + JOIN order.
For each table (in join order), append one `ColumnMeta` per catalog column:

```rust
ColumnMeta {
    name:       col.name.clone(),
    data_type:  column_type_to_datatype(col.col_type),
    nullable:   col.nullable || is_outer_join_nullable(join_type),
    table_name: Some(table_def.table_name.clone()),
}
```

**Nullability for outer joins:**
- For `LEFT JOIN`: all columns from the **right** table become nullable (they
  can be NULL when no match is found).
- For `RIGHT JOIN`: all columns from the **left** table become nullable.
- For `INNER JOIN` and `CROSS JOIN`: nullability matches the catalog definition.

`QualifiedWildcard("u")` expands to only the columns from table with alias/name `u`.

---

## Table resolution with JOINs

For `FROM a JOIN b ON ...`:

```
// Resolve FROM table
left_def = resolver.resolve_table("a")?
left_cols = left_def.columns

// Resolve each JOIN table in order
for join in stmt.joins:
  right_def = resolver.resolve_table(join.table_name)?
  right_cols = right_def.columns
```

The `col_offset` for each table is the running sum of all preceding column
counts:

```
tables[0].col_offset = 0
tables[1].col_offset = tables[0].columns.len()
tables[2].col_offset = tables[0].columns.len() + tables[1].columns.len()
...
```

This is used for `USING` condition evaluation.

---

## Use cases

### 1. INNER JOIN — matching rows only

```sql
SELECT u.name, o.total
FROM users u
JOIN orders o ON u.id = o.user_id
WHERE o.total > 100;
```

Combined row: `[u.id, u.name, u.age, u.email, o.id, o.user_id, o.total, o.status]`
- `u.name` → `col_idx=1`, `o.total` → `col_idx=6`
- Only emit where `combined[0] == combined[5]` (ON) and `combined[6] > 100` (WHERE)

### 2. LEFT JOIN — all left rows preserved

```sql
SELECT u.name, o.total
FROM users u
LEFT JOIN orders o ON u.id = o.user_id;
```

Users without orders get `o.total = NULL`.

### 3. CROSS JOIN — cartesian product

```sql
SELECT a.id, b.id FROM a CROSS JOIN b;
```

If `a` has 3 rows and `b` has 4 rows, result has 12 rows.

### 4. Three-table JOIN

```sql
SELECT u.name, o.total, p.name AS product
FROM users u
JOIN orders o ON u.id = o.user_id
JOIN products p ON o.product_id = p.id;
```

Stage 1: `users ⨝ orders` → combined row of 8 columns
Stage 2: `(users ⨝ orders) ⨝ products` → combined row of 11 columns

### 5. JOIN with USING

```sql
SELECT * FROM employees e JOIN departments d USING (dept_id);
```

Equivalent to `ON e.dept_id = d.dept_id`.

### 6. RIGHT JOIN — unmatched right rows preserved

```sql
SELECT u.name, o.total
FROM users u
RIGHT JOIN orders o ON u.id = o.user_id;
```

Orders without a matching user get `u.name = NULL`.

---

## Acceptance criteria

- [ ] INNER JOIN returns only rows where ON condition is truthy
- [ ] LEFT JOIN returns all left rows; unmatched have NULLs on the right
- [ ] RIGHT JOIN returns all right rows; unmatched have NULLs on the left
- [ ] CROSS JOIN returns all left × right combinations
- [ ] FULL OUTER JOIN returns `NotImplemented`
- [ ] WHERE clause applied after join (against combined row)
- [ ] `SELECT *` with JOIN returns all columns from all tables in order
- [ ] `SELECT u.name, o.total` (qualified) resolves to correct combined row positions
- [ ] Multiple JOINs (`A JOIN B JOIN C`) work correctly
- [ ] `USING(col)` generates the correct equality condition
- [ ] Outer-join nullable columns are marked `nullable: true` in `ColumnMeta`
- [ ] Integration tests for all 4 join types
- [ ] Integration test for 3-table join
- [ ] Integration test for join + WHERE filter
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- FULL OUTER JOIN (Phase 4.8+)
- Subqueries in JOIN `ON` or in the `JOIN table` position (Phase 4.11)
- Hash join or sort-merge join (Phase 6 / query planner)
- Index-nested-loop join (Phase 6)
- JOIN with no `ON` or `USING` (syntactic — parser handles this as CROSS JOIN)
- Self-join optimization (just works via nested loop)

---

## Dependencies

- `axiomdb-sql/src/executor.rs` — only file modified
- No new crates, no new dependencies
