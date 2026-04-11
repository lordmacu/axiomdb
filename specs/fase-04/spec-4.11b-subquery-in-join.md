# Spec: 4.11b — Subquery in JOIN

## What to build (not how)

Support derived tables on the right-hand side of an existing JOIN chain:

- `SELECT ... FROM t JOIN (SELECT ...) AS alias ON ...`
- `SELECT ... FROM t LEFT|RIGHT|FULL JOIN (SELECT ...) AS alias ON ...`
- `SELECT ... FROM t JOIN (SELECT ...) AS alias USING (col, ...)`

The join-side subquery must behave like the already-supported `FROM (SELECT ...) AS alias`
path, but as a JOIN operand:

- The inner query is analyzed and executed under the same statement snapshot as the outer query.
- The inner query is materialized once per outer statement execution, not once per outer row.
- Its output columns are exposed through the JOIN alias in `ON`, `USING`, `WHERE`, `GROUP BY`,
  `HAVING`, `ORDER BY`, `SELECT alias.*`, and `SELECT *`.
- Existing JOIN semantics remain unchanged for INNER, LEFT, RIGHT, FULL, and CROSS joins.
- JOIN chains may mix base tables and join-side subqueries in any order.

This subphase closes only the executor/analyzer gap for JOIN operands that are derived tables.
It does not introduce lateral semantics or new optimizer rules.

## Inputs / Outputs

- Input: `SelectStmt` whose `joins[*].table` is `FromClause::Subquery { query, alias }`
- Output: `QueryResult::Rows`
- Errors:
  - Propagates parse/analyze/execute errors from the inner subquery
  - `DbError::TableNotFound` if a JOIN alias or qualifier is invalid
  - `DbError::ColumnNotFound` if `ON`, `USING`, `WHERE`, or projection references a missing column
  - `DbError::AmbiguousColumn` if an unqualified column becomes ambiguous after adding the join subquery
  - `DbError::NotImplemented` for correlated/LATERAL-style references from the join-side subquery to outer tables

## Use cases

1. Basic INNER JOIN against a filtered derived table
```sql
SELECT u.id, recent.total
FROM users u
JOIN (
    SELECT user_id, total
    FROM orders
    WHERE total >= 1000
) AS recent ON recent.user_id = u.id;
```

2. LEFT JOIN against an aggregated derived table
```sql
SELECT u.id, stats.order_count
FROM users u
LEFT JOIN (
    SELECT user_id, COUNT(*) AS order_count
    FROM orders
    GROUP BY user_id
) AS stats ON stats.user_id = u.id;
```

3. JOIN ... USING with a derived table
```sql
SELECT *
FROM users
JOIN (
    SELECT id, name
    FROM archived_users
) AS old_users USING (id);
```

4. Chained joins mixing tables and derived tables
```sql
SELECT u.id, totals.total, d.name
FROM users u
JOIN (
    SELECT user_id, SUM(amount) AS total
    FROM invoices
    GROUP BY user_id
) AS totals ON totals.user_id = u.id
LEFT JOIN departments d ON d.id = u.department_id;
```

5. `SELECT alias.*` against a join-side subquery
```sql
SELECT stats.*
FROM users u
JOIN (
    SELECT user_id, COUNT(*) AS order_count
    FROM orders
    GROUP BY user_id
) AS stats ON stats.user_id = u.id;
```

6. Outer query filtering/sorting by derived join columns
```sql
SELECT u.id
FROM users u
JOIN (
    SELECT user_id, MAX(total) AS biggest
    FROM orders
    GROUP BY user_id
) AS mx ON mx.user_id = u.id
WHERE mx.biggest > 500
ORDER BY mx.biggest DESC;
```

## Acceptance criteria

- [x] `JOIN (SELECT ...) alias ON ...` executes without `NotImplemented`
- [x] `LEFT JOIN (SELECT ...) alias ON ...` preserves unmatched left rows with NULL-extended right side
- [x] `RIGHT JOIN (SELECT ...) alias ON ...` preserves unmatched derived-table rows with NULL-extended left side
- [x] `FULL JOIN (SELECT ...) alias ON ...` preserves unmatched rows from both sides
- [x] `JOIN (SELECT ...) alias USING (col)` resolves column names correctly on both sides
- [x] `SELECT *` expands columns from base tables and join-side derived tables in join order
- [x] `SELECT alias.*` expands only the derived-table columns for that alias
- [x] `WHERE`, `GROUP BY`, `HAVING`, and `ORDER BY` can reference columns produced by the join-side subquery
- [x] Chained joins mixing base tables and join-side subqueries work correctly
- [x] The join-side subquery is analyzed before execution so its internal column references are resolved
- [x] Base table JOIN behavior remains unchanged for queries without join-side subqueries
- [x] `cargo test -p axiomdb-sql` passes with new unit/integration coverage

## Out of scope

- `LATERAL` joins
- Correlated join-source subqueries that reference columns from tables already present in the outer join chain
- Optimizer flattening / decorrelation of derived joins into base-table joins
- Reordering join plans or introducing a cost-based planner for derived join operands

## Dependencies

- Phase 4.8 JOIN execution is already in place
- Phase 4.11 scalar subqueries and `FROM (SELECT ...) AS alias` are already in place
- The analyzer already knows how to synthesize virtual columns from `FromClause::Subquery`

## ⚠️ DEFERRED

- Correlated/LATERAL join-source subqueries (`JOIN (SELECT ... WHERE inner.x = outer.y) AS s ...`)
  → pending in a future LATERAL/correlated-join subphase after Phase 4
