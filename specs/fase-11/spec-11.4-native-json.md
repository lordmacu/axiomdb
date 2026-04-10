# Spec: 11.4 — Native JSON Type

## What to build (not how)

Add a single SQL `JSON` type that stores only syntactically valid JSON values and
allows path extraction from SQL. The type must be visible through DDL, catalog
metadata, row encoding/decoding, expression evaluation, the embedded API, and the
MySQL wire result path.

For Phase 11.4, JSON values are stored as validated UTF-8 JSON text using the same
u24 length-prefixed payload shape as `TEXT`; the column type remains `JSON`, so
callers do not see it as plain `TEXT`. Large JSON values continue to rely on the
Phase 11.2 TOAST path when the row must be externalized.

The user-visible SQL surface for this subphase is:

```sql
CREATE TABLE docs (id INT, data JSON);
INSERT INTO docs VALUES (1, '{"name":"Alice","age":30}');

SELECT JSON_EXTRACT(data, '$.name') FROM docs;
SELECT data->>'name' FROM docs WHERE data->>'name' = 'Alice';
SELECT JSON_SET(data, '$.active', true) FROM docs;
SELECT JSON_REMOVE(data, '$.age') FROM docs;
SELECT JSON_KEYS(data), JSON_VALID(data), JSON_TYPE(data) FROM docs;
```

## Inputs / Outputs

- Input: `CREATE TABLE` column definitions using `JSON`.
- Input: `INSERT` and `UPDATE` values targeting a `JSON` column. Accepted inputs
  are `Value::Json` or text values coercible to valid JSON.
- Input: scalar JSON function calls: `JSON_EXTRACT(json, path)`,
  `JSON_SET(json, path, value)`, `JSON_REMOVE(json, path)`, `JSON_KEYS(json)`,
  `JSON_VALID(value)`, and `JSON_TYPE(json)`.
- Input: field extraction operator `json_expr->>'field'`, equivalent to
  `JSON_EXTRACT(json_expr, '$.field')` returning a SQL scalar/text value.
- Output: `Value::Json(String)` for stored JSON documents and JSON-returning
  functions.
- Output: SQL scalars for extracted JSON scalars: string -> `Value::Text`,
  integer -> `Value::Int` or `Value::BigInt`, floating number -> `Value::Real`,
  boolean -> `Value::Bool`, JSON null or missing path -> `Value::Null`.
- Errors: invalid JSON stored into a `JSON` column returns
  `DbError::InvalidValue`.
- Errors: malformed path strings return `DbError::InvalidValue`.
- Errors: wrong function arity returns `DbError::TypeMismatch`.

## Use cases

1. Store application metadata in one validated column and extract a top-level key:
   `SELECT JSON_EXTRACT(data, '$.status') FROM docs`.
2. Filter by a JSON string field with PostgreSQL-compatible syntax:
   `SELECT id FROM docs WHERE data->>'kind' = 'invoice'`.
3. Reject corrupted JSON early: `INSERT INTO docs VALUES (1, '{bad')` must fail.
4. Atomically derive a changed document value with
   `JSON_SET(data, '$.active', true)` or `JSON_REMOVE(data, '$.oldKey')`.
5. Preserve SQL NULL semantics: JSON functions called with SQL `NULL` return
   SQL `NULL`, except `JSON_VALID(NULL)` returns `0`.

## Acceptance criteria

- [ ] `CREATE TABLE t (data JSON)` parses, stores catalog metadata, and shows the
  column as `JSON`.
- [ ] `INSERT` and `UPDATE` validate JSON syntax before writing rows and reject
  invalid JSON with `DbError::InvalidValue`.
- [ ] Row codec round-trips `Value::Json` using `DataType::Json`.
- [ ] `JSON_EXTRACT(data, '$.name')` returns the correct SQL scalar for strings,
  numbers, booleans, JSON null, and missing paths.
- [ ] `data->>'name'` works in both `SELECT` output expressions and `WHERE`
  predicates.
- [ ] `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`, `JSON_VALID`, and `JSON_TYPE`
  work for simple object paths.
- [ ] JSON functions on SQL `NULL` follow the null behavior in the use cases.
- [ ] `local_bench.py` includes a `json_extract` scenario with MariaDB comparison.
- [ ] Integration tests cover DDL, invalid insert rejection, extraction, mutation
  functions, `->>` in `SELECT` and `WHERE`, and NULL handling.
- [ ] Docs-site user and internals pages explain the JSON type, examples, error
  behavior, and storage trade-off.

## Out of scope

- Binary JSONB storage layout.
- Automatic GIN index creation for JSON columns.
- Full SQL:2016 JSONPath; Phase 11.4 supports only `$`, `$.key`,
  `$.key1.key2`, and array indexes written as path components.
- `->` JSON-returning operator.
- `JSON_MERGE_PATCH`.
- JSON containment operators such as `@>`.

## Dependencies

- Phase 11.2 TOAST must exist for oversized row handling.
- Existing DDL parser, catalog `ColumnType`, row codec, coercion API, expression
  evaluator, and MySQL result serialization must accept the new JSON type.
- `serde_json` must be available for validation and simple JSON manipulation.
- Existing NFC normalization for text storage remains in force for JSON strings.

## ⚠️ DEFERRED

- Binary JSONB storage, automatic GIN indexing, full SQL:2016 JSONPath,
  `->`, and `JSON_MERGE_PATCH` are tracked by `docs/progreso.md` as part of the
  broader Native JSON ambition, but they require a dedicated binary layout and
  JSON index access method. Revisit after this text-backed native JSON subphase
  is closed and before marking the broader JSON roadmap item fully complete.
