# Spec: 11.12 — Correlated Subquery Materialization

## What to build (not how)

Detect correlated scalar subqueries with a single equijoin pattern and replace
repeated per-row re-execution with a one-shot materialization + hash lookup.

**Current cost:** O(N_outer × N_inner) — 1000 users × full orders scan = 0.12x MariaDB.
**Target cost:** O(N_outer + N_inner) — 1 orders scan + 1000 hash lookups ≈ 1x MariaDB.

## Research findings

### Current AxiomDB (exec_subquery.rs)
- **CorrelatedCache** exists: `HashMap<(usize, u64), QueryResult>` keyed by
  `(AST pointer, hash of outer-row correlated columns)`.
- Cache is correct but has 0% hit rate when all outer keys are unique (PK joins).
- Each miss clones the entire AST (`substitute_outer`), re-parses, re-executes.

### PostgreSQL (nodeSubplan.c)
- `ExecHashSubPlan`: materializes UNCORRELATED subqueries into TupleHashTable.
- Correlated subqueries: uses `ExecScanSubPlan` (re-scan per row).
- PostgreSQL does NOT auto-materialize correlated scalar subqueries — the planner
  rewrites them into JOINs via `convert_ANY_sublink_to_join()` when possible.

### MySQL 8.0 / MariaDB (item_subselect.cc)
- `subselect_hash_sj_engine`: hash-based semi-join for IN subqueries.
- `left_expr_cache`: caches outer expression values to detect duplicates.
- MariaDB's optimizer often rewrites correlated subqueries into derived table
  JOINs (`optimizer_switch=materialization=on`).

### Key insight from research
Both PostgreSQL and MySQL prefer **query rewrite** (subquery → JOIN/derived table)
over runtime memoization. The rewrite happens at the planner level, before execution.

AxiomDB's approach for 11.12: detect the pattern at EXECUTION time (cheaper than
a full planner rewrite) and materialize the inner query as a hash table.

## Pattern to detect

```sql
SELECT ... (SELECT agg(col) FROM inner WHERE inner.fk = outer.pk) ...
FROM outer
```

Conditions for materialization:
1. Scalar subquery (single column, single row or aggregate)
2. Single equijoin predicate: `inner.col = OuterColumn(idx)`
3. The equijoin column is the ONLY correlation (no other OuterColumn refs)
4. Inner query has no LIMIT, OFFSET, or non-deterministic functions

When all conditions are met, rewrite execution to:
```
1. Execute: SELECT inner.fk, agg(col) FROM inner GROUP BY inner.fk
2. Build: HashMap<fk_value, agg_result>
3. Per outer row: lookup outer.pk in HashMap → O(1)
```

## Inputs / Outputs
- Input: correlated scalar `SelectStmt` with `Expr::OuterColumn` references
- Output: `QueryResult` (same as current, but O(N+M) instead of O(N×M))
- Errors: falls back to current re-execution if pattern doesn't match

## Use cases
1. `SELECT id, (SELECT SUM(amount) FROM orders WHERE user_id = users.id) FROM users`
   → materializes `SELECT user_id, SUM(amount) FROM orders GROUP BY user_id`
2. `SELECT id, (SELECT COUNT(*) FROM orders WHERE user_id = users.id) FROM users`
   → materializes `SELECT user_id, COUNT(*) FROM orders GROUP BY user_id`
3. `SELECT id, (SELECT MAX(date) FROM orders WHERE user_id = users.id) FROM users`
   → same pattern, any single-value aggregate

## Acceptance criteria
- [ ] Pattern detection correctly identifies single-equijoin correlated subqueries
- [ ] Materialization executes inner query ONCE with GROUP BY
- [ ] Hash lookup returns correct result per outer row
- [ ] NULL outer keys handled correctly (return NULL aggregate)
- [ ] Falls back to current re-execution for non-matching patterns
- [ ] subquery_scalar benchmark improves from 0.12x to ≥ 0.5x vs MariaDB
- [ ] No regression on other subquery benchmarks (subquery_in, subquery_exists)
- [ ] Existing subquery tests pass unchanged

## Out of scope
- Planner-level subquery → JOIN rewrite (future optimizer phase)
- Multi-column correlation (e.g., WHERE a = outer.a AND b = outer.b)
- Non-aggregate correlated subqueries (e.g., SELECT col FROM inner WHERE ...)
- EXISTS/IN subquery materialization (already optimized via InSetCache)

## Dependencies
- exec_subquery.rs CorrelatedCache infrastructure (already exists)
- eval/core.rs HashableValue (already exists)
