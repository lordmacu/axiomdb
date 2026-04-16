# Plan: 21.21 — GROUPING SETS / ROLLUP / CUBE

Phase: 21 — Advanced SQL
Task: 21.21 — SQL standard multi-dimensional aggregation
Spec: specs/fase-21/spec-21.21-grouping-sets.md
Status: in-progress

## Summary

Four steps, each a green commit. Step 1 migrates the AST from loose fields
(`group_by: Vec<Expr>` + `with_rollup: bool`) to a clean `GroupByClause` enum and updates
all ~25 callsites — all existing tests stay green. Step 2 extends the parser to recognize
`ROLLUP(...)`, `CUBE(...)`, `GROUPING SETS(...)` and produce `GroupByClause::Sets` at parse
time (cross-product for mixed GROUP BY). Step 3 adds `execute_select_grouped_sets` in
`agg_hash.rs` — multi-pass aggregation with null-out and hidden grouping mask. Step 4 adds
`GROUPING()` function: AST node, analyzer resolution, evaluator that reads the hidden mask.

## Dependencies

Must be done first:
- [x] spec-21.21-grouping-sets.md approved
- [x] 21.9 LATERAL joins closed (✅)

Blocks (until this plan is done):
- [ ] 21.23 Advanced SQL test suite

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_grouping_sets.rs` — integration tests

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add `GroupByClause`, replace fields, update struct literals
- `crates/axiomdb-sql/src/parser/dml.rs` — parse ROLLUP/CUBE/GROUPING SETS
- `crates/axiomdb-sql/src/executor/agg_hash.rs` — new Sets dispatch + execute_select_grouped_sets
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/select_core.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/select_helpers.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/exec_subquery.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/recursive_cte_exec.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — callsite migration
- `crates/axiomdb-sql/src/executor/agg_sorted.rs` — callsite migration
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — group_by resolution + GROUPING() resolution
- `crates/axiomdb-sql/src/plan_deps.rs` — callsite migration
- `crates/axiomdb-sql/src/eval/functions/` — GROUPING() evaluator (new file or added to existing)
- `tools/wire-test.py` — new GROUPING SETS smoke assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — user docs
- `docs-site/src/internals/sql-parser.md` — internals docs

---

## Step 1 — AST migration: GroupByClause enum

**Goal:** Replace `group_by: Vec<Expr>` + `with_rollup: bool` with `group_by: GroupByClause`
and migrate every callsite so all existing tests stay green.

**Files:** `ast.rs`, all executor files listed above, `analyzer_stmt.rs`, `plan_deps.rs`

**Approach:** Add the enum with helper methods first, then do a mechanical callsite migration.

### GroupByClause enum + helpers

```rust
// crates/axiomdb-sql/src/ast.rs  (new, before SelectStmt)

/// GROUP BY clause representation.
/// Replaces the former `group_by: Vec<Expr>` + `with_rollup: bool` fields on SelectStmt.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupByClause {
    /// No GROUP BY.
    None,
    /// Plain `GROUP BY expr, ...`
    Simple(Vec<Expr>),
    /// MySQL `GROUP BY expr, ... WITH ROLLUP`
    WithRollup(Vec<Expr>),
    /// SQL standard ROLLUP / CUBE / GROUPING SETS.
    /// `universe`: all unique exprs across all sets (dedup, in first-appearance order).
    /// `sets`: each inner Vec<usize> indexes into universe. Empty inner vec = grand total.
    Sets {
        universe: Vec<Expr>,
        sets: Vec<Vec<usize>>,
    },
}

impl GroupByClause {
    /// Returns the flat list of group-by expressions.
    /// For `None`: empty.
    /// For `Simple`/`WithRollup`: the expression list.
    /// For `Sets`: the universe list.
    pub fn exprs(&self) -> &[Expr] {
        match self {
            Self::None => &[],
            Self::Simple(v) | Self::WithRollup(v) => v,
            Self::Sets { universe, .. } => universe,
        }
    }

    /// Mutable exprs for in-place resolution (used by analyzer).
    pub fn exprs_mut(&mut self) -> &mut Vec<Expr> {
        match self {
            Self::None => panic!("exprs_mut on GroupByClause::None"),
            Self::Simple(v) | Self::WithRollup(v) => v,
            Self::Sets { universe, .. } => universe,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::None) || self.exprs().is_empty()
    }

    pub fn is_with_rollup(&self) -> bool {
        matches!(self, Self::WithRollup(_))
    }

    pub fn is_sets(&self) -> bool {
        matches!(self, Self::Sets { .. })
    }
}
```

### SelectStmt field change

```rust
// ast.rs – replace:
//   pub group_by: Vec<Expr>,
//   pub with_rollup: bool,
// with:
pub group_by: GroupByClause,
```

Update the three `..Default::default()` struct literals at lines ~939, ~969, ~1166:
```rust
group_by: GroupByClause::None,
// remove: with_rollup: false,
```

### Callsite migration pattern

Most callsites follow one of these patterns:

| Old pattern | New pattern |
|---|---|
| `stmt.group_by.is_empty()` | `stmt.group_by.is_empty()` (same — method exists) |
| `&stmt.group_by` (passed as `&[Expr]`) | `stmt.group_by.exprs()` |
| `for gb in &stmt.group_by` | `for gb in stmt.group_by.exprs()` |
| `stmt.group_by.iter().any(...)` | `stmt.group_by.exprs().iter().any(...)` |
| `stmt.group_by = resolve(...).collect()` | update `exprs_mut()` or rebuild enum |
| `stmt.with_rollup` | `stmt.group_by.is_with_rollup()` |
| `stmt.with_rollup = false` | replace by constructing `Simple(v)` from `WithRollup(v)` |

**Key callsites to handle carefully:**

- `agg_hash.rs:120` — `resolve_positional_group_by` takes `&[Expr]` and returns `Vec<Expr>`;
  call with `.exprs()`, then rebuild the same variant:
  ```rust
  let resolved = resolve_positional_group_by(stmt.group_by.exprs(), &stmt.columns);
  stmt.group_by = match stmt.group_by {
      GroupByClause::Simple(_) => GroupByClause::Simple(resolved),
      GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved),
      GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved, sets },
      GroupByClause::None => GroupByClause::None,
  };
  ```
- `agg_hash.rs:204` — `level_stmt.group_by.truncate(k)` — now done inside
  `execute_select_grouped_rollup` by building `Simple(truncated)` directly.
- `agg_hash.rs:160/181` — `stripped.with_rollup = false` — replace by extracting
  the inner `Vec<Expr>` and setting `group_by = Simple(v)`.
- `analyzer_stmt.rs:421` — map over exprs:
  ```rust
  let resolved_exprs = stmt.group_by.exprs().to_owned()
      .into_iter().map(|e| resolve_expr_full(e, &ctx, ...)).collect::<Result<_,_>>()?;
  // rebuild same variant
  stmt.group_by = match stmt.group_by {
      GroupByClause::Simple(_) => GroupByClause::Simple(resolved_exprs),
      GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved_exprs),
      GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved_exprs, sets },
      GroupByClause::None => GroupByClause::None,
  };
  ```
- `exec_subquery.rs:614` — same pattern (substituting outer refs).
- `parser/dml.rs` — produce `GroupByClause::Simple(exprs)` or `GroupByClause::WithRollup(exprs)`.

### Verification

```bash
# Lima
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo nextest run -p axiomdb-sql 2>&1 | tail -5"
```

All existing tests must pass. No new tests needed in this step.

### Commit

```
feat(fase-21): refactor GroupByClause enum, migrate all callsites

Step 1 of specs/fase-21/plan-21.21-grouping-sets.md
```

---

## Step 2 — Parser: ROLLUP / CUBE / GROUPING SETS

**Goal:** Parse `GROUP BY ROLLUP(...)`, `GROUP BY CUBE(...)`, `GROUP BY GROUPING SETS(...)`,
and mixed `GROUP BY a, ROLLUP(b,c)` into `GroupByClause::Sets` with the correct universe + sets.

**Files:** `parser/dml.rs` (replace `parse_group_by_clause` inline block with a helper fn)

### Parsing algorithm

After `GROUP BY`, instead of calling `parse_expr_list` directly, call a new helper
`parse_group_by_items(p)` that returns `GroupByClause`:

```rust
// parser/dml.rs  (new helper)

fn parse_group_by_items(p: &mut Parser) -> Result<GroupByClause, ParseError> {
    // Each "item" in GROUP BY is either:
    //   - A plain expr  → contributes {expr_idx} to a single grouping set
    //   - ROLLUP(e1,e2,...) → contributes N+1 sets
    //   - CUBE(e1,e2,...)   → contributes 2^N sets
    //   - GROUPING SETS(...) → contributes the explicit sets
    //
    // Multiple items are cross-producted into the final list of grouping sets.
    // If no ROLLUP/CUBE/GROUPING SETS appears at all, return GroupByClause::Simple.

    let mut universe: Vec<Expr> = Vec::new();  // deduplicated
    let mut item_sets: Vec<Vec<Vec<usize>>> = Vec::new(); // one Vec<Vec<usize>> per item

    // Helper: find or insert expr in universe, return its index
    fn intern(universe: &mut Vec<Expr>, expr: Expr) -> usize { ... }

    let mut has_special = false;

    loop {
        if p.peek_ident_ci("ROLLUP") {
            has_special = true;
            p.advance(); // ROLLUP
            p.expect(&Token::LParen)?;
            let rollup_exprs = parse_grouping_set_args(p)?; // comma-sep exprs/tuples
            p.expect(&Token::RParen)?;
            // ROLLUP(e1,...,eN) → N+1 sets: {e1..eN}, {e1..e(N-1)}, ..., {e1}, {}
            let idxs: Vec<usize> = rollup_exprs.into_iter()
                .map(|e| intern(&mut universe, e)).collect();
            let mut sets: Vec<Vec<usize>> = Vec::new();
            sets.push(vec![]); // grand total first (matches PG prefix-build order)
            // Actually build sets from full down to empty (matches DuckDB order):
            // full set first: idxs[0..N], then idxs[0..N-1], ..., {}
            // Order: [{0,1,...,N-1}, {0,1,...,N-2}, ..., {0}, {}]
            for k in (0..=idxs.len()).rev() {
                sets.push(idxs[..k].to_vec());
            }
            sets.reverse(); // so full set is first, grand total last
            item_sets.push(sets);
        } else if p.peek_ident_ci("CUBE") {
            has_special = true;
            p.advance(); // CUBE
            p.expect(&Token::LParen)?;
            let cube_exprs = parse_grouping_set_args(p)?;
            p.expect(&Token::RParen)?;
            let n = cube_exprs.len();
            if n > 16 {
                return Err(ParseError::new(format!(
                    "CUBE with {} dimensions would produce {} sets (maximum is 65536)",
                    n, 1usize << n
                )));
            }
            let idxs: Vec<usize> = cube_exprs.into_iter()
                .map(|e| intern(&mut universe, e)).collect();
            // 2^N subsets, sorted by cardinality descending (full first, empty last)
            let total = 1usize << n;
            let mut sets: Vec<Vec<usize>> = (0..total).map(|mask| {
                (0..n).filter(|&i| (mask >> i) & 1 == 1)
                      .map(|i| idxs[i]).collect()
            }).collect();
            // sort: more elements first (full detail first)
            sets.sort_by_key(|s| std::cmp::Reverse(s.len()));
            item_sets.push(sets);
        } else if p.peek_ident_ci("GROUPING") && matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("SETS")) {
            has_special = true;
            p.advance(); // GROUPING
            p.advance(); // SETS
            p.expect(&Token::LParen)?;
            let sets = parse_grouping_sets_list(p, &mut universe)?; // may be nested ROLLUP/CUBE
            p.expect(&Token::RParen)?;
            item_sets.push(sets);
        } else {
            // Plain expression — contributes a single single-element grouping set
            let e = parse_expr(p)?;
            let idx = intern(&mut universe, e);
            item_sets.push(vec![vec![idx]]);
        }

        if !p.eat(&Token::Comma) { break; }
    }

    if !has_special {
        // Pure plain exprs — return Simple (no need for Sets path)
        return Ok(GroupByClause::Simple(universe));
    }

    // Cross-product of all item_sets
    let mut result: Vec<Vec<usize>> = vec![vec![]];
    for item in item_sets {
        let mut new_result = Vec::new();
        for existing in &result {
            for set in &item {
                let mut combined = existing.clone();
                for &idx in set {
                    if !combined.contains(&idx) {
                        combined.push(idx);
                    }
                }
                combined.sort();
                new_result.push(combined);
            }
        }
        result = new_result;
    }

    if result.len() > 65535 {
        return Err(ParseError::new(format!(
            "Grouping set count {} exceeds maximum 65535", result.len()
        )));
    }

    Ok(GroupByClause::Sets { universe, sets: result })
}
```

`parse_grouping_sets_list` handles the `(...)` content of GROUPING SETS, including nested
ROLLUP/CUBE inside it (flatten per PostgreSQL semantics).

`parse_grouping_set_args` parses a comma-separated list of exprs or `(expr, expr)` tuples.

Also: check `WITH ROLLUP` after the clause (existing MySQL syntax) and return `WithRollup`.
If both `WITH ROLLUP` and any ROLLUP/CUBE/GROUPING SETS appear simultaneously → `ParseError`.

### Test (parsing only, executor not needed yet)

```rust
// tests/integration_grouping_sets.rs

#[test]
fn test_parse_rollup_two_cols() {
    // Verify ROLLUP(a, b) parses without error and connects (smoke)
    let mut conn = connect();
    conn.query_drop("CREATE TABLE t_gs1 (a INT, b INT, v INT)").unwrap();
    conn.query_drop("INSERT INTO t_gs1 VALUES (1,1,10),(1,2,20),(2,1,30)").unwrap();
    // Just check it doesn't return a parse error
    let r: Result<Vec<(Option<i32>,Option<i32>,i32)>, _> =
        conn.query("SELECT a, b, SUM(v) FROM t_gs1 GROUP BY ROLLUP(a, b)");
    assert!(r.is_ok(), "ROLLUP parse should succeed: {:?}", r);
    conn.query_drop("DROP TABLE t_gs1").unwrap();
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo nextest run -p axiomdb-sql 2>&1 | tail -5"
```

Parser tests pass; executor returns `NotImplemented` for `Sets` (acceptable at this step).

### Commit

```
feat(fase-21): parse ROLLUP/CUBE/GROUPING SETS in GROUP BY

Step 2 of specs/fase-21/plan-21.21-grouping-sets.md
```

---

## Step 3 — Executor: execute_select_grouped_sets

**Goal:** Implement multi-pass aggregation for `GroupByClause::Sets` with correct null-out,
HAVING per-pass, hidden grouping mask, and post-union ORDER BY / LIMIT / DISTINCT.

**Files:** `crates/axiomdb-sql/src/executor/agg_hash.rs`,
`tests/integration_grouping_sets.rs`

### Dispatch in execute_select_grouped

```rust
fn execute_select_grouped(
    mut stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError> {
    // Resolve positional GROUP BY / ORDER BY
    let resolved = resolve_positional_group_by(stmt.group_by.exprs(), &stmt.columns);
    stmt.group_by = match stmt.group_by {
        GroupByClause::Simple(_) => GroupByClause::Simple(resolved),
        GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved),
        GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved, sets },
        GroupByClause::None => GroupByClause::None,
    };
    stmt.order_by = resolve_positional_order_by(&stmt.order_by, &stmt.columns);

    match &stmt.group_by {
        GroupByClause::WithRollup(_) =>
            execute_select_grouped_rollup(stmt, combined_rows, strategy),
        GroupByClause::Sets { .. } =>
            execute_select_grouped_sets(stmt, combined_rows, strategy),
        _ => match strategy {
            GroupByStrategy::Hash => execute_select_grouped_hash(stmt, combined_rows),
            GroupByStrategy::Sorted { presorted } =>
                execute_select_grouped_sorted(stmt, combined_rows, presorted),
        }
    }
}
```

### execute_select_grouped_sets implementation

```rust
fn execute_select_grouped_sets(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    _strategy: GroupByStrategy,  // always hash for sets (no index-sorted path)
) -> Result<QueryResult, DbError> {
    let (universe, sets) = match &stmt.group_by {
        GroupByClause::Sets { universe, sets } => (universe.clone(), sets.clone()),
        _ => unreachable!(),
    };

    // Precompute: for each universe expr at index i, which SELECT item positions
    // correspond to it? (may be multiple if the same expr appears twice in SELECT)
    let select_slots_for_universe: Vec<Vec<usize>> = universe.iter().map(|ue| {
        stmt.columns.iter().enumerate()
            .filter_map(|(pos, item)| match item {
                SelectItem::Expr { expr, .. } if expr == ue => Some(pos),
                _ => None,
            }).collect()
    }).collect();

    // Get column meta template from a probe run (first set, or grand total if sets[0] empty)
    let probe_set = sets.first().cloned().unwrap_or_default();
    let probe_exprs: Vec<Expr> = probe_set.iter().map(|&i| universe[i].clone()).collect();
    let mut probe_stmt = stmt.clone();
    probe_stmt.group_by = if probe_exprs.is_empty() {
        GroupByClause::None
    } else {
        GroupByClause::Simple(probe_exprs)
    };
    probe_stmt.order_by.clear();
    probe_stmt.limit = None;
    probe_stmt.offset = None;
    probe_stmt.distinct = false;
    probe_stmt.calc_found_rows = false;
    let out_cols_template = match execute_select_grouped_hash(probe_stmt, combined_rows.clone())? {
        QueryResult::Rows { columns, .. } => columns,
        other => return Ok(other),
    };

    // Run one pass per grouping set
    let n_universe = universe.len();
    let mut all_rows: Vec<Row> = Vec::new();

    for set_indices in &sets {
        // Build a stmt for this specific grouping set
        let set_exprs: Vec<Expr> = set_indices.iter().map(|&i| universe[i].clone()).collect();
        let mut pass_stmt = stmt.clone();
        pass_stmt.group_by = if set_exprs.is_empty() {
            GroupByClause::None
        } else {
            GroupByClause::Simple(set_exprs)
        };
        pass_stmt.order_by.clear();
        pass_stmt.limit = None;
        pass_stmt.offset = None;
        pass_stmt.distinct = false;
        pass_stmt.calc_found_rows = false;
        // HAVING is kept per-pass (SQL standard)

        let pass_rows = match execute_select_grouped_hash(pass_stmt, combined_rows.clone())? {
            QueryResult::Rows { rows, .. } => rows,
            // empty result is fine
            QueryResult::Affected { .. } => vec![],
            other => return Ok(other),
        };

        // Compute grouping mask for this set: bit i = 1 if universe[i] NOT in set
        let mut mask: u64 = 0;
        for i in 0..n_universe {
            if !set_indices.contains(&i) {
                mask |= 1u64 << i;
            }
        }

        // Null out SELECT positions for absent universe exprs, then append mask
        let mut pass_rows = pass_rows;
        for row in pass_rows.iter_mut() {
            // Null out rolled-up positions
            for (ui, slots) in select_slots_for_universe.iter().enumerate() {
                if !set_indices.contains(&ui) {
                    for &slot in slots {
                        if slot < row.len() {
                            row[slot] = Value::Null;
                        }
                    }
                }
            }
            // Inject hidden grouping mask as last column
            row.push(Value::BigInt(mask as i64));
        }

        all_rows.extend(pass_rows);
    }

    // Post-union: DISTINCT, ORDER BY (may reference GROUPING()), LIMIT/OFFSET
    if stmt.distinct {
        // Strip mask before dedup (mask is internal, not part of user output)
        for row in all_rows.iter_mut() { row.pop(); }
        all_rows = apply_distinct_with_session(all_rows);
        // Mask gone; GROUPING() in ORDER BY won't work after DISTINCT (edge case, acceptable)
    } else {
        let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
        // ORDER BY may include GROUPING() exprs — evaluator reads last column (mask)
        all_rows = apply_order_by_with_grouping_mask(all_rows, &remapped_ob, n_universe)?;
        // Strip mask after ORDER BY
        for row in all_rows.iter_mut() { row.pop(); }
    }

    if stmt.calc_found_rows {
        set_found_rows(all_rows.len() as u64);
    }
    all_rows = apply_limit_offset(all_rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows { columns: out_cols_template, rows: all_rows })
}
```

> `apply_order_by_with_grouping_mask` is a thin wrapper around `apply_order_by` that evaluates
> `Expr::GroupingResolved { universe_indices }` against the mask in `row.last()`.

### Integration tests (in `tests/integration_grouping_sets.rs`)

```rust
// 1. ROLLUP(a, b) — 3 sets: {a,b}, {a}, {}
fn test_rollup_two_columns();
// 2. ROLLUP(a) — 2 sets: {a}, {}
fn test_rollup_single_column();
// 3. CUBE(a, b) — 4 sets: {a,b},{a},{b},{}
fn test_cube_two_columns();
// 4. GROUPING SETS explicit: ((a,b),(a),())
fn test_grouping_sets_explicit();
// 5. Grand total row has NULLs in all group keys
fn test_grand_total_row_nulls();
// 6. HAVING per-pass (filters within each set)
fn test_having_per_pass();
// 7. Mixed GROUP BY a, ROLLUP(b,c) — cross-product
fn test_mixed_plain_and_rollup();
// 8. ORDER BY applied post-union
fn test_order_by_post_union();
// 9. LIMIT applied post-union
fn test_limit_post_union();
// 10. MySQL WITH ROLLUP still works (regression)
fn test_with_rollup_mysql_regression();
// 11. GROUPING SETS with duplicate sets produces duplicate rows
fn test_grouping_sets_duplicate_sets();
// 12. Data NULLs in group column: not confused with rolled-up NULLs
fn test_real_null_not_confused();
```

### Verification

```bash
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo nextest run -p axiomdb-sql --test integration_grouping_sets 2>&1"
```

### Commit

```
feat(fase-21): executor multi-pass aggregation for GROUPING SETS/ROLLUP/CUBE

Step 3 of specs/fase-21/plan-21.21-grouping-sets.md
```

---

## Step 4 — GROUPING() function + docs + close

**Goal:** Add `GROUPING(expr, ...)` function: AST node, analyzer resolution, evaluator.
Then update wire-test, docs, progreso.md, and commit.

**Files:** `ast.rs`, `analyzer_stmt.rs`, eval functions, `parser/expr.rs`,
`tools/wire-test.py`, `docs-site/`, `docs/progreso.md`

### AST

```rust
// ast.rs — add to Expr enum:

/// SQL standard `GROUPING(expr, ...)` function.
/// Before analysis: holds the raw argument expressions.
/// After analysis (resolved): holds universe indices (into GroupByClause::Sets.universe).
/// Outside a GROUPING SETS context: always evaluates to 0.
Grouping {
    /// Raw args before analysis, universe indices after analysis.
    args: Vec<Expr>,
    /// Populated by analyzer: index of each arg in the current query's universe.
    /// None means "not yet resolved" (pre-analysis) or "not in a Sets context".
    resolved_indices: Option<Vec<usize>>,
},
```

### Parser

In `parser/expr.rs`, add `GROUPING` to the function-like keyword dispatch:

```rust
// When parsing a primary expression, check for GROUPING(...)
if p.peek_ident_ci("GROUPING") && matches!(p.peek_at(1), Token::LParen) {
    p.advance(); // GROUPING
    p.advance(); // (
    if p.peek() == &Token::RParen {
        return Err(ParseError::new("GROUPING() requires at least one argument"));
    }
    let args = parse_expr_list(p)?;
    p.expect(&Token::RParen)?;
    return Ok(Expr::Grouping { args, resolved_indices: None });
}
```

### Analyzer

In `analyzer_stmt.rs`, inside `resolve_expr_full`, handle `Expr::Grouping`:

```rust
Expr::Grouping { args, .. } => {
    // Resolve each arg
    let resolved_args: Vec<Expr> = args.into_iter()
        .map(|a| resolve_expr_full(a, ctx, outer_scopes, agg_state))
        .collect::<Result<_, _>>()?;

    // Try to find the current query's universe
    let universe = match ctx.grouping_universe() {
        Some(u) => u,  // Vec<Expr> stored in BindContext during group_by resolution
        None => {
            // Not in a Sets context — GROUPING() always returns 0
            return Ok(Expr::Grouping { args: resolved_args, resolved_indices: Some(vec![]) });
        }
    };

    // Match each arg against universe
    let mut indices = Vec::new();
    for arg in &resolved_args {
        match universe.iter().position(|u| u == arg) {
            Some(idx) => indices.push(idx),
            None => return Err(AnalysisError::new(format!(
                "GROUPING() argument must be a GROUP BY expression"
            ))),
        }
    }
    Ok(Expr::Grouping { args: resolved_args, resolved_indices: Some(indices) })
}
```

`ctx.grouping_universe()`: small addition to `BindContext` — a `Option<Vec<Expr>>` field,
populated in the `GroupByClause::Sets` branch of group_by resolution in `analyzer_stmt.rs`.

### Evaluator

In `eval/functions/` (or directly in the main `eval_expr`):

```rust
Expr::Grouping { resolved_indices, .. } => {
    let indices = match resolved_indices {
        Some(v) if !v.is_empty() => v,
        _ => return Ok(Value::Int(0)),  // outside Sets context
    };
    // Read hidden mask from last column of current row
    let mask = match row.last() {
        Some(Value::BigInt(m)) => *m as u64,
        Some(Value::Int(m)) => *m as u64,
        _ => 0u64,
    };
    // bitmask: leftmost arg = MSB (PostgreSQL compatible)
    let n = indices.len();
    let mut result: u64 = 0;
    for (i, &universe_idx) in indices.iter().enumerate() {
        if universe_idx < 64 && (mask >> universe_idx) & 1 == 1 {
            result |= 1u64 << (n - 1 - i);
        }
    }
    Ok(Value::BigInt(result as i64))
}
```

### Additional integration tests

```rust
// 13. GROUPING(a) = 1 on grand-total row, 0 on detail rows
fn test_grouping_single_arg();
// 14. GROUPING(a, b) bitmask: a-rolled/b-active → 2
fn test_grouping_two_args_bitmask();
// 15. ORDER BY GROUPING(region) puts detail rows first, grand total last
fn test_order_by_grouping();
// 16. HAVING GROUPING(a) = 0 filters out grand total rows
fn test_having_grouping_filter();
```

### Wire test assertions

```python
# tools/wire-test.py — add [21.21] section
# ROLLUP basic
assert_rows("SELECT a, b, SUM(v) FROM gs_tbl GROUP BY ROLLUP(a,b)", 6)  # 3 detail + 2 subtotal + 1 grand

# GROUPING() function
assert_query(
    "SELECT a, SUM(v), GROUPING(a) FROM gs_tbl GROUP BY ROLLUP(a)",
    expected_last_row_grouping=1  # grand total
)

# MySQL WITH ROLLUP regression
assert_rows("SELECT a, SUM(v) FROM gs_tbl GROUP BY a WITH ROLLUP", 3)  # 2 groups + 1 grand
```

### Docs update

`docs-site/src/user-guide/sql-reference/dml.md`:
- Add "GROUPING SETS / ROLLUP / CUBE (Phase 21.21)" section with syntax table + examples

`docs-site/src/internals/sql-parser.md`:
- Add section on `GroupByClause` enum design, multi-pass execution, `__grouping_mask__` mechanism, GROUPING() resolution

### Final verification

```bash
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo nextest run --workspace 2>&1 | tail -10 &&
  cargo clippy --workspace -- -D warnings 2>&1 | tail -5"
cargo fmt  # locally (virtiofs read-only)
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo fmt --check 2>&1"
```

Wire test (pre-flight mandatory):
```bash
pkill axiomdb-server 2>/dev/null; sleep 0.5
limactl shell axiomdb -- bash -c "
  source ~/.cargo/env
  CARGO_TARGET_DIR=\$HOME/axiomdb-target \
  cargo build --release -p axiomdb-server 2>&1 | tail -3"
rm -f /Users/cristian/nexusdb/target/release/axiomdb-server
./target/release/axiomdb-server &
sleep 1
python3 tools/wire-test.py
```

### Commit

```
feat(fase-21): close 21.21 GROUPING SETS / ROLLUP / CUBE

Implements specs/fase-21/spec-21.21-grouping-sets.md
Plan: specs/fase-21/plan-21.21-grouping-sets.md
Tests: 16 new tests in tests/integration_grouping_sets.rs
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `execute_select_grouped_hash` doesn't handle `GroupByClause::None` (no GROUP BY, just aggs) | medium | Add guard: if `group_by.is_empty()`, treat as full-table aggregate (already done in existing code) |
| HAVING applied pre-projection vs post — interaction with Sets pass | medium | HAVING is evaluated inside `execute_select_grouped_hash` which is called per-pass — naturally per-pass |
| `resolve_positional_group_by` receives universe exprs but ROLLUP uses positional refs | low | Positional GROUP BY (GROUP BY 1) inside ROLLUP() is unusual — reject at parse time |
| Hidden mask column length mismatch if row is shorter than expected | low | Guard `row.last()` check in evaluator |
| `apply_order_by` not aware of hidden mask column length | medium | Strip mask after ORDER BY (safe since ORDER BY is done before strip) |

## Rollback plan

1. `git reset --hard <commit before Step 1>`
2. Or branch `abandoned/plan-grouping-sets-<date>`
3. Revert spec status to `draft`

## Estimated effort

Total: ~6-8 hours
- Step 1 (AST migration): 1.5h
- Step 2 (parser): 1.5h  
- Step 3 (executor): 2.5h
- Step 4 (GROUPING() + docs + close): 2h
