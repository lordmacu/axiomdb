# Spec: 11.20d2 — `JSON_TABLE` as first `FROM` + `CROSS APPLY` / `OUTER APPLY`

## What to build (not how)

Two independent grammar-level extensions on top of Phase 11.20a–d1 that
both reuse the existing nested-loop JOIN machinery in
`execute_select_with_joins_ctx`:

1. **JSON_TABLE as the first `FROM` entry combined with JOINs.** Today
   `execute_select_json_table_source` (`select_core.rs:461`) rejects any
   query of the form

   ```sql
   SELECT … FROM JSON_TABLE(doc, '$[*]' COLUMNS(…)) AS j JOIN t ON …
   ```

   with `NotImplemented: "JSON_TABLE as the first FROM entry combined
   with JOIN — deferred to 11.20d"`. This spec lifts that restriction
   **for the non-correlated case** — the `doc` expression is still
   evaluated once against an empty environment (LATERAL correlated doc
   stays deferred to 11.20d3).

2. **`CROSS APPLY` / `OUTER APPLY`** parser-level aliases for non-
   correlated `INNER JOIN … ON TRUE` / `LEFT JOIN … ON TRUE` on the
   right side of a JOIN. Semantics identical to:

   ```sql
   FROM t CROSS APPLY JSON_TABLE(t.doc, '$[*]' COLUMNS(…)) AS j
    ≡  FROM t JOIN JSON_TABLE(t.doc, '$[*]' COLUMNS(…)) AS j ON TRUE
   ```

   Because the `doc` expression is still non-correlated in 11.20d2, any
   reference to outer columns (`t.doc`) on the APPLY right side raises
   the same `NotImplemented` 11.20d3 error as today's
   `select_joins_ctx.rs:63-70` path. CROSS APPLY with a literal or
   parameter-only doc (e.g. `CROSS APPLY JSON_TABLE('[1,2]', '$[*]' …)`)
   works.

No new evaluator primitives. No AST changes beyond optionally tagging a
`JoinType` variant for `CrossApply` / `OuterApply` (can also desugar at
parse time — decision deferred to the plan).

## Inputs / outputs

### Grammar

```
from_item  ::= table_ref
             | subquery_alias
             | json_table_call [ alias ]

join_item  ::= join_keyword join_source [ join_condition ]
             | 'CROSS' 'APPLY' join_source
             | 'OUTER' 'APPLY' join_source

join_source ::= table_ref | subquery_alias | json_table_call [ alias ]
```

Both `JSON_TABLE(…)` in a first-FROM position and in an `APPLY` position
resolve to the same `FromClause::JsonTable` AST variant already produced
by 11.20a. First-FROM + JOIN combinations accepted for Table, Subquery,
and JsonTable interchangeably.

### AST deltas

Minimum: none required. `CROSS APPLY` can desugar at parse time into
`Join { join_type: JoinType::Inner, table, condition: Expr::Bool(true) }`
and `OUTER APPLY` into `JoinType::LeftOuter`.

Optional clean route: add `JoinType::CrossApply` / `JoinType::OuterApply`
variants for round-trip printing fidelity. Recommendation (decided in
plan): desugar at parse time — fewer AST touch points, no printer
regressions, semantics are identical.

### Executor path

`execute_select_json_table_source` (current dead-end for non-empty
`stmt.joins`) reroutes non-empty-joins cases into a shared
`execute_select_with_joins_ctx`-style loop that accepts
`FromClause::JsonTable` as the first source. Concretely:

- Compile the first-FROM `JsonTableAst` once.
- Evaluate `doc` against an empty row (identical to the right-side
  non-correlated path).
- Materialize rows via `materialize_json_table`.
- Feed into the same `all_sources` / `scanned` vectors that
  `execute_select_with_joins_ctx` already builds.
- Apply joins, WHERE, GROUP BY, HAVING, ORDER BY, LIMIT exactly as
  today.

## Use cases

```sql
-- 1. JSON_TABLE first, JOIN to a real table.
SELECT j.id, t.name
  FROM JSON_TABLE('[{"id":1},{"id":2}]', '$[*]'
         COLUMNS (id INT PATH '$.id')) AS j
  JOIN users t ON t.id = j.id;

-- 2. JSON_TABLE first, JOIN to a subquery.
SELECT j.v, q.c
  FROM JSON_TABLE('[{"v":10}]', '$[*]' COLUMNS (v INT PATH '$.v')) AS j
  LEFT JOIN (SELECT 10 AS c) q ON q.c = j.v;

-- 3. CROSS APPLY on the right, non-correlated JSON doc.
SELECT t.id, j.val
  FROM users t
  CROSS APPLY JSON_TABLE('[1,2,3]', '$[*]'
                COLUMNS (val INT PATH '$')) AS j;

-- 4. OUTER APPLY, non-correlated empty result preserves the left row.
SELECT t.id, j.val
  FROM users t
  OUTER APPLY JSON_TABLE('[]', '$[*]'
                COLUMNS (val INT PATH '$')) AS j;
```

Correlated APPLY (e.g. `CROSS APPLY JSON_TABLE(t.doc, …)`) still raises
`NotImplemented (11.20d3)`.

## Acceptance criteria

- [ ] `SELECT … FROM JSON_TABLE(…) AS j JOIN t ON …` parses and
      executes with JSON_TABLE as the first source.
- [ ] `LEFT JOIN` / `INNER JOIN` both work with JSON_TABLE as the first
      source.
- [ ] `JSON_TABLE(…) AS j JOIN (SELECT …) q ON …` works (JSON_TABLE
      first, subquery second).
- [ ] `JSON_TABLE(…) AS j1 JOIN JSON_TABLE(…) AS j2 ON …` works (both
      sources JSON_TABLE).
- [ ] `CROSS APPLY JSON_TABLE(…)` produces identical rows to
      `JOIN JSON_TABLE(…) ON TRUE`.
- [ ] `OUTER APPLY JSON_TABLE(…)` preserves left rows when the
      right side yields zero rows.
- [ ] `CROSS APPLY t2` on a regular table works (APPLY is a generic
      alias, not JSON_TABLE-specific).
- [ ] Correlated `CROSS APPLY JSON_TABLE(t.doc, …)` still returns the
      existing 11.20d3 `NotImplemented` error with an actionable
      message.
- [ ] WHERE, ORDER BY, GROUP BY, LIMIT over the joined output work
      identically to existing join paths.
- [ ] WRAPPER / QUOTES / PASSING (11.20d1) and NESTED PATH (11.20b/c)
      work unchanged when JSON_TABLE is the first source.
- [ ] 11.20a, 11.20b, 11.20c, 11.20d1 regression tests all still pass.
- [ ] Wire smoke test (`tools/wire-test.py`) adds at least 3 assertions
      covering first-FROM JSON_TABLE + JOIN, CROSS APPLY, OUTER APPLY.
- [ ] `cargo test --workspace` green, `cargo clippy -- -D warnings`
      clean, `cargo fmt --check` clean.

## Out of scope

- LATERAL-correlated `doc` / PASSING expressions that reference outer
  columns (→ 11.20d3).
- JSON_TABLE as the target of `UPDATE` / `DELETE` / `MERGE`
  (→ 11.20d4).
- Standards-explicit `LATERAL` keyword in front of subqueries or
  JSON_TABLE in the FROM list (PG syntax). Can be added as a no-op
  keyword in 11.20d3 when correlation lands.
- Round-trip AST printing that preserves the literal `CROSS APPLY` /
  `OUTER APPLY` surface form — we desugar at parse time; printers emit
  the equivalent `JOIN … ON TRUE`.

## Dependencies

- Phase 11.20a — `FromClause::JsonTable`, `materialize_json_table`,
  `compile_json_table`, `doc_has_column_refs`.
- Phase 11.20b/c — NESTED PATH / multi-sibling (orthogonal, must keep
  working).
- Phase 11.20d1 — WRAPPER / QUOTES / PASSING on columns (orthogonal,
  must keep working).
- Existing join infrastructure in
  `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`.
