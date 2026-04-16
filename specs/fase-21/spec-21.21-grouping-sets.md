# Spec: 21.21 — GROUPING SETS / ROLLUP / CUBE

Phase: 21 — Advanced SQL
Task: 21.21 — SQL standard multi-dimensional aggregation
Status: approved

## Context

AxiomDB already supports MySQL-style `GROUP BY ... WITH ROLLUP` (`SelectStmt.with_rollup: bool`
+ `execute_select_grouped_rollup` in `agg_hash.rs`). This subphase adds the SQL:1999 standard
syntax: `GROUP BY ROLLUP(...)`, `GROUP BY CUBE(...)`, and `GROUP BY GROUPING SETS(...)`, plus
the `GROUPING()` function. It also replaces the two loose fields (`group_by: Vec<Expr>` +
`with_rollup: bool`) with a clean `GroupByClause` enum. Comes after 21.9 (LATERAL joins);
blocks 21.23 (advanced SQL test suite).

## Goal

Implement SQL standard GROUPING SETS / ROLLUP / CUBE aggregation plus the `GROUPING()` function,
keeping full backward compatibility with `GROUP BY ... WITH ROLLUP`.

## Non-goals

- `GROUPING_ID()` (Oracle extension) — deferred
- Parallel multi-pass execution — deferred to Phase 25
- CUBE with more than 16 dimensions (returns error)
- Window functions inside grouping sets — not changed by this subphase
- Materialized grouping sets (optimizer caches the base scan) — deferred
- `GROUP BY ALL` (DuckDB extension) — not planned

## Behavior

### AST change

```rust
// crates/axiomdb-sql/src/ast.rs

/// GROUP BY clause representation.
/// Replaces the former `group_by: Vec<Expr>` + `with_rollup: bool` fields.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupByClause {
    /// No GROUP BY at all.
    None,
    /// Plain `GROUP BY expr, ...`
    Simple(Vec<Expr>),
    /// MySQL `GROUP BY expr, ... WITH ROLLUP`
    WithRollup(Vec<Expr>),
    /// SQL standard ROLLUP / CUBE / GROUPING SETS (or any mix).
    ///
    /// `universe`: deduplicated list of all expressions referenced across
    ///             all grouping sets (same order as first appearance).
    /// `sets`:     each inner Vec<usize> is one grouping set, given as
    ///             indices into `universe`. Empty inner vec = grand total.
    Sets {
        universe: Vec<Expr>,
        sets: Vec<Vec<usize>>,
    },
}

pub struct SelectStmt {
    // ... existing fields ...
    // REMOVED: pub group_by: Vec<Expr>,
    // REMOVED: pub with_rollup: bool,
    /// Replaces group_by + with_rollup.
    pub group_by: GroupByClause,
    // ... rest unchanged ...
}
```

> Note: the field name stays `group_by` (was `Vec<Expr>`), just the type changes.
> All call-sites that previously accessed `stmt.group_by` (now `GroupByClause`) need updating.

### Parser behavior

```sql
-- These forms must parse without error and produce the correct GroupByClause::Sets:

-- Standard ROLLUP: N+1 sets (full, drop last, ..., grand total)
SELECT region, year, SUM(amount)
FROM sales
GROUP BY ROLLUP(region, year);
-- sets: [{0,1}, {0}, {}]  universe: [region, year]

-- Standard CUBE: 2^N sets (all subsets)
SELECT region, year, SUM(amount)
FROM sales
GROUP BY CUBE(region, year);
-- sets: [{}, {0}, {1}, {0,1}]  universe: [region, year]
-- (empty set first — grand total first, then each singleton, then all)

-- Explicit GROUPING SETS
SELECT region, year, SUM(amount)
FROM sales
GROUP BY GROUPING SETS((region, year), (region), ());
-- sets: [{0,1}, {0}, {}]  universe: [region, year]

-- Mixed: plain column + ROLLUP → cross-product
SELECT a, b, c, SUM(v)
FROM t
GROUP BY a, ROLLUP(b, c);
-- plain {a} cross-product with ROLLUP [{b,c},{b},{}]
-- result sets: [{0,1,2},{0,1},{0}]  universe: [a,b,c]

-- Grouping sets can contain composite elements (tuples):
GROUP BY GROUPING SETS((a, b), (c), ())
-- sets: [{0,1}, {2}, {}]  universe: [a, b, c]

-- Nested ROLLUP inside GROUPING SETS is flattened (PostgreSQL compatible):
GROUP BY GROUPING SETS(ROLLUP(a, b), (c))
-- ROLLUP(a,b) = [{0,1},{0},{}], (c) = [{2}]
-- result sets: [{0,1},{0},{},{2}]  universe: [a, b, c]
```

Error cases during parsing:

| Input | Error |
|-------|-------|
| `CUBE(a,b,c,...) with > 16 columns` | `ParseError: CUBE with N columns produces 2^N sets — cap is 16` |
| `GROUP BY GROUPING SETS()` (empty outer list) | `ParseError: GROUPING SETS requires at least one grouping set` |
| `GROUP BY ROLLUP()` (empty parens) | `ParseError: ROLLUP requires at least one expression` |

### Executor semantics

For `GroupByClause::Sets { universe, sets }`:

1. Evaluate `universe` expressions against the source rows to build an augmented row buffer
   (append universe eval results as extra columns, similar to ORDER BY eval).
2. For each grouping set `s` in `sets`:
   a. Build a sub-`SelectStmt` with `group_by = Simple(exprs_in_s)` (only exprs at those indices).
   b. Run `execute_select_grouped_hash` on that sub-stmt.
   c. For each output row, NULL-out the SELECT positions corresponding to universe expressions
      **not** in `s` (i.e., the "absent" dimensions).
   d. Inject a hidden `__grouping_mask__: u64` as the last value of each row.
      `__grouping_mask__` has bit `i` set if universe index `i` is **not** in this set
      (i.e., that expression is rolled up / nulled out in this row).
3. Collect all rows from all passes, then:
   - Apply outer `DISTINCT` (if requested).
   - Apply `ORDER BY` (including `ORDER BY GROUPING(expr)` which evaluates the mask).
   - Apply `LIMIT` / `OFFSET`.
4. Strip the hidden `__grouping_mask__` column before returning `QueryResult::Rows`.

**HAVING** is applied **per pass** (before step 2d), not post-union. This matches PostgreSQL and SQL standard semantics.

For `GroupByClause::Simple` and `GroupByClause::WithRollup`: behavior unchanged from current code.
For `GroupByClause::None`: behavior unchanged (no grouping).

### GROUPING() function

```sql
-- Returns 1 if the expression was "rolled up" (NULLed) in this grouping set row.
-- Returns 0 if the expression is a real group key in this row.
-- With multiple args: returns a bitmask (arg0 = most significant bit).

SELECT region,
       year,
       SUM(amount),
       GROUPING(region)       AS g_region,   -- 0 or 1
       GROUPING(year)         AS g_year,     -- 0 or 1
       GROUPING(region, year) AS g_both      -- 0..3
FROM sales
GROUP BY ROLLUP(region, year);
```

**Semantics**:
- `GROUPING(expr_1, ..., expr_n)` → `u64` bitmask.
- Bit position `n-1-i` corresponds to `expr_i` (leftmost arg = most significant bit, matching PostgreSQL).
- If `expr_i` is absent from the current grouping set (its universe index has bit set in `__grouping_mask__`), bit `n-1-i` is 1; otherwise 0.
- Outside a GROUPING SETS context (plain GROUP BY or no GROUP BY), `GROUPING()` always returns 0.
- Each argument must be one of the expressions in the current query's `GroupByClause::Sets.universe`; analyzer emits an error otherwise.

**AST**:
```rust
// Expr variant added:
Expr::Grouping(Vec<Expr>),
// Analyzed to:
Expr::GroupingResolved { universe_indices: Vec<usize> },
// (or a single resolved variant that carries the indices directly)
```

**Evaluator**: reads `__grouping_mask__` from the last column of the current row.

### Error cases (runtime)

| Condition | Error |
|-----------|-------|
| `GROUPING(expr)` where `expr` is not in the GROUPING SETS universe | `AnalysisError: GROUPING() argument must be a GROUP BY expression` |
| `GROUPING()` with zero arguments | `ParseError: GROUPING() requires at least one argument` |
| CUBE with > 16 dimensions | `ParseError: CUBE with N dimensions would produce 2^N sets (max 65536)` |
| GROUPING SETS with > 65535 sets after expansion | `ParseError: grouping set count N exceeds maximum 65535` |

## Edge cases

- [ ] Grand total row (empty grouping set `{}`): all group-key columns NULL, aggregates over all rows
- [ ] Single-element ROLLUP: `ROLLUP(a)` → sets `[{0}, {}]`
- [ ] Single-element CUBE: `CUBE(a)` → sets `[{}, {0}]`
- [ ] GROUPING SETS with duplicate sets: `GROUPING SETS((a),(a))` → two rows per group value (correct per SQL standard)
- [ ] Original data has NULLs in group-by columns: `GROUPING()` still returns 0 for real detail rows (not confused with rolled-up NULLs)
- [ ] `GROUPING(a, b)` bitmask: a-not-in-set and b-in-set → returns 2 (bit 1 set)
- [ ] `ORDER BY GROUPING(region)` sorts rolled-up rows last
- [ ] `HAVING SUM(v) > 100` applied before union across sets
- [ ] `WITH ROLLUP` MySQL syntax: no regression
- [ ] `GROUP BY GROUPING SETS((a), ())` with LIMIT 1: still applies post-union
- [ ] Cross-product mixed: `GROUP BY a, ROLLUP(b,c)` → 3 sets, not 2

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| ROLLUP on 100K rows, 3 dimensions | < 50ms | < 150ms |
| CUBE on 100K rows, 4 dimensions (16 sets) | < 200ms | < 500ms |

Reference: existing `execute_select_grouped_rollup` does N+1 passes and is acceptable for current WITH ROLLUP use cases.

## Dependencies

- Depends on: 21.9 closed (done ✅), existing `agg_hash.rs` infrastructure
- Blocks: 21.23 (advanced SQL test suite)

## Open questions

All resolved:

- **HAVING per-pass or post-union?** → Per-pass (SQL standard + PostgreSQL semantics). ✅
- **GROUPING() bitmask bit order?** → Leftmost arg = MSB (PostgreSQL compatible). ✅
- **CUBE dimension cap?** → 16 dimensions max (2^16 = 65536 sets). ✅
- **GroupByClause field name?** → Keep `group_by` (just change the type from `Vec<Expr>` to `GroupByClause`). ✅
- **GROUPING SETS nesting (ROLLUP inside GROUPING SETS)?** → Flatten at parse time (PostgreSQL behavior). ✅

## Done criteria

- [ ] `GroupByClause` enum in `ast.rs`; `group_by: Vec<Expr>` + `with_rollup: bool` removed
- [ ] All existing callsites updated; `cargo nextest -p axiomdb-sql` clean before new tests added
- [ ] Parser handles `ROLLUP(...)`, `CUBE(...)`, `GROUPING SETS(...)` in GROUP BY position
- [ ] Parser handles mixed `GROUP BY a, ROLLUP(b, c)` cross-product
- [ ] `execute_select_grouped_sets` in `agg_hash.rs`: multi-pass, null-out, union, post-agg ops
- [ ] HAVING applied per-pass
- [ ] `GROUPING(expr)` / `GROUPING(expr, expr, ...)` returns correct bitmask
- [ ] `GROUPING()` in ORDER BY and HAVING works
- [ ] MySQL `GROUP BY ... WITH ROLLUP` still works (regression test passes)
- [ ] ≥ 12 integration tests in `tests/integration_grouping_sets.rs`
- [ ] Wire smoke test updated with GROUPING SETS + GROUPING() assertions
- [ ] `cargo nextest -p axiomdb-sql` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `docs-site/` updated: user-guide DML section + internals sql-parser section

## References

- DuckDB: `research/duckdb/src/parser/transform/helpers/transform_groupby.cpp`
- PostgreSQL: `research/postgres/src/backend/parser/parse_clause.c:2361`
- PostgreSQL: `research/postgres/src/backend/executor/nodeAgg.c`
- SQL:1999 standard §7.9 (group by clause)
- AxiomDB existing: `crates/axiomdb-sql/src/executor/agg_hash.rs:136` (WITH ROLLUP reference)
- AxiomDB AST: `crates/axiomdb-sql/src/ast.rs:442`
