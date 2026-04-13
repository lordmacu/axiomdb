# Plan: 11.20d1 — `JSON_TABLE` WRAPPER / QUOTES / PASSING

## Architectural decision

**Migrate JSON_TABLE column/row/NESTED paths from the legacy restricted
walker (`parse_restricted_path` + `walk_path_owned`) to the full JSONPath
engine in `eval::functions::json` (`parse_jsonpath` + `execute_jsonpath`
/ `execute_jsonpath_owned`).**

Reasons:

1. `PASSING $var` bindings need a variable-aware evaluator. Adding them
   to the legacy restricted walker AND the full engine would double the
   surface and diverge behavior.
2. The 11.21* suite (`jsonb_path_*`, `@?`, `@@`, `.size()`, `.type()`,
   filters, arithmetic) already lives in the full engine; JSON_TABLE
   users reasonably expect the same path dialect in both places
   (PG parity).
3. `WRAPPER` / `QUOTES` need multi-item results. The restricted walker
   already returns a `Vec`, so no semantic change for paths without
   filters — wildcards/`[*]` already produce multi-hit match sets.

One risk: the full engine's filter-capable walker is slower on trivial
`$.a.b` paths than the restricted walker. Mitigation: filter-free paths
still compile down to `Key`/`Index` step lists, which both walkers
execute identically — there's no realistic perf delta on simple paths.
Criterion regressions on the 11.20 bench (if any) → follow-up, not a
blocker.

## Files to create / modify

### Parser

- `crates/axiomdb-sql/src/parser/json_table.rs` — modify:
  - after `row_path`, parse optional `PASSING passing_item { ',' passing_item }`
  - in `parse_column_def` for regular columns: parse optional
    `WRAPPER` then optional `QUOTES` (reuse helpers pulled out of
    `sql_json_query` — see next bullet).
- `crates/axiomdb-sql/src/parser/sql_json_common.rs` — **new**:
  - `pub(crate) fn parse_wrapper_clause(p: &mut Parser) -> Result<SqlJsonWrapper, DbError>`
  - `pub(crate) fn parse_quotes_clause(p: &mut Parser, returning: Option<DataType>) -> Result<SqlJsonQuotes, DbError>`
  - `pub(crate) fn parse_passing_clause(p: &mut Parser) -> Result<Vec<(Expr, String)>, DbError>`
  (extract from existing code in `parser/expr.rs` SqlJsonQuery parser; re-export for both call sites.)
- `crates/axiomdb-sql/src/parser/expr.rs` — call the new helpers in the
  SqlJsonQuery parser.
- `crates/axiomdb-sql/src/parser/mod.rs` — register new submodule.

### AST

- `crates/axiomdb-sql/src/ast.rs`:
  - `JsonTable { doc, row_path, passing: Vec<(Expr, String)>, columns, alias }`
  - `JsonTableColumn::Regular { name, ty, path, wrapper: SqlJsonWrapper, quotes: SqlJsonQuotes, on_empty, on_error }`
  - enforce: WRAPPER / QUOTES absent → `Without` / `Keep` defaults.

### JSONPath engine — variable support

- `crates/axiomdb-sql/src/eval/functions/json.rs`:
  - `enum FilterSide` gains `Var(String)`.
  - `parse_filter_side_str` accepts `$ident` → `FilterSide::Var(ident)`.
  - `resolve_filter_side` signature becomes
    `(side, node, env: &PassingEnv) -> Option<serde_json::Value>`;
    `Var(name)` → `env.get(name).cloned()` (unknown var → `None`,
    which treats the filter as false on that row, matching PG).
  - Public entry points gain `_env` siblings:
    - `pub(crate) fn execute_jsonpath_env<'a>(root, steps, env: &PassingEnv) -> Vec<&'a serde_json::Value>`
    - `pub(crate) fn execute_jsonpath_owned_env(root, steps, env: &PassingEnv) -> Vec<serde_json::Value>`
    - The existing non-`_env` entry points become thin shims that call
      the `_env` variant with an empty env (zero callers break).
  - `PassingEnv = HashMap<String, Arc<serde_json::Value>>` defined in a
    small new module `eval::functions::json_passing` (or inline if kept
    under 50 LoC).

### Executor — JSON_TABLE

- `crates/axiomdb-sql/src/json_table.rs`:
  - Drop `PathStepOwned`, `parse_restricted_path`, `walk_path_owned`.
  - `JsonTableSpec` / `JsonTableColumnSpec` store `Vec<PathStep>`
    (from `eval::functions::json`) instead of `Vec<PathStepOwned>`.
  - `JsonTableColumnKind::Regular` gains `wrapper: SqlJsonWrapper,
    quotes: SqlJsonQuotes`.
  - `compile_json_table(jt, /* no env change */)` calls the full
    `parse_jsonpath` for every path string; unknown features surface as
    `DbError::ParseError` at compile time.
  - New `CompiledPassing = Vec<(Expr, String)>` attached to
    `JsonTableSpec`.
  - `materialize_json_table(spec, doc, outer_row, sq)`:
    1. evaluate each `(expr, var)` in `spec.passing` once → build
       `PassingEnv` (JSON-ify via `value_to_serde_json`).
    2. walk the row path via `execute_jsonpath_owned_env(doc, row_path, &env)`.
    3. thread `&env` into `emit_rows_rec` → `materialize_regular` →
       `materialize_exists` → each call becomes
       `execute_jsonpath_owned_env(parent, path, env)`.
  - `materialize_regular` applies WRAPPER/QUOTES:
    - if `hits.len() >= 1`: call `apply_wrapper(&hits, wrapper)` (reused
      from `sql_json_query.rs` — needs to be `pub(crate)`).
    - `WrapOutcome::Scalar(v)` with `quotes = Omit` AND `v` is a JSON
      string scalar AND returning type is TEXT → render as
      `Value::Text(literal)`; else go through `serde_to_value_typed`.
    - `WrapOutcome::MultiError` → route via `on_error`.
  - Compile-time error if `WRAPPER`/`QUOTES` appears on Ordinality /
    Exists / Nested columns (guarded in `parse_column_def` already; a
    second guard in `compile_columns_recursive` keeps us safe against
    AST mutations done by analyzer passes).
  - Compile-time error if `OMIT QUOTES` appears on a non-TEXT returning
    type (SQL:2016 §9.42 + PG parity).
  - Duplicate variable names in `PASSING` → compile-time error
    (`DbError::ParseError`).

### Cross-module visibility

- `crates/axiomdb-sql/src/eval/functions/sql_json_query.rs`:
  - `apply_wrapper` and `WrapOutcome` become `pub(crate)` (move into a
    small shared module `sql_json_wrap` if cleanliness requires; inline
    `pub(crate)` is fine otherwise).

### Analyzer

- `crates/axiomdb-sql/src/analyzer_stmt.rs` / `analyzer_bind.rs`:
  - ensure `passing` expressions in `JsonTable` are visited for binding
    and expansion like any other FROM-level expression. For 11.20d1 the
    bindings are independent of outer columns (no correlation), so the
    visitor just needs to type-check and constant-fold.

### Tests

- `crates/axiomdb-sql/tests/integration_json_table_wrapper.rs` — **new**:
  positive + negative matrix described in the spec.
- `tools/wire-test.py` — append one regression case.

### Benchmarks

- `crates/axiomdb-sql/benches/json_table_bench.rs` (if it exists) — add
  one wrapper case to guard against regressions; skip if the bench file
  doesn't exist yet — this subphase's perf surface is narrow.

## Algorithm / data structures

### PASSING env lifecycle

- Compile time: `parse_passing_clause` produces `Vec<(Expr, String)>`.
- Materialize time: evaluate each expression once, JSON-encode the
  resulting `Value`, stash in `PassingEnv`. The env is shared by every
  path walk of that JSON_TABLE invocation.
- Correlation (11.20d3) will later allow `Expr` to reference outer
  columns; the env is then rebuilt per outer row. The current subphase
  builds it ONCE because `outer_row = []` for the first-FROM case.

### Variable resolution in filters

- `FilterSide::Var(name)` in `resolve_filter_side`:
  - `env.get(name)` → `Some(&Arc<Value>)` → `Some(cloned serde_json)`.
  - Missing → `None`. `None` on either side of a comparison makes the
    whole filter false for that row (matches PG `EXECUTE` semantics
    when a var is unresolved in `lax` mode).

### WRAPPER routing

```
materialize_regular(parent, path, ty, wrapper, quotes, on_empty, on_error, env, sq):
  hits = execute_jsonpath_owned_env(parent, path, env)
  if hits.is_empty():                     → on_empty
  wrap = apply_wrapper(&hits, wrapper)
  match wrap:
    WrapOutcome::MultiError               → on_error
    WrapOutcome::Scalar(v):
      if quotes == Omit && v is JSON string && ty == Text:
        return Value::Text(raw_string)
      return serde_to_value_typed(&v, ty)   or on_error on coercion failure
```

## Implementation phases

1. **Engine: FilterSide::Var + env threading** — add variant, extend
   `resolve_filter_side`, add `_env` entry points, shim old ones.
   Landing this first isolates the engine-level change; nothing in
   JSON_TABLE / `sql_json_query` changes yet. Smoke: existing jsonpath
   unit tests pass with `env = empty`.
2. **Parser helpers extracted** — `parser::sql_json_common` +
   refactor `parser/expr.rs` call site. Landing this isolates the
   refactor and keeps its regression surface local.
3. **Parser: JSON_TABLE WRAPPER / QUOTES / PASSING** — wire the
   helpers into `parse_column_def` + `parse_json_table_call`.
4. **AST migration** — update struct definitions + default values;
   fix every existing call site (should be ~4 places under grep).
5. **Executor: migrate to full path engine** — drop
   `parse_restricted_path` / `walk_path_owned`, swap in `parse_jsonpath`
   and `execute_jsonpath_owned_env`. No WRAPPER/QUOTES yet — just env
   threading and path engine swap. Existing 11.20a/b/c integration
   tests must still pass.
6. **Executor: WRAPPER / QUOTES application** — lift `apply_wrapper`
   to `pub(crate)`, apply in `materialize_regular` per the algorithm
   above. Handle `OMIT QUOTES` on non-TEXT types (compile-time error).
7. **Integration tests** — `integration_json_table_wrapper.rs` full
   matrix. Wire-test append.
8. **Close**: run workspace test / clippy / fmt / wire-test; docs.

## Anti-patterns to avoid

- **Do not** add textual `$var` substitution before `parse_jsonpath`.
  That would break escape / quoting semantics for JSON-valued vars and
  leak string-concat-style footguns. Variables are first-class filter
  operands.
- **Do not** try to keep the restricted walker as a "fast path". A
  second divergent walker invites semantic drift (wildcard semantics,
  recursive descent handling, etc.) — and the perf delta on simple
  paths is noise.
- **Do not** allow `PASSING` expressions to reference outer columns in
  this subphase. That requires invalidating the env per outer row;
  11.20d3 will lift the restriction. For now: analyzer rejects any
  `ColumnRef` inside a `PASSING` expression with a clear error
  pointing at 11.20d3.
- **Do not** conflate `WITHOUT ARRAY WRAPPER` with "error on multi-item"
  in the default case. The default is `Without`, i.e. silent single-
  item behaviour; only an explicit multi-item hit under `Without`
  routes to ON ERROR. Preserve the existing 11.20a behaviour for
  unambiguous single-hit paths.
- **Do not** regress 11.20c NESTED rules. The recursive emitter stays
  exactly the same — only the path walker changes.

## Tests to write (integration_json_table_wrapper.rs)

Positive cases (≥ 12):

1. `WITH UNCONDITIONAL ARRAY WRAPPER` on `$.tags[*]` → JSON-array literal.
2. `WITH CONDITIONAL ARRAY WRAPPER` on an already-array match → unwrapped.
3. `WITH CONDITIONAL ARRAY WRAPPER` on a single scalar → wrapped `[x]`.
4. `WITHOUT WRAPPER` on a single-item match → scalar through (no change).
5. `WITHOUT WRAPPER` on multi-item match + `NULL ON ERROR` → NULL.
6. `WITHOUT WRAPPER` on multi-item + `ERROR ON ERROR` → DbError.
7. `KEEP QUOTES` on TEXT + JSON string → `'"hello"'`.
8. `OMIT QUOTES` on TEXT + JSON string → `'hello'`.
9. `PASSING 15 AS min` threaded into `$[*] ? (@.price > $min)`.
10. `PASSING` with two vars, both referenced.
11. `PASSING` visible in a NESTED PATH filter.
12. `PASSING` + `WRAPPER` combined on the same column.

Negative cases (≥ 4):

13. `OMIT QUOTES` on INT column → compile-time error.
14. `PASSING 1 AS a, 2 AS a` → duplicate-name compile-time error.
15. `WRAPPER` on `FOR ORDINALITY` column → compile-time error.
16. Reference to `$undeclared` in a filter → row excluded (filter false);
    no panic, no DbError (lax mode).

## Risks

- **Filter parser strictness**: adding `FilterSide::Var` to
  `parse_filter_side_str` must not break arithmetic precedence or
  identifier parsing. Add unit tests around the parser directly.
- **Path-engine semantic divergence**: a few 11.20a paths may have
  been relying on the restricted walker's quirks (e.g., recursive-
  descent emission order). Running the full 11.20a/b/c integration
  suite after phase 5 is the gate; any drift is a bug in the full
  walker (fix there, not in JSON_TABLE).
- **Clippy**: `apply_wrapper` move to `pub(crate)` may trip missing-
  doc / visibility lints — add a one-line doc comment.

## Effort recommendation for /implement-task

**`max`** — cross-cutting: engine extension (FilterSide::Var + env)
touches a module shared by JSON_VALUE / JSON_QUERY / `jsonb_path_*`
/ `@?` / `@@` (riesgo alto de regresión), plus JSON_TABLE migration
off the restricted walker, plus parser helpers refactor.
