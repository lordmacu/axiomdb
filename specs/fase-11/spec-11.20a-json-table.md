# Spec: 11.20a — `JSON_TABLE` (flat, no NESTED PATH)

## What to build (not how)

Implement SQL:2016 `JSON_TABLE(doc, row_path COLUMNS (…))` as a table-valued
function usable inside a `FROM` clause. This subphase covers the **flat** form
only — i.e. every emitted row comes from one element of the `row_path`
iteration. `NESTED PATH`, multi-level shredding, and wrapper/quotes support
are deferred to 11.20b–11.20d.

Supported column forms in `COLUMNS ( … )`:

1. `name TYPE PATH 'jsonpath' [DEFAULT expr ON EMPTY] [DEFAULT expr ON ERROR | NULL ON EMPTY/ERROR | ERROR ON EMPTY/ERROR]`
   — scalar projection (semantics of `JSON_VALUE`).
2. `name FOR ORDINALITY` — 1-based `BIGINT` counter incremented once per
   emitted row (single occurrence allowed per this subphase's `COLUMNS` list;
   duplicate raises parse error, MariaDB/PG parity).
3. `name TYPE EXISTS PATH 'jsonpath' [ERROR|TRUE|FALSE|UNKNOWN ON ERROR]`
   — 1/0 (or `TRUE/FALSE`) existence predicate with PG semantics.

Grammar rule (flat subset):

```
json_table      := 'JSON_TABLE' '(' expr ',' string_literal
                     'COLUMNS' '(' column_list ')' ')' [ alias ]
column_list     := column_def (',' column_def)*
column_def      := ident type 'PATH' string_literal
                     [ json_on_behavior 'ON' 'EMPTY' ]
                     [ json_on_behavior 'ON' 'ERROR' ]
                 | ident 'FOR' 'ORDINALITY'
                 | ident type 'EXISTS' 'PATH' string_literal
                     [ json_exists_on_error ]
json_on_behavior := 'NULL' | 'ERROR' | 'DEFAULT' expr
json_exists_on_error := 'TRUE'|'FALSE'|'UNKNOWN'|'ERROR' 'ON' 'ERROR'
```

Clause order follows SQL:2016 / PG `parse_jsontable.c` conventions: path first,
then per-column behavior clauses (`ON EMPTY` before `ON ERROR`).

## Inputs / Outputs

**Input**: any SQL expression whose value coerces to a JSON document — i.e.
`Value::Jsonb`, `Value::Json`, or `Value::Text` (text is parsed via
`serde_json::from_str`; parse failure is a runtime `DbError::InvalidCoercion`).
`NULL` document → zero rows (PG/MariaDB parity; *not* an error).

**Output**: a table-valued row source whose schema is defined by `COLUMNS(...)`
in declaration order. Each row is `Vec<Value>` aligned with that schema,
emitted once per non-NULL, matching element of the row path.

## Use cases

```sql
-- Shred an array of objects
SELECT t.id, t.name
FROM JSON_TABLE(
    '[{"id":1,"name":"Ada"},{"id":2,"name":"Babbage"}]',
    '$[*]'
    COLUMNS (
        id   INT  PATH '$.id',
        name TEXT PATH '$.name'
    )
) AS t;
-- → (1, 'Ada'), (2, 'Babbage')

-- Ordinality + DEFAULT ON EMPTY
SELECT t.n, COALESCE(t.age, 0) AS age
FROM JSON_TABLE('[{"n":"Ada"},{"n":"Babbage","age":61}]', '$[*]'
    COLUMNS (
        n    TEXT   PATH '$.n',
        age  INT    PATH '$.age' DEFAULT 0 ON EMPTY,
        ord  BIGINT FOR ORDINALITY
    )
) AS t;

-- Join with a base table
SELECT u.id, j.tag
FROM users u
JOIN JSON_TABLE(u.tags, '$[*]' COLUMNS (tag TEXT PATH '$')) AS j
  ON TRUE;

-- EXISTS column
SELECT t.*
FROM JSON_TABLE('[{"a":1},{"a":null},{"b":2}]', '$[*]'
    COLUMNS (
        has_a BOOLEAN EXISTS PATH '$.a',
        a     INT     PATH '$.a'
    )
) AS t;
```

## Acceptance criteria

- [ ] Parser accepts the flat grammar above; emits `FromClause::JsonTable { … }`.
- [ ] `JSON_TABLE` is recognized case-insensitively before the generic
  table-ref / subquery dispatch, so it cannot be shadowed by a user table
  named `json_table` (error message explains the conflict if one exists).
- [ ] Clause-ordering errors are explicit (`ON EMPTY` must come before
  `ON ERROR`; mixing column forms must parse correctly regardless of order
  among columns).
- [ ] Duplicate `FOR ORDINALITY` in same column list → parse error.
- [ ] Analyzer registers each declared column as an addressable column in the
  surrounding scope under the alias, with the declared `DataType`.
- [ ] Executor evaluates `doc`, parses it to `serde_json::Value`, walks
  `row_path` via the existing `parse_jsonpath` + `execute_jsonpath_owned`, and
  emits one row per match.
- [ ] NULL doc → zero rows (no error).
- [ ] `Text` doc with invalid JSON → `DbError::InvalidCoercion`.
- [ ] `PATH` column: missing path → `ON EMPTY` branch; JSON type incompatible
  with declared type → `ON ERROR` branch. `DEFAULT expr` is re-evaluated per
  row for each miss (PG parity, MariaDB also permits non-constant DEFAULT).
- [ ] `FOR ORDINALITY` starts at 1 and increments once per emitted row,
  independent of whether any column hit `ON EMPTY`.
- [ ] `EXISTS PATH` returns `TRUE`/`FALSE`; `UNKNOWN ON ERROR` → NULL;
  `ERROR ON ERROR` → `DbError`; default is `FALSE ON ERROR`.
- [ ] Integration with the FROM list: usable as the first source, as a
  right-side of `INNER`/`LEFT`/`CROSS JOIN`, and with WHERE predicates
  referencing its columns.
- [ ] Wire visible: `mysql -e "SELECT … FROM JSON_TABLE(...)"` returns rows.
- [ ] `integration_json_table.rs` covers: basic shred, ordinality, ON EMPTY
  DEFAULT, ON ERROR NULL, ON ERROR ERROR raises, EXISTS TRUE/FALSE/NULL,
  join with base table, NULL doc → 0 rows, Text doc parse error, unknown
  column type coercion.

## Out of scope (→ later subphases)

- `NESTED PATH` of any form → 11.20b (single level) / 11.20c (multi-sibling,
  multi-level).
- `FORMAT JSON` column modifier and `WRAPPER` / `QUOTES` clauses on
  JSON_TABLE columns (`JSON_QUERY`-shaped output) → 11.20d.
- `PASSING name AS var` bindings on `JSON_TABLE`'s row path → 11.20d
  (the `SqlJsonQuery` side already has them from 11.19c; the two paths can
  converge).
- Direct use as `UPDATE … FROM JSON_TABLE(…)` / `DELETE … USING JSON_TABLE(…)`
  / `MERGE … USING JSON_TABLE(…)` source → 11.20d.
- Lateral reference from JSON_TABLE's `doc` expression to outer FROM items
  beyond a single base column (already works for simple column refs like
  `u.tags`; deeper correlation is 11.20d).
- Full JSONPath engine upgrade (filters, arithmetic, etc. inside the row
  path) — already available via the 11.21 stack.
- `jsonb_path_ops` GIN push-down for JSON_TABLE row paths → 11.21h.

## Dependencies

- AST enums `SqlJsonOnBehavior`, `SqlJsonPathMode` (11.19a) — reused for
  `ON EMPTY`/`ON ERROR` behaviors.
- `parse_jsonpath`, `execute_jsonpath_owned` (11.16, extended in 11.21a/d) —
  reused as-is.
- `value_to_serde_json`, `sql_to_serde_json` helpers (11.16, 11.19a) —
  reused; exposed to the new module if currently private.
- `axiomdb_types::coerce::coerce` — reused for per-column RETURNING
  coercion (same contract as 11.19a `JSON_VALUE`).
- `FromClause` enum (ast.rs:216) gains a new variant `JsonTable`.
- `analyzer_bind::bound_from_clause` gains a branch mirroring the existing
  subquery branch, publishing one `BoundTable` with the declared columns.
- Executor `select_joins_ctx` gains a branch to materialize `JsonTable`
  rows; single-table SELECTs route through the standard join machinery after
  a trivial "one source" path.

## ⚠️ DEFERRED (noted in progreso.md)

- NESTED PATH → pending in 11.20b / 11.20c.
- WRAPPER / QUOTES on JSON_TABLE columns → pending in 11.20d.
- UPDATE/DELETE/MERGE integration → pending in 11.20d.
