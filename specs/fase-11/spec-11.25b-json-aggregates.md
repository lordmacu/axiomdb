# Spec: 11.25b — JSON aggregates + constructors (PG/MySQL parity)

## What was built

Two families of functions landed in this subphase:

### Aggregate functions

| Function | DB | Output type | Arity |
|---|---|---|---|
| `jsonb_agg(expr)` | PG | JSONB binary | 1-arg |
| `json_agg(expr)` | PG | JSON text | 1-arg |
| `JSON_ARRAYAGG(expr)` | MySQL | JSON text | 1-arg |
| `jsonb_object_agg(key, value)` | PG | JSONB binary | 2-arg |
| `json_object_agg(key, value)` | PG | JSON text | 2-arg |
| `JSON_OBJECTAGG(key, value)` | MySQL | JSON text | 2-arg |

**Array aggregate** (`jsonb_agg` / `json_agg` / `JSON_ARRAYAGG`): collects all input values
into a JSON array. Empty input → NULL (PG semantics).

**Object aggregate** (`jsonb_object_agg` / `json_object_agg` / `JSON_OBJECTAGG`):
collects key-value pairs into a JSON object. Duplicate keys → last-write-wins
(PG + MySQL agree). NULL key → error at accumulation time.

All six names are registered in `agg_descriptor::is_aggregate()`.
`jsonb_object_agg` / `json_object_agg` / `JSON_OBJECTAGG` use the new
`AggExpr::JsonbObjectAgg` variant with `returns_jsonb` flag. The three
1-arg variants reuse `AggExpr::Simple` with a normalized name; the
accumulator distinguishes JSON vs JSONB output via the `returns_jsonb`
flag in `JsonArrayAgg`.

Accumulator: `executor/agg_accum.rs` — new `JsonArrayAgg` and `JsonObjectAgg`
variants of `AggAccumulator`, with `update()` (collect value / insert-or-overwrite pair)
and `finalize()` (build array or object, encode to JSONB binary or return JSON text).

`value_to_serde_for_agg()` in `jsonb_srf.rs` converts any SQL `Value` to
`serde_json::Value` for aggregate accumulation: native JSON/JSONB pass through,
primitives map to their JSON equivalents, temporals/UUIDs/bytes fall back to
string representation.

### Constructor / mutator functions

| Function | DB | Description |
|---|---|---|
| `JSON_ARRAY(v, ...)` | MySQL/PG | Build JSON array; empty args → `[]` |
| `jsonb_build_array(v, ...)` | PG alias | Same as `JSON_ARRAY` but returns JSONB binary |
| `JSON_OBJECT(k, v, ...)` | MySQL/PG | Build JSON object; even arg count required; NULL key → error |
| `jsonb_build_object(k, v, ...)` | PG alias | Same as `JSON_OBJECT` but returns JSONB binary |
| `to_json(v)` | PG alias | Same as `to_jsonb(v)` but returns JSON text instead of JSONB binary |
| `JSON_MERGE_PRESERVE(d1, d2, ...)` | MySQL | Arrays concatenate; objects key-merge recursively (conflict → both wrap into array); other type mismatch → promote to unified array; any NULL arg → NULL |
| `JSON_CONTAINS_PATH(doc, 'one'\|'all', p1, p2, ...)` | MySQL | Returns true if path exists; `'one'` = any path; `'all'` = all paths |

All dispatch via `eval/functions/json.rs::eval()` on lowercase name.

## Acceptance criteria

- [ ] `SELECT jsonb_agg(col) FROM t` returns JSONB binary array
- [ ] `SELECT json_agg(col) FROM t` returns JSON text array
- [ ] `SELECT JSON_ARRAYAGG(col) FROM t` returns JSON text array
- [ ] `SELECT jsonb_object_agg(k, v) FROM t` returns JSONB binary object
- [ ] `SELECT json_object_agg(k, v) FROM t` returns JSON text object
- [ ] `SELECT JSON_OBJECTAGG(k, v) FROM t` returns JSON text object
- [ ] `jsonb_object_agg(NULL, v)` → error mentioning "null key"
- [ ] Duplicate keys in object aggregate → last value wins
- [ ] Empty grouped set → NULL (not empty array)
- [ ] `JSON_ARRAY()` with no args → `[]`
- [ ] `JSON_OBJECT()` with odd args → error
- [ ] `JSON_MERGE_PRESERVE('[1]', '[2]')` → `[1, 2]`
- [ ] `JSON_MERGE_PRESERVE('{"a":1}', '{"a":2}')` → `{"a":[1,2]}`
- [ ] `JSON_CONTAINS_PATH(doc, 'one', '$.a', '$.b')` → true if any path exists
- [ ] `JSON_CONTAINS_PATH(doc, 'all', '$.a', '$.b')` → true only if all exist
- [ ] 10 integration tests in `integration_jsonb_agg.rs` pass
- [ ] 10 integration tests in `integration_jsonb_mutators_completion.rs` pass
- [ ] 2 wire smoke tests pass (MySQL protocol end-to-end)
