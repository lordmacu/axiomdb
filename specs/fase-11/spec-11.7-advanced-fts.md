# Spec: 11.7 — Advanced FTS

## What to build
Extend MATCH() with boolean operators, phrase queries, and prefix matching.

## Query syntax
```sql
MATCH(col, '+rust +database')        -- AND (both required)
MATCH(col, 'rust -python')           -- NOT (exclude python)
MATCH(col, 'rust | golang')          -- OR (either)
MATCH(col, '"database engine"')      -- PHRASE (exact sequence)
MATCH(col, 'data*')                  -- PREFIX (starts with data)
```

## Acceptance criteria
- [ ] `+term` requires term present (AND)
- [ ] `-term` excludes rows containing term
- [ ] `term1 | term2` matches either (OR)
- [ ] `"exact phrase"` matches consecutive tokens using positions
- [ ] `prefix*` matches terms starting with prefix
- [ ] Combinations work: `+rust +"database engine" -python`
