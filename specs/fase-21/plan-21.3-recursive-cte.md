# Plan: 21.3 WITH RECURSIVE

## Scope MVP

Single recursive CTE per WITH list. Body must be exactly:
`SELECT base UNION [ALL] SELECT step`. Referenced in main query's
FROM directly (not inside nested subquery). Iteration caps at
MAX_RECURSION=1000.

## Files

### Modify

- `ast.rs`:
  - `CteBinding.recursive: bool` (default false).
  - `SelectStmt.recursive_with: bool` — set by parser when
    `WITH RECURSIVE` seen.
  - New `FromClause::RecursiveCte(Box<RecursiveCteClause>)` with
    `alias`, `column_names`, `base`, `step`, `union_all`.
- `parser/dml.rs`:
  - `parse_dml` detects `Token::With` + RECURSIVE keyword, sets
    flag on resulting SELECT and on each CteBinding.
  - `parse_cte_list` unchanged.
- `analyzer_stmt.rs::expand_ctes`:
  - For each `CteBinding` with `recursive=true`:
    - Validate body shape: must be `Stmt::SetOp` with one tail,
      kind=Union. Extract base SELECT and step SELECT.
    - Analyze base (no self-ref).
    - Derive column schema from base select-list.
    - Store synthetic `stub_cte` binding (points to a zero-row
      version of base) in the dict so step analysis resolves
      self-refs.
    - Analyze step with the stub in scope.
    - Store a `RecursiveCteClause` node in a new dict keyed by CTE
      name.
  - Substitution pass: if `FromClause::Table` matches recursive CTE,
    rewrite to `FromClause::RecursiveCte(...)`.
- `executor/select_core.rs`:
  - Detect `FromClause::RecursiveCte(_)`, dispatch to new
    `execute_select_recursive_cte_source`.
  - The function runs the iteration loop (described below) and
    feeds rows through the outer SELECT projection.
- `executor/select_ctx.rs`: same delegation.
- Other match sites (`analyzer_bind`, `select_joins_ctx`,
  `dml_join`, `select_helpers`, `exec_explain`, `plan_deps`,
  parser UPDATE/DELETE rejection): minimal arms — bind a virtual
  BoundTable with the declared columns; reject in executor join
  paths (recursive CTE in JOIN right side is out of scope).

### Create

- `tests/integration_recursive_cte.rs` — 7+ tests.

## Iteration algorithm

```rust
const MAX_RECURSION: usize = 1000;

fn execute_recursive(
    clause: &RecursiveCteClause,
    exec_ctx, ctx,
) -> Result<Vec<Row>> {
    // 1. base.
    let mut rt: Vec<Row> = execute_select_inline(&clause.base, ...)?;
    let mut wt = rt.clone();
    let mut seen: HashSet<Vec<Value>> = if !clause.union_all {
        rt.iter().cloned().collect()
    } else { HashSet::new() };

    for depth in 0..MAX_RECURSION {
        if wt.is_empty() { return Ok(rt); }
        // Substitute cte_name → VALUES(wt) inside step.
        let step = inline_wt_into_step(&clause.step, &clause.alias, &wt, &clause.column_names)?;
        let new_rows = execute_select_inline(&step, ...)?;
        let filtered: Vec<Row> = if clause.union_all {
            new_rows
        } else {
            new_rows.into_iter().filter(|r| seen.insert(r.clone())).collect()
        };
        if filtered.is_empty() { return Ok(rt); }
        rt.extend(filtered.clone());
        wt = filtered;
    }
    Err(DbError::Other(format!(
        "recursive CTE exceeded MAX_RECURSION={}", MAX_RECURSION
    )))
}
```

`inline_wt_into_step`: rewrite step's AST so references to the CTE
name become `FromClause::Values { rows: wt_as_literals, alias, cols }`.
Clone step, walk FROM + joins, replace, re-run analyzer (cheap
because catalog already known).

Simpler alt: keep step pre-analyzed once (schema-only); at each
iteration, bind working set via a thread-local or pass it explicitly
as a parameter. Avoids re-analysis per iteration.

For MVP, re-analysis per iter is acceptable (cost = iteration count ×
analyzer cost, bounded by MAX_RECURSION).

## Tests

1. `counter_base_case_only` — base with no self-ref works.
2. `counter_1_to_10` — loop to 10.
3. `counter_union_all_preserves_dups` — duplicate emissions kept.
4. `counter_union_dedups` — UNION (no ALL) removes duplicates.
5. `tree_two_levels` — emp hierarchy.
6. `max_recursion_exceeded_errors` — infinite recursion hits cap.
7. `with_without_recursive_keyword_rejects_self_ref` — error points
   to 21.3.
8. `regression_non_recursive_still_works`.

## Risks

- Step analysis needs CTE name resolvable. Solution: stub binding
  with same schema as base; executor replaces at iteration time.
- Dedup cost for UNION (not ALL) on large results: HashSet of row
  tuples; acceptable for MVP; optimize later.
- Infinite recursion: MAX_RECURSION cap + clear error.
- Step referencing outer columns: not supported — recursive CTE
  scope is just the CTE name.
