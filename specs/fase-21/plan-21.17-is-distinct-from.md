# Plan: 21.17 — `IS [NOT] DISTINCT FROM`

## Files

### Modify

- `crates/axiomdb-sql/src/parser/expr.rs::parse_is_null` — extend the
  `IS [NOT] ...` dispatch with a `Token::Distinct` arm that expects
  `FROM`, parses a predicate RHS, and desugars:
  - `IS NOT DISTINCT FROM` → `BinaryOp::NullSafe(lhs, rhs)`
  - `IS DISTINCT FROM`     → `UnaryOp::Not(BinaryOp::NullSafe(lhs, rhs))`
  Error message on other tokens updated to mention DISTINCT.

### Create

- `crates/axiomdb-sql/tests/integration_is_distinct_from.rs` —
  7 tests covering the truth table + usage in WHERE / ON / SELECT.

## Algorithm

```rust
Token::Distinct => {
    p.advance();
    p.expect(&Token::From)?;
    let rhs = parse_predicate(p)?;
    let eq = Expr::BinaryOp {
        op: BinaryOp::NullSafe,
        left:  Box::new(expr),
        right: Box::new(rhs),
    };
    if negated {
        // IS NOT DISTINCT FROM → eq
        Ok(eq)
    } else {
        // IS DISTINCT FROM → NOT eq
        Ok(Expr::UnaryOp { op: UnaryOp::Not, operand: Box::new(eq) })
    }
}
```

## Tests

`tests/integration_is_distinct_from.rs`:

1. `distinct_both_non_null_equal_is_false`
2. `distinct_both_non_null_different_is_true`
3. `distinct_one_null_is_true`
4. `distinct_both_null_is_false`
5. `not_distinct_both_null_is_true`
6. `not_distinct_mixed_null_is_false`
7. `is_distinct_from_in_where_clause_filters_rows`

Wire smoke: 1 assertion — NULL-safe join using `IS NOT DISTINCT FROM`.

## Phases

1. Parser change (~25 LoC) — already drafted in expr.rs.
2. 7 integration tests.
3. Wire assertion.
4. Close protocol.

## Anti-patterns

- Don't introduce a new `BinaryOp::Distinct` variant — `NullSafe`
  already covers the semantics. Extra variant = extra evaluator +
  printer + planner branches for zero gain.
- Don't forget precedence: `IS DISTINCT FROM` must bind at the same
  level as `IS NULL` / `IS TRUE`, above comparison operators. The
  current parse_is_null placement already gives this.

## Risks

- Collision with other `IS <keyword>` forms (`IS NULL`, `IS TRUE`,
  `IS FALSE`). The match arm is inside the existing `IS` branch, so
  no collision — `DISTINCT` is a new token-level arm.
- `DISTINCT` keyword is already used as a `SELECT DISTINCT` modifier.
  Context disambiguation is handled by the parser position
  (post-`IS` inside an expression). Standalone `DISTINCT` at atom
  position remains a parse error, unchanged.
