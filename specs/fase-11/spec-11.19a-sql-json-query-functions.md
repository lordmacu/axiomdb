# Spec: 11.19a — SQL/JSON standard query functions (phase A)

## What to build (not how)

The SQL:2016 / SQL:2023 standard JSON query functions —
`JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS` — with their mandatory
surrounding clauses (`RETURNING`, `ON EMPTY`, `ON ERROR`, path-mode
prefix). These are **special-form expressions** with keyword-driven
grammar, not variadic function calls. Behavior tracks PostgreSQL 16
exactly (`src/backend/parser/gram.y:17117-17172`,
`src/backend/utils/adt/jsonpath_exec.c`,
`src/backend/executor/execExprInterp.c:4977-5030`).

Three expressions:

| Form | Shape | Returns |
|------|-------|---------|
| `JSON_VALUE(doc, jsonpath [RETURNING type] [ON EMPTY ...] [ON ERROR ...])` | Scalar-only extraction | Coerced to RETURNING type (TEXT default) |
| `JSON_QUERY(doc, jsonpath [RETURNING type] [ON EMPTY ...] [ON ERROR ...])` | JSON / JSONB subtree extraction | `Value::Jsonb` default; RETURNING TEXT emits JSON text |
| `JSON_EXISTS(doc, jsonpath [ON ERROR {TRUE | FALSE | UNKNOWN | ERROR}])` | Predicate | `Value::Bool` |

Path mode prefix:

- `strict $.a.b` — any missing intermediate / type mismatch / out-of-
  range index raises the execution error. SQL:2016 default. Matches
  PG `jsonpath_exec.c:263` `jspStrictAbsenceOfErrors`.
- `lax $.a.b` — silent NULL on missing parts, automatic unwrapping
  of arrays when used in scalar context (PG `jspAutoUnwrap`). Same
  as AxiomDB's existing `JSON_EXTRACT` behavior.

`ON EMPTY` / `ON ERROR` handlers:

```
ERROR           -- raise the condition
NULL            -- return SQL NULL (spec default)
DEFAULT expr    -- evaluate expr and return it (coerced to RETURNING type)
```

For `JSON_EXISTS`, `ON ERROR` supports additional literal forms
`TRUE | FALSE | UNKNOWN`.

## Inputs / Outputs

### SQL syntax

```sql
-- JSON_VALUE — scalar extraction with type coercion.
SELECT JSON_VALUE(doc, '$.price' RETURNING INT) FROM t;
SELECT JSON_VALUE(doc, 'strict $.missing'
                  ON ERROR DEFAULT -1
                  ON EMPTY DEFAULT 0)
FROM t;

-- JSON_QUERY — subtree extraction (object / array / scalar as JSONB).
SELECT JSON_QUERY(doc, '$.tags' RETURNING JSONB) FROM t;
SELECT JSON_QUERY(doc, 'strict $.children'
                  ON EMPTY ERROR
                  ON ERROR NULL)
FROM t;

-- JSON_EXISTS — predicate.
SELECT JSON_EXISTS(doc, '$.tags[0]') FROM t;
SELECT JSON_EXISTS(doc, 'strict $.missing' ON ERROR FALSE) FROM t;
```

### Outputs

- `JSON_VALUE` default return: `Value::Text`. When `RETURNING <type>`
  is provided, coerce to that SQL type. On coercion failure the
  `ON ERROR` handler runs. Non-scalar match (array/object under `lax`,
  or non-scalar root without unwrapping) → `ON ERROR`.
- `JSON_QUERY` default return: `Value::Jsonb` (binary JSONB). With
  `RETURNING TEXT` / `RETURNING VARCHAR(n)` → `Value::Text` with JSON
  textual rendering.
- `JSON_EXISTS` return: `Value::Bool`. Path errors route via
  `ON ERROR`; match/no-match is never an error.

## Use cases

1. **Typed scalar extraction** — `JSON_VALUE(doc, '$.id' RETURNING
   INT)` gives a native integer instead of TEXT, eliminating the
   `CAST(JSON_EXTRACT(...) AS INT)` boilerplate.
2. **Defensive extraction with fallbacks** — `JSON_VALUE(doc,
   '$.timeout_ms' RETURNING INT ON EMPTY DEFAULT 30000 ON ERROR
   DEFAULT 30000)` gives the config value or a compile-time fallback
   in one expression.
3. **Migration from Oracle / DB2** — both use the SQL:2016 grammar;
   AxiomDB immediately accepts their queries.
4. **Strict-mode validation** — `JSON_VALUE(doc, 'strict $.required'
   ON ERROR ERROR)` forces the call to fail if the key is missing,
   giving a compile-time-friendly schema-assertion.
5. **Predicate in WHERE** — `WHERE JSON_EXISTS(config, '$.flags.beta')`
   is more readable than `WHERE JSON_EXTRACT(config, '$.flags.beta')
   IS NOT NULL`.

## Acceptance criteria

- [ ] **Grammar** — parser accepts every bullet in the syntax above,
      including:
  - path mode prefix `strict` / `lax`;
  - `RETURNING <data_type>` after path;
  - `ON EMPTY {ERROR | NULL | DEFAULT <expr>}`;
  - `ON ERROR {ERROR | NULL | DEFAULT <expr>}` (+ `TRUE|FALSE|UNKNOWN`
    for `JSON_EXISTS`).
- [ ] **Grammar — clause ordering** matches PG: path mode is a prefix
      inside the jsonpath string; `RETURNING` before `ON EMPTY`;
      `ON EMPTY` before `ON ERROR`. Wrong ordering → parse error with
      a clear message.
- [ ] **Path mode default** — **strict** (SQL:2016 + PG default).
      Missing key / missing array index / scalar intermediate under
      strict triggers an error, routed to `ON ERROR`.
- [ ] **`JSON_VALUE` scalar-only rule** — if the match is an array or
      object, route to `ON ERROR`. Matches PG `IsAJsonbScalar`
      (`jsonfuncs.c:4367`).
- [ ] **`JSON_VALUE` RETURNING type** — implemented for: `INT`,
      `BIGINT`, `SMALLINT`, `REAL`, `FLOAT`, `DOUBLE PRECISION`,
      `TEXT`, `VARCHAR(n)`, `BOOL`, `DATE`, `TIMESTAMP`, `JSONB`.
      Coercion failure → `ON ERROR`.
- [ ] **`JSON_QUERY` default RETURNING** — `JSONB`. `RETURNING TEXT`
      emits compact JSON text.
- [ ] **`JSON_QUERY` multi-item** — MVP errors when the path yields
      more than one element. Routed to `ON ERROR`. `WITH WRAPPER`
      deferred to 11.19b.
- [ ] **`JSON_EXISTS`** — `ON ERROR` defaults to `FALSE` (matches
      PG `JsonExpr.on_error_code = JSON_ON_BEHAVIOR_FALSE`).
- [ ] **`ON EMPTY DEFAULT expr`** — the `DEFAULT` expression may
      reference columns in scope; it is evaluated lazily only when
      the empty case fires.
- [ ] **`ON ERROR DEFAULT expr`** — same lazy rule. Coercion of the
      DEFAULT value to `RETURNING` type reuses the same coercer.
- [ ] **Input doc** — accepts `Value::Jsonb`, `Value::Json`,
      `Value::Text` (the last two parsed as JSON at eval time).
- [ ] **NULL doc** → NULL result (like every other JSON function,
      spec-compliant).
- [ ] **Integration tests** in
      `crates/axiomdb-sql/tests/integration_sql_json_query.rs` cover
      every acceptance bullet and the PG-regression-parity inputs.
- [ ] `cargo test --workspace` clean.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.

## Out of scope

- **`PASSING` clause** (bound variables `$min` in jsonpath) — AxiomDB's
  jsonpath compiler does not accept bound variables yet. Deferred to
  11.19b.
- **`WITH [CONDITIONAL|UNCONDITIONAL] WRAPPER`** on `JSON_QUERY` —
  multi-item results default to `ON ERROR` in MVP. Deferred to 11.19b.
- **`QUOTES` clause** (`KEEP QUOTES` / `OMIT QUOTES`) — affects TEXT-
  typed `RETURNING` on a JSON string. MVP always keeps the quotes
  (MySQL-compatible). Deferred to 11.19b.
- **`ARRAY` / `ROWSET` wrappers**.
- **Oracle-specific flags** like `ABSENT ON NULL`, `ERROR ON EMPTY
  ARRAY`.

## Dependencies

- Existing jsonpath runtime (`crates/axiomdb-sql/src/eval/functions/
  json.rs::parse_jsonpath` + `execute_jsonpath`) — will gain a mode
  parameter.
- Existing type coercer (`axiomdb-types::coerce::coerce`) — reused
  for `RETURNING`.
- Existing `Expr` enum + parser — new variant + clause-parsing helper.

## Effort for next step

- **Plan: medium** — one new `Expr` variant, one new parser
  special-form helper, one evaluator dispatcher, full test matrix
  (~25 tests).
