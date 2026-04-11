# Spec: 11.16 — Binary JSONB + SQL:2016 JSONPath

## What to build

Replace the text-backed `Value::Json(String)` storage tier introduced in Phase
11.4 with a binary JSONB representation that can be accessed without parsing UTF-8
each time a field is read. The new type lives alongside `Value::Json` during a
transitional period: existing `JSON` columns on disk remain readable using the
Phase 11.4 text path, while new `JSONB` columns and function results use the
binary form.

The user-visible SQL surface added by this subphase:

```sql
-- Binary sub-document extraction (returns JSONB value, not text scalar)
SELECT data->'address' FROM users;
SELECT data->0 FROM arrays_table;

-- Explicit casts to binary format
SELECT TO_JSONB('{"x":1}');
SELECT JSONB('{"x":1}');

-- RFC 7396 merge-patch
SELECT JSON_MERGE_PATCH(data, '{"active":true}') FROM users;

-- Containment and overlap tests
SELECT id FROM docs WHERE JSON_CONTAINS(data, '{"role":"admin"}');
SELECT id FROM docs WHERE JSON_OVERLAPS(tags, '["urgent","high"]');

-- Full SQL:2016 JSONPath
SELECT JSON_PATH_EXISTS(data, '$.addresses[*].city');
SELECT JSON_PATH_QUERY(data, '$.orders[*].amount');
SELECT JSON_PATH_QUERY_FIRST(data, '$.orders[0].id');

-- Metadata and structural functions
SELECT JSON_ARRAY_LENGTH(data, '$.items');
SELECT JSON_DEPTH(data);
SELECT JSON_PRETTY(data);
```

Existing functions `JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`,
`JSON_VALID`, and `JSON_TYPE` are upgraded to operate on binary JSONB without
re-parsing the UTF-8 text on each call when the input is already `Value::Jsonb`.

## Inputs / Outputs

**Inputs:**
- `CREATE TABLE` columns declared as `JSON` or `JSONB`. Both are valid DDL.
  `JSONB` maps to `ColumnType::Jsonb = 10`; `JSON` continues to map to
  `ColumnType::Json = 9` (backward compatible).
- `INSERT` / `UPDATE` values for `JSONB` columns supplied as text literals; the
  executor validates and encodes them to binary JSONB before writing rows.
- `Value::Json(String)` from old rows (pre-11.16 format) that must be lazily
  coerced to `Value::Jsonb` when a function needs binary access.
- JSON function call arguments: any expression evaluating to `Value::Json`,
  `Value::Jsonb`, or coercible `Value::Text`.
- JSONPath string literals passed to path-taking functions.

**Outputs:**
- `Value::Jsonb(Arc<Vec<u8>>)` — the binary JSONB blob, heap-allocated and
  reference-counted so cloning a JSONB value is a cheap pointer bump.
- SQL scalars for leaf-level extractions: `Value::Text`, `Value::Int`,
  `Value::BigInt`, `Value::Real`, `Value::Bool`, or `Value::Null`.
- `Value::Jsonb` for sub-document extractions via `->` and `JSON_PATH_QUERY`.
- `Value::Bool` for containment/overlap predicates.
- `Value::Int` for `JSON_ARRAY_LENGTH` and `JSON_DEPTH`.
- `Value::Text` for `JSON_PRETTY`.
- Error `DbError::InvalidValue` for malformed JSON text inputs.
- Error `DbError::InvalidValue` for syntactically invalid JSONPath strings.

## Use cases

1. **Hot-path field access without re-parsing** — a dashboard query extracts
   five fields from 100,000 JSONB rows calling `data->'user'->'email'` without
   paying the `serde_json::from_str` cost on every row.

2. **Sub-document passing between functions** — chaining `->` and JSONPath
   functions operates on the binary blob throughout:
   `JSON_ARRAY_LENGTH(data->'orders')`.

3. **RFC 7396 merge-patch for partial updates** — an application streams a delta
   document and merges it into the stored record:
   `UPDATE users SET data = JSON_MERGE_PATCH(data, ?)`.

4. **Containment predicate** — `JSON_CONTAINS(tags, '["urgent"]')` finds rows
   where the tags array includes "urgent" without a full table scan once GIN indexes
   (Phase 11.17) are available.

5. **Recursive descent querying** — `JSON_PATH_EXISTS(doc, '$..id')` finds
   whether any `id` field exists at any nesting level using SQL:2016 JSONPath
   syntax rather than ad-hoc recursive CTEs.

6. **Array slicing** — `JSON_PATH_QUERY(data, '$.events[0:5]')` returns the
   first five events as a JSONB array without materialising the full array.

7. **Schema-on-read validation** — `JSON_PATH_EXISTS(doc, '$.required_field')`
   inside a WHERE clause validates incoming documents before archiving.

8. **Pretty-printing for diagnostics** — `JSON_PRETTY(data)` returns a
   human-readable version of a stored binary JSONB value.

9. **Overlap predicate for tag arrays** — `JSON_OVERLAPS(doc->'tags', '["sale","featured"]')`
   returns true if any element matches.

10. **Explicit binary cast for application code** — `JSONB('{"x":1}')` and
    `TO_JSONB(expr)` let applications explicitly request binary encoding.

## Acceptance criteria

- [ ] `Value::Jsonb(Arc<Vec<u8>>)` exists in `axiomdb-types::Value` and is
      round-tripped by the row codec under `DataType::Jsonb` (discriminant 10).
- [ ] `ColumnType::Jsonb = 10` is reserved in the catalog and
      `CREATE TABLE t (data JSONB)` stores the column with that discriminant.
- [ ] `CREATE TABLE t (data JSON)` continues to work unchanged; `ColumnType::Json = 9`
      rows decode as `Value::Json(String)` with zero change to Phase 11.4 behavior.
- [ ] `->` operator tokenized as `Token::JsonExtractSub` and lowered to
      `BinaryOp::JsonSub`, evaluating to `Value::Jsonb`.
- [ ] `data->'key'` (string RHS) returns `Value::Jsonb` containing the sub-document.
- [ ] `data->0` (integer RHS) returns `Value::Jsonb` for the first array element.
- [ ] `JSON_MERGE_PATCH(doc, patch)` implements RFC 7396 exactly: null patch
      values delete keys, non-object patches replace the entire document.
- [ ] `JSON_CONTAINS(doc, candidate)` and `JSON_CONTAINS(doc, candidate, path)`
      return `Value::Bool` true when `candidate` is a structural subset of the target.
- [ ] `JSON_OVERLAPS(doc1, doc2)` returns `Value::Bool` true when any element or
      key appears in both sides.
- [ ] `parse_jsonpath(input)` compiles a SQL:2016 JSONPath string to `Vec<PathStep>`
      and returns `DbError::InvalidValue` for syntactically invalid paths.
- [ ] `execute_jsonpath` in `PathMode::Lax` auto-unwraps arrays when the path
      expects a scalar, matching SQL:2016 §9.39 lax semantics.
- [ ] `JSON_PATH_EXISTS(doc, path)` returns `Value::Bool`.
- [ ] `JSON_PATH_QUERY(doc, path)` returns `Value::Jsonb` wrapping an array of
      all matches.
- [ ] `JSON_PATH_QUERY_FIRST(doc, path)` returns the first match as `Value::Jsonb`
      or `Value::Null` when no match exists.
- [ ] `JSON_ARRAY_LENGTH(doc)` returns `Value::Int` for array roots and
      `Value::Null` for non-arrays; optional `JSON_ARRAY_LENGTH(doc, path)` variant.
- [ ] `JSON_DEPTH(doc)` returns `Value::Int` where depth 1 = scalar, 2 = flat
      object/array, and so on recursively.
- [ ] `JSON_PRETTY(doc)` returns `Value::Text` with two-space-indented JSON.
- [ ] `TO_JSONB(expr)` and `JSONB(text)` cast any JSON-compatible value to
      `Value::Jsonb`.
- [ ] Existing functions `JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`,
      `JSON_VALID`, and `JSON_TYPE` accept `Value::Jsonb` input without
      deserializing back to a `String`; they call `JsonbRef` directly.
- [ ] Row codec encodes `Value::Jsonb` as `u24 length + raw binary bytes` under
      `DataType::Jsonb` and TOAST path is exercised for blobs exceeding threshold.
- [ ] MySQL wire layer serializes `Value::Jsonb` as a `VAR_STRING` payload
      containing canonical JSON text.
- [ ] `JsonbEncoder` builds a valid binary blob that `JsonbDecoder` round-trips for
      all value types: null, bool, integer, float, string, nested objects, arrays.
- [ ] Key lookup in an object with 1000 keys using binary search on the sorted
      JEntry key section completes without heap allocation for the lookup itself.
- [ ] All existing Phase 11.4 integration tests in `integration_json.rs` pass
      without modification.
- [ ] `cargo test --workspace` passes clean with no warnings.

## Out of scope

- GIN / inverted index creation for JSONB columns (Phase 11.17).
- Automatic migration of existing `JSON` rows to binary JSONB on table open.
- `@>` / `<@` as standalone SQL operators (containment available as functions).
- JSONB generation functions: `JSON_BUILD_OBJECT`, `JSON_BUILD_ARRAY`, `JSON_AGG`
  (Phase 29.12).
- JSONPath `datetime()` filter function.
- Writing JSONB columns through the MySQL binary prepared-statement protocol.
- In-place binary mutation of JSONB blobs (Phase 11.17 delta update).
- User-defined JSON schema validation (Phase 12.x).

## Dependencies

- Phase 11.4 (native JSON): `Value::Json`, `DataType::Json`, `ColumnType::Json`,
  and all six existing JSON functions must be present and passing.
- Phase 11.2 (TOAST): binary JSONB blobs exceeding `TOAST_THRESHOLD` TOAST
  through the existing overflow chain.
- `serde_json`: still required for parsing input text and for `JSON_PRETTY`; not
  required for field access once the binary blob is formed.
- No new third-party crates are required; the binary encoding is implemented from
  scratch using Rust primitives.
