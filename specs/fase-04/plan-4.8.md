# Plan: 4.8 — JOIN (Nested Loop)

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | modify | Add `execute_select_with_joins`, `apply_join`, JOIN helpers |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add JOIN integration tests |

One file modified, one file extended. No new crates.

---

## Algorithm / Data structures

### Step 1 — Update `execute_select` dispatch

Remove the early `return Err(NotImplemented)` for non-empty `stmt.joins` and
dispatch to the appropriate path:

```rust
fn execute_select(...):
  // guards: GROUP BY, ORDER BY, LIMIT, DISTINCT remain
  match stmt.from:
    None => ... // unchanged
    Some(Subquery) => NotImplemented // unchanged
    Some(Table(table_ref)) => {
      if stmt.joins.is_empty() {
        execute_select_single_table(stmt, table_ref, storage, txn)  // existing logic
      } else {
        execute_select_with_joins(stmt, table_ref, storage, txn)    // NEW
      }
    }
```

Extract the existing single-table body into `execute_select_single_table` (or
keep it inline and just add the new branch) — inline is fine to avoid
over-engineering.

### Step 2 — `execute_select_with_joins`

```
fn execute_select_with_joins(stmt, from_table_ref, storage, txn):

  // 1. Resolve all tables and compute col_offsets.
  let mut all_tables: Vec<ResolvedTable> = Vec::new();
  let mut col_offsets: Vec<usize> = Vec::new();
  let mut running_offset = 0;

  // FROM table
  let from_resolved = resolve(storage, txn, from_table_ref);
  col_offsets.push(running_offset);
  running_offset += from_resolved.columns.len();
  all_tables.push(from_resolved);

  // Each JOIN table
  for join in &stmt.joins:
    if let FromClause::Table(tref) = &join.table:
      let resolved = resolve(storage, txn, tref);
      col_offsets.push(running_offset);
      running_offset += resolved.columns.len();
      all_tables.push(resolved);
    else:
      return Err(NotImplemented("subquery in JOIN — Phase 4.11"))

  // 2. Pre-scan all tables once (snapshot taken once; all scans consistent).
  let snap = txn.active_snapshot()?;
  let mut scanned: Vec<Vec<Row>> = Vec::new();
  for t in &all_tables:
    scanned.push(scan_rows(storage, t, snap)?);

  // 3. Build combined rows using progressive nested-loop.
  // Start with FROM rows.
  let mut combined_rows: Vec<Row> = scanned[0].clone();
  let mut left_col_count = all_tables[0].columns.len();

  for (i, join) in stmt.joins.iter().enumerate():
    let right_rows = &scanned[i + 1];
    let right_col_count = all_tables[i + 1].columns.len();
    let right_col_offset = col_offsets[i + 1];

    if join.join_type == JoinType::Full:
      return Err(NotImplemented("FULL OUTER JOIN — Phase 4.8+"))

    combined_rows = apply_join(
      combined_rows,
      right_rows,
      left_col_count,
      right_col_count,
      join.join_type,
      &join.condition,
      right_col_offset,
      &all_tables[i + 1].columns,
    )?;
    left_col_count += right_col_count;

  // 4. Apply WHERE filter against the full combined row.
  if let Some(wc) = stmt.where_clause:
    combined_rows.retain(|row| {
      eval(&wc, row).map(|v| is_truthy(&v)).unwrap_or(false)
    });

  // 5. Build ColumnMeta for output.
  let out_cols = build_join_column_meta(&stmt.columns, &all_tables, &stmt.joins)?;

  // 6. Project each combined row.
  let rows = combined_rows.iter()
    .map(|r| project_row(&stmt.columns, r))
    .collect::<Result<Vec<_>, _>>()?;

  Ok(QueryResult::Rows { columns: out_cols, rows })
```

### Step 3 — `apply_join`

```rust
fn apply_join(
    left_rows: Vec<Row>,
    right_rows: &[Row],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    right_col_offset: usize,       // position of right table's first col in combined
    right_columns: &[CatalogColumnDef],
) -> Result<Vec<Row>, DbError>
```

#### INNER JOIN

```
result = []
for left in left_rows:
  for right in right_rows:
    combined = left.clone() + right.clone()
    if eval_join_cond(&condition, &combined, right_col_offset, right_columns)? truthy:
      result.push(combined)
return Ok(result)
```

#### CROSS JOIN

```
result = []
for left in left_rows:
  for right in right_rows:
    result.push(left.clone() + right.clone())
return Ok(result)
```

#### LEFT JOIN

```
result = []
for left in left_rows:
  matched = false
  for right in right_rows:
    combined = left.clone() + right.clone()
    if eval_join_cond(&condition, &combined, ...)? truthy:
      result.push(combined)
      matched = true
  if !matched:
    result.push(left.clone() + vec![Value::Null; right_col_count])
return Ok(result)
```

#### RIGHT JOIN

```
matched_right = vec![false; right_rows.len()]
result = []
for left in left_rows:
  for (i, right) in right_rows.iter().enumerate():
    combined = left.clone() + right.clone()
    if eval_join_cond(&condition, &combined, ...)? truthy:
      result.push(combined)
      matched_right[i] = true
// Emit unmatched right rows with NULL left side
for (i, right) in right_rows.iter().enumerate():
  if !matched_right[i]:
    result.push(vec![Value::Null; left_col_count] + right.clone())
return Ok(result)
```

### Step 4 — `eval_join_cond` helper

Evaluates either `On(Expr)` or `Using(names)` against a combined row:

```rust
fn eval_join_cond(
    cond: &JoinCondition,
    combined: &[Value],
    right_col_offset: usize,
    right_columns: &[CatalogColumnDef],
) -> Result<bool, DbError>
```

#### On(expr):
`return Ok(is_truthy(&eval(expr, combined)?))`

#### Using(names):
```
result = true
for name in names:
  // Find col_idx in left side (columns before right_col_offset)
  left_idx = find in combined positions 0..right_col_offset by column name
             (scan left table columns, they're at positions 0..right_col_offset)
  // Find col_idx in right side
  right_idx = right_col_offset + position of name in right_columns

  equal = combined[left_idx] == combined[right_idx]  // Value::PartialEq
  result = result && equal  // short-circuit on false? Not needed for correctness.
return Ok(result)
```

**Implementation note for `Using` left lookup:** The left side may be a
multi-table accumulation. We can't simply scan `all_tables[0].columns` —
we need to scan all accumulated columns in the left side. The simplest approach:
pass the left table's columns as additional context. For Phase 4.8 with typical
2-table queries, we can require that USING columns exist in the immediate left
table (the one just before the current join). This is the standard SQL behavior.

Actually, USING(col) requires that `col` exists in BOTH the immediately
preceding table in the join sequence AND the right table. The left side's
col_idx for USING is: `col_offsets[i] + position_of_col_in_table[i]` where `i`
is the index of the table immediately to the left of the current join. Pass
this as `left_using_col_offset` and `left_using_columns`.

### Step 5 — `build_join_column_meta`

Builds `Vec<ColumnMeta>` for the output of a JOIN query:

```
fn build_join_column_meta(
    items: &[SelectItem],
    all_tables: &[ResolvedTable],
    joins: &[JoinClause],
) -> Result<Vec<ColumnMeta>>

for item in items:
  Wildcard | QualifiedWildcard:
    for (t_idx, table) in all_tables.enumerate():
      if Wildcard or table name/alias matches:
        for col in table.columns:
          nullable = col.nullable || is_outer_nullable(t_idx, joins)
          push ColumnMeta { name: col.name, data_type: ..., nullable, table_name: Some(table.table_name) }

  Expr { expr, alias }:
    name = alias or expr_column_name(expr)
    (dt, nullable) = infer_expr_type_in_join(expr, all_tables)
    push ColumnMeta { name, data_type: dt, nullable, table_name: None }
```

`is_outer_nullable(t_idx, joins)`:
- Table 0 (FROM table): nullable if any join where table 0 is on the RIGHT side
  (i.e., a RIGHT JOIN is the first join — but actually RIGHT JOIN makes table 0
  nullable only for unmatched rows). Simplified: table 0 is nullable if `joins[0]`
  is RIGHT.
- Table `i` (JOIN[i-1] table): nullable if `joins[i-1]` is LEFT.

---

## Implementation order

1. **Remove the JOIN guard** in `execute_select` and add the join/no-join branch.
   `cargo check -p axiomdb-sql` must pass.

2. **Implement `eval_join_cond`** — On(expr) case first, then Using.
   Unit test: `On(Expr::Literal(Bool(true)))` always passes.

3. **Implement `apply_join`** — INNER first (simplest), then CROSS, LEFT, RIGHT.
   For each type: write the implementation, then write at least 2 unit tests.

4. **Implement `execute_select_with_joins`** — table resolution, pre-scan,
   progressive join, WHERE, ColumnMeta, projection.

5. **Implement `build_join_column_meta`** — Wildcard and Expr cases.

6. **Write integration tests** in `integration_executor.rs`.

7. **Full check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Unit tests in `executor.rs`

```
test_apply_join_inner_basic
  — 2 left rows, 2 right rows, ON always true → 4 combined rows

test_apply_join_inner_filter
  — ON condition filters some pairs → correct subset

test_apply_join_cross
  — 3×4 = 12 rows, no condition check needed

test_apply_join_left_all_match
  — every left row matches → same as INNER

test_apply_join_left_no_match
  — no left row matches → all left rows with NULL right side

test_apply_join_left_partial_match
  — some left rows match, some don't → mixed output

test_apply_join_right_all_match
  — same as INNER (symmetric)

test_apply_join_right_no_match
  — all right rows emitted with NULL left side

test_apply_join_right_partial
  — unmatched right rows appear at the end with NULLs on left
```

### Integration tests in `integration_executor.rs`

All tests use real SQL parsed and analyzed through the full pipeline.

```
test_inner_join_basic
  — CREATE TABLE users(id INT, name TEXT), orders(id INT, user_id INT, total INT)
  — INSERT 3 users, INSERT 4 orders (some with matching user_id, one without)
  — SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id
  — verify: only matching pairs returned, correct col values

test_inner_join_where_filter
  — same tables, add WHERE o.total > 100 → subset of matched pairs

test_inner_join_select_star
  — SELECT * returns all columns from both tables in correct order

test_left_join_unmatched_left
  — user with no orders → appears with NULL total
  — user with orders → appears paired

test_left_join_all_matched
  — every user has orders → same result as INNER JOIN

test_right_join_unmatched_right
  — order with no matching user → appears with NULL name
  — order with matching user → appears paired

test_cross_join
  — 2 rows × 3 rows = 6 combined rows

test_three_table_join
  — users JOIN orders ON ... JOIN products ON ...
  — verify combined row has cols from all 3 tables

test_join_column_meta_nullable
  — LEFT JOIN: right table columns are nullable=true in ColumnMeta
  — INNER JOIN: nullability matches catalog definition

test_full_outer_join_not_implemented
  — SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id → NotImplemented
```

---

## Anti-patterns to avoid

- **DO NOT** call `scan_table` inside the inner join loop. All tables are
  pre-scanned once before the nested loop. Scanning inside the loop would
  re-read the same data O(n) times and could see updated rows mid-iteration.
- **DO NOT** forget NULL padding length. For LEFT JOIN's unmatched row:
  `vec![Value::Null; right_col_count]` — use `right_col_count`, not the total
  combined row length.
- **DO NOT** modify `combined_rows` while iterating to apply WHERE. The `retain`
  approach (collect all, then filter) is correct. Alternatively, filter during
  accumulation.
- **DO NOT** apply the WHERE clause inside `apply_join`. WHERE is applied to the
  fully combined row AFTER all joins. Pushing WHERE down is an optimization for
  Phase 6 (query planner).
- **DO NOT** `unwrap()` anywhere in `src/` code.

---

## Risks

| Risk | Mitigation |
|---|---|
| Memory for large CROSS JOINs (O(n×m) rows) | Acceptable for Phase 4.8. Streaming evaluation is Phase 14. |
| USING column lookup when left side is multi-table (3-table join) | Pass the immediate-left table's columns and offset; USING always refers to the two adjacent tables in the join chain. |
| RIGHT JOIN result order: unmatched right rows appear AFTER matched rows | This is correct per SQL semantics. Document it. |
| `JoinCondition::On` for CROSS JOIN: parser may emit `On(Literal(Bool(true)))` or the condition may be absent | Check what the parser produces for CROSS JOIN. If it wraps in On(true), eval always returns true — correct. If JoinCondition is always required, cross join works. |
