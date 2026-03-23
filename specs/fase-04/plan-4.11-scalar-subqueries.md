# Plan: 4.11 — Scalar Subqueries, IN/EXISTS, and Derived Tables

## Files to create/modify

| File | Action | What changes |
|------|--------|--------------|
| `crates/axiomdb-core/src/error.rs` | modify | Add `CardinalityViolation` variant + SQLSTATE 21000 |
| `crates/axiomdb-sql/src/expr.rs` | modify | Add `Subquery`, `InSubquery`, `Exists`, `OuterColumn` variants |
| `crates/axiomdb-sql/src/parser/expr.rs` | modify | Parse `(SELECT ...)`, `IN (SELECT ...)`, `EXISTS (...)`, `NOT EXISTS` |
| `crates/axiomdb-sql/src/parser/dml.rs` | modify | Expose `parse_select_stmt` for use from `expr.rs` |
| `crates/axiomdb-sql/src/analyzer.rs` | modify | Outer scope stack, resolve new Expr variants |
| `crates/axiomdb-sql/src/eval.rs` | modify | Add `SubqueryRunner` trait + `eval_with` |
| `crates/axiomdb-sql/src/executor.rs` | modify | Subquery execution, `substitute_outer`, derived tables |
| `crates/axiomdb-sql/tests/subqueries.rs` | create | Integration tests |

---

## Algorithm / Data structures

### 1. New Expr variants (`expr.rs`)

```rust
/// `(SELECT col FROM ...)` — must return 1 col, 0–1 rows.
/// 0 rows → NULL.  >1 rows → CardinalityViolation.
Subquery(Box<SelectStmt>),

/// `expr [NOT] IN (SELECT col FROM ...)`
InSubquery {
    expr: Box<Expr>,
    query: Box<SelectStmt>,
    negated: bool,
},

/// `[NOT] EXISTS (SELECT ...)`
Exists {
    query: Box<SelectStmt>,
    negated: bool,
},

/// Column reference resolved to the OUTER query's row.
/// Emitted by the analyzer when a name is not found in the inner scope
/// but IS found in an enclosing scope. `col_idx` is the position in
/// the outer row as resolved by the outer `BindContext`.
OuterColumn { col_idx: usize, name: String },
```

---

### 2. SubqueryRunner trait + `eval_with` (`eval.rs`)

```rust
/// Provides a subquery execution backend to the expression evaluator.
///
/// Implementing `SubqueryRunner` for a zero-size type and using
/// monomorphization means the compiler can eliminate all subquery branches
/// in `eval_with::<NoSubquery>` at zero runtime cost.
pub trait SubqueryRunner {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError>;
}

/// Zero-cost runner: used by `eval()` (pure, no subquery support).
/// The optimizer eliminates all subquery arms when this type is used.
pub struct NoSubquery;

impl SubqueryRunner for NoSubquery {
    fn run(&mut self, _: &SelectStmt) -> Result<QueryResult, DbError> {
        Err(DbError::NotImplemented {
            feature: "subqueries require eval_with; use eval_ctx instead".into(),
        })
    }
}

/// Evaluates `expr` against `row` using subquery runner `sq`.
///
/// `eval(expr, row)` remains unchanged — it calls `eval_with(expr, row, &mut NoSubquery)`.
pub fn eval_with<R: SubqueryRunner>(
    expr: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError>
```

`eval` becomes:
```rust
pub fn eval(expr: &Expr, row: &[Value]) -> Result<Value, DbError> {
    eval_with(expr, row, &mut NoSubquery)
}
```

**`eval_with` match arms for subquery variants:**

```rust
Expr::Subquery(stmt) => {
    let result = sq.run(stmt)?;
    match result {
        QueryResult::Rows { rows, .. } => match rows.len() {
            0 => Ok(Value::Null),
            1 => rows.into_iter().next().unwrap()
                    .into_iter().next()
                    .ok_or(DbError::Internal { message: "empty row from scalar subquery".into() }),
            n => Err(DbError::CardinalityViolation { count: n }),
        },
        _ => Err(DbError::Internal { message: "scalar subquery did not return rows".into() }),
    }
}

Expr::InSubquery { expr, query, negated } => {
    let left = eval_with(expr, row, sq)?;
    if left == Value::Null {
        return Ok(Value::Null);
    }
    let result = sq.run(query)?;
    let subquery_rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        _ => vec![],
    };
    let mut found = false;
    let mut has_null = false;
    for row in &subquery_rows {
        let v = row.first().cloned().unwrap_or(Value::Null);
        if v == Value::Null {
            has_null = true;
        } else if v == left {
            found = true;
            break;
        }
    }
    let raw = if found {
        Value::Bool(true)
    } else if has_null {
        Value::Null      // no match + NULL in set → UNKNOWN
    } else {
        Value::Bool(false)
    };
    if *negated {
        Ok(match raw {
            Value::Bool(b) => Value::Bool(!b),
            other => other,   // NULL stays NULL
        })
    } else {
        Ok(raw)
    }
}

Expr::Exists { query, negated } => {
    let result = sq.run(query)?;
    let has_rows = matches!(&result, QueryResult::Rows { rows, .. } if !rows.is_empty());
    let b = if *negated { !has_rows } else { has_rows };
    Ok(Value::Bool(b))
}

Expr::OuterColumn { col_idx, name } => {
    // OuterColumn must be substituted by the executor before eval_with is called.
    // Reaching here means substitute_outer was not applied — programming error.
    Err(DbError::Internal {
        message: format!("OuterColumn '{name}' (col_idx={col_idx}) was not substituted"),
    })
}
```

**All compound expressions recurse through `eval_with`** (not the pure `eval`), so subqueries nested inside AND/OR/CASE etc. are correctly handled.

---

### 3. Outer scope resolution (`analyzer.rs`)

**Extend `resolve_expr` signature:**

```rust
fn resolve_expr(expr: Expr, ctx: &BindContext) -> Result<Expr, DbError>
// becomes:
fn resolve_expr_scoped(
    expr: Expr,
    ctx: &BindContext,
    outer_scopes: &[&BindContext],
) -> Result<Expr, DbError>
```

Existing `resolve_expr(expr, ctx)` becomes
`resolve_expr_scoped(expr, ctx, &[])` — zero-cost for the common (non-subquery) path.

**Modified Column resolution arm:**
```rust
Expr::Column { col_idx: _, name } => {
    // 1. Try the inner (current) scope
    if let Ok(idx) = ctx.resolve_column(&name) {
        return Ok(Expr::Column { col_idx: idx, name });
    }
    // 2. Try outer scopes (depth-1 for Phase 4.11)
    for outer_ctx in outer_scopes {
        if let Ok(idx) = outer_ctx.resolve_column(&name) {
            return Ok(Expr::OuterColumn { col_idx: idx, name });
        }
    }
    // 3. Not found anywhere
    Err(DbError::ColumnNotFound { ... })
}
```

**New resolve_expr arms:**
```rust
Expr::Subquery(inner) => {
    let analyzed = analyze_select_scoped(*inner, storage, snapshot, default_schema, &[ctx])?;
    Ok(Expr::Subquery(Box::new(analyzed)))
}
Expr::InSubquery { expr, query, negated } => {
    let expr = Box::new(resolve_expr_scoped(*expr, ctx, outer_scopes)?);
    let query = Box::new(analyze_select_scoped(*query, storage, snapshot, default_schema, &[ctx])?);
    Ok(Expr::InSubquery { expr, query, negated })
}
Expr::Exists { query, negated } => {
    let query = Box::new(analyze_select_scoped(*query, storage, snapshot, default_schema, &[ctx])?);
    Ok(Expr::Exists { query, negated })
}
Expr::OuterColumn { .. } => Ok(expr),  // pass through (re-analyzed subquery)
```

**`analyze_select_scoped`** — `analyze_select` with an `outer_scopes: &[&BindContext]` parameter,
passed down into `resolve_expr_scoped`.

---

### 4. `substitute_outer` (`executor.rs`)

Pure tree-walk that replaces `OuterColumn { col_idx }` with `Literal(outer_row[col_idx])`:

```rust
fn substitute_outer(stmt: SelectStmt, outer_row: &[Value]) -> SelectStmt {
    // Clone the stmt, then walk every Expr in:
    // - stmt.columns (SelectItem::Expr)
    // - stmt.where_clause
    // - stmt.group_by
    // - stmt.having
    // - stmt.order_by
    // - stmt.joins (conditions)
    // Recursively, including nested Subquery/Exists/InSubquery
    ...
}

fn subst_expr(expr: Expr, outer_row: &[Value]) -> Expr {
    match expr {
        Expr::OuterColumn { col_idx, .. } =>
            Expr::Literal(outer_row.get(col_idx).cloned().unwrap_or(Value::Null)),
        // All compound nodes: recurse
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(subst_expr(*left, outer_row)),
            right: Box::new(subst_expr(*right, outer_row)),
        },
        // ... all other variants with children
        // Leaf nodes (Literal, Column): return as-is
        other => other,
    }
}
```

---

### 5. Executor subquery runner and derived tables (`executor.rs`)

**Subquery runner construction (used in WHERE filter and SELECT projection):**

```rust
// After raw_rows are collected and storage is no longer borrowed by the iterator:
let execute_inner = |stmt: &SelectStmt, outer_row: &[Value]| -> Result<QueryResult, DbError> {
    let bound = substitute_outer(stmt.clone(), outer_row);
    execute_select_ctx(bound, storage, txn, ctx)
};
```

**WHERE filter with subquery support:**
```rust
for (_rid, values) in raw_rows {
    if let Some(ref wc) = stmt.where_clause {
        let outer = values.as_slice();
        let mut runner = ClosureRunner(|s: &SelectStmt| execute_inner(s, outer));
        if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
            continue;
        }
    }
    combined_rows.push(values);
}
```

where `ClosureRunner` is a local wrapper:
```rust
struct ClosureRunner<F>(F);
impl<F: FnMut(&SelectStmt) -> Result<QueryResult, DbError>> SubqueryRunner for ClosureRunner<F> {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> { (self.0)(stmt) }
}
```

**Derived table execution (currently `NotImplemented`):**

In `execute_select` (the `FromClause::Subquery` branch that currently returns `NotImplemented`):

```rust
FromClause::Subquery { query, alias } => {
    let inner_result = execute_select_ctx(*query, storage, txn, ctx)?;
    let (inner_cols, inner_rows) = match inner_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => return Err(DbError::Internal { message: "derived table did not return rows".into() }),
    };
    // Treat inner_rows as the "scanned" rows for this table
    // inner_cols gives us column metadata indexed by alias
    (inner_cols, inner_rows, alias)
}
```

---

### 6. Parser changes (`parser/expr.rs`, `parser/dml.rs`)

**Expose `parse_select_stmt` from `dml.rs`:**
Make it `pub(super)` so `expr.rs` can call `super::dml::parse_select_stmt(p)`.

**`parse_atom` — detect `(SELECT ...)`:**
```rust
Token::LParen => {
    p.advance();
    if matches!(p.peek(), Token::Select) {
        let query = super::dml::parse_select_stmt(p)?;
        p.expect(&Token::RParen)?;
        return Ok(Expr::Subquery(Box::new(query)));
    }
    let expr = parse_expr(p)?;
    p.expect(&Token::RParen)?;
    Ok(expr)
}
```

**`parse_predicate` — IN (SELECT ...) detection:**
```rust
Token::In => {
    p.advance();
    p.expect(&Token::LParen)?;
    if matches!(p.peek(), Token::Select) {
        let query = super::dml::parse_select_stmt(p)?;
        p.expect(&Token::RParen)?;
        return Ok(Expr::InSubquery { expr: Box::new(left), query: Box::new(query), negated });
    }
    // existing value list path
    let mut list = vec![parse_expr(p)?];
    while p.eat(&Token::Comma) { list.push(parse_expr(p)?); }
    p.expect(&Token::RParen)?;
    Ok(Expr::In { expr: Box::new(left), list, negated })
}
```

**`parse_not` — EXISTS and NOT EXISTS:**
```rust
fn parse_not(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Not) {
        if matches!(p.peek(), Token::Exists) {
            p.advance();
            p.expect(&Token::LParen)?;
            let query = super::dml::parse_select_stmt(p)?;
            p.expect(&Token::RParen)?;
            return Ok(Expr::Exists { query: Box::new(query), negated: true });
        }
        let operand = parse_not(p)?;
        return Ok(Expr::UnaryOp { op: UnaryOp::Not, operand: Box::new(operand) });
    }
    if matches!(p.peek(), Token::Exists) {
        p.advance();
        p.expect(&Token::LParen)?;
        let query = super::dml::parse_select_stmt(p)?;
        p.expect(&Token::RParen)?;
        return Ok(Expr::Exists { query: Box::new(query), negated: false });
    }
    parse_is_null(p)
}
```

---

## Implementation phases (in dependency order)

### Phase A — Foundation (no executor changes yet)

1. `error.rs` — add `CardinalityViolation { count: usize }`, SQLSTATE 21000, error message.

2. `expr.rs` — add `Subquery`, `InSubquery`, `Exists`, `OuterColumn` variants.
   - Add convenience constructor `Expr::subquery(stmt: SelectStmt) -> Self`
   - Compile: `cargo check --workspace` (eval and analyzer will fail — expected)

3. `parser/dml.rs` — make `parse_select_stmt` accessible as `pub(super)` (or `pub(crate)`).

4. `parser/expr.rs` — add parsing for all 4 new forms.
   - Unit tests in the parser module for each new form.

### Phase B — Semantic analysis

5. `analyzer.rs` — add `resolve_expr_scoped` + outer scope logic + new Expr arms.
   - Keep `resolve_expr(expr, ctx)` as a thin wrapper calling `resolve_expr_scoped(expr, ctx, &[])`.
   - Add `analyze_select_scoped` with outer scopes parameter.
   - `analyze_select` becomes a wrapper calling `analyze_select_scoped(s, ..., &[])`.
   - Unit tests: uncorrelated subquery resolves without OuterColumn; correlated emits OuterColumn.

### Phase C — Evaluator

6. `eval.rs` — add `SubqueryRunner` trait, `NoSubquery`, `eval_with<R: SubqueryRunner>`.
   - `eval` becomes `eval_with(expr, row, &mut NoSubquery)`.
   - `eval_with` handles ALL Expr variants (not just subquery ones) — it's the new primary path.
   - For non-subquery compound nodes, `eval_with` recurses through itself (not `eval`).
   - Unit tests: scalar subquery, IN subquery NULL semantics, EXISTS TRUE/FALSE.

### Phase D — Executor wiring

7. `executor.rs` — implement subquery execution:

   a. Add `substitute_outer(stmt: SelectStmt, outer_row: &[Value]) -> SelectStmt`.
   b. Add `ClosureRunner<F>` for building a `SubqueryRunner` from a closure.
   c. Change `execute_select` WHERE filter to use `eval_with` + subquery runner.
   d. Change `execute_select` SELECT-list projection to use `eval_with`.
   e. Implement `FromClause::Subquery` (derived table) — currently `NotImplemented`.
   f. Implement `FromClause::Subquery` in JOIN — currently `NotImplemented`.
   g. Remove `NotImplemented` placeholders for subquery-in-FROM and subquery-in-JOIN.

### Phase E — Integration tests

8. `tests/subqueries.rs`:
   - Scalar uncorrelated in WHERE
   - Scalar uncorrelated in SELECT list
   - Scalar returning 0 rows → NULL
   - Scalar returning >1 row → CardinalityViolation
   - IN subquery: match → TRUE
   - IN subquery: no match, no NULL → FALSE
   - IN subquery: no match, NULL in set → NULL (UNKNOWN)
   - NOT IN subquery NULL semantics
   - EXISTS correlated
   - NOT EXISTS correlated
   - Derived table in FROM
   - Derived table with GROUP BY inside
   - Nested subquery (2 levels)
   - Correlated scalar in SELECT list

---

## Tests to write

**Unit** (in `eval.rs`):
- `eval_with` with a mock SubqueryRunner returning controlled results
- NULL propagation for IN subquery (all 4 cases from the spec)
- EXISTS never returns NULL
- Cardinality violation for scalar >1 row

**Unit** (in `analyzer.rs`):
- Uncorrelated subquery: `Expr::Column` in inner → resolved to `Column`, not `OuterColumn`
- Correlated subquery: `Expr::Column` matching outer table → resolved to `OuterColumn`
- Column not in either scope → `ColumnNotFound`

**Integration** (in `tests/subqueries.rs`):
All 14 cases listed in Phase E above, each using real MmapStorage.

---

## Anti-patterns to avoid

- **DO NOT call `eval` inside `eval_with` for compound nodes** — subqueries nested inside
  `AND`/`OR`/`CASE` must be handled by the recursive `eval_with` call, not the pure `eval`.
  Calling `eval` would silently skip subquery evaluation for nested cases.

- **DO NOT clone `SelectStmt` on every row for correlated subqueries** — `substitute_outer`
  takes ownership of a cloned stmt and returns the substituted copy. The clone happens
  in the caller (executor), not inside `substitute_outer`. For uncorrelated subqueries,
  detect them at the analyzer (no `OuterColumn` nodes) and cache the result before the scan.

- **DO NOT rely on `OuterColumn` col_idx being stable across re-analyses** — `OuterColumn`
  indices are set by the outer scope's `BindContext`. After `substitute_outer`, all
  `OuterColumn` nodes become `Literal` — there should be zero `OuterColumn` left in the
  stmt passed to `execute_select_ctx`. Validate this in debug builds with an assertion.

- **DO NOT return `DbError::NotImplemented` for `OuterColumn` in `eval_with`** — it must
  return a clear `Internal` error. Reaching that arm is always a programming error
  (forgotten `substitute_outer`), not a user-facing SQL limitation.

- **DO NOT skip `resolve_expr_scoped` for `Subquery`/`InSubquery`/`Exists` nodes in
  `analyze_select`** — all expression positions (WHERE, SELECT list, HAVING, ORDER BY)
  must call the scoped resolver so inner column references get proper `OuterColumn` marking.

---

## Risks

- **`parse_select_stmt` visibility** — currently `parse_select` might be private or have
  a different signature. Check and adjust visibility before Phase A step 3.
  Mitigation: make it `pub(crate)` from `mod.rs`.

- **`substitute_outer` completeness** — any `Expr` variant added in the future that is
  missed in the `subst_expr` tree-walk will silently leave `OuterColumn` nodes unresolved.
  Mitigation: use a `#[deny(unreachable_patterns)]`-style exhaustive match in `subst_expr`;
  add a post-substitution assertion in debug builds that zero `OuterColumn` nodes remain.

- **Derived table column type inference** — `virtual_columns_from_select` in the analyzer
  currently uses `ColumnType::Text` for all columns (type unknown without full inference).
  This is already the existing behavior for Phase 4. No new regression introduced here.

- **Correlated subquery in SELECT list performance** — a correlated `(SELECT COUNT(*) FROM orders WHERE user_id = u.id)` in the SELECT list of a 1M-row outer query runs 1M inner scans.
  Correct for Phase 4.11; optimization via join decorrelation is Phase 10. Document with
  a warning comment in the executor.
