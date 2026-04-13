# Spec: 11.21a — PG `jsonb_path_*` function family

## What to build
PostgreSQL-named JSONPath evaluator functions that expose the existing
`parse_jsonpath`/`execute_jsonpath` infrastructure under PG-standard names.
Three new, two aliases.

## Functions
| Name | Semantics | Return |
|---|---|---|
| `jsonb_path_exists(target, path)` | alias of existing `json_path_exists` | `bool` |
| `jsonb_path_query(target, path)` | alias of existing `json_path_query` (PG returns setof; AxiomDB returns JSON array of matches) | `json` |
| `jsonb_path_query_first(target, path)` | alias of existing `json_path_query_first` | scalar JSON / NULL |
| `jsonb_path_query_array(target, path)` | wrap all matches in a JSONB array | `jsonb` |
| `jsonb_path_match(target, path)` | path must produce a boolean; returns that bool. If zero or multi-result / non-boolean → NULL | `bool` / NULL |

## Acceptance criteria
- [ ] `jsonb_path_exists('{"a":1}', '$.a')` → true
- [ ] `jsonb_path_exists('{"a":1}', '$.z')` → false
- [ ] `jsonb_path_query('[1,2,3]', '$[*]')` → `[1,2,3]` (JSON text)
- [ ] `jsonb_path_query_first('[10,20]', '$[*]')` → 10
- [ ] `jsonb_path_query_array('[1,2,3]', '$[*]')` → JSONB `[1,2,3]`
- [ ] `jsonb_path_query_array('{}', '$.z')` → JSONB `[]` (empty, not NULL)
- [ ] `jsonb_path_match('{"a":true}', '$.a')` → true
- [ ] `jsonb_path_match('{"a":false}', '$.a')` → false
- [ ] `jsonb_path_match('{"a":1}', '$.a')` — non-boolean → NULL (permissive) or error (strict); default permissive (return NULL)
- [ ] `jsonb_path_match('{}', '$.z')` — no match → NULL
- [ ] NULL doc → NULL for all
- [ ] ≥ 12 integration tests, clippy/fmt clean

## Out of scope (→ 11.21b/c)
- `@?` / `@@` binary operators
- JSONPath variables (`$var`) and `PASSING` bindings
- Richer accessors (`.type()`, `.size()`, arithmetic in filters beyond existing support)
- Planner predicate extraction
- `jsonb_path_ops` hash-based GIN opclass

## Dependencies
- Existing `parse_jsonpath`, `execute_jsonpath`, `PathStep`, `value_to_serde_json`,
  `jsonb_blob_from_serde`, `serde_json_to_sql_value`.
