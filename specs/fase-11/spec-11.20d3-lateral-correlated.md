# Spec: 11.20d3 — LATERAL-correlated `JSON_TABLE` (doc + PASSING)

## What to build (not how)

Lift the three `NotImplemented` 11.20d3 errors that today block correlated
`JSON_TABLE` on the right side of a JOIN / CROSS APPLY / OUTER APPLY, plus
the equivalent first-FROM guardrail. After this subphase the following
all execute:

```sql
-- correlated doc
SELECT t.id, j.v
  FROM orders t
  CROSS APPLY JSON_TABLE(t.payload, '$.items[*]'
                COLUMNS (v INT PATH '$.qty')) AS j;

-- correlated PASSING
SELECT t.id, j.hit
  FROM users t
  CROSS APPLY JSON_TABLE('[1,2,3,4,5]', '$[*] ? (@ > $threshold)'
                PASSING t.min_age AS threshold
                COLUMNS (hit INT PATH '$')) AS j;

-- PG-style LATERAL keyword (no-op sugar for JSON_TABLE)
SELECT t.id, j.v
  FROM orders t
  JOIN LATERAL JSON_TABLE(t.payload, '$.items[*]'
                COLUMNS (v INT PATH '$.qty')) AS j ON TRUE;
```

Both correlated `doc` and correlated `PASSING` expressions rebuild their
environment per outer row; the JSON_TABLE row source runs once per outer
row against that environment. Non-correlated JSON_TABLE keeps the
existing single-materialization fast path.

## Inputs / outputs

### Grammar

```
from_item := ... | 'LATERAL'? json_table_call [ alias ]
                 | 'LATERAL'? '(' select ')' alias   -- LATERAL no-op here too
join_item := 'JOIN' 'LATERAL'? source join_condition
           | 'CROSS APPLY' source                    -- already in 11.20d2
           | 'OUTER APPLY' source                    -- already in 11.20d2
           | (existing arms)
```

`LATERAL` is accepted as an **optional prefix** before `JSON_TABLE(...)`
and before parenthesized subqueries in both first-FROM and join-source
positions. It is semantically a no-op today — AxiomDB's analyzer already
allows correlated `doc`/`PASSING` expressions on JSON_TABLE sources when
this subphase lands, and bare subqueries remain non-correlated (a spec-
level deviation from PG, consistent with current AxiomDB behavior). The
keyword exists for PG syntactic compatibility.

### AST deltas

None required if LATERAL is parsed-and-discarded. For debuggability the
spec suggests a single `bool lateral` flag on `FromClause::JsonTable` and
`FromClause::Subquery`, defaulting to `false`. That flag is not currently
consulted by the executor — it is informational.

### Executor path

1. **Right-side correlated JSON_TABLE** in `select_joins_ctx.rs`:
   - Replace the `NotImplemented 11.20d3` early-return.
   - Detect correlation: `doc_has_column_refs(jt.doc) ||
     passing_has_column_refs(jt.passing)`.
   - If non-correlated → existing path (materialize once, feed into
     `apply_join`).
   - If correlated → a new per-outer-row path:
     - For each `outer_row` in `combined_rows`:
       - Evaluate `doc` against `outer_row`.
       - Evaluate each `PASSING` expr against `outer_row`, building a
         `PassingEnv`.
       - Call `materialize_json_table(spec, &sj, outer_row, env, runner)`
         (the spec currently threads the outer row; PassingEnv path came
         in 11.20d1).
       - Combine with `outer_row` per join semantics:
         - `INNER` / `CROSS APPLY`: emit each `(outer, right)` pair that
           passes the ON condition.
         - `LEFT` / `OUTER APPLY`: if no right row matches, emit
           `(outer, NULL-padded-right)`.
         - `RIGHT` / `FULL`: unsupported on a correlated JT right side
           (PG rejects; AxiomDB raises `NotImplemented` with a clear
           message).
         - `CROSS JOIN`: treated as `INNER ON TRUE`.

2. **First-FROM correlated doc** in `execute_select_json_table_source`:
   - By definition there is no outer source → correlated `doc` is still
     a semantic error. Keep the existing `doc_has_column_refs` guard but
     update the error message away from `deferred to 11.20d` to
     `not allowed: first-FROM JSON_TABLE has no outer source to reference`.

3. **PASSING with column refs:**
   - Parser currently accepts any `Expr` in `PASSING expr AS name`.
     Analyzer must resolve those expressions against the outer scope
     when the JT is a right-side source.
   - Executor evaluates the resolved expression against the outer row
     each iteration.

### Algorithm / Semantics

Correlated-JT-right-side nested loop:

```
INNER / CROSS APPLY / CROSS JOIN:
    for outer in combined_rows:
        env = build_env(passing, outer)
        doc_val = eval(doc, outer)
        rows = materialize(spec, doc_val, env, outer)
        for right in rows:
            if eval_on_condition(outer ++ right):
                out.push(outer ++ right)

LEFT / OUTER APPLY:
    for outer in combined_rows:
        env = build_env(passing, outer)
        doc_val = eval(doc, outer)
        rows = materialize(spec, doc_val, env, outer)
        matched = false
        for right in rows:
            if eval_on_condition(outer ++ right):
                out.push(outer ++ right)
                matched = true
        if !matched:
            out.push(outer ++ nulls(right_cols))

RIGHT / FULL:
    NotImplemented — correlated right side makes these ill-defined
    unless the left is re-scanned per right row. Match PG's behavior
    (rejects correlation for RIGHT/FULL LATERAL).
```

Hash-join / spill paths are skipped for correlated JT right sides;
they only fire for `left_rows.len() + right_rows.len() >= N`, but
correlated mode never builds the full `right_rows` up front, so the
fast path doesn't apply.

## Use cases

Beyond the examples in "What to build":

```sql
-- correlated doc, NESTED PATH, multi-level shred per outer row
SELECT t.id, j.tag, j.sub
  FROM orders t
  CROSS APPLY JSON_TABLE(t.payload, '$.items[*]'
                COLUMNS (
                  tag TEXT PATH '$.tag',
                  NESTED PATH '$.subs[*]' COLUMNS (sub TEXT PATH '$')
                )) AS j;

-- outer PASSING into NESTED filter
SELECT t.id, j.v
  FROM cfg t
  CROSS APPLY JSON_TABLE('[1,2,3,4]', '$[*]'
                PASSING t.min AS lo, t.max AS hi
                COLUMNS (
                  v INT PATH '$ ? (@ >= $lo && @ <= $hi)'
                )) AS j;

-- LATERAL keyword
SELECT u.id, j.v
  FROM users u
  JOIN LATERAL JSON_TABLE(u.tags_json, '$[*]'
                COLUMNS (v TEXT PATH '$')) AS j ON TRUE;
```

## Acceptance criteria

- [ ] `CROSS APPLY JSON_TABLE(outer.col, ...)` executes — rows emit per
      outer row using that outer row's `doc`.
- [ ] `OUTER APPLY JSON_TABLE(outer.col, ...)` preserves outer row
      with NULL-padded right when `doc` yields no rows.
- [ ] `JOIN JSON_TABLE(outer.col, ...) ON cond` (INNER) executes with
      correlation; ON condition filters per outer row.
- [ ] `LEFT JOIN JSON_TABLE(outer.col, ...) ON cond` preserves outer
      rows with no matching JT rows, NULL-padding.
- [ ] `PASSING outer.col AS var` threads outer values into the row
      path / column paths / NESTED paths / filter exprs (`$var`).
- [ ] NESTED PATH + WRAPPER / QUOTES / multi-sibling / multi-level
      (11.20b/c/d1) keep working when `doc` / PASSING are correlated.
- [ ] `JOIN LATERAL src` parses; LATERAL is a no-op keyword.
- [ ] `FROM LATERAL JSON_TABLE(...)` parses; LATERAL is a no-op
      (no outer scope so correlation still rejected).
- [ ] `RIGHT JOIN JSON_TABLE(outer.col, ...)` and `FULL JOIN ...` raise
      a clear `NotImplemented` — correlation not allowed on that side.
- [ ] First-FROM correlated `doc` (no outer source) raises
      `correlated JSON_TABLE requires an outer FROM source` (renamed
      from the current 11.20d3 placeholder).
- [ ] Non-correlated `JSON_TABLE` path is unchanged — regression
      tests 11.20a/b/c/d1/d2 all pass.
- [ ] 10–14 integration tests in
      `tests/integration_json_table_correlated.rs`.
- [ ] 2–3 new wire smoke assertions under `[11.20d3]`.
- [ ] `cargo test --workspace`, `cargo clippy -- -D warnings`,
      `cargo fmt --check` all clean.

## Out of scope

- LATERAL for plain subqueries with outer-column references in the
  inner SELECT list / WHERE. That requires the analyzer's
  `BindContext` to expose outer scope to derived-table resolution, a
  separate subphase.
- RIGHT/FULL JOIN LATERAL — PG rejects too; we match.
- LATERAL in UPDATE / DELETE / MERGE sources (→ 11.20d4).
- Hash/spill optimization for correlated JT right sides — always
  nested-loop per-outer-row. Acceptable: correlated batch sizes are
  bounded by outer row count × JT shape; spill is overkill.
- Planner pushdown of JT predicates into underlying GIN (→ 11.21h).

## Dependencies

- Phase 11.20a — `FromClause::JsonTable`, `compile_json_table`,
  `materialize_json_table`, `doc_has_column_refs`,
  `column_metas_for_spec`, `doc_to_serde`.
- Phase 11.20b/c — NESTED PATH infrastructure. Must keep working.
- Phase 11.20d1 — `PassingEnv`, `execute_jsonpath_env` /
  `execute_jsonpath_owned_env`, `materialize_regular` WRAPPER/QUOTES
  reuse.
- Phase 11.20d2 — `execute_select_with_joins_first_materialized`
  shared entry point; CROSS / OUTER APPLY desugar.
- Existing `apply_join` stays untouched — correlated JT right side
  bypasses it with a targeted inline loop.
