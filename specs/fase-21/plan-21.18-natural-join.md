# Plan: 21.18 — NATURAL JOIN

## Files

### Modify

- `crates/axiomdb-sql/src/lexer.rs`: add `Token::Natural`.
- `crates/axiomdb-sql/src/ast.rs::JoinClause` — add field
  `pub natural: bool` (default false).
- `crates/axiomdb-sql/src/parser/dml.rs::parse_join_clauses`:
  recognize optional `NATURAL` prefix before the join-type keywords.
  Flag `natural=true`, reject CROSS combo, emit
  `JoinCondition::Using(vec![])` placeholder (no ON/USING after).
- `crates/axiomdb-sql/src/analyzer_stmt.rs` (and
  `analyzer_ddl.rs` for UPDATE/DELETE) — when `join.natural`, compute
  the shared-column list from left-side BindContext + right-side
  `BoundTable`, replace the placeholder with the real `Using(cols)`.
  Error if intersection is empty.
- `crates/axiomdb-sql/src/executor/joins.rs::apply_join` / `eval_join_cond`
  — unchanged; the resolved `Using(cols)` drives existing path.
- `crates/axiomdb-sql/src/ast.rs` — update `JoinClause` printer /
  Debug if any.

### Create

- `crates/axiomdb-sql/tests/integration_natural_join.rs` — 7 tests.

## Algorithm

### Parser

```rust
// Inside parse_join_clauses loop:
let natural = p.eat(&Token::Natural);
// ... then the existing INNER/LEFT/RIGHT/FULL/JOIN dispatch.
// If natural and join_type == JoinType::Cross → error.
// After parse_from_item(p), condition must be absent:
if natural {
    if matches!(p.peek(), Token::On | Token::Using) {
        return Err(parse_err("NATURAL JOIN does not accept ON / USING"));
    }
    condition = JoinCondition::Using(vec![]);  // placeholder
} else {
    // existing ON / USING parsing
}
joins.push(JoinClause { natural, join_type, table, condition });
```

### Analyzer

```rust
// After building BindContext.tables for all joins:
for (i, join) in s.joins.iter_mut().enumerate() {
    if !join.natural { continue; }
    // Left side = everything accumulated before this join.
    let left_cols: Vec<String> = ctx.tables[..=i]   // first + earlier joins
        .iter()
        .flat_map(|bt| bt.columns.iter().map(|c| c.name.clone()))
        .collect();
    // Right side = this join's BoundTable.
    let right_cols: Vec<String> = ctx.tables[i + 1].columns
        .iter().map(|c| c.name.clone()).collect();
    let shared: Vec<String> = left_cols.iter()
        .filter(|c| right_cols.iter().any(|r| r.eq_ignore_ascii_case(c)))
        .cloned()
        .collect();
    if shared.is_empty() {
        return Err(DbError::ParseError { message:
            format!("NATURAL JOIN: no shared columns between {} and {}",
                    ctx.tables[i].name, ctx.tables[i + 1].name), ... });
    }
    // Dedup (case-insensitive) and replace placeholder.
    join.condition = JoinCondition::Using(shared);
    join.natural = false;  // consumed — no more processing needed
}
```

Placement: right before the existing join-condition resolution loop
in `analyze_select_with_outer` (and the DDL variants).

### Printer / round-trip

`NATURAL` flag surface gets dropped after analysis. Not a concern
— surface-form round-trip isn't preserved for this subphase (same
as 11.20d2 CROSS APPLY).

## Tests

`tests/integration_natural_join.rs`:

1. `natural_join_one_shared_column` — shared `id` column, inner.
2. `natural_join_multiple_shared_columns` — shared `(a, b)`.
3. `natural_left_join_preserves_left_rows` — LEFT modifier.
4. `natural_right_join_preserves_right_rows` — RIGHT modifier.
5. `natural_full_outer_join_both_sides_preserved` — FULL modifier.
6. `natural_join_no_shared_columns_errors` — clean error.
7. `natural_join_case_insensitive_match` — `ID` vs `id` match.
8. `natural_join_rejects_on_clause` — `ON` after `NATURAL JOIN` is
   parse error.

Wire smoke: 1 assertion — `NATURAL JOIN` over 2 seeded tables.

## Phases

1. Lexer: `Token::Natural`.
2. AST: `JoinClause.natural: bool`.
3. Parser: `NATURAL` prefix + rejection guards.
4. Analyzer: shared-column resolution in both `analyze_select_with_outer`
   and `analyze_update` / `analyze_delete`.
5. Tests.
6. Close protocol.

## Anti-patterns

- Don't introduce `JoinCondition::Natural` — the placeholder
  `Using(vec![])` + natural flag is enough; analyzer rewrites in
  place.
- Don't build the shared-column list at parser time — parser has no
  catalog visibility.
- Don't forget DML analyzer paths. 11.20d4 required JSON_TABLE in
  UPDATE/DELETE join-side analysis; natural-column resolution is the
  same pattern.

## Risks

- Case-insensitive match vs AxiomDB identifier rules. Standard: SQL
  unquoted identifiers are case-insensitive. `eq_ignore_ascii_case`
  handles ASCII-only. Non-ASCII identifier parity deferred (AxiomDB
  generally uses ASCII comparisons today).
- Projection dedup for `SELECT *`. Existing USING already dedups
  (see `apply_join` / projection builder). Verify the shared-column
  dedup handles the NATURAL case via the same path. If not,
  executor changes might be needed — expected zero diff.
- Aliased table NATURAL JOIN: `FROM a AS x NATURAL JOIN b AS y`.
  BoundTable alias resolution already works — shared-column set is
  computed from `.columns`, alias-independent.
