# Spec: 11.22b — `jsonb_set_lax` with `null_value_treatment`

## What to build
PostgreSQL `jsonb_set_lax(target, path, new_value, create_if_missing, null_value_treatment)` —
variant of `jsonb_set` that dispatches on SQL-NULL `new_value` via a 4-way enum:
`raise_exception`, `use_json_null`, `delete_key`, `return_target`. Default =
`use_json_null` (matches PG default which makes `jsonb_set_lax` equivalent to `jsonb_set`
unless the caller opts out).

## Inputs / Outputs
- `target jsonb`  (JSONB doc, or NULL)
- `path   text[]` (or string `$.a.b` / JSON array path, consistent with 11.22a)
- `new_value jsonb` (or SQL NULL — triggers dispatch)
- `create_if_missing bool` default `true`
- `null_value_treatment text` default `'use_json_null'`
- Returns `jsonb` or SQL NULL

## Use cases
```sql
-- normal: non-null value → same as jsonb_set
SELECT jsonb_set_lax('{"a":1}'::jsonb, '{a}', '42'::jsonb);       -- {"a":42}

-- SQL NULL + default use_json_null → embed JSON null
SELECT jsonb_set_lax('{"a":1}'::jsonb, '{a}', NULL);               -- {"a":null}

-- delete_key → drop leaf
SELECT jsonb_set_lax('{"a":1,"b":2}', '{a}', NULL, true, 'delete_key');  -- {"b":2}

-- return_target → untouched
SELECT jsonb_set_lax('{"a":1}', '{z}', NULL, true, 'return_target');     -- {"a":1}

-- raise_exception
SELECT jsonb_set_lax('{"a":1}', '{a}', NULL, true, 'raise_exception');   -- ERROR
```

## Acceptance criteria
- [ ] Signature: 3/4/5 args accepted; SQL NULL for `target`/`path`/`create_if_missing` returns NULL
- [ ] SQL NULL for `null_value_treatment` raises error (PG parity)
- [ ] Non-NULL `new_value` behaves exactly like `jsonb_set` (same `create_if_missing` semantics)
- [ ] `new_value = NULL` dispatch:
  - `use_json_null` (default) → embed JSON `null`
  - `raise_exception` → `DbError::InvalidValue`
  - `delete_key` → `jsonb_delete_path`
  - `return_target` → target unchanged
  - any other string → error
- [ ] Path accepts string `$.a.b` and JSON-array `["a","b"]` forms (11.22a convention)
- [ ] Wildcards rejected
- [ ] Integration tests: ≥ 10 cases covering the 4 branches + NULL handling
- [ ] clippy/fmt/workspace clean

## Out of scope
- `jsonb_set_lax` inside `UPDATE ... SET col = jsonb_set_lax(...)` — inherits existing mutation path.
- MySQL analog: MySQL has no direct equivalent; not exposing alias.

## Dependencies
- 11.22a helpers: `parse_mutation_path`, `set_path_ext`, `remove_path_parts`,
  `value_to_serde_json`, `sql_to_serde_json`, `jsonb_blob_from_serde`, `is_truthy_arg`.

## Research
- PG `src/backend/utils/adt/jsonfuncs.c:4898-4959` — full semantics
- PG docs `doc/src/sgml/func/func-json.sgml:1506-1528`
