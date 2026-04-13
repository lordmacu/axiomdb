# Plan: 11.22b — `jsonb_set_lax`

## Files to modify
- `crates/axiomdb-sql/src/eval/functions/json.rs` — add `"jsonb_set_lax"` arm
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — register name
- `crates/axiomdb-sql/tests/integration_jsonb_set_lax.rs` — new test file

## Algorithm
```
fn jsonb_set_lax(args):
    if args.len() < 3 or > 5: TypeMismatch
    target = eval(args[0])
    path   = eval(args[1])
    if target == NULL or path == NULL: return NULL
    new_value = eval(args[2])
    cim = if args.len() >= 4 {
        v = eval(args[3]); if v == NULL { return NULL }; is_truthy(v)
    } else { true }
    treatment = if args.len() == 5 {
        v = eval(args[4])
        if v == NULL: InvalidValue("null_value_treatment must be ...")
        as_text(v)
    } else { "use_json_null" }

    // non-null new_value → delegate to jsonb_set
    if new_value != NULL:
        return jsonb_set_core(target, path, new_value, cim)

    match treatment:
        "use_json_null"   → jsonb_set_core(target, path, JsonNull, cim)
        "raise_exception" → InvalidValue("JSON value must not be null")
        "delete_key"      → jsonb_delete_path_core(target, path)
        "return_target"   → return target (as Jsonb)
        _                 → InvalidValue("null_value_treatment must be ...")
```

## Implementation
Reuse existing 11.22a helpers (`set_path_ext`, `remove_path_parts`, `parse_mutation_path`,
`jsonb_blob_from_serde`, `value_to_serde_json`, `sql_to_serde_json`). Factor the shared
core steps inline in the arm (no need for new helper — both `set_path_ext` and
`remove_path_parts` already do the work).

## Tests (≥ 10)
1. 3-arg non-null value → jsonb_set behavior
2. 3-arg NULL value → default `use_json_null` embeds JSON null
3. 5-arg `use_json_null` explicit
4. 5-arg `raise_exception` → error
5. 5-arg `delete_key` removes leaf
6. 5-arg `return_target` returns target unchanged
7. 5-arg invalid treatment → error
8. NULL target → NULL
9. NULL path → NULL
10. NULL create_if_missing → NULL
11. NULL treatment literal (SQL NULL in arg 5) → error
12. `create_if_missing = false` + missing path + non-null value → target unchanged
13. Wildcard path → rejected

## Anti-patterns to avoid
- Do NOT treat SQL NULL new_value the same as JSON null — they route differently
- Do NOT short-circuit NULL treatment before checking treatment-arg NULL
- Do NOT auto-coerce arbitrary text to treatment enum — strict match only

## Risks
- Arg-count confusion between 3/4/5 forms → clear branches per arity
- Case sensitivity of treatment strings → PG uses exact lowercase match; mirror that
