# Plan: 11.20d3 — LATERAL-correlated JSON_TABLE

## Files to create/modify

### Modify

- `crates/axiomdb-sql/src/analyzer_stmt.rs::resolve_json_table`
  — add resolution of `jt.passing` exprs using the same
  `resolve_expr_full(expr, ctx, outer_scopes, state)` call the `doc`
  already uses. Today PASSING exprs are never resolved at the
  JSON_TABLE call site (they were fine in 11.20d1 because only
  literals / params were used). Required so correlated PASSING exprs
  pick up their outer-column bindings.
- `crates/axiomdb-sql/src/lexer.rs` — add `Token::Lateral`
  (`#[token("LATERAL", ignore(ascii_case))]`).
- `crates/axiomdb-sql/src/parser/dml.rs`:
  - `parse_from_item` — accept optional `LATERAL` prefix before
    `JSON_TABLE(...)` and before `(` (subquery). Consume and discard
    (no-op).
  - `parse_join_clauses` — accept optional `LATERAL` after `JOIN`
    (`INNER / LEFT / RIGHT / FULL / CROSS JOIN LATERAL src`). Same
    no-op consumption.
  - `CROSS APPLY` / `OUTER APPLY` (11.20d2) are left unchanged — they
    are semantically already LATERAL, and accepting `LATERAL` after
    `APPLY` would be redundant / non-standard.
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`:
  - Replace the current `NotImplemented 11.20d3` early-return for
    correlated JSON_TABLE with a new correlated branch. Structure:

    ```rust
    let jt_correlated = doc_has_column_refs(&jt.doc)
        || jt.passing.iter().any(|(e, _)| doc_has_column_refs(e));
    if jt_correlated {
        // buffer an unresolved-right-source marker instead of a flat
        // scanned[right_idx] Vec<Row>.
        ...
    }
    ```

  - Extend the per-join match/loop so that when a right source is
    flagged correlated, the join loop calls a new helper
    `apply_correlated_jt_join(outer_rows, jt_ast, spec, column_metas,
    join_type, condition, left_schema, right_col_offset,
    right_columns, storage, txn)` instead of `apply_join(...,
    &scanned[right_idx], ...)`.

- `crates/axiomdb-sql/src/executor/joins.rs` (or a sibling file):
  - Add `apply_correlated_jt_join(...)` — per-outer-row nested loop
    that:
    1. Evaluates `doc` against `outer_row`.
    2. Evaluates each PASSING expr against `outer_row`.
    3. Calls `materialize_json_table(spec, &sj, outer_row, &mut
       runner)` — already outer-row aware since 11.20d1.
    4. For each right row emitted, tests the ON condition
       (`eval_join_cond`) and appends combined rows.
    5. For LEFT / OUTER APPLY: if zero matches, emit
       `concat_rows(outer_row, &null_right)`.
    6. For INNER / CROSS APPLY / CROSS JOIN: only emit matched
       rows.
    7. For RIGHT / FULL: return `DbError::NotImplemented` with clear
       message ("RIGHT/FULL JOIN LATERAL on correlated JSON_TABLE
       unsupported — PG-compatible rejection").

- `crates/axiomdb-sql/src/executor/select_core.rs::execute_select_json_table_source`
  — rename the 11.20d3 placeholder error to the permanent
  `correlated JSON_TABLE requires an outer FROM source`. The guard
  itself stays (first-FROM JT still can't reference outer columns).

### Create

- `crates/axiomdb-sql/tests/integration_json_table_correlated.rs` —
  10–14 integration tests covering every acceptance criterion in the
  spec.

### No changes needed

- `crates/axiomdb-sql/src/json_table.rs` — `materialize_json_table`
  already accepts `outer_row` and builds `PassingEnv` per call. Reuse
  as-is.
- `crates/axiomdb-sql/src/ast.rs` — no new AST variants. `LATERAL`
  keyword is parsed and discarded.
- `apply_join` — untouched; correlated JT right sides bypass it.

## Algorithm / Data structure

### Correlation detection

```rust
fn jsontable_is_correlated(jt: &JsonTable) -> bool {
    doc_has_column_refs(&jt.doc)
        || jt.passing.iter().any(|(expr, _)| doc_has_column_refs(expr))
}
```

Same predicate, reused on both right-side detection and first-FROM
guardrail (where it still rejects).

### Per-outer-row right source

```rust
fn apply_correlated_jt_join(
    left_rows: Vec<Row>,
    jt: &JsonTable,
    spec: &JsonTableSpec,
    right_cols: &[ColumnMeta],
    left_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<Vec<Row>, DbError> {
    let right_col_count = right_cols.len();
    let null_right: Row = vec![Value::Null; right_col_count];
    let mut out = Vec::with_capacity(left_rows.len());
    let mut runner = /* subquery runner over exec_ctx */;
    for outer in &left_rows {
        // doc can contain Expr::Column/OuterColumn resolved to
        // combined-row indices — eval_with reads from outer.
        let doc_val = eval_with(&jt.doc, outer, &mut runner)?;
        let sj = match doc_to_serde(&doc_val)? {
            None => { handle_null_doc(); continue; }
            Some(v) => v,
        };
        let right_rows = materialize_json_table(spec, &sj, outer, &mut runner)?;
        let mut matched = false;
        for right in &right_rows {
            let combined = concat_rows(outer, right);
            if eval_join_cond(condition, &combined, left_schema,
                              right_col_offset, right_cols)? {
                out.push(combined);
                matched = true;
            }
        }
        if !matched && matches!(join_type, JoinType::Left) {
            out.push(concat_rows(outer, &null_right));
        }
    }
    match join_type {
        JoinType::Right | JoinType::Full => {
            return Err(DbError::NotImplemented {
                feature: "RIGHT/FULL JOIN on correlated JSON_TABLE \
                          (outer re-scan required — PG rejects too)"
                    .into(),
            });
        }
        _ => {}
    }
    Ok(out)
}
```

NULL doc semantics: PG / MariaDB behavior for non-correlated is
"zero rows". We keep that for correlated too — for LEFT, NULL doc
→ NULL-pad the outer row; for INNER / CROSS APPLY, NULL doc → outer
row skipped.

### LATERAL no-op parser

```
from_item :=
    [ LATERAL ] json_table_call [ alias ]
  | [ LATERAL ] '(' select ')' alias
  | ...

join_item :=
    join_type 'JOIN' [ LATERAL ] source ...
  | CROSS APPLY / OUTER APPLY source          -- no LATERAL after APPLY
```

Parser eats the token and proceeds as today. Optional `lateral: bool`
flag on `FromClause::JsonTable` / `FromClause::Subquery` recorded
for future use (not consulted by executor).

## Implementation phases

1. **Analyzer — PASSING expr resolution (≈6 LoC).** Thread
   `resolve_expr_full` through `jt.passing`. Add 1 unit test
   ensuring a PASSING outer-column ref resolves (surfaces a
   `Expr::Column` / `Expr::OuterColumn` node after analysis).

2. **Parser — LATERAL keyword (≈20 LoC).** Add token. Accept
   optional `LATERAL` in `parse_from_item` and at the right-source
   position of `parse_join_clauses`. Store `lateral` flag on
   `FromClause::JsonTable` / `FromClause::Subquery`.

3. **Executor — correlated JT branch (≈100 LoC).**
   - Add `jsontable_is_correlated(&jt)` helper.
   - Change the join-loop source buffering so correlated right sides
     don't prefetch into `scanned[right_idx]` — instead tag them and
     defer to `apply_correlated_jt_join` during the combine step.
   - Implement `apply_correlated_jt_join` as above.
   - Rename first-FROM guardrail message in
     `execute_select_json_table_source`.

4. **Integration tests (≈14 tests).** File
   `tests/integration_json_table_correlated.rs`:
   - `cross_apply_correlated_doc_basic`
   - `outer_apply_correlated_doc_empty_preserves_left`
   - `inner_join_correlated_doc_with_on_condition`
   - `left_join_correlated_doc_null_pad`
   - `passing_outer_column_into_filter`
   - `passing_two_outer_columns_into_range_filter`
   - `correlated_doc_null_skips_inner_emits_left`
   - `correlated_doc_with_nested_path`
   - `correlated_doc_with_wrapper_on_column`
   - `lateral_keyword_accepted_on_json_table`
   - `lateral_keyword_accepted_on_join`
   - `right_join_correlated_jt_rejected`
   - `full_join_correlated_jt_rejected`
   - `first_from_correlated_doc_rejected` (renamed error)

5. **Wire smoke (≈3 assertions).**
   - correlated CROSS APPLY basic
   - correlated OUTER APPLY NULL-pad
   - correlated PASSING into filter

6. **Close protocol.**
   - `cargo test --workspace`, clippy, fmt.
   - Docs: `docs-site/src/internals/sql-parser.md`,
     `docs-site/src/user-guide/sql-reference/dml.md` (LATERAL +
     correlated example paragraphs).
   - `docs/fase-11.md`, `docs/progreso.md`.
   - `memory/architecture.md`, `memory/project_state.md`.
   - Commit + push.

## Tests to write

See phase 4 above. In addition, regression check the full
11.20a/b/c/d1/d2 suites — correlated detection must not misfire on
non-correlated calls (guard: `doc_has_column_refs` already works
correctly on literal/param-only docs).

## Anti-patterns to avoid

- **Don't modify `apply_join`.** Correlated JT right sides go
  through a sibling function; the batch path stays clean.
- **Don't try to hash-join correlated right sides.** The whole point
  of correlation is per-outer-row materialization; building a full
  `right_rows` up front defeats it.
- **Don't allow correlated `doc` on first-FROM JT.** It's a clear
  semantic error (no outer source). Rename the placeholder, keep
  the guard.
- **Don't let LATERAL enable correlated subqueries** in this phase.
  Subquery LATERAL is a separate, larger analyzer refactor
  (outer-scope exposure to derived-table SELECT). LATERAL here is
  purely a no-op sugar.
- **Don't silently accept `LATERAL CROSS APPLY` / `LATERAL OUTER
  APPLY`.** APPLY is already semantically LATERAL; adding
  `LATERAL` in front would be a parse error for clarity.
- **Don't forget RIGHT/FULL rejection.** PG rejects correlated
  right-side LATERAL as RIGHT/FULL; AxiomDB matches. Clear error
  message, not a panic.
- **Don't re-evaluate PASSING per-row when non-correlated.**
  `materialize_json_table` rebuilds the env every call — that's
  fine; the non-correlated path still calls it exactly once.

## Risks

- **`doc_has_column_refs` false positives.** The predicate returns
  true for `Expr::Column / OuterColumn / InsertValue` anywhere in
  the tree. It is already used in 11.20d2 for the same detection
  and has no known false positives on resolved exprs. Low risk.
- **PASSING resolution breaking 11.20d1 tests.** Adding
  `resolve_expr_full` on `jt.passing` could surface a binding error
  on a previously-accepted expression. Mitigation: resolve before
  the JSON_TABLE-as-first-FROM guardrail so existing literal-only
  PASSING tests flow through unchanged (literals / params resolve
  to themselves with no context).
- **Subquery runner lifetime.** `apply_correlated_jt_join` needs an
  `ExecSubqueryRunner` threaded from `execute_select_with_joins_ctx`
  (cache / bloom / outer-row bookkeeping). The existing join loop
  already has access via `exec_ctx` + `ctx` — pass them through.
- **LEFT / OUTER APPLY cardinality blow-up** when a huge outer is
  paired with a huge per-outer JT. Unchanged behavior from
  non-correlated: bounded by user SQL. Not a correctness risk.
- **LATERAL keyword collisions.** LATERAL is not an existing
  identifier in the codebase; PG reserves it as a keyword. Mitigation:
  lex as keyword (same pattern as `APPLY` in 11.20d2). Users with
  a column named `lateral` would need to quote — acceptable for
  parity with PG.
