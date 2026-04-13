# Spec: 11.19b — JSON_QUERY WRAPPER and QUOTES clauses

## What to build
SQL:2016 `WITH [CONDITIONAL|UNCONDITIONAL] ARRAY WRAPPER` / `WITHOUT [ARRAY] WRAPPER`
and `KEEP|OMIT QUOTES [ON SCALAR STRING]` clauses on `JSON_QUERY`. Defaults:
`WITHOUT ARRAY WRAPPER` + `KEEP QUOTES`.

## Grammar (extends 11.19a)
```
JSON_QUERY ( doc, path
  [ RETURNING type ]
  [ WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY] WRAPPER
  | WITHOUT [ARRAY] WRAPPER ]
  [ {KEEP|OMIT} QUOTES [ON SCALAR STRING] ]
  [ ON EMPTY behavior ]
  [ ON ERROR behavior ] )
```

## Acceptance criteria
- [ ] Single-item match + WITHOUT → scalar (unchanged from 11.19a)
- [ ] Single non-array + WITH UNCONDITIONAL → `[x]`
- [ ] Array match + WITH CONDITIONAL → returns array unchanged
- [ ] Non-array match + WITH CONDITIONAL → wraps
- [ ] Multi-item + WITHOUT → ON ERROR (unchanged)
- [ ] Multi-item + WITH (either) → wraps all into one array
- [ ] `WITH ARRAY WRAPPER` (no modifier) → defaults to UNCONDITIONAL
- [ ] `OMIT QUOTES` on scalar string → raw text (Value::Text), no surrounding `"`
- [ ] `OMIT QUOTES` on non-string → no effect
- [ ] `KEEP QUOTES` (default) → JSON-text preserves `"…"`
- [ ] `ON SCALAR STRING` suffix parses but is a no-op modifier
- [ ] WRAPPER on JSON_VALUE / JSON_EXISTS → parse error
- [ ] QUOTES on JSON_VALUE / JSON_EXISTS → parse error
- [ ] ≥ 12 integration tests

## Out of scope
- `PASSING` variable bindings → 11.19c
- JSON_VALUE quote handling (already strips by virtue of scalar extraction)

## Research
- SQL:2016 § 6.29 JSON Query grammar
- PG `src/backend/parser/gram.y` + `execExprInterp.c` JSON_QUERY path
