# Plan: 11.22a — JSONB mutation parity (phase A)

## Files to create/modify

**Modify:**

- `crates/axiomdb-sql/src/eval/functions/mod.rs`
  - Register the five new function names in the JSON dispatch arm:
    `jsonb_set`, `jsonb_insert`, `jsonb_delete_path`, `json_insert`,
    `json_replace`.

- `crates/axiomdb-sql/src/eval/functions/json.rs`
  - New handler arms for the five functions.
  - New private helper `parse_mutation_path(&Value) -> Result<Vec<String>, DbError>`
    that accepts string (`'$.a.b'`) OR JSON-array (`'["a","b"]'`)
    forms. Wildcards rejected; other path-parse errors surface
    as-is.
  - New private helper `set_path_ext(target, path, new_value, flags)`
    extending the existing `set_path` with three booleans:
    `create_if_missing`, `insert_after`, `raise_on_existing_key`.
  - New private helper `jsonb_blob_from_serde(&serde_json::Value) ->
    Value::Jsonb` for PG-function outputs.
  - JSON_INSERT / JSON_REPLACE simply call `set_path_ext` with the
    appropriate flag combination and loop over path/value pairs.

- `crates/axiomdb-sql/tests/integration_jsonb_mutations.rs` (**new**)
  - Coverage matrix — see **Tests** below.

**No change:**

- `axiomdb-types` — the JSONB binary layout is reused as-is.
- parser / lexer / AST / planner — these are function calls, no new
  tokens.

## Algorithm / Data structure

### Path-argument normalizer

```text
parse_mutation_path(arg):
    match arg:
        Value::Text(s) | Value::Json(s):
            if s.trim_start().starts_with('$'):
                # MySQL JSONPath — reuse existing parser.
                parts = parse_jsonpath_string(s)
                if any(part is wildcard): err "wildcard not allowed"
                return parts
            if s.trim_start().starts_with('['):
                # PG text[] workaround as JSON array literal.
                arr = serde_json::from_str(s)?
                return arr.iter().map(|e| e.as_string_or_number()).collect()
            err "unsupported path form"
        Value::Jsonb(b):
            sj = JsonbDecoder::decode(b)?
            require sj is Array
            return sj.iter().map(|e| e.to_string_step()).collect()
        other: err "path must be text or jsonb array"
```

The normalized path is a `Vec<String>` where each element is either a
bare key (when operating on an object) or a stringified integer index
(when the current container is an array). The existing `set_path`
already branches on whether the current container is object/array and
interprets string path steps accordingly (PG behavior, `setPath` in
`jsonfuncs.c:5269,5408`).

### Extended `set_path`

```text
set_path_ext(target, path[0..], new_value, flags):
    if path.is_empty():
        # At the leaf.
        if flags.raise_on_existing_key and current exists:
            err "cannot replace existing key"
        return replace_current_with(new_value)

    let head = path[0]
    match target:
        Object:
            if target has key head:
                if flags.raise_on_existing_key and path.len() == 1:
                    err "cannot replace existing key"
                recurse into target[head] with path[1..]
            else:
                if flags.create_if_missing:
                    target[head] = recurse into empty-object with path[1..]
                else:
                    return target unchanged
        Array:
            idx = parse_int(head) or err "path element must be integer for array"
            real = if idx<0 { len+idx } else { idx }
            if path.len() == 1 and flags.insert_before|insert_after:
                insert new_value at computed position
                return
            if real in bounds:
                recurse into target[real] with path[1..]
            else:
                if flags.create_if_missing:
                    # PG behavior: append to array if beyond end
                    append recursive empty target
                else:
                    return target unchanged
        Scalar:
            err "cannot set path in scalar"
```

### `JSONB_SET`

```text
jsonb_set(target, path, new_value, create_if_missing=true):
    if target is NULL: return NULL
    sj = to_serde_json(target)
    parts = parse_mutation_path(path)
    set_path_ext(sj, parts, to_serde(new_value),
                 { create_if_missing, raise_on_existing_key=false })
    return Jsonb(encode(sj))
```

### `JSONB_INSERT`

```text
jsonb_insert(target, path, new_value, insert_after=false):
    if target is NULL: return NULL
    sj = to_serde_json(target)
    parts = parse_mutation_path(path)
    set_path_ext(sj, parts, to_serde(new_value),
                 { create_if_missing=false,
                   raise_on_existing_key=true,
                   insert_after })
    return Jsonb(encode(sj))
```

### `JSONB_DELETE_PATH`

```text
jsonb_delete_path(target, path):
    if target is NULL: return NULL
    sj = to_serde_json(target)
    parts = parse_mutation_path(path)
    if parts.is_empty(): return target unchanged
    if sj is scalar: err "cannot delete path in scalar"
    remove_path(sj, parts)     # existing helper
    return Jsonb(encode(sj))
```

### `JSON_INSERT` (MySQL, variadic)

```text
json_insert(doc, p1, v1, p2, v2, ...):
    if doc is NULL: return NULL
    sj = to_serde_json(doc)
    for each (pi, vi) pair:
        parts = parse_mutation_path(pi)
        if path exists in sj: continue         # silent no-op
        set_path_ext(sj, parts, to_serde(vi),
                     { create_if_missing=true,
                       raise_on_existing_key=false })
    return Json(sj.to_string())
```

### `JSON_REPLACE` (MySQL, variadic)

```text
json_replace(doc, p1, v1, p2, v2, ...):
    if doc is NULL: return NULL
    sj = to_serde_json(doc)
    for each (pi, vi) pair:
        parts = parse_mutation_path(pi)
        if path does NOT exist in sj: continue # silent no-op
        set_path_ext(sj, parts, to_serde(vi),
                     { create_if_missing=false,
                       raise_on_existing_key=false })
    return Json(sj.to_string())
```

## Implementation phases

1. **Spec → complete.**
2. **Plan → complete.**
3. **Path-argument normalizer (~40 LOC):** the `parse_mutation_path`
   helper in `eval/functions/json.rs`.
4. **`set_path_ext` (~120 LOC):** extended traversal with three flag
   booleans. Unit-test via the integration test file (no isolated
   cargo test-ops file for eval helpers yet).
5. **PG functions (~120 LOC):** `jsonb_set`, `jsonb_insert`,
   `jsonb_delete_path` + dispatcher registration.
6. **MySQL functions (~90 LOC):** `json_insert`, `json_replace` with
   variadic loop + dispatcher registration.
7. **Integration tests (~450 LOC):** coverage matrix.
8. **Close**: progreso.md update, clippy/fmt/workspace clean, commit.

## Tests to write

In `tests/integration_jsonb_mutations.rs`:

### Path-normalizer

1. `path_string_form_parses` — `'$.a.b'`.
2. `path_json_array_form_parses` — `'["a","b"]'`.
3. `path_rejects_wildcards` — `'$.*'`, `'$[*]'`.

### JSONB_SET

4. `jsonb_set_updates_existing_leaf`.
5. `jsonb_set_creates_missing_when_create_is_true`.
6. `jsonb_set_noop_when_create_is_false_and_path_missing`.
7. `jsonb_set_stores_json_null_without_deletion`.
8. `jsonb_set_negative_array_index_sets_from_end`.
9. `jsonb_set_scalar_root_raises`.

### JSONB_INSERT

10. `jsonb_insert_array_before`.
11. `jsonb_insert_array_after`.
12. `jsonb_insert_object_missing_key_adds`.
13. `jsonb_insert_object_existing_key_raises` — **PG divergence**.

### JSONB_DELETE_PATH

14. `jsonb_delete_path_removes_object_key`.
15. `jsonb_delete_path_removes_array_element`.
16. `jsonb_delete_path_empty_returns_target`.
17. `jsonb_delete_path_scalar_raises`.

### JSON_INSERT (MySQL)

18. `json_insert_missing_key_adds`.
19. `json_insert_existing_key_is_silent_noop` — **MySQL divergence**.
20. `json_insert_variadic_multiple_pairs`.

### JSON_REPLACE (MySQL)

21. `json_replace_existing_updates`.
22. `json_replace_missing_is_silent_noop`.
23. `json_replace_variadic_multiple_pairs`.

### Cross-cutting

24. `null_target_returns_null_on_every_function`.
25. `non_json_target_errors`.

## Anti-patterns to avoid

- **Do not** duplicate the path-parsing logic per function. One
  normalizer for all mutation functions.
- **Do not** return `Value::Json(...)` from the PG-named functions
  — they must emit `Value::Jsonb(...)` to match PG's result type.
- **Do not** silently swallow the PG jsonb_insert existing-key error;
  it is a user-visible behavior that tooling sometimes relies on.
- **Do not** treat missing intermediates the same across
  `jsonb_set(create_if_missing=true)` and `jsonb_insert`. PG's
  `jsonb_insert` does NOT create intermediates — our impl must
  match.
- **Do not** accept wildcards in mutation paths. Both PG and MySQL
  reject them and a silent wildcard "apply-to-all" would be a
  footgun.

## Risks

1. **Divergent semantics between `jsonb_insert` and `JSON_INSERT`
   look identical at the call site.** Mitigation: tests #13 and
   #19 assert the opposite outcomes for the same operand shape;
   commit message + progreso.md document the difference.
2. **Path-as-JSON-array edge cases.** A JSON array containing
   numbers like `[0, 1]` steps through arrays; one containing
   strings like `["a", "b"]` steps through objects. Mitigation:
   the normalizer always returns `Vec<String>` and `set_path_ext`
   disambiguates by looking at the current container type (same
   pattern as PG `setPath`).
3. **Negative array indices out of range.** PG raises; MySQL
   silently no-ops. Our shared helper must accept a per-function
   policy. Mitigation: the current `set_path` already handles
   indices conservatively — confirm with a test.
4. **`new_value = SQL NULL` vs JSON `null`.** PG `jsonb_set`
   requires non-NULL `new_value` at SQL level. Our impl makes a
   choice: SQL NULL `new_value` propagates to SQL NULL output (no
   mutation). `jsonb_set_lax` (deferred) will honor the
   `null_value_treatment` enum.
5. **Variadic MySQL functions with odd arg counts.** `JSON_INSERT(doc, p1)`
   missing the value partner must error. Mitigation: explicit arity
   check, tested by #20 / #23.
6. **Return-type mismatch on round-trips.** `JSONB_SET` returns
   `Value::Jsonb`. Callers storing its output into a `TEXT` column
   must see text. Our coercion layer handles `Jsonb → Text` already
   (Phase 11.16). Mitigation: the acceptance test asserts the
   result renders as JSON text when projected over the wire.
