# Plan: 11.19a — SQL/JSON standard query functions (phase A)

## Files to create/modify

**Modify:**

- `crates/axiomdb-sql/src/expr.rs`
  - New `Expr::SqlJsonQuery {
        kind: SqlJsonQueryKind,
        doc: Box<Expr>,
        path: String,
        path_mode: SqlJsonPathMode,
        returning: Option<DataType>,
        on_empty: SqlJsonOnBehavior,
        on_error: SqlJsonOnBehavior,
    }` variant.
  - New enums:
    - `SqlJsonQueryKind { Value, Query, Exists }`
    - `SqlJsonPathMode { Strict, Lax }`
    - `SqlJsonOnBehavior { Null, Error, TrueLit, FalseLit, Unknown,
      Default(Box<Expr>) }`
  - Each derives `Debug, Clone, PartialEq` for AST invariants.

- `crates/axiomdb-sql/src/parser/expr.rs`
  - New special-form branch at the top of `parse_atom` when the
    current token is the Ident `JSON_VALUE` / `JSON_QUERY` /
    `JSON_EXISTS` **and** the next is `LParen`. Parses the full
    SQL:2016 grammar:
    1. expect `LParen`, parse doc expression
    2. expect `Comma`, parse path as StringLit
    3. optional `RETURNING <data_type>` (only for VALUE/QUERY)
    4. optional `ON EMPTY <behavior>` (not for EXISTS)
    5. optional `ON ERROR <behavior>`
    6. expect `RParen`
  - Path string parsed once here: detect `strict ` / `lax ` prefix,
    strip, store `SqlJsonPathMode`, leave the rest as the jsonpath
    string.
  - `parse_sql_json_behavior` helper for the shared `{ERROR | NULL
    | DEFAULT expr | TRUE | FALSE | UNKNOWN}` rule.

- `crates/axiomdb-sql/src/parser/ddl.rs`
  - Reused `parse_data_type` (already exists) for the `RETURNING
    <type>` clause. No changes expected.

- `crates/axiomdb-sql/src/eval/functions/json.rs` (evaluator)
  - `pub(crate) fn eval_sql_json_query(...)` called from
    `crate::eval::core::eval_with` for the new `Expr::SqlJsonQuery`
    variant.
  - Under the hood:
    1. Evaluate `doc` → `serde_json::Value`.
    2. Parse `path` with `parse_jsonpath` (existing) augmented to
       return matches under the chosen mode.
    3. Produce an `SqlJsonOutcome { Matched(Vec<serde_json::Value>),
       Empty, Error(DbError) }`.
    4. Dispatch by kind:
       - `Exists`: `Matched(_)` → true; `Empty` → false; `Error(_)`
         → `on_error` behavior.
       - `Value`: `Empty` → `on_empty`; `Error` → `on_error`; one
         matched scalar → coerce to `returning`; multi-item or
         non-scalar → `on_error`.
       - `Query`: `Empty` → `on_empty`; `Error` → `on_error`; single
         match → return (JSONB by default, TEXT if RETURNING TEXT);
         multi-match → `on_error` (MVP: no wrapper).

- `crates/axiomdb-sql/src/eval/core.rs`
  - Dispatch the new `Expr::SqlJsonQuery` variant in both `eval` and
    `eval_with` (one match arm each).

- `crates/axiomdb-sql/src/expr_to_sql.rs` + `executor/ddl_alter_
  constraint.rs` + `partial_index.rs` + `plan_deps.rs` + 
  `executor/select_ctx.rs` + `executor/shared.rs` + 
  `executor/agg_descriptor.rs` + `executor/agg_group_table.rs`
  - Pattern-match arms for the new leaf variant (mirrors the way we
    added `Expr::InsertValue` and `Expr::OuterColumn` earlier).
    Walk the `doc` subexpression plus every `Default(expr)` inside
    `on_empty` / `on_error`.

- `crates/axiomdb-sql/tests/integration_sql_json_query.rs` (**new**)
  - Coverage matrix — see **Tests** below.

**No change:** lexer (no new tokens), AST column codec, storage.

## Algorithm / Data structure

### Strict vs lax evaluation

The existing `parse_jsonpath` + `execute_jsonpath` do a "lax"-style
walk by default (missing keys silently drop). To implement strict:

```text
execute_sql_json_path(doc, path, mode):
    let steps = parse_jsonpath(path);
    walk(doc, steps):
        for each step:
            match (current, step):
                (Object{m}, Key(k)) if m.contains(k): descend
                (Object{m}, Key(k)) if !m.contains(k):
                    if strict: return Error("missing key")
                    else: return Empty
                (Array{a}, Index(i)) if i in bounds: descend
                (Array{a}, Index(i)) if !in bounds:
                    if strict: return Error("index out of range")
                    else: return Empty
                (Scalar, Key|Index):
                    if strict: return Error("scalar cannot be indexed")
                    else: return Empty
                (Array{a}, Key(k)):  # lax auto-unwrap
                    if strict: return Error("type mismatch")
                    else: expand over each element and collect
    return Matched(results)
```

This is a thin wrapper around the existing executor; we add a
`mode: SqlJsonPathMode` parameter and surface errors rather than
returning early.

### Outcome / behavior dispatch

```rust
match (outcome, kind) {
    (Empty,     Value) => apply_behavior(on_empty),
    (Error(e),  Value) => apply_behavior(on_error, e),
    (Matched(v), Value) => coerce_to_returning(v, returning),

    (Empty,     Query) => apply_behavior(on_empty),
    (Error(e),  Query) => apply_behavior(on_error, e),
    (Matched(v), Query) => emit_query_result(v, returning),

    (Empty,     Exists) => Ok(Value::Bool(false)),
    (Error(e),  Exists) => apply_behavior_exists(on_error, e),
    (Matched(_), Exists) => Ok(Value::Bool(true)),
}
```

### `apply_behavior`

```rust
fn apply_behavior(behavior, err_if_error) -> Result<Value> {
    match behavior {
        Null     => Ok(Value::Null),
        Error    => Err(err_if_error.unwrap_or("empty/error condition")),
        Default(e) => eval(e, row).and_then(coerce_to_returning),
        TrueLit  => Ok(Value::Bool(true)),
        FalseLit => Ok(Value::Bool(false)),
        Unknown  => Ok(Value::Null),       // SQL UNKNOWN ≡ NULL
    }
}
```

### Coercion for `RETURNING`

Reuse `axiomdb_types::coerce::coerce(value, target_type, mode)`.
On coercion failure the evaluator falls back to `on_error`. The
`Default(expr)` result is also routed through the same coercer so a
`DEFAULT '0'` string flowing into `RETURNING INT` yields `0`.

## Implementation phases

1. **Spec → complete.**
2. **Plan → complete.**
3. **AST additions** (~60 LOC): new variant + enums in
   `expr.rs`; propagate the pattern-match arm to the ~8 walker
   sites that currently enumerate every `Expr` variant.
4. **Parser** (~180 LOC): special-form dispatch in `parse_atom`,
   `parse_sql_json_behavior`, path-mode split.
5. **Evaluator** (~200 LOC): outcome dispatch, strict/lax walker,
   coercion routing.
6. **Core `eval` arms** (~40 LOC).
7. **Integration tests** (~500 LOC, ~28 cases).
8. **Close**: progreso.md update, clippy/fmt/workspace clean, commit.

## Tests to write

In `tests/integration_sql_json_query.rs`:

### JSON_VALUE

1. `value_extracts_scalar_as_text_by_default`
2. `value_returning_int_coerces`
3. `value_returning_bool_coerces`
4. `value_returning_date_coerces_from_iso_string`
5. `value_on_missing_key_strict_routes_to_on_error`
6. `value_on_missing_key_lax_routes_to_on_empty`
7. `value_on_empty_default_expression_runs`
8. `value_on_error_default_expression_runs`
9. `value_array_match_routes_to_on_error` (JSON_VALUE is scalar-only)
10. `value_null_doc_returns_null`
11. `value_coercion_failure_routes_to_on_error`
12. `value_clause_ordering_wrong_is_parse_error`

### JSON_QUERY

13. `query_returns_jsonb_by_default`
14. `query_returning_text_renders_json_text`
15. `query_single_array_element_ok`
16. `query_multi_item_result_routes_to_on_error`
17. `query_on_empty_null_default`
18. `query_on_empty_default_expr`

### JSON_EXISTS

19. `exists_true_on_match`
20. `exists_false_on_miss_lax`
21. `exists_on_error_routes_correctly`
22. `exists_on_error_true_literal`
23. `exists_on_error_false_default_literal`

### Path mode

24. `strict_mode_missing_key_raises_via_on_error_default`
25. `lax_mode_missing_key_silent_null`
26. `lax_mode_auto_unwraps_array_for_scalar_extraction`
27. `strict_mode_type_mismatch_raises`

### NULL propagation

28. `null_doc_propagates_to_null_for_all_three_functions`

## Anti-patterns to avoid

- **Do not** treat `JSON_VALUE` as a regular variadic function with
  optional keyword args. The SQL:2016 grammar is positional with
  special clause keywords — use a dedicated `Expr` variant.
- **Do not** default to lax mode. SQL:2016 and PG default to
  **strict**; diverging would silently hide schema errors.
- **Do not** silently wrap multi-item `JSON_QUERY` results in an
  array. That's a PG extension behind `WITH WRAPPER`. MVP routes
  multi-item → `on_error`.
- **Do not** share an `on_empty`/`on_error` instance between
  `Expr::SqlJsonQuery` clones. The `Default(Box<Expr>)` arm owns
  its sub-expression.
- **Do not** inline-parse the jsonpath inside the parser — parsing
  is done at eval time so we can surface path errors through
  `on_error`.

## Risks

1. **AST walker churn**: new `Expr` variants require ~8 files to add
   a pattern arm. Mitigation: identical pattern to the
   `Expr::InsertValue` change in 11.22a / ODKU — the exact file list
   is known, errors compile-time.
2. **Coercion edge cases**: `'true'` string → `BOOL`, `'2024-01-01'`
   → `DATE`, etc. The existing `coerce` routine is already used by
   CAST and has unit tests; reuse it verbatim.
3. **Strict vs lax walker divergence**: the existing
   `execute_jsonpath` may silently drop missing parts. The new
   walker flag must turn those drops into explicit `Error` variants.
   Mitigation: add tests #5, #6, #24 - #27 to nail the contract.
4. **Parser clause ordering error message**: a user writing `ON
   ERROR NULL RETURNING INT` must see a clear error. Mitigation:
   the parser rejects the wrong keyword at each position and the
   error uses the exact production name.
5. **Default expression eval scope**: `ON ERROR DEFAULT x` where `x`
   is a column must see the current row. Mitigation: evaluate the
   `Default(expr)` with the same `row` and `runner` that was passed
   to the outer `eval_with`.
6. **NULL vs SQL UNKNOWN**: the spec distinguishes; both collapse to
   `Value::Null` in our 3VL model. Document the mapping in progreso.
