# Plan: 21.22 — VALUES as inline table

## Files

### Modify

- `crates/axiomdb-sql/src/ast.rs` — add `FromClause::Values(Box<ValuesClause>)` +
  `ValuesClause { rows, alias, column_names }`.
- `crates/axiomdb-sql/src/parser/dml.rs::parse_from_item` — inside
  the existing `if p.eat(&Token::LParen)` subquery branch, peek for
  `Token::Values` before `Token::Select` and dispatch to a new
  `parse_values_inline` helper.
- All `FromClause` match sites need a `FromClause::Values(_)` arm
  (same pattern as 11.25a `FromClause::JsonbSrf`):
  - `analyzer_bind.rs::bound_from_clause` — publish a BoundTable with
    the declared column names + inferred types from the first row.
  - `analyzer_stmt.rs::analyze_select_with_outer` — resolve each
    expression in each row against an **empty** scope (no outer
    correlation — consistent with 11.25a SRF). First-FROM and
    join-side paths both need the arm.
  - `analyzer_ddl.rs::analyze_update` / `analyze_delete` — join-side
    arm resolves exprs against empty scope; target-side rejection.
  - `executor/select_core.rs::execute_select` — new
    `execute_select_values_source` for first-FROM (with / without
    joins).
  - `executor/select_ctx.rs` — delegate to `execute_select`.
  - `executor/select_joins_ctx.rs` — join-side arm evaluates rows and
    feeds into the combine loop.
  - `executor/dml_join.rs` — same for UPDATE/DELETE joins.
  - `executor/select_helpers.rs` — non-ctx JOIN path NotImplemented
    arm (same pattern as JsonbSrf).
  - `executor/exec_explain.rs` — NotImplemented arm.
  - `parser/dml.rs::parse_update` / `parse_delete` — target
    rejection arm.
  - `plan_deps.rs::visit_from_clause` — recurse into row expressions.

### Create

- `crates/axiomdb-sql/src/values_clause.rs` — small module:
  - `fn materialize_values(rows: &[Vec<Expr>]) -> Result<Vec<Vec<Value>>>` —
    evaluate each expr against empty row.
  - `fn column_metas_for_values(vc: &ValuesClause) -> Vec<ColumnMeta>`
    — types inferred from first row, names from `column_names` or
    `column1..N` default.
  - `fn column_defs_for_values(vc: &ValuesClause) -> Vec<ColumnDef>`
    — analyzer binding shape.

- `crates/axiomdb-sql/tests/integration_values_inline.rs` — 8 tests.

## Algorithm

Parser for `(VALUES (…), (…)) [AS] alias [(cols)]`:

```rust
// Inside the existing `if p.eat(&Token::LParen)` branch of parse_from_item:
if matches!(p.peek(), Token::Values) {
    p.advance();
    let mut rows: Vec<Vec<Expr>> = Vec::new();
    loop {
        p.expect(&Token::LParen)?;
        let mut row = vec![parse_expr(p)?];
        while p.eat(&Token::Comma) {
            row.push(parse_expr(p)?);
        }
        p.expect(&Token::RParen)?;
        if !rows.is_empty() && row.len() != rows[0].len() {
            return Err(parse_err("VALUES: inconsistent row width"));
        }
        rows.push(row);
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    p.eat(&Token::As);
    let alias = p.parse_identifier()?;
    let column_names = if p.eat(&Token::LParen) {
        let mut cols = vec![p.parse_identifier()?];
        while p.eat(&Token::Comma) {
            cols.push(p.parse_identifier()?);
        }
        p.expect(&Token::RParen)?;
        Some(cols)
    } else {
        None
    };
    return Ok(FromClause::Values(Box::new(ValuesClause {
        rows, alias, column_names,
    })));
}
// Otherwise fall through to the existing SELECT subquery branch.
```

Materialization at execution time:

```rust
pub fn materialize_values(rows: &[Vec<Expr>]) -> Result<Vec<Vec<Value>>> {
    rows.iter()
        .map(|row| row.iter().map(|e| crate::eval::eval(e, &[])).collect())
        .collect()
}
```

Column metadata:

```rust
pub fn column_metas_for_values(vc: &ValuesClause) -> Vec<ColumnMeta> {
    let n = vc.rows[0].len();
    (0..n).map(|i| {
        let name = vc.column_names.as_ref()
            .and_then(|names| names.get(i).cloned())
            .unwrap_or_else(|| format!("column{}", i + 1));
        // First-row type inference (literal-aware).
        let ty = infer_expr_type(&vc.rows[0][i]);
        ColumnMeta { name, data_type: ty, nullable: true,
                     table_name: Some(vc.alias.clone()) }
    }).collect()
}
```

## Tests

1. `values_basic_two_rows`.
2. `values_with_column_names`.
3. `values_default_column_names`.
4. `values_single_row`.
5. `values_join_with_real_table`.
6. `values_inconsistent_width_errors`.
7. `values_in_update_join_rejected_target` — UPDATE where target is
   VALUES is rejected.
8. `values_from_join_drives_where` — VALUES on right side filters
   rows.

Wire smoke: 1 assertion — VALUES + JOIN.

## Phases

1. AST variant + ValuesClause struct.
2. Parser dispatch in parse_from_item.
3. Match-arm additions across all FromClause consumers (~10 sites).
4. New values_clause.rs module with materialize + metas.
5. Executor wiring (first-FROM + join-side SELECT + DML).
6. Tests.
7. Close protocol.

## Anti-patterns

- Don't desugar to UNION ALL of SELECTs — it's possible but adds
  encoding complexity for multi-row cases. Native variant is
  cleaner.
- Don't allow UPDATE/DELETE target to be VALUES — there's no RID.
- Don't support correlated VALUES in this subphase (expressions
  referencing outer columns). Consistent with 11.25a non-correlated
  SRF path.

## Risks

- Type inference from first row: if first row has NULL as a column,
  type is unknown. Default to TEXT (lenient). Subsequent rows could
  tighten — defer that pass to a future subphase.
- Column-count mismatch across rows: parse-time error, clean.
- Mixing Values and other non-table sources in deeply nested joins:
  tested via the existing JoinsSchema machinery; no new code path.
