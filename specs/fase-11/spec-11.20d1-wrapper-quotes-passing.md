# Spec: 11.20d1 — `JSON_TABLE` WRAPPER / QUOTES / PASSING

## What to build

Lift three runtime-level JSON path extras from `JSON_QUERY` / `JSON_VALUE`
(Phases 11.19b, 11.19c) into the `JSON_TABLE` grammar and executor:

1. **Per-column `WRAPPER`**: `WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY]
   WRAPPER` / `WITHOUT [ARRAY] WRAPPER` on regular JSON_TABLE columns,
   controlling how multi-item matches from the column PATH are rendered.
2. **Per-column `QUOTES`**: `KEEP QUOTES [ON SCALAR STRING]` / `OMIT QUOTES
   [ON SCALAR STRING]` on regular JSON_TABLE columns, controlling whether
   scalar-string results keep surrounding JSON double-quotes when emitted
   into a TEXT-typed column.
3. **`PASSING` on the row path**: a top-level `PASSING expr AS var [, …]`
   clause attached to the `JSON_TABLE(doc, '$row' COLUMNS(...))` call.
   Bindings are available to the row path AND to every column/NESTED
   path inside `COLUMNS(...)`.

No new evaluation primitives — everything reuses the Phase 11.19b/c
infrastructure (`apply_wrapper`, `apply_quotes`, JSONPath PASSING env).

## Inputs / outputs

### Grammar

```
json_table_call ::= 'JSON_TABLE' '(' doc ',' row_path
                    [ 'PASSING' passing_item { ',' passing_item } ]
                    'COLUMNS' '(' column_def { ',' column_def } ')' ')'

passing_item ::= expr 'AS' identifier

column_def (regular) ::=
      identifier data_type 'PATH' row_path
      [ wrapper_clause ]
      [ quotes_clause ]
      [ on_empty_error ]

wrapper_clause ::=
      'WITH' [ 'CONDITIONAL' | 'UNCONDITIONAL' ] [ 'ARRAY' ] 'WRAPPER'
    | 'WITHOUT' [ 'ARRAY' ] 'WRAPPER'

quotes_clause ::=
      'KEEP' 'QUOTES' [ 'ON' 'SCALAR' 'STRING' ]
    | 'OMIT' 'QUOTES' [ 'ON' 'SCALAR' 'STRING' ]
```

Clause order on regular columns: `PATH` → optional `WRAPPER` → optional
`QUOTES` → optional `ON EMPTY` / `ON ERROR`. Same order PG / Oracle / MySQL
accept.

### AST deltas

- `JsonTable.passing: Vec<(Expr, String)>` (new field; empty by default,
  preserves existing call sites when defaulted).
- `JsonTableColumn::Regular` gains two fields:
  - `wrapper: SqlJsonWrapper` (default `Without`)
  - `quotes: SqlJsonQuotes` (default `Keep`)
- `JsonTableColumn::Exists` / `Nested` / `Ordinality` unchanged — WRAPPER/
  QUOTES are nonsensical on them (SQL:2016 is explicit about this).

### Executor plumbing

`compile_json_table` stores one `CompiledPassingEnv` per JSON_TABLE call.
At row-emission time the materializer evaluates every `(expr, var)` pair
once per OUTER scope (constant bindings from FROM — correlation lives in
11.20d3) and threads the resulting `PassingEnv` into:

- the row-path evaluator,
- the column-path evaluator,
- the NESTED path evaluator (same env, any depth).

For WRAPPER/QUOTES the materializer reuses the existing helpers from
`sql_json_query.rs`:

```
raw_jsonb = eval_jsonpath(doc, col.path, passing_env, path_mode=Lax)
wrapped   = apply_wrapper(raw_jsonb, col.wrapper)
final_val = coerce_wrapped_to(col.ty, wrapped, col.quotes, col.on_empty, col.on_error)
```

## Use cases

### 1 — `WITH UNCONDITIONAL ARRAY WRAPPER` on an array-valued PATH

```sql
SELECT id, tags FROM JSON_TABLE(
  '[{"id":1,"tags":["a","b","c"]}]',
  '$[*]' COLUMNS (
      id   INT  PATH '$.id',
      tags JSON PATH '$.tags[*]' WITH UNCONDITIONAL ARRAY WRAPPER
  )
) AS t;
-- (1, '["a","b","c"]')   ← one row, tags stays an array literal
```

### 2 — `WITHOUT WRAPPER` forces single-item semantics

```sql
SELECT id, first_tag FROM JSON_TABLE(
  '[{"id":1,"tags":["a","b"]}]',
  '$[*]' COLUMNS (
      id        INT  PATH '$.id',
      first_tag TEXT PATH '$.tags[*]' WITHOUT WRAPPER NULL ON ERROR
  )
) AS t;
-- (1, NULL)   ← '$.tags[*]' returns 2 items → ON ERROR → NULL
```

### 3 — `OMIT QUOTES` strips the outer `"…"` on TEXT columns

```sql
SELECT name FROM JSON_TABLE(
  '[{"name":"Alice"}]',
  '$[*]' COLUMNS (
      name TEXT PATH '$.name' OMIT QUOTES ON SCALAR STRING
  )
) AS t;
-- ('Alice')    -- not ('"Alice"')
```

### 4 — `PASSING` supplies a constant from the outer scope

```sql
SELECT oid, price FROM JSON_TABLE(
  '{"items":[{"price":10},{"price":20},{"price":30}]}',
  '$.items[*] ? (@.price > $min)'
  PASSING 15 AS min
  COLUMNS (
      oid   FOR ORDINALITY,
      price INT PATH '$.price'
  )
) AS t;
-- (1, 20)
-- (2, 30)
```

## Acceptance criteria

- [ ] Grammar: `WITH [...] WRAPPER`, `WITHOUT [...] WRAPPER`, `KEEP QUOTES`,
  `OMIT QUOTES`, and `PASSING x AS n [, …]` are accepted on JSON_TABLE
  exactly in the SQL:2016 / PG / Oracle order (`PATH → WRAPPER → QUOTES →
  ON EMPTY → ON ERROR`).
- [ ] Default semantics unchanged: `WRAPPER` absent = `Without`;
  `QUOTES` absent = `Keep`. All 11.20a / 11.20b / 11.20c regressions pass
  unchanged.
- [ ] Multi-item match behavior matches PG `JSON_QUERY`: `WITHOUT WRAPPER`
  routes through ON ERROR; `UNCONDITIONAL` always wraps; `CONDITIONAL`
  wraps only when result is not already a single array.
- [ ] `OMIT QUOTES` strips the JSON double-quote pair on TEXT-typed columns
  only when the underlying value is a JSON string scalar (no effect on
  numbers, booleans, arrays, objects); error on OMIT + non-TEXT return
  type (parity with SQL:2016 spec §9.42).
- [ ] `PASSING` bindings are visible in row path AND every column / NESTED
  path; duplicate variable names → compile-time error (parity with
  11.19c); referencing an undeclared `$var` → evaluation error surfaced
  via the column's ON ERROR clause.
- [ ] `WRAPPER` / `QUOTES` on `FOR ORDINALITY` / `EXISTS` / `NESTED PATH`
  → compile-time error.
- [ ] New integration test file `integration_json_table_wrapper.rs`:
  ≥ 12 positive cases (wrapper variants × 3, quotes variants × 2, PASSING
  constant × 3, PASSING + WRAPPER combo × 2, PASSING into column path
  filter × 2) and ≥ 4 negative (OMIT on non-TEXT, duplicate PASSING names,
  WRAPPER on ORDINALITY, unknown $var).
- [ ] Wire smoke: one end-to-end roundtrip case in `tools/wire-test.py`
  (UNCONDITIONAL WRAPPER + OMIT QUOTES + one PASSING binding).

## Out of scope — DEFERRED to 11.20d2 / d3 / d4

- JSON_TABLE as the first FROM entry combined with JOIN/CROSS APPLY
  (11.20d2).
- LATERAL-correlated `doc` / `PASSING` expressions that reference outer
  columns (11.20d3).
- JSON_TABLE as UPDATE / DELETE source (11.20d4). `MERGE` stays deferred
  until `MERGE` itself lands.

## Dependencies

- Phase 11.19b (`SqlJsonWrapper`, `SqlJsonQuotes`, `apply_wrapper`,
  quote-strip helper).
- Phase 11.19c (`PASSING` env plumbing in JSONPath evaluator).
- Phase 11.20c (recursive `JsonTableColumn` AST + materializer).

## Plan highlights (for /plan-task)

- Parser: extract `parse_wrapper_clause` / `parse_quotes_clause` helpers
  from `expr.rs` into a shared `parser::sql_json_common` module, call from
  both `sql_json_query` and `parser::json_table::parse_column_def`.
- Parser: add top-level `PASSING` parsing between `row_path` and
  `COLUMNS` in `parse_json_table_call`; reuse the existing `PASSING`
  parser from `sql_json_query`.
- AST: extend `JsonTable` and `JsonTableColumn::Regular` as described.
- Executor: extend `compile_json_table` to compile PASSING bindings once,
  evaluate them once per JSON_TABLE invocation (constants for now;
  correlation in 11.20d3), thread the env into every path eval.
- Executor: pipe `wrapper` / `quotes` through `materialize_row_value`
  using existing helpers; emit typed `DbError::JsonQueryRuntime` for
  WITHOUT-WRAPPER multi-item matches, routed through the column's ON
  ERROR.
- Tests: new `tests/integration_json_table_wrapper.rs`; wire-test hook.

## Recommended effort for /plan-task

`high` — parser refactor (shared helpers) + AST migration (two new
column fields, `PassingEnv` plumbing) + executor threading across three
path sites (row / column / NESTED) + ON ERROR routing for WRAPPER.
