# Spec: 11.24a — Oracle JSON scalar functions

## What to build
Three bounded Oracle JSON surface functions.

## Functions
| Name | Semantics | Return |
|---|---|---|
| `JSON_EQUAL(a, b)` | Deep structural equality (key-order insensitive, recursive). NULL if either input is NULL. | bool |
| `JSON_SCALAR(v)` | Wraps a SQL scalar into a JSONB scalar value. NULL → NULL. | jsonb |
| `JSON_SERIALIZE(v)` | Canonical TEXT rendering of any JSON/JSONB input. Normalizes whitespace. NULL → NULL. | text |

## Acceptance criteria
- [ ] `JSON_EQUAL('{"a":1}','{"a":1}')` → true
- [ ] `JSON_EQUAL('{"a":1,"b":2}','{"b":2,"a":1}')` → true (key-order insensitive)
- [ ] `JSON_EQUAL('{"a":1}','{"a":2}')` → false
- [ ] `JSON_EQUAL(NULL, x)` → NULL
- [ ] `JSON_EQUAL(JSONB, TEXT)` bridges surfaces
- [ ] `JSON_SCALAR(42)` → jsonb `42`
- [ ] `JSON_SCALAR('hi')` → jsonb `"hi"`
- [ ] `JSON_SCALAR(NULL)` → NULL
- [ ] `JSON_SERIALIZE(jsonb)` → canonical text
- [ ] `JSON_SERIALIZE('[1, 2 , 3]')` → `[1,2,3]` (normalized)
- [ ] `JSON_SERIALIZE(NULL)` → NULL
- [ ] ≥ 10 integration tests

## Out of scope (→ 11.24b/c/d)
- `JSON_TRANSFORM` (11.24b)
- Dot notation (11.24c)
- Data Guide / duality views (11.24d)
- `JSON_EQUAL`'s Oracle-specific error handlers (ERROR/FALSE ON ERROR) — default
  lenient; errors from malformed JSON already surface via `value_to_serde_json`.
