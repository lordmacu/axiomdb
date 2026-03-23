# Plan: 4.24 — CASE WHEN

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/expr.rs` | modify | Add `Expr::Case` variant |
| `crates/axiomdb-sql/src/parser/expr.rs` | modify | Add `Token::Case` arm in `parse_atom` |
| `crates/axiomdb-sql/src/analyzer.rs` | modify | Add `Expr::Case` arm in `resolve_expr` |
| `crates/axiomdb-sql/src/eval.rs` | modify | Add `Expr::Case` arm in `eval` |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | Add CASE WHEN tests |

---

## Algorithm

### Step 1 — Add `Expr::Case` to `expr.rs`

Insert after the last variant (`Function`) before the closing `}`:

```rust
/// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`
///
/// ## Searched CASE (`operand = None`)
/// Each `when_thens.0` is a boolean condition evaluated by `is_truthy`.
///
/// ## Simple CASE (`operand = Some(base)`)
/// Each `when_thens.0` is a value compared to `base` using `=` semantics
/// (NULL base or NULL value → UNKNOWN → no match).
///
/// If no WHEN branch matches and `else_result` is `None`, evaluates to `NULL`.
Case {
    operand: Option<Box<Expr>>,
    when_thens: Vec<(Expr, Expr)>,
    else_result: Option<Box<Expr>>,
},
```

### Step 2 — Parser: `parse_atom` in `parser/expr.rs`

Add before the fallback `other => Err(...)` arm:

```rust
Token::Case => {
    p.advance();

    // Simple CASE: operand present if next token is NOT When.
    let operand = if !matches!(p.peek(), Token::When) {
        Some(Box::new(parse_expr(p)?))
    } else {
        None
    };

    // Parse one or more WHEN ... THEN ... pairs.
    let mut when_thens: Vec<(Expr, Expr)> = Vec::new();
    while p.eat(&Token::When) {
        let condition = parse_expr(p)?;
        p.expect(&Token::Then)?;
        let result = parse_expr(p)?;
        when_thens.push((condition, result));
    }
    if when_thens.is_empty() {
        return Err(DbError::ParseError {
            message: format!(
                "CASE requires at least one WHEN branch, found {:?} at position {}",
                p.peek(),
                p.current_pos()
            ),
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

### Step 3 — Analyzer: `resolve_expr` in `analyzer.rs`

Add before the closing `}` of the `match` block (after `Expr::Function`):

```rust
Expr::Case { operand, when_thens, else_result } => {
    // Resolve column refs in the base expression (simple CASE).
    let operand = operand
        .map(|e| resolve_expr(*e, ctx).map(Box::new))
        .transpose()?;

    // Resolve column refs in all WHEN conditions/values and THEN results.
    let when_thens = when_thens
        .into_iter()
        .map(|(w, t)| {
            Ok((resolve_expr(w, ctx)?, resolve_expr(t, ctx)?))
        })
        .collect::<Result<Vec<_>, DbError>>()?;

    // Resolve column refs in ELSE.
    let else_result = else_result
        .map(|e| resolve_expr(*e, ctx).map(Box::new))
        .transpose()?;

    Ok(Expr::Case { operand, when_thens, else_result })
}
```

### Step 4 — Evaluator: `eval()` in `eval.rs`

Add before the closing `}` of the `match` block (after `Expr::Function`):

```rust
Expr::Case { operand, when_thens, else_result } => {
    match operand {
        // ── Searched CASE: conditions are boolean expressions ─────────────
        None => {
            for (when_expr, then_expr) in when_thens {
                let condition = eval(when_expr, row)?;
                if is_truthy(&condition) {
                    return eval(then_expr, row);
                }
            }
        }

        // ── Simple CASE: compare base value against WHEN values ───────────
        Some(base_expr) => {
            let base_val = eval(base_expr, row)?;
            for (val_expr, then_expr) in when_thens {
                let val = eval(val_expr, row)?;
                // Equality via eval() for correct NULL and coercion handling.
                let eq = eval(
                    &Expr::BinaryOp {
                        op: BinaryOp::Eq,
                        left: Box::new(Expr::Literal(base_val.clone())),
                        right: Box::new(Expr::Literal(val)),
                    },
                    &[],
                )?;
                if is_truthy(&eq) {
                    return eval(then_expr, row);
                }
            }
        }
    }

    // No WHEN matched — return ELSE or NULL.
    match else_result {
        Some(else_expr) => eval(else_expr, row),
        None => Ok(Value::Null),
    }
}
```

---

## Implementation order

1. **`expr.rs`** — add `Expr::Case` variant. `cargo check --workspace` must
   compile with exhaustiveness warnings (the new variant is not yet handled).

2. **`eval.rs`** — add the `Expr::Case` arm. `cargo check -p axiomdb-sql`.
   The match is now exhaustive.

3. **`analyzer.rs`** — add the `Expr::Case` arm. `cargo check -p axiomdb-sql`.

4. **`parser/expr.rs`** — add the `Token::Case` arm in `parse_atom`.
   `cargo check -p axiomdb-sql`.

5. **Write integration tests** in `integration_executor.rs`.

6. **Full check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Integration tests in `integration_executor.rs`

```
test_case_when_searched_basic
  — SELECT CASE WHEN id = 1 THEN 'one' WHEN id = 2 THEN 'two' ELSE 'other' END FROM t
  — verify correct branch returned for each row

test_case_when_no_else_returns_null
  — SELECT CASE WHEN id = 99 THEN 'found' END FROM t (no match)
  — returns NULL for each row

test_case_when_null_condition_not_truthy
  — CASE WHEN NULL THEN 1 ELSE 0 END → 0 (NULL is not truthy)

test_case_simple_form
  — CASE status WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 0 END
  — simple CASE equality mapping

test_case_simple_null_value_no_match
  — CASE NULL WHEN NULL THEN 1 END → NULL (NULL ≠ NULL in simple form)

test_case_in_where
  — SELECT * FROM t WHERE CASE type WHEN 'x' THEN 1 ELSE 0 END = 1

test_case_in_order_by
  — SELECT * FROM t ORDER BY CASE dept WHEN 'eng' THEN 1 ELSE 2 END

test_case_in_group_by
  — GROUP BY CASE WHEN salary > 80000 THEN 'high' ELSE 'low' END

test_case_nested
  — CASE WHEN a > 0 THEN CASE WHEN b > 0 THEN 'both' ELSE 'a_only' END ELSE 'none' END

test_case_then_null
  — CASE WHEN TRUE THEN NULL ELSE 1 END → NULL (THEN can return NULL)

test_case_no_when_parse_error
  — "SELECT CASE END" → ParseError
```

---

## Anti-patterns to avoid

- **DO NOT** use `==` for simple CASE equality. Use `eval(BinaryOp{Eq, ...})`
  to get correct NULL and coercion semantics.
- **DO NOT** short-circuit evaluation of WHEN branches that won't be reached
  in a way that causes side effects — always evaluate left-to-right and stop at
  first match.
- **DO NOT** `unwrap()` anywhere in `src/` code.
- **DO NOT** forget to add `Expr::Case` to `contains_aggregate` in `executor.rs`
  — if a CASE expression contains aggregate calls, it must be handled in the
  GROUP BY path. The existing `contains_aggregate` function recursively walks
  `Expr` variants; add the `Case` arm to it.

---

## Risks

| Risk | Mitigation |
|---|---|
| `contains_aggregate` in executor misses Case variant | Add `Expr::Case` arm to `contains_aggregate` and `collect_agg_exprs_from` |
| `eval_with_aggs` in executor misses Case variant | Add `Expr::Case` arm to `eval_with_aggs` |
| `eval_join_cond` / `compare_rows_for_sort` indirectly call `eval()` which handles Case | No change needed — they call `eval()` which will handle the new variant |
| `contains_aggregate` closure in GROUP BY path | Already handled if we add Case to the recursive function |
