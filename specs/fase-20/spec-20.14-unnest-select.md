# Spec: 20.14 UNNEST in SELECT list

Phase: 20 — Types + import/export
Task: UNNEST as set-returning function in the SELECT projection list
Status: approved

## Context

Phase 20.4 added `FROM UNNEST(array) AS u(elem)` — UNNEST as a table-valued function
in the FROM clause, including LATERAL correlation and multi-array zip. Subphase 20.14
extends UNNEST to the SELECT projection list: `SELECT id, UNNEST(tags) AS tag FROM posts`.
This is the PostgreSQL "set-returning function in targetlist" feature, implemented here
via a pre-analysis AST rewrite rather than a new execution node. The rewrite converts
SELECT-list UNNESTs into an implicit LATERAL join before the analyzer runs, so the
existing UNNEST executor handles all execution.

## Goal

Allow `UNNEST(array_expr)` anywhere in the SELECT projection list, producing one output
row per array element with all non-SRF columns repeated.

## Non-goals

- UNNEST inside a subexpression (e.g., `UNNEST(arr) + 1`) — only top-level SELECT-list position
- GENERATE_SUBSCRIPTS — separate subphase
- UNNEST in WHERE/HAVING/GROUP BY position — SQL error (consistent with PostgreSQL)
- UNNEST of non-array type → existing type error from materialize_unnest
- Timestamp / interval element types in UNNEST — existing array type constraints apply
- `WITH ORDINALITY` — deferred to a later subphase

## Behavior

### SQL surface syntax

```sql
-- Single UNNEST: expands posts into one row per tag
SELECT id, UNNEST(tags) AS tag FROM posts;

-- Multiple UNNESTs: zipped together (not cross-joined)
SELECT UNNEST(names), UNNEST(scores) FROM athletes;

-- No FROM clause: UNNEST is the sole source
SELECT UNNEST(ARRAY[1, 2, 3]) AS n;

-- With WHERE (filter applies before expansion)
SELECT id, UNNEST(tags) AS tag FROM posts WHERE id > 5;

-- With ORDER BY (applies after expansion)
SELECT id, UNNEST(tags) AS tag FROM posts ORDER BY tag;

-- In CTE
WITH expanded AS (SELECT id, UNNEST(tags) AS tag FROM posts)
SELECT * FROM expanded WHERE tag = 'rust';
```

### Rewrite semantics

The normalizer (`srf_normalize.rs`) transforms the SelectStmt before analysis:

1. Scan `s.columns: Vec<SelectItem>` for items matching
   `SelectItem::Expr { expr: Expr::Function { name: "unnest", args: [array_expr] }, alias }`.
2. Collect all such items (in projection order) into arrays `srf_arrays` and `srf_aliases`.
3. Create one synthetic `UnnestClause`:
   - `exprs`: all collected array expressions (zip semantics for multiple UNNESTs)
   - `alias`: `"__srf__"`
   - `column_names`: `["__srf_0__", "__srf_1__", ...]` (internal names, one per UNNEST)
   - `lateral: true` (array exprs may reference outer table columns)
4. Replace each UNNEST `SelectItem` expr with `Expr::Column { col_idx: 0, name: "__srf_N__" }`.
   Set the SelectItem alias to the user alias if given, or to `"unnest"` / `"unnest_1"` etc.
   when none is given (matching PostgreSQL default column name).
5. Inject the `UnnestClause`:
   - If `s.from.is_none()`: set `s.from = Some(FromClause::Unnest(Box::new(unnest_clause)))`.
   - If `s.from.is_some()`: push a `JoinClause { join_type: JoinType::Cross,
     table: FromClause::Unnest(Box::new(unnest_clause)),
     condition: JoinCondition::On(Expr::Literal(Value::Bool(true))), natural: false }`.

The analyzer then processes the rewritten SelectStmt normally. The `__srf_0__` etc. column
names are found in the injected UnnestClause's BoundTable. Existing LATERAL UNNEST
execution handles per-row re-materialization when the array expr references an outer column.

### Multiple UNNEST zip semantics

When two or more UNNESTs appear in the same SELECT list, they are combined into ONE
`UnnestClause` with multiple expressions. The executor zips them: row `i` gets element `i`
from each array. Arrays of different lengths: the shorter array's extra positions produce
NULL (existing behavior from `materialize_unnest`).

```sql
SELECT UNNEST(ARRAY[1,2,3]), UNNEST(ARRAY['a','b','c']);
-- Result: (1,'a'), (2,'b'), (3,'c') — zip, not 9-row cross product
```

### Column naming rules

| Projection form | Output column name |
|-----------------|--------------------|
| `UNNEST(arr) AS tag` | `tag` |
| `UNNEST(arr)` (first UNNEST, no alias) | `unnest` |
| `UNNEST(arr)` (second UNNEST, no alias) | `unnest_1` |
| `UNNEST(arr)` (third UNNEST, no alias) | `unnest_2` |

### Error cases

| Input | Expected error |
|-------|----------------|
| `UNNEST(non_array_column)` | `DbError::InvalidCoercion` or `DbError::InvalidValue` from `materialize_unnest` |
| `UNNEST()` (zero args) | `DbError::InvalidValue("UNNEST requires exactly one argument")` |
| `UNNEST(a, b)` in SELECT list | `DbError::InvalidValue("UNNEST in SELECT list takes exactly one argument; use UNNEST(a,b) in FROM for multi-array zip")` |
| `1 + UNNEST(arr)` (nested SRF) | Not rewritten — falls through as unknown function, returns `DbError::NotImplemented` or function error |

Note: multi-arg UNNEST in SELECT list is rejected to prevent confusion (the FROM position
supports multi-arg already). Users wanting zip must use `FROM UNNEST(a, b) AS u(x, y)`.

## Edge cases

- [x] NULL array → 0 rows (existing `materialize_unnest` behavior; verify against PostgreSQL)
- [x] Empty array `ARRAY[]::int[]` → 0 rows
- [x] Single-element array → 1 row
- [x] No FROM clause: `SELECT UNNEST(ARRAY[1,2,3])` → UNNEST becomes the sole FROM
- [x] UNNEST mixed with scalars: `SELECT 1, UNNEST(ARRAY['a','b'])` → scalar repeats
- [x] Multiple UNNESTs, same length → zip
- [x] Multiple UNNESTs, different lengths → shorter pads with NULL
- [x] CTE body with UNNEST in SELECT: rewrite runs inside CTE body analysis
- [x] `SELECT * FROM (SELECT id, UNNEST(tags) FROM posts) AS sub` → subquery works
- [x] ORDER BY on UNNEST result column → applies post-expansion
- [x] WHERE on base table column before UNNEST → applies pre-expansion
- [x] LIMIT after UNNEST expansion → applies post-expansion
- [x] UNNEST(NULL::int[]) → 0 rows (NULL array treated as empty)

## Performance budget

No specific throughput target. The rewrite is a zero-copy AST mutation (O(k) where k =
number of UNNEST calls in the SELECT list, typically 1–2). Execution is bounded by the
existing UNNEST executor which iterates array elements — same O(n) as FROM UNNEST.

## Dependencies

- Depends on: Phase 20.4 (FROM UNNEST), GAP-20.4b (LATERAL UNNEST in JOIN) — both complete
- Depends on: `analyzer_stmt.rs`, `analyzer_bind.rs`, `unnest.rs` — all stable
- Blocks: nothing (standalone feature)

## Open questions

All resolved:
- Multi-arg UNNEST in SELECT list: rejected with clear error (users use FROM UNNEST for zip)
- Default column name: "unnest" / "unnest_1" (PostgreSQL behavior)
- NULL array: 0 rows (existing behavior, verified against PostgreSQL docs)
- UNNEST inside expression (e.g., `UNNEST(arr) + 1`): out of scope (deferred)

## Done criteria

- [ ] `SELECT id, UNNEST(tags) AS tag FROM posts` returns one row per tag per post
- [ ] Multiple UNNESTs zip (not cross-join): `SELECT UNNEST(a), UNNEST(b)` same length → N rows
- [ ] `SELECT UNNEST(ARRAY[1,2,3])` works with no FROM clause
- [ ] NULL array → 0 rows (not an error)
- [ ] Empty array → 0 rows
- [ ] Scalar columns repeat for each expanded row
- [ ] CTE body containing UNNEST in SELECT list resolves correctly
- [ ] `SELECT *` on subquery containing UNNEST in SELECT works (column names visible)
- [ ] ORDER BY on UNNEST output column works
- [ ] Parser test: UNNEST in SELECT parses as `Expr::Function { name: "unnest", ... }`
- [ ] 15+ integration tests in `tests/integration_unnest_select.rs` pass
- [ ] `cargo nextest run -p axiomdb-sql` passes (Lima VM)
- [ ] `cargo nextest run --workspace` passes (Lima VM)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke: 4 new assertions, all pass
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated with UNNEST-in-SELECT section
- [ ] `docs-site/src/internals/sql-parser.md` updated

## References

- PostgreSQL `nodeProjectSet.c`: `research/postgres/src/backend/executor/nodeProjectSet.c`
- Existing UNNEST executor: `crates/axiomdb-sql/src/unnest.rs`
- LATERAL UNNEST joins: `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` lines 275–308
- Phase 20.4 spec: `specs/fase-20/spec-20.4-arrays.md`
- PostgreSQL docs: "Set-Returning Functions" and "Table Functions"
