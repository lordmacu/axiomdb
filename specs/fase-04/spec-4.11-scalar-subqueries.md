# Spec: 4.11 — Scalar Subqueries, IN/EXISTS, and Derived Tables

## What to build (not how)

Five classes of subquery support, all composable with the existing executor:

1. **Scalar subquery** — `(SELECT expr FROM ...)` anywhere an expression is
   valid (WHERE, SELECT list, HAVING, ORDER BY). Must return exactly 1 column.
   If it returns 0 rows → `NULL`. If it returns >1 row → SQLSTATE 21000
   (cardinality violation).

2. **IN subquery** — `expr [NOT] IN (SELECT col FROM ...)`. The subquery may
   return any number of rows. NULL semantics follow SQL `IN`: if no match is
   found and the subquery contains NULL values, the result is NULL (UNKNOWN);
   if no match and no NULLs, the result is FALSE.

3. **EXISTS / NOT EXISTS** — `[NOT] EXISTS (SELECT ...)`. TRUE if the subquery
   returns at least one row, FALSE otherwise. Never NULL — EXISTS is immune to
   NULL propagation.

4. **Correlated subqueries** — the inner query references a column from the
   outer query (e.g., `WHERE o.user_id = u.id` inside an EXISTS). The inner
   query is re-executed once per outer row.

5. **Derived table (subquery in FROM)** — `FROM (SELECT ...) AS alias`.
   The inner result is materialized in memory and treated as a virtual table
   for the duration of the outer query.

---

## Inputs / Outputs

### Scalar subquery
- Input: `Expr::Subquery(Box<SelectStmt>)`, outer row `&[Value]`
- Output: `Value` — the single cell from the single result row
- Errors:
  - `DbError::CardinalityViolation` (SQLSTATE 21000) — more than one row returned
  - Propagates any error from executing the inner query

### IN subquery
- Input: `Expr::InSubquery { expr, query, negated }`, outer row
- Output: `Value::Bool(true/false)` or `Value::Null`
- Errors: propagates inner execution errors

### EXISTS
- Input: `Expr::Exists { query, negated }`, outer row
- Output: `Value::Bool(true)` if ≥1 row, `Value::Bool(false)` if 0 rows — never Null
- Errors: propagates inner execution errors

### Derived table
- Input: `FromClause::Subquery { query, alias }`
- Output: materialized `Vec<Row>` with column metadata
- Errors: propagates inner execution errors

---

## Use cases

### 1. Uncorrelated scalar in WHERE
```sql
SELECT * FROM employees
WHERE salary > (SELECT AVG(salary) FROM employees);
```
Inner query runs once. Result replaces the Subquery node.

### 2. Correlated scalar in SELECT list
```sql
SELECT u.name,
       (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS total_orders
FROM users u;
```
Inner query runs once per outer row with `u.id` bound to outer value.

### 3. IN subquery
```sql
SELECT * FROM orders
WHERE user_id IN (SELECT id FROM users WHERE role = 'vip');
```
Inner query materializes a set of values; each outer row checks membership.

### 4. EXISTS (correlated)
```sql
SELECT * FROM users u
WHERE EXISTS (
    SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 1000
);
```
For each outer row, inner query runs with `u.id` bound. TRUE if any row found.

### 5. NOT EXISTS
```sql
SELECT * FROM products p
WHERE NOT EXISTS (
    SELECT 1 FROM order_items oi WHERE oi.product_id = p.id
);
```

### 6. Derived table in FROM
```sql
SELECT dept_avg.dept, dept_avg.avg_sal
FROM (
    SELECT dept, AVG(salary) AS avg_sal
    FROM employees
    GROUP BY dept
) AS dept_avg
WHERE dept_avg.avg_sal > 50000;
```
Inner query materialized; outer query runs against its result rows.

### 7. NULL propagation in IN subquery
```sql
-- If subquery returns {1, NULL, 3}:
SELECT 2 IN (SELECT id FROM t);  -- NULL (no match, but NULL in set)
SELECT 4 IN (SELECT id FROM t);  -- NULL (no match, but NULL in set)
SELECT 1 IN (SELECT id FROM t);  -- TRUE (match found, NULL irrelevant)
SELECT 5 NOT IN (SELECT id FROM t);  -- NULL (NULL in set, no definite FALSE)
```

### 8. Cardinality violation
```sql
SELECT (SELECT id FROM users);  -- ERROR if users has more than 1 row
SELECT (SELECT id FROM users WHERE id = 99);  -- NULL (0 rows → NULL)
```

### 9. Nested subqueries (two levels)
```sql
SELECT * FROM a WHERE id IN (
    SELECT a_id FROM b WHERE val > (SELECT AVG(val) FROM b)
);
```
The innermost subquery runs once; its result is used in the middle query.

---

## Acceptance criteria

- [ ] `(SELECT MAX(id) FROM t)` in WHERE evaluates correctly and runs once
- [ ] `(SELECT MAX(id) FROM t)` in SELECT list evaluates correctly per row
- [ ] 0-row scalar subquery returns NULL (no error)
- [ ] >1-row scalar subquery returns `DbError::CardinalityViolation` (SQLSTATE 21000)
- [ ] `IN (SELECT ...)` — match → TRUE, no match + no NULL → FALSE, no match + NULL → NULL
- [ ] `NOT IN (SELECT ...)` — inverted semantics, NULL propagation preserved
- [ ] `EXISTS (SELECT ...)` — TRUE/FALSE, never NULL
- [ ] `NOT EXISTS` — inverted
- [ ] Correlated EXISTS with `outer_col = inner_col` executes correctly per row
- [ ] Correlated scalar in SELECT list executes once per outer row
- [ ] Derived table (`FROM (SELECT ...) AS alias`) is materialized and queryable
- [ ] Subquery in HAVING clause evaluates correctly
- [ ] Nested subqueries (2 levels) resolve correctly
- [ ] `cargo test --workspace` passes clean
- [ ] All new paths have unit and integration tests

---

## Out of scope

- `ANY / ALL` predicates (`WHERE x > ANY (SELECT ...)`) — Phase 6
- Multi-level correlated subqueries (subquery inside subquery, both correlated) — Phase 6
- Subquery unnesting / decorrelation into JOINs (optimizer) — Phase 10
- `LATERAL` joins — Phase 6
- Subqueries in `UPDATE SET col = (SELECT ...)` — follow-up subfase 4.11b

---

## New `Expr` variants (added to `expr.rs`)

```rust
/// `(SELECT expr FROM ...) — scalar subquery.
/// Returns NULL if 0 rows; error if >1 row.
Subquery(Box<SelectStmt>),

/// `expr [NOT] IN (SELECT col FROM ...)`
InSubquery {
    expr: Box<Expr>,
    query: Box<SelectStmt>,
    negated: bool,
},

/// `[NOT] EXISTS (SELECT ...)`
Exists {
    query: Box<SelectStmt>,
    negated: bool,
},

/// A column reference resolved to the OUTER query's row, set by the
/// semantic analyzer when a name is found in an enclosing scope but
/// not the current scope. `col_idx` is the offset in the outer row.
OuterColumn { col_idx: usize, name: String },
```

---

## `eval_ctx` — extended evaluator

```rust
/// Evaluates `expr` against `row` with subquery support.
///
/// `sq` is called for each subquery node encountered. The executor
/// provides a closure capturing `storage`, `txn`, and `ctx`.
///
/// For correlated subqueries, the inner `SelectStmt` will contain
/// `Expr::OuterColumn` nodes. The `sq` closure must substitute these
/// with values from `outer_row` before executing.
pub fn eval_ctx(
    expr: &Expr,
    row: &[Value],
    sq: &mut dyn FnMut(&SelectStmt) -> Result<QueryResult, DbError>,
) -> Result<Value, DbError>
```

`eval()` (pure, no subqueries) remains unchanged. `eval_ctx` delegates all
non-subquery nodes to `eval` internally. The executor calls `eval_ctx` wherever
the expression may contain a subquery; it falls back to `eval` for contexts
that are provably subquery-free (e.g., DEFAULT expressions, CHECK constraints).

---

## Outer column substitution

Before passing the inner `SelectStmt` to `execute_select_*`, the executor
replaces all `Expr::OuterColumn { col_idx }` nodes in the inner AST with
`Expr::Literal(outer_row[col_idx])`. This is done by a pure AST tree-walk:

```rust
fn substitute_outer(stmt: &SelectStmt, outer_row: &[Value]) -> SelectStmt
```

Returns a new `SelectStmt` (cloned) with all `OuterColumn` replaced by
`Literal`. This is safe and deterministic — no mutation of the original.

---

## Semantic analyzer changes

The analyzer must support a **scope stack** for nested query resolution:

1. When entering a subquery, push the outer scope's table columns onto the stack.
2. When resolving a column reference `name` in the inner query:
   a. Try to resolve against the inner query's tables first.
   b. If not found, walk up the scope stack.
   c. If found in an outer scope at depth `d`: emit `Expr::OuterColumn { col_idx }`.
   d. If not found anywhere: emit `UnknownColumn` error.
3. When exiting a subquery, pop the scope.

For Phase 4.11, support depth-1 correlation only (one level of nesting).
Depth > 1 gets a `NotImplemented` error pointing to Phase 6.

---

## Cardinality error

```rust
/// A scalar subquery returned more than one row.
/// SQLSTATE 21000 — Cardinality Violation.
#[error("subquery must return exactly one row, but returned {count} rows")]
CardinalityViolation { count: usize },
```

---

## IN subquery NULL semantics (exact)

```
result of `expr IN (SELECT ...)`
  - If expr is NULL → NULL
  - If any row value == expr → TRUE
  - If all row values are non-NULL and none match → FALSE
  - If no match found and at least one row value is NULL → NULL
```

Inverted by `negated = true`:
```
result of `expr NOT IN (SELECT ...)`
  - If expr is NULL → NULL
  - If any row value == expr → FALSE
  - If all row values are non-NULL and none match → TRUE
  - If no match and at least one NULL → NULL
```

---

## Dependencies

- `Expr::Subquery`, `InSubquery`, `Exists`, `OuterColumn` must exist in `expr.rs`
- Parser must recognize `(SELECT ...)` in expression positions
- Parser must recognize `expr IN (SELECT ...)` and `EXISTS (SELECT ...)`
- Semantic analyzer scope stack must be in place before correlated refs resolve
- `substitute_outer` tree-walk must handle all `Expr` variants including nested
- `eval_ctx` must be complete before executor can use it
- `DbError::CardinalityViolation` must be added to the error catalog

## ⚠️ DEFERRED

- Subquery in `UPDATE SET col = (SELECT ...)` → pendiente en subfase 4.11b
- Depth >1 correlated subqueries → pendiente en Phase 6
- `ANY / ALL` → pendiente en Phase 6
- Subquery unnesting / JOIN decorrelation → pendiente en Phase 10 (optimizer)
