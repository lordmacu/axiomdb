# Spec: 11.21b — `@?` JSONPath-exists operator

## What to build
PostgreSQL `@?` binary operator: `doc @? 'jsonpath'` returns boolean true
when the path has at least one match against `doc`. Equivalent to the
`jsonb_path_exists(doc, path)` function surface from 11.21a.

## Acceptance criteria
- [ ] `'{"a":1}'::jsonb @? '$.a'` → true
- [ ] `'{"a":1}'::jsonb @? '$.z'` → false
- [ ] NULL doc → NULL
- [ ] NULL path → NULL
- [ ] Works as WHERE predicate on a JSONB column
- [ ] Works on text doc (implicit JSON parse)
- [ ] No regression in 11.18a `?` or 11.17 `@>` operators

## Out of scope (→ 11.21c)
- `@@` JSONPath-match operator (conflicts with MySQL session-var `@@name`)
- JSONPath variables + `PASSING`
- `jsonb_path_ops` GIN opclass
- Planner predicate extraction for indexable JSONPath

## Research
- PG `src/backend/utils/adt/jsonfuncs.c` — `@?` = `jsonb_path_exists_opr`
- PG docs on JSONPath operators
