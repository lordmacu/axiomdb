# Plan: 21.2 — CTE non-recursive

## Files

### Modify

- `crates/axiomdb-sql/src/ast.rs`:
  - Add `pub with_ctes: Vec<CteBinding>` to `SelectStmt`.
  - Add struct `CteBinding { name, column_names, query }`.
- `crates/axiomdb-sql/src/parser/dml.rs`:
  - `parse_dml`: detect `WITH` at top — parse CTE list, then expect
    `SELECT`, parse SELECT, attach the CTE list.
  - New helper `parse_with_clause(p)` returns
    `Vec<CteBinding>`.
  - Each CTE: `ident [(col1,...)] AS ( SELECT ... )`. After `SELECT`
    keyword inside the paren, reuse `parse_select`.
  - Reject `WITH RECURSIVE` with clear error (mentions 21.3).
- `crates/axiomdb-sql/src/analyzer_stmt.rs`:
  - At the top of `analyze_select_with_outer`, if `s.with_ctes` is
    non-empty:
    1. For each CTE in order, analyze its body with current
       accumulated CTE dictionary in scope (so later CTEs can see
       earlier).
    2. Store analyzed `SelectStmt` bodies keyed by CTE name (case-
       insensitive).
    3. Apply column-name override if specified.
    4. Clear `s.with_ctes` (consumed — no need to carry into
       executor) and substitute.
  - Substitution: walk `s.from` and each `join.table`. If a
    `FromClause::Table(tref)` resolves to a CTE name AND tref has
    no database/schema qualifier, rewrite to
    `FromClause::Subquery { query: cte_body_clone, alias }` where
    `alias = tref.alias.clone().unwrap_or(cte_name)`.
  - Repeat substitution recursively for joins inside nested
    subqueries that reference the CTE (limited scope: only direct
    child SELECT levels, not arbitrary nesting).

### Create

- `crates/axiomdb-sql/tests/integration_cte.rs` — 10 tests.

## Algorithm

### Parser

```rust
pub(crate) fn parse_dml(p) -> Result<Stmt, DbError> {
    match p.peek() {
        Token::With => {
            // Phase 21.2 — WITH clause before SELECT.
            p.advance();
            if p.eat(&Token::Recursive) {
                return Err(parse_err(
                    "WITH RECURSIVE — recursive CTEs are Phase 21.3",
                ));
            }
            let ctes = parse_cte_list(p)?;
            p.expect(&Token::Select)?;
            let mut s = parse_select(p)?;
            s.with_ctes = ctes;
            if matches!(p.peek(), Token::Union | Token::Intersect | Token::Except) {
                return parse_set_op(p, s);
            }
            Ok(Stmt::Select(s))
        }
        Token::Select => { /* existing */ }
        ...
    }
}

fn parse_cte_list(p) -> Result<Vec<CteBinding>, DbError> {
    let mut ctes = Vec::new();
    loop {
        let name = p.parse_identifier()?;
        let column_names = if p.eat(&Token::LParen) {
            let mut cols = vec![p.parse_identifier()?];
            while p.eat(&Token::Comma) { cols.push(p.parse_identifier()?); }
            p.expect(&Token::RParen)?;
            Some(cols)
        } else { None };
        p.expect(&Token::As)?;
        p.expect(&Token::LParen)?;
        p.expect(&Token::Select)?;
        let body = parse_select(p)?;
        p.expect(&Token::RParen)?;
        ctes.push(CteBinding { name, column_names, query: Box::new(body) });
        if !p.eat(&Token::Comma) { break; }
    }
    Ok(ctes)
}
```

### Analyzer (new helper `expand_ctes`)

```rust
fn expand_ctes(
    ctes: Vec<CteBinding>,
    s: &mut SelectStmt,
    storage, snapshot, default_database, default_schema,
) -> Result<(), DbError> {
    // Analyze each CTE body in order.
    let mut dict: HashMap<String /*lowercase*/, Box<SelectStmt>> =
        HashMap::new();
    for cte in ctes {
        let mut body = *cte.query;
        // Prepend already-analyzed CTEs as additional bindings visible
        // inside this body.
        for (name, prev) in &dict {
            body.with_ctes.push(CteBinding {
                name: name.clone(),
                column_names: None,
                query: prev.clone(),
            });
        }
        let analyzed = analyze_select_impl(body, ...)?;
        // Column-name override.
        let analyzed = if let Some(cols) = cte.column_names {
            apply_column_rename(analyzed, &cols)?
        } else {
            analyzed
        };
        dict.insert(cte.name.to_ascii_lowercase(), Box::new(analyzed));
    }
    // Substitute in FROM / joins of `s`.
    substitute_cte_refs(&mut s.from, &dict);
    for join in &mut s.joins {
        substitute_cte_refs(&mut Some(join.table.clone()), &dict);
    }
    Ok(())
}

fn substitute_cte_refs(from: &mut Option<FromClause>, dict: &HashMap<String, Box<SelectStmt>>) {
    if let Some(FromClause::Table(tref)) = from {
        if tref.database.is_none() && tref.schema.is_none() {
            if let Some(body) = dict.get(&tref.name.to_ascii_lowercase()) {
                let alias = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                *from = Some(FromClause::Subquery {
                    query: body.clone(),
                    alias,
                });
            }
        }
    }
    // Recurse into Subquery { query } — CTEs are visible inside
    // directly-nested subqueries as well.
    if let Some(FromClause::Subquery { query, .. }) = from {
        substitute_in_select(query, dict);
    }
}
```

Apply column rename: clone each SelectItem's alias to match the new
column name (positional). Preserves types.

## Tests

1. `basic_cte_single_with_ref_once`
2. `multi_cte_two_bindings`
3. `cte_reads_earlier_cte`
4. `cte_column_name_override`
5. `cte_in_join`
6. `cte_referenced_multiple_times`
7. `cte_without_ref_parses_ok` (body analyzed but unused)
8. `cte_with_recursive_rejected`
9. `select_without_cte_still_works` (regression)
10. `cte_with_group_by_outer`

Wire smoke: 1 assertion — basic CTE.

## Phases

1. AST additions.
2. Parser (WITH clause + CTE list).
3. Analyzer substitution.
4. Integration tests.
5. Close protocol.

## Anti-patterns

- Don't inline CTE bodies at parse time — loses debuggability; do
  substitution at analyzer level where catalog is available.
- Don't materialize CTE results upfront — each reference re-executes
  the body (PG 12+ default). Materialization is a follow-up
  optimization.
- Don't allow CTE self-reference — parser detects WITH RECURSIVE but
  doesn't support the execution; we hard-reject in 21.2 and defer
  the real implementation to 21.3.

## Risks

- Nested CTE reference resolution (CTE inside a subquery referencing
  an outer CTE) — narrow scope to direct FROM/JOIN substitution in
  this subphase; document nested-subquery-CTE-ref as best-effort.
- Column-rename override for CTE body with `SELECT *` — after
  analysis the select list has explicit columns, renaming is positional
  against that list.
- Recursion limit on cloning — bounded by user SQL (WITH nesting is
  finite); no infinite loop risk for non-recursive form.
