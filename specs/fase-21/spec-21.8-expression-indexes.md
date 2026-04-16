# Spec: 21.8 Expression Indexes

## What to build (not how)

An expression index stores the result of evaluating an expression per row, and the query planner can use it when a query's WHERE clause matches the index expression. The parser already accepts `CREATE INDEX ON t(LOWER(col))` syntax — this spec covers wiring expression columns through the catalog, executor, and planner.

## Inputs / Outputs

**SQL surface:**
- `CREATE INDEX ON t(LOWER(email))` — index over `LOWER(email)`
- `CREATE INDEX ON t((col1 + col2))` — arithmetic expression
- `CREATE INDEX ON t(UPPER(name)) WHERE active = true` — combined with partial index
- `SELECT * FROM t WHERE LOWER(email) = 'foo'` — planner uses the expression index

**Catalog:**
- `IndexColumnDef` gains `expr: Option<String>` (SQL text, stored like `predicate`)
- `IndexDef.to_bytes` / `from_bytes` serializes/deserializes expression column expressions as SQL strings

**Planner:**
- Matches WHERE clause expressions against indexed expressions (not just bare column references)
- Supports both equality and LIKE-based range for expression indexes

**Executor:**
- `IndexLookup` evaluates the index expression per row during scans
- `IndexRange` evaluates expression bounds for range scans

## Use cases

| Use case | SQL example |
|---|---|
| Case-insensitive lookups | `WHERE LOWER(email) = 'foo'` uses `CREATE INDEX ON t(LOWER(email))` |
| Arithmetic speedup | `WHERE price * qty > 1000` uses `CREATE INDEX ON t(price * qty)` |
| Function call optimization | `WHERE LEFT(name, 4) = 'John'` uses function-call index |
| Combined with partial | `WHERE LOWER(email) = 'foo' AND active = true` |

## Acceptance criteria

1. [ ] `CREATE INDEX ON t(LOWER(col))` parses, stores expression in catalog, builds index evaluating `LOWER(col)` per row
2. [ ] `SELECT * FROM t WHERE LOWER(col) = 'literal'` uses IndexLookup against the expression index (planner predicate pushdown)
3. [ ] `SELECT * FROM t WHERE LOWER(col) LIKE 'foo%'` uses IndexRange where applicable
4. [ ] INSERT/UPDATE/DELETE maintain expression index correctly (evaluate expression per row)
5. [ ] Expression index combined with partial index WHERE clause works
6. [ ] Expression index on clustered tables works
7. [ ] Multi-column expression index: `CREATE INDEX ON t(col1 + col2)` works
8. [ ] Expression with multiple column references: `CREATE INDEX ON t(UPPER(first_name) || ' ' || UPPER(last_name))`
9. [ ] Rejects disallowed constructs: subqueries, window functions, aggregates in expression index (at parse/compile time)
10. [ ] Existing B-Tree index behavior unchanged for non-expression columns
11. [ ] `EXPLAIN` shows correct index usage for expression indexes

## Out of scope

- Functional indexes on types that don't support B-Tree (GIN/GIN-specific expressions, FTS/TRIGRAM already have special handling)
- Expression index statistics (separate subphase)
- Unique expression indexes (should work since infrastructure is shared with unique B-Tree, but test explicitly)

## Dependencies

- Phase 6.7 partial indexes — predicate compilation infrastructure in `partial_index.rs`
- Phase 4 expression evaluator — `eval.rs`
- Parser already handles `CREATE INDEX ON t(LOWER(col))` in `parse_index_column` (parser/ddl.rs:914-954)
