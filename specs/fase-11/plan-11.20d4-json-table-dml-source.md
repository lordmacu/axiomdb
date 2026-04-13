# Plan: 11.20d4 — JSON_TABLE as UPDATE/DELETE source

## Files to modify

- `crates/axiomdb-sql/src/executor/dml_join.rs`:
  - `collect_dml_join_candidates_ctx` (line 264) — replace the
    `FromClause::JsonTable` `NotImplemented` arm (line 334) with the
    11.20a/d3 two-branch logic: non-correlated materializes once;
    correlated pushes a placeholder and records into a new
    `correlated_jt: Vec<Option<JsonTableSpec>>` tracker.
  - The combine loop (around line 352) dispatches via the tracker:
    correlated → `apply_correlated_jt_dml_join(...)`, non-correlated
    → existing `apply_dml_join(...)`.
  - Add the helper `apply_correlated_jt_dml_join(...)` — identical
    shape to `apply_correlated_jt_join` in `joins.rs` but on
    `DmlJoinRow` rows. Inner loop: eval `doc` against outer
    `DmlJoinRow.values`, materialize, wrap each into
    `DmlJoinRow { values, target: None }`, combine via
    `concat_dml_join_rows`, test ON, accumulate. LEFT / OUTER APPLY
    NULL-pad unmatched outer; RIGHT / FULL reject with
    `NotImplemented`.
  - `execute_update_join_ctx` (line 15) and `execute_delete_join_ctx`
    (line 191) already drive `collect_dml_join_candidates_ctx`; no
    changes needed there.

### Create

- `crates/axiomdb-sql/tests/integration_json_table_dml.rs` — 8–12
  tests.

### No changes

- `crates/axiomdb-sql/src/json_table.rs` — reuse
  `jsontable_is_correlated`, `materialize_json_table`,
  `column_metas_for_spec`, `doc_to_serde`, `compile_json_table`.
- `crates/axiomdb-sql/src/ast.rs` — unchanged.
- Parser — unchanged.
- `apply_dml_join` — unchanged.

## Algorithm

Identical to 11.20d3 (`apply_correlated_jt_join`) but with
`DmlJoinRow` rows:

```rust
fn apply_correlated_jt_dml_join(
    left_rows: Vec<DmlJoinRow>,
    jt_ast: &JsonTable,
    spec: &JsonTableSpec,
    right_columns: &[ColumnMeta],
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
) -> Result<Vec<DmlJoinRow>, DbError> {
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        return Err(DbError::NotImplemented { ... });
    }
    let null_right = DmlJoinRow {
        values: vec![Value::Null; right_col_count],
        target: None,
    };
    let mut out = Vec::with_capacity(left_rows.len());
    for outer in &left_rows {
        let doc_val = eval(&jt_ast.doc, &outer.values)?;
        let rows = match doc_to_serde(&doc_val)? {
            None => Vec::new(),
            Some(sj) => materialize_json_table(spec, &sj, &outer.values, &mut NoSubquery)?,
        };
        let mut matched = false;
        for values in &rows {
            let right = DmlJoinRow { values: values.clone(), target: None };
            let combined = concat_dml_join_rows(outer, &right);
            if eval_join_cond(condition, &combined.values, left_schema,
                              right_col_offset, right_columns)? {
                out.push(combined);
                matched = true;
            }
        }
        if !matched && matches!(join_type, JoinType::Left) {
            out.push(concat_dml_join_rows(outer, &null_right));
        }
    }
    Ok(out)
}
```

## Tests

`tests/integration_json_table_dml.rs`:

1. `update_join_json_table_basic` — UPDATE with non-correlated JT.
2. `update_cross_apply_correlated_doc` — UPDATE with `CROSS APPLY
   JSON_TABLE(t.col, ...)`.
3. `update_outer_apply_correlated_empty_preserves_row` — OUTER
   APPLY in UPDATE NULL-pads so outer row stays; but with SET
   referencing JT column, becomes `Value::Null` — document this.
4. `update_left_join_json_table_no_match_unchanged` — LEFT JOIN:
   rows with no JT match keep original values.
5. `delete_join_json_table` — DELETE matched rows.
6. `delete_cross_apply_correlated` — DELETE correlated.
7. `update_nested_path_correlated` — NESTED PATH + correlated.
8. `update_passing_outer_column` — PASSING from target table.
9. `update_right_join_jt_rejected` — RIGHT JOIN → NotImplemented.
10. `update_full_join_jt_rejected` — FULL JOIN → NotImplemented.
11. `regression_update_with_subquery_still_works` — non-JT path
    unchanged.
12. `delete_with_subquery_still_works` — same for DELETE.

Wire smoke: 2 assertions under `[11.20d4]`:
- UPDATE with JSON_TABLE JOIN
- DELETE with JSON_TABLE JOIN

## Phases

1. Add `apply_correlated_jt_dml_join` to `dml_join.rs` (or a sibling
   position).
2. Replace the `NotImplemented` arm at line 334 with the two-branch
   logic.
3. Add dispatch in the combine loop using the new tracker.
4. Tests.
5. Close protocol (docs + progreso + memory + commit + push).

## Anti-patterns

- Don't touch `apply_dml_join`.
- Don't allow `UPDATE JSON_TABLE(...) AS j` as first-FROM — JSON
  rows have no RID, can't be modified. Rejected by the general DML
  flow since `execute_update_join_ctx` requires a table target.

## Risks

- Clone cost per-outer for correlated JT: same as SELECT path.
  Acceptable.
- `DmlJoinRow.target` propagation: `concat_dml_join_rows` already
  takes `target` from the left operand, so JSON_TABLE rows (target=
  None) on the right don't clobber the target. Verified in the
  existing subquery path.
