# Spec: 11.23a — `JSON_SCHEMA_VALID` predicate (Draft-07 subset)

## What to build
Boolean predicate `JSON_SCHEMA_VALID(schema, doc)` implementing a robust
subset of JSON Schema Draft-07 sufficient for most user-facing
constraints.

## Supported keywords
- `type` (string or union array)
- `enum`, `const`
- `required`, `properties` (recursive), `additionalProperties` (bool)
- `items` (single schema or positional array of schemas)
- `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`
- `minLength`, `maxLength` (Unicode codepoint-aware)
- `minItems`, `maxItems`
- Boolean schemas `true` (accept-all) / `false` (reject-all)

## Acceptance criteria
- [ ] Type matching per spec (integer accepts whole-valued numbers)
- [ ] Required keys enforced on objects
- [ ] Properties recursion into sub-schemas
- [ ] `additionalProperties: false` rejects extra keys
- [ ] Numeric bounds (incl. exclusive) enforced
- [ ] String length bounds enforced (codepoint-aware)
- [ ] Array items validation (homogeneous + positional)
- [ ] Enum + const exact-match
- [ ] `true`/`false` boolean schemas
- [ ] NULL on either arg → NULL
- [ ] JSONB and TEXT inputs accepted
- [ ] ≥ 15 integration tests

## Out of scope (→ 11.23b/c/d)
- Validation report with path/keyword/message (11.23b)
- Catalog-stored named schemas + DDL (11.23c)
- Regex `pattern` / `patternProperties`, `format`, `oneOf|anyOf|allOf|not`,
  `$ref`, `dependencies`, `uniqueItems`, `propertyNames`, `if/then/else` (11.23d)

## Research
- JSON Schema Draft-07 spec (json-schema.org)
- DuckDB `yyjson` pattern for deep structural walks
