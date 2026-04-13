# Plan: 11.25a — JSONB SRF

## Files

### Modify

- `crates/axiomdb-sql/src/ast.rs` — add `JsonbSrfKind` enum,
  `JsonbSrf` struct, `FromClause::JsonbSrf(Box<JsonbSrf>)` variant.
- `crates/axiomdb-sql/src/parser/dml.rs::parse_from_item` — after
  the JSON_TABLE dispatch, add a similar one for the five SRF
  names. Consume `ident(expr)` and optional `[AS] alias`.
- `crates/axiomdb-sql/src/analyzer_bind.rs::bound_table_from` —
  add a `FromClause::JsonbSrf` arm that publishes a `BoundTable`
  with the per-kind virtual columns.
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — resolve `srf.doc`
  against outer scope (first-FROM and join-side paths), following
  `resolve_json_table` pattern.
- `crates/axiomdb-sql/src/analyzer_ddl.rs` — same for
  `analyze_update` / `analyze_delete` join-side paths.
- `crates/axiomdb-sql/src/executor/select_core.rs`:
  - `execute_select` / `execute_select_ctx` — dispatch on
    `FromClause::JsonbSrf` at the top, analogous to `JsonTable`.
  - New `execute_select_jsonb_srf_source` (no joins) plus delegate
    to the multi-source helper when joins are present.
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — new
  join-side arm materializing the SRF into `scanned[i]`. Correlated
  SRF (doc references outer cols) → placeholder + tracker →
  `apply_correlated_jsonb_srf_join` helper. Pattern copied from
  JT 11.20d3.
- `crates/axiomdb-sql/src/executor/dml_join.rs` — same arm for
  UPDATE/DELETE.
- `crates/axiomdb-sql/src/executor/joins.rs` — add
  `apply_correlated_jsonb_srf_join` (SELECT path) and
  `apply_correlated_jsonb_srf_dml_join` (DML path) twin helpers.

### Create

- `crates/axiomdb-sql/src/jsonb_srf.rs` — new module. Public:
  - `fn materialize_jsonb_srf(kind, doc_val, outer_row) -> Result<Vec<Row>>`
  - `fn column_metas_for_srf(kind, alias) -> Vec<ColumnMeta>`
  - `fn column_defs_for_srf_ast(srf) -> Vec<ColumnDef>` (used by
    analyzer_bind)
  - `fn srf_is_correlated(srf) -> bool` — thin wrapper around
    `doc_has_column_refs(srf.doc)`.

- `crates/axiomdb-sql/tests/integration_jsonb_srf.rs` — integration
  tests.

### No changes

- `axiomdb-types`, `axiomdb-catalog`, storage — SRF emits in-memory
  rows only.

## Algorithm

```rust
pub fn materialize_jsonb_srf(
    kind: JsonbSrfKind,
    doc_val: &Value,
    _outer_row: &[Value],
) -> Result<Vec<Vec<Value>>, DbError> {
    let sj = match doc_to_serde(doc_val)? {
        None => return Ok(Vec::new()),
        Some(v) => v,
    };
    match kind {
        JsonbSrfKind::Each => {
            let obj = sj.as_object().ok_or_else(|| type_err("jsonb_each"))?;
            Ok(obj.iter().map(|(k, v)|
                vec![Value::Text(k.clone()), jsonb_from_serde(v)?]
            ).collect())
        }
        JsonbSrfKind::EachText => { same but text projection }
        JsonbSrfKind::ObjectKeys => { obj.keys().map(|k| vec![Text(k)]) }
        JsonbSrfKind::ArrayElements => {
            let arr = sj.as_array().ok_or_else(|| type_err("jsonb_array_elements"))?;
            arr.iter().map(|v| vec![jsonb_from_serde(v)?]).collect()
        }
        JsonbSrfKind::ArrayElementsText => { same but text }
    }
}
```

Helpers `jsonb_from_serde` and text projection reuse existing
`axiomdb_types::jsonb::encode` and `serde_json::Value::to_string`
for text rendering of non-string JSON values (numbers, bools, null).
String JSON values are unquoted in the `_text` variants (same rule
as `->>`).

## Tests

1. `jsonb_each_basic` — SELECT from SRF returns pairs.
2. `jsonb_each_text_scalar_string_unquoted` — TEXT variant strips
   outer quotes on JSON strings.
3. `jsonb_object_keys_basic` — single-column output.
4. `jsonb_array_elements_basic` — array iteration.
5. `jsonb_array_elements_text_unquoted` — TEXT variant.
6. `jsonb_each_on_array_errors` — type mismatch error.
7. `jsonb_array_elements_on_object_errors` — type mismatch.
8. `jsonb_each_null_doc_zero_rows` — NULL → empty result.
9. `srf_join_with_real_table` — non-correlated JOIN.
10. `srf_cross_apply_correlated` — per-row re-materialization.
11. `srf_in_update_join` — DML source.
12. `srf_outer_apply_empty_preserves_left` — OUTER APPLY NULL-pad
    when SRF yields zero rows.

Wire smoke: 2 assertions:
- `jsonb_each` basic
- `jsonb_array_elements` + CROSS APPLY on a real table

## Implementation phases

1. AST variant + parser dispatch (~50 LoC).
2. Analyzer binding + resolve (~40 LoC).
3. `jsonb_srf.rs` module — materialize + metas (~120 LoC).
4. Executor plumbing (SELECT + DML, correlated + non-correlated)
   (~150 LoC).
5. Tests (~200 LoC).
6. Close protocol.

## Anti-patterns

- Don't try to desugar via JSON_TABLE at parse time — JSON_TABLE
  lacks a `FOR KEY` column type, so `jsonb_each` / `jsonb_object_keys`
  cannot be expressed directly. Native dispatch is cleaner.
- Don't introduce new storage / catalog — SRFs are pure row-emitters.
- Don't forget the correlated path: users frequently write
  `SELECT u.id, k, v FROM users u, jsonb_each(u.data)` (PG implicit
  LATERAL), which needs per-outer-row materialization.

## Risks

- PG implicit LATERAL in comma-separated FROM list (`FROM u,
  jsonb_each(u.data)`). AxiomDB doesn't auto-apply LATERAL to
  comma-joined sources today. **Scope note:** this subphase
  supports SRF with explicit `CROSS APPLY` / `JOIN LATERAL`;
  bare-comma implicit LATERAL is deferred (separate subphase).
  Clear error message when the comma-FROM form is used with a
  correlated SRF.
- Text-projection semantics for `_text` variants must unquote
  strings. Existing `->>` operator already does this — reuse the
  same helper.
