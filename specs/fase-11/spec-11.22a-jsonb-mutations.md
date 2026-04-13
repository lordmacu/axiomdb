# Spec: 11.22a — JSONB mutation parity (phase A)

## What to build (not how)

A robust, cross-engine set of JSON/JSONB mutation functions that
together cover PostgreSQL's write-side surface and complete MySQL's
family. Every function routes through a shared `set_path` / `remove_path`
core so semantics stay consistent while call-site behavior matches the
vendor the user wrote their SQL against.

New functions:

| Function | Return | Engine | Semantics |
|----------|--------|--------|-----------|
| `JSONB_SET(target, path, new_value [, create_if_missing])` | `Value::Jsonb` | PG | Upsert. Creates missing intermediates and leaf when `create_if_missing=true` (default); otherwise leaves target unchanged if any path element is missing. |
| `JSONB_INSERT(target, path, new_value [, insert_after])` | `Value::Jsonb` | PG | Array: insert before/after; Object: add key — **raises error when the key already exists** (PG semantics). |
| `JSONB_DELETE_PATH(target, path)` | `Value::Jsonb` | PG | Delete element at path. Empty path returns target. Root scalar raises. |
| `JSON_INSERT(doc, path, val, ...)` | `Value::Json` | MySQL | Variadic. Adds only if path missing. **Silent no-op on existing key** (MySQL semantics). |
| `JSON_REPLACE(doc, path, val, ...)` | `Value::Json` | MySQL | Variadic. Updates only if path exists. Silent no-op on missing path. |

Already present (Phase 11.4) — untouched:
- `JSON_SET(doc, path, val, ...)` — upsert, variadic, `Value::Json`.
- `JSON_REMOVE(doc, path, ...)` — delete, variadic.

## Inputs / Outputs

### Path arguments (all functions)

Accept **two** forms at the same call site, detected by runtime type
of the path argument:

- **String path** (MySQL style, SQL:2016 JSONPath subset):
  `'$.a.b'`, `'$[0]'`, `'$."quoted key"'`, `'$[0].name'`.
  Reuses the existing `eval_path_arg` → `parse_jsonpath_string`.
- **JSON array literal** (PG `text[]` workaround):
  `'["a","b"]'::jsonb` or `'["a", 0]'` as JSON text.
  Each array element becomes one path step. Strings are object keys,
  integers are array indices.

Wildcards (`$.*`, `$[*]`, `$..key`) are **rejected** on mutation
functions (both engines reject them — mutation is a concrete-path
operation).

### Outputs

- PG functions return `Value::Jsonb` (binary). NULL input → NULL.
- MySQL functions return `Value::Json` (text). NULL input → NULL.

## Use cases

1. **Upsert a JSON field** — `UPDATE cfg SET doc = JSONB_SET(doc,
   '$.max_retries', '5')` creates the key if missing.
2. **Insert array element at position** — `JSONB_INSERT(doc,
   '$.tags[0]', '"critical"')` with `insert_after=false` pushes
   the tag to the front.
3. **Hard-delete at path** — `UPDATE cfg SET doc =
   JSONB_DELETE_PATH(doc, '$.deprecated')`.
4. **MySQL app compatibility** — `JSON_INSERT` and `JSON_REPLACE`
   in an app porting from MySQL don't need a rewrite.
5. **Portable cross-engine SQL** — a data-sync script that works
   against PG, MySQL, and AxiomDB can call the engine's canonical
   name on each and rely on consistent semantics.

## Acceptance criteria

- [ ] **Path parsing** accepts:
  - strings starting with `$` (JSONPath)
  - JSON array literals (`'[...]'`) as text OR `Value::Jsonb` arrays
- [ ] **Wildcards rejected** (`$.*`, `$[*]`, `$..key`) on every
  mutation function with a clear error message.
- [ ] **`JSONB_SET` — upsert**:
  - updates existing leaf;
  - creates missing leaf when `create_if_missing=true` (default);
  - returns target unchanged when `create_if_missing=false` and any
    path element is missing;
  - `new_value = NULL` stores JSON `null` (never deletes, never
    becomes SQL NULL), matching `jsonb_set` semantics in
    `jsonfuncs.c:4856`.
- [ ] **`JSONB_INSERT` — array + object semantics**:
  - array path: inserts before/after index based on `insert_after`;
  - object path, missing key: adds the key;
  - **object path, existing key: raises** `DbError::InvalidValue`
    (matches `jsonfuncs.c:5305-5310`).
- [ ] **`JSONB_DELETE_PATH`**:
  - deletes element at path;
  - empty path `[]` → returns target unchanged;
  - path on scalar root → error (matches `jsonfuncs.c:4980-4983`).
- [ ] **`JSON_INSERT` — MySQL semantics**:
  - variadic `JSON_INSERT(doc, p1, v1, p2, v2, ...)`;
  - adds key only when path is missing;
  - **silent no-op on existing key** (no error — diverges from PG).
- [ ] **`JSON_REPLACE` — MySQL semantics**:
  - variadic;
  - updates only when path exists;
  - silent no-op on missing path.
- [ ] **NULL propagation**: NULL target → NULL output.
- [ ] **Error on non-JSON target**: `InvalidValue`.
- [ ] **Integration tests** in
  `crates/axiomdb-sql/tests/integration_jsonb_mutations.rs` cover
  every bullet including the PG vs MySQL divergence.
- [ ] `cargo test --workspace` clean.
- [ ] `cargo clippy -p axiomdb-sql --tests -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.

## Out of scope

- **`jsonb_set_lax` `null_value_treatment`**: PG's 4-mode NULL
  handling (`raise_exception`, `use_json_null`, `delete_key`,
  `return_target`). Deferred to `spec-11.22b`.
- **Operator forms** `#-`, `#>`, `#>>`: require TEXT[] RHS;
  tracked by `spec-11.18b`.
- **Nested wildcard mutations**: `$[*].x = 1`. Both PG and MySQL
  reject these for mutation — we match.
- **Native SQL `TEXT[]` type**: separate type-system feature; the
  path-as-JSON-array workaround here is documented as temporary.

## Dependencies

- Existing `eval_path_arg` + `parse_jsonpath_string` (Phase 11.16).
- Existing `set_path` / `remove_path` / `jsonb_to_serde` /
  `serde_json_to_sql_value` helpers in
  `crates/axiomdb-sql/src/eval/functions/json.rs`.
- `axiomdb-types::JsonbEncoder` for emitting `Value::Jsonb` outputs.

## Effort for next step

- **Plan: medium** — three new PG helpers with distinct semantics,
  two MySQL variadic wrappers, a shared path-argument normalizer
  that accepts both string and JSON-array forms.
