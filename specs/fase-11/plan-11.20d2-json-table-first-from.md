# Plan: 11.20d2 — JSON_TABLE as first FROM + CROSS/OUTER APPLY

## Files to create/modify

### Modify

- `crates/axiomdb-sql/src/lexer.rs` — add `Token::Apply` keyword
  (`#[token("APPLY", ignore(ascii_case))]`).
- `crates/axiomdb-sql/src/parser/dml.rs`:
  - `parse_join_clauses` (line 335) — add two new match arms:
    - `Token::Cross` followed by `Token::Apply` → desugar to
      `JoinType::Inner` with `JoinCondition::On(Expr::Literal(
      Value::Bool(true)))`. Disambiguation: peek2 to distinguish
      `CROSS APPLY` from the existing `CROSS JOIN` arm. Consume
      both tokens. No `ON` / `USING` allowed after.
    - `Token::Outer` followed by `Token::Apply` → desugar to
      `JoinType::Left` with the same `ON TRUE` dummy. (Note the
      keyword is `OUTER APPLY` — consume `Outer` then `Apply`.)
  - Adjust the existing `Token::Cross` arm to look at the next token:
    if it is `Apply`, fall through into the new arm; otherwise proceed
    as CROSS JOIN.
- `crates/axiomdb-sql/src/executor/select_core.rs`:
  - `execute_select_json_table_source` (line 461) — remove the
    `NotImplemented` early-return when `!stmt.joins.is_empty()`.
    Instead, route non-empty-joins cases through a new helper
    `execute_select_json_table_source_with_joins` that materializes
    JSON_TABLE as the first source and feeds into the existing
    nested-loop join pipeline.
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`:
  - Extract the first-FROM source materialization into a small helper
    that accepts any `FromClause` (Table / Subquery / JsonTable) and
    returns `(JoinSourceSchema, Vec<Row>, column_count)`. Or — simpler
    — add a sibling entry point
    `execute_select_with_joins_first_jsontable(stmt, jt, exec_ctx,
    conn_txn, ctx)` that replaces only the first branch (lines 23-30)
    with JSON_TABLE materialization and reuses the rest verbatim.
    Choose extract-helper for less duplication; decide in step 1 of
    implementation.

### Create

- `crates/axiomdb-sql/tests/integration_json_table_first_from.rs` —
  new integration test file. 11–13 tests covering the acceptance
  criteria.

### No changes needed

- `crates/axiomdb-sql/src/ast.rs` — `CROSS APPLY` / `OUTER APPLY`
  desugar at parse time, so no new `JoinType` variants.
- `crates/axiomdb-sql/src/json_table.rs` — reuse existing
  `compile_json_table`, `doc_to_serde`, `materialize_json_table`,
  `column_metas_for_spec`, `doc_has_column_refs`.
- `crates/axiomdb-sql/src/parser/json_table.rs` — grammar for the
  `JSON_TABLE(…)` call itself is unchanged.

## Algorithm / Data structure

### Parse path

```
CROSS APPLY  source        →  JoinClause {
                                  join_type: JoinType::Inner,
                                  table:      source,
                                  condition:  JoinCondition::On(
                                                Expr::Literal(Bool(true))),
                              }

OUTER APPLY  source        →  JoinClause {
                                  join_type: JoinType::Left,
                                  table:      source,
                                  condition:  JoinCondition::On(
                                                Expr::Literal(Bool(true))),
                              }
```

Disambiguation detail: the `CROSS` token in `parse_join_clauses`
currently always expects `JOIN` next. We change it to peek at the
next token; if `Apply`, emit the APPLY desugar; if `Join`, continue
the existing CROSS JOIN path; otherwise parse error.

`source` in the APPLY position is just `parse_from_item(p)` — same
function that already parses tables, subqueries, and `JSON_TABLE(...)`
calls. APPLY is a generic alias; it is not JSON_TABLE-specific.

### Execute path

Before (rejected):
```
execute_select_ctx
  └─ stmt.from = Some(FromClause::JsonTable(_))
       └─ execute_select_json_table_source
            └─ !joins.is_empty() → NotImplemented
```

After:
```
execute_select_ctx
  └─ stmt.from = Some(FromClause::JsonTable(_))
       └─ execute_select_json_table_source
            ├─ joins empty   → current materialize-and-project path
            └─ joins present → execute_select_json_table_source_with_joins
                 ├─ compile + eval doc (empty row)
                 ├─ materialize first JSON_TABLE → scanned[0]
                 ├─ build JoinSourceSchema from column_metas_for_spec
                 └─ fall into the same loop as select_joins_ctx
                     (from join[0] onwards, handling Table / Subquery
                      / JsonTable sources exactly as today)
```

The right-side join loop already works for Table / Subquery /
JsonTable (non-correlated) — no changes there.

### Scope / LATERAL guardrail

Correlation in the `doc` expression of an APPLY right-side source is
still rejected by the existing `doc_has_column_refs` check in
`select_joins_ctx.rs:63-70`. Same error message, deferred to 11.20d3.

For the **first-FROM** JSON_TABLE: the `doc` there cannot reference
outer columns by definition (there is no outer source). We still run
`doc_has_column_refs` defensively so a malformed AST raises the same
11.20d3 message instead of a panic downstream.

## Implementation phases

1. **Parser — CROSS APPLY / OUTER APPLY (≈40 LoC).**
   - Add `Token::Apply` to lexer.
   - Extend `parse_join_clauses` with APPLY dispatch.
   - 3–4 parser-level unit tests in the existing parser test file
     (or inline `#[cfg(test)] mod tests`).

2. **Executor — first-FROM + JOIN (≈80 LoC).**
   - Decide helper shape (extracted helper vs sibling entry point).
   - Remove the `NotImplemented` early-return in
     `execute_select_json_table_source`.
   - Wire the first-JSON_TABLE-then-join path.
   - Keep the no-joins path untouched.

3. **Integration tests.**
   - Create `tests/integration_json_table_first_from.rs` with the
     cases listed below.

4. **Regression sweep.**
   - Run 11.20a/b/c/d1 suites.
   - Run `cargo test -p axiomdb-sql`.

5. **Close protocol.**
   - `cargo test --workspace`, clippy, fmt.
   - Wire smoke test: add 3 assertions.
   - Docs: `docs-site/src/internals/sql-parser.md`,
     `docs-site/src/user-guide/sql-reference/dml.md` (JSON_TABLE
     section — add APPLY paragraph and first-FROM example).
   - `docs/fase-11.md`, `docs/progreso.md`.
   - Commit + push.

## Tests to write

`tests/integration_json_table_first_from.rs`:

1. `jt_first_inner_join_real_table` — JSON_TABLE first, INNER JOIN to a
   real table, expected row count + projection match.
2. `jt_first_left_join_subquery` — JSON_TABLE first, LEFT JOIN to a
   subquery, unmatched row preserved with NULL padding.
3. `jt_first_join_jt_second` — JSON_TABLE first, JOIN to another
   JSON_TABLE on a shared key.
4. `cross_apply_vs_join_on_true_equivalence` — same query written as
   `CROSS APPLY` and `JOIN … ON TRUE` return identical rows.
5. `cross_apply_json_table_non_correlated` — `t CROSS APPLY
   JSON_TABLE(literal_doc, …)` — product of left rows × materialized
   JSON rows.
6. `outer_apply_preserves_left_on_empty` — `t OUTER APPLY JSON_TABLE(
   '[]', …)` keeps each left row with NULL-padded right side.
7. `cross_apply_regular_table` — `CROSS APPLY t2` works on a plain
   table (APPLY is a generic alias).
8. `correlated_apply_rejected_11_20d3` — `t CROSS APPLY JSON_TABLE(
   t.doc, …)` returns the documented `NotImplemented` error, same
   message as today.
9. `jt_first_where_filter` — WHERE clause applied after the join.
10. `jt_first_order_by_limit` — ORDER BY + LIMIT over joined output.
11. `jt_first_with_nested_path_and_passing` — 11.20b NESTED PATH + 11.20d1
    PASSING still work when JSON_TABLE is the first source.
12. `jt_first_group_by_aggregate` — GROUP BY / aggregate over joined
    output.

Wire smoke (`tools/wire-test.py`):
- 1 assertion: first-FROM JSON_TABLE + JOIN basic shape.
- 1 assertion: CROSS APPLY with a real table + JSON_TABLE.
- 1 assertion: OUTER APPLY preserves left rows on empty JSON.

## Anti-patterns to avoid

- **Don't duplicate the join loop body.** If the helper-extract route
  turns out awkward, prefer adding a tiny dispatch at the top of
  `execute_select_with_joins_ctx` rather than copy-pasting the ~120-
  line loop body.
- **Don't introduce a new AST variant for CROSS/OUTER APPLY.**
  Desugar at parse time. Round-trip printer fidelity is not required
  (out of scope per spec).
- **Don't silently accept `CROSS APPLY … ON …`.** APPLY takes no ON /
  USING. Parser must reject that explicitly.
- **Don't re-enable correlated `doc` accidentally.** Keep the existing
  `doc_has_column_refs` check on APPLY right-side sources. First-FROM
  source also runs it defensively.
- **Don't break prepared-statement `?` placeholder** when parsing
  APPLY. `Apply` is a separate keyword token, no collision.
- **Don't create a new execution crate or module.** Everything fits in
  `select_core.rs` + `select_joins_ctx.rs`.

## Risks

- **Keyword conflict on `APPLY`.** PG/MySQL do not reserve APPLY, but
  some users may have columns named `apply`. Mitigation: follow the
  existing pattern for context-sensitive keywords (e.g. `NESTED`,
  `PASSING`) — `Apply` is only consumed in join-parsing position
  after `CROSS` or `OUTER`, never as a standalone identifier.
  Identifier parsing elsewhere must not steal the token. Verify by
  running the full test suite post-change.
- **`CROSS` → `APPLY` vs `CROSS` → `JOIN` lookahead.** One-token
  peek suffices; no grammar ambiguity because both paths start with
  `CROSS`.
- **`OUTER APPLY` uses `Token::Outer` which is currently only used
  after LEFT/RIGHT/FULL.** After this change, `Outer` at top-level of
  `parse_join_clauses` becomes a new entry point. Guard by requiring
  `Token::Apply` immediately after; otherwise parse error (keeps
  `LEFT OUTER JOIN` path untouched because `Outer` is consumed inside
  the `Left`/`Right`/`Full` arms, never reaches the top-level match).
- **Performance.** First-FROM materialization is O(|json_rows|) as
  before; join loop complexity unchanged. No regression expected.
- **Scope creep toward LATERAL.** The temptation to wire correlated
  `doc` here is real but explicit out of scope — 11.20d3. Keep the
  guardrail error in place.
