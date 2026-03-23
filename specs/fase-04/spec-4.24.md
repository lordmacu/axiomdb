# Spec: 4.24 — CASE WHEN

## What to build (not how)

`CASE WHEN ... END` is a conditional expression that returns different values
based on conditions. It must work wherever any other expression works: `SELECT`
list, `WHERE`, `ORDER BY`, `GROUP BY`, `HAVING`, `INSERT VALUES`, `UPDATE SET`.

---

## Two forms

### Searched CASE (conditions are boolean expressions)

```sql
CASE
  WHEN salary > 100000 THEN 'senior'
  WHEN salary > 50000  THEN 'mid'
  ELSE 'junior'
END
```

### Simple CASE (compare a value against a list)

```sql
CASE status
  WHEN 'active'   THEN 1
  WHEN 'inactive' THEN 0
  ELSE -1
END
```

The simple form is semantically equivalent to the searched form with equality
conditions: `CASE WHEN status = 'active' THEN 1 ...`. Both are represented
with the same `Expr::Case` AST node — the optional `operand` field distinguishes
them.

---

## AST change: `Expr::Case`

Add to `expr.rs`:

```rust
/// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`
///
/// - Searched CASE: `operand = None`. Each `when_thens.0` is a boolean condition.
/// - Simple CASE: `operand = Some(base)`. Each `when_thens.0` is compared to `base`.
///
/// If no WHEN branch matches and `else_result` is `None`, evaluates to `NULL`.
Case {
    /// `None` for searched CASE; `Some(base_expr)` for simple CASE.
    operand: Option<Box<Expr>>,
    /// `(condition_or_value, result)` pairs, evaluated left-to-right.
    when_thens: Vec<(Expr, Expr)>,
    /// Optional `ELSE result`. Returns `NULL` if absent and no WHEN matches.
    else_result: Option<Box<Expr>>,
},
```

---

## Parser change: `parse_atom`

In `parser/expr.rs`, inside `parse_atom`, add before the fallback arm:

```
Token::Case => {
    p.advance();

    // Distinguish simple vs searched CASE.
    let operand = if !matches!(p.peek(), Token::When) {
        Some(Box::new(parse_expr(p)?))  // simple CASE: consume the base expr
    } else {
        None                             // searched CASE: no base expr
    };

    // Parse WHEN ... THEN ... pairs.
    let mut when_thens: Vec<(Expr, Expr)> = Vec::new();
    while p.eat(&Token::When) {
        let condition = parse_expr(p)?;
        p.expect(&Token::Then)?;
        let result = parse_expr(p)?;
        when_thens.push((condition, result));
    }
    if when_thens.is_empty() {
        return Err(DbError::ParseError {
            message: "CASE requires at least one WHEN branch".into(),
        });
    }

    // Optional ELSE.
    let else_result = if p.eat(&Token::Else) {
        Some(Box::new(parse_expr(p)?))
    } else {
        None
    };

    p.expect(&Token::End)?;

    Ok(Expr::Case { operand, when_thens, else_result })
}
```

---

## Analyzer change: `resolve_expr`

Add a `Expr::Case` arm in `analyzer.rs`:

```rust
Expr::Case { operand, when_thens, else_result } => {
    let operand = operand
        .map(|e| resolve_expr(*e, ctx).map(Box::new))
        .transpose()?;
    let when_thens = when_thens
        .into_iter()
        .map(|(w, t)| Ok((resolve_expr(w, ctx)?, resolve_expr(t, ctx)?)))
        .collect::<Result<Vec<_>, DbError>>()?;
    let else_result = else_result
        .map(|e| resolve_expr(*e, ctx).map(Box::new))
        .transpose()?;
    Ok(Expr::Case { operand, when_thens, else_result })
}
```

---

## Evaluator change: `eval()`

Add a `Expr::Case` arm in `eval.rs`:

### Searched CASE (`operand = None`)

```
for (when_expr, then_expr) in &when_thens:
    condition = eval(when_expr, row)?
    if is_truthy(&condition):
        return eval(then_expr, row)
// No WHEN matched.
if let Some(else_e) = else_result:
    return eval(else_e, row)
return Ok(Value::Null)
```

### Simple CASE (`operand = Some(base_expr)`)

```
base = eval(base_expr, row)?
for (val_expr, then_expr) in &when_thens:
    val = eval(val_expr, row)?
    // Equality: use eval() on a synthetic BinaryOp so NULL semantics are correct.
    // NULL base or NULL val → NULL (UNKNOWN) → is_truthy = false → no match.
    eq = eval(&BinaryOp { Eq, Literal(base.clone()), Literal(val) }, &[])?
    if is_truthy(&eq):
        return eval(then_expr, row)
// No WHEN matched.
if let Some(else_e) = else_result:
    return eval(else_e, row)
return Ok(Value::Null)
```

**Why use `eval()` for equality:** Reuses all existing type coercion and NULL
propagation logic. `Value::Real(NaN)` in a simple CASE would correctly produce
NULL (NaN ≠ NaN per IEEE 754, but NaN is forbidden in stored values anyway).

---

## NULL semantics

| Scenario | Result |
|---|---|
| `CASE WHEN NULL THEN 1 ELSE 0 END` | `0` — NULL is UNKNOWN, not truthy |
| `CASE WHEN TRUE THEN NULL ELSE 1 END` | `NULL` — THEN can produce NULL |
| `CASE NULL WHEN NULL THEN 1 END` | `NULL` — simple CASE, NULL ≠ NULL |
| `CASE 1 WHEN 1 THEN 'a' END` (no ELSE) | `'a'` |
| `CASE 1 WHEN 2 THEN 'a' END` (no match) | `NULL` |

---

## Use cases

### 1. Categorical mapping in SELECT

```sql
SELECT name,
       CASE WHEN salary > 100000 THEN 'senior'
            WHEN salary > 50000  THEN 'mid'
            ELSE 'junior'
       END AS level
FROM employees;
```

### 2. Simple CASE

```sql
SELECT name,
       CASE status
         WHEN 'a' THEN 'Active'
         WHEN 'i' THEN 'Inactive'
         ELSE 'Unknown'
       END
FROM users;
```

### 3. CASE in WHERE

```sql
SELECT * FROM orders
WHERE CASE type WHEN 'priority' THEN 1 ELSE 0 END = 1;
```

### 4. CASE in ORDER BY

```sql
SELECT name, dept FROM employees
ORDER BY CASE dept WHEN 'eng' THEN 1 WHEN 'sales' THEN 2 ELSE 3 END;
```

### 5. CASE in GROUP BY

```sql
SELECT CASE WHEN salary > 80000 THEN 'high' ELSE 'low' END AS tier,
       COUNT(*)
FROM employees
GROUP BY CASE WHEN salary > 80000 THEN 'high' ELSE 'low' END;
```

### 6. Nested CASE

```sql
SELECT CASE
  WHEN a > 0 THEN
    CASE WHEN b > 0 THEN 'both' ELSE 'a_only' END
  ELSE 'none'
END FROM t;
```

---

## Acceptance criteria

- [ ] `Expr::Case` variant added to `expr.rs` with `Clone + Debug + PartialEq`
- [ ] Parser handles `CASE WHEN ... THEN ... END` (searched form)
- [ ] Parser handles `CASE expr WHEN ... THEN ... END` (simple form)
- [ ] Parser handles `ELSE` clause
- [ ] Parser handles `CASE` without `ELSE` → `else_result = None`
- [ ] Error if `CASE` has zero `WHEN` branches
- [ ] Analyzer resolves `col_idx` in all sub-expressions (operand, conditions, results, ELSE)
- [ ] Evaluator: searched CASE returns first truthy branch
- [ ] Evaluator: simple CASE uses equality with NULL semantics
- [ ] No WHEN match + no ELSE → `Value::Null`
- [ ] `CASE WHEN NULL THEN 1 ELSE 0 END` → `0` (NULL is not truthy)
- [ ] `CASE NULL WHEN NULL THEN 1 END` → `NULL` (NULL ≠ NULL in simple form)
- [ ] Nested `CASE` works (CASE inside CASE)
- [ ] CASE in SELECT, WHERE, ORDER BY, GROUP BY works
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- `CASE WHEN ... THEN ... END` in DDL (not a valid SQL position anyway)
- Type inference for the result type of CASE (returns whatever the matching THEN produces; `DataType::Text` fallback in `ColumnMeta` — Phase 6)

---

## Dependencies

Three files modified, no new dependencies:

| File | Change |
|---|---|
| `axiomdb-sql/src/expr.rs` | Add `Expr::Case` variant |
| `axiomdb-sql/src/parser/expr.rs` | Add `Token::Case` arm in `parse_atom` |
| `axiomdb-sql/src/analyzer.rs` | Add `Expr::Case` arm in `resolve_expr` |
| `axiomdb-sql/src/eval.rs` | Add `Expr::Case` arm in `eval` |
