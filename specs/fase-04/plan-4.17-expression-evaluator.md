# Plan: 4.17 + 4.17b — Expression Evaluator + NULL Semantics

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-sql/src/expr.rs` | CREATE | `Expr`, `BinaryOp`, `UnaryOp` |
| `crates/axiomdb-sql/src/eval.rs` | CREATE | `eval()`, `is_truthy()`, `like_match()`, coercion helpers |
| `crates/axiomdb-sql/src/lib.rs` | MODIFY | expose modules + re-exports |
| `crates/axiomdb-sql/Cargo.toml` | MODIFY | add `axiomdb-types` dependency |
| `crates/axiomdb-core/src/error.rs` | MODIFY | add `DivisionByZero`, `Overflow`, `ColumnIndexOutOfBounds` |
| `crates/axiomdb-sql/tests/integration_eval.rs` | CREATE | integration tests |

---

## Algorithm / Data structure

### eval — main dispatch

```rust
pub fn eval(expr: &Expr, row: &[Value]) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, name } => {
            row.get(*col_idx)
               .cloned()
               .ok_or(DbError::ColumnIndexOutOfBounds { idx: *col_idx, len: row.len() })
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval(operand, row)?;
            eval_unary(*op, v)
        }

        Expr::BinaryOp { op, left, right } => {
            // AND and OR short-circuit BEFORE evaluating both sides
            match op {
                BinaryOp::And => eval_and(left, right, row),
                BinaryOp::Or  => eval_or(left, right, row),
                _ => {
                    let l = eval(left, row)?;
                    let r = eval(right, row)?;
                    eval_binary(*op, l, r)
                }
            }
        }

        Expr::IsNull { expr, negated } => {
            let v = eval(expr, row)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }

        Expr::Between { expr, low, high, negated } => {
            let v = eval(expr, row)?;
            let lo = eval(low, row)?;
            let hi = eval(high, row)?;
            // Between ≡ v >= lo AND v <= hi
            let ge = eval_binary(BinaryOp::GtEq, v.clone(), lo)?;
            let le = eval_binary(BinaryOp::LtEq, v, hi)?;
            let result = apply_and(ge, le);
            if *negated { Ok(apply_not(result)) } else { Ok(result) }
        }

        Expr::Like { expr, pattern, negated } => {
            let v = eval(expr, row)?;
            let p = eval(pattern, row)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = like_match(&text, &pat);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                })
            }
        }

        Expr::In { expr, list, negated } => {
            let v = eval(expr, row)?;
            let result = eval_in(v, list, row)?;
            if *negated { Ok(apply_not(result)) } else { Ok(result) }
        }

        Expr::Function { name, args: _ } => {
            Err(DbError::NotImplemented {
                feature: format!("function '{name}' (Phase 4.19)"),
            })
        }
    }
}
```

### eval_and — short-circuit AND

```
eval_and(left, right, row):
    l = eval(left, row)?
    match l:
        Bool(false) → return Ok(Bool(false))   // short-circuit: FALSE wins
        Bool(true)  → return eval(right, row)   // result is right
        Null        →                           // need to evaluate right
            r = eval(right, row)?
            match r:
                Bool(false) → Ok(Bool(false))   // FALSE wins over NULL
                _           → Ok(Null)           // TRUE or NULL → UNKNOWN
```

### eval_or — short-circuit OR

```
eval_or(left, right, row):
    l = eval(left, row)?
    match l:
        Bool(true)  → return Ok(Bool(true))     // short-circuit: TRUE wins
        Bool(false) → return eval(right, row)    // result is right
        Null        →
            r = eval(right, row)?
            match r:
                Bool(true) → Ok(Bool(true))      // TRUE wins over NULL
                _          → Ok(Null)             // FALSE or NULL → UNKNOWN
```

### eval_unary

```
eval_unary(op, v):
    match v:
        Null → Ok(Null)   // NULL propagates through unary ops
        _ →
            match op:
                Neg → match v:
                    Int(n)    → n.checked_neg() → Ok(Int) or Err(Overflow)
                    BigInt(n) → same
                    Real(f)   → Ok(Real(-f))
                    Decimal(m,s) → Ok(Decimal(-m, s))
                    _ → Err(TypeMismatch)
                Not → match v:
                    Bool(b) → Ok(Bool(!b))
                    _ → Err(TypeMismatch)
```

### eval_binary — all ops except AND/OR

```
eval_binary(op, l, r):
    // NULL propagation for all binary ops except IS NULL
    if l == Null or r == Null:
        return Ok(Null)

    match op:
        Add/Sub/Mul/Div/Mod → eval_arithmetic(op, l, r)
        Eq/NotEq/Lt/LtEq/Gt/GtEq → eval_comparison(op, l, r)
        Concat → eval_concat(l, r)
```

### eval_arithmetic — with type coercion

```
eval_arithmetic(op, l, r):
    // Coerce to common type
    (l, r) = coerce_numeric(l, r)?

    match (l, r):
        (Int(a), Int(b)) →
            match op:
                Add → a.checked_add(b) → Ok(Int) or Err(Overflow)
                Sub → a.checked_sub(b) → ...
                Mul → a.checked_mul(b) → ...
                Div → if b==0 { Err(DivisionByZero) }
                      else { Ok(Int(a/b)) }   // truncates toward zero
                Mod → if b==0 { Err(DivisionByZero) }
                      else { Ok(Int(a%b)) }
        (BigInt(a), BigInt(b)) → same pattern
        (Real(a), Real(b)) →
            match op:
                Div → if b==0.0 { Ok(Real(f64::INFINITY)) }  // IEEE 754
                else: standard f64 arithmetic
        (Decimal(m1,s1), Decimal(m2,s2)) →
            Add/Sub: align scales, add/sub mantissas
            Mul: m1*m2, scale = s1+s2
            Div: complex — defer to Phase 4.18b for full precision
```

### coerce_numeric — implicit type promotion

```
coerce_numeric(l, r):
    match (l, r):
        (Int(a), BigInt(b))    → (BigInt(a as i64), BigInt(b))
        (BigInt(a), Int(b))    → (BigInt(a), BigInt(b as i64))
        (Int(a), Real(b))      → (Real(a as f64), Real(b))
        (BigInt(a), Real(b))   → (Real(a as f64), Real(b))
        (Real(a), Int(b))      → (Real(a), Real(b as f64))
        (Real(a), BigInt(b))   → (Real(a), Real(b as f64))
        (Int(a), Decimal(m,s)) → (Decimal(a as i128, 0), Decimal(m,s))
        (BigInt(a), Decimal(m,s)) → (Decimal(a as i128, 0), Decimal(m,s))
        same types → return as-is
        incompatible → Err(TypeMismatch)
```

### eval_comparison

```
eval_comparison(op, l, r):
    // Coerce numeric types for fair comparison
    (l, r) = coerce_for_comparison(l, r)?
    cmp = compare_values(l, r)?  // returns Ordering
    Ok(Bool(match op:
        Eq    → cmp == Equal
        NotEq → cmp != Equal
        Lt    → cmp == Less
        LtEq  → cmp != Greater
        Gt    → cmp == Greater
        GtEq  → cmp != Less
    ))

compare_values(l, r):
    // Same type comparisons:
    (Bool, Bool): false < true
    (Int, Int): numeric
    (BigInt, BigInt): numeric
    (Real, Real): f64 ordering (NaN → TypeMismatch — NaN can't be stored)
    (Text, Text): lexicographic
    (Bytes, Bytes): lexicographic
    (Date, Date): numeric
    (Timestamp, Timestamp): numeric
    (Uuid, Uuid): byte-by-byte
    // Mixed types after coercion: should not happen → TypeMismatch
```

### eval_in

```
eval_in(v, list, row):
    if v == Null: return Ok(Null)

    has_null = false
    for item_expr in list:
        item = eval(item_expr, row)?
        if item == Null:
            has_null = true
            continue
        if values_equal(v, item): return Ok(Bool(true))  // match found

    if has_null: Ok(Null)    // no match, but NULL in list → UNKNOWN
    else: Ok(Bool(false))    // no match, no NULL → definitively FALSE
```

### like_match — iterative O(n·m)

```rust
fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let (n, m) = (text.len(), pat.len());

    let mut ti = 0usize;
    let mut pi = 0usize;
    let mut star_pi = usize::MAX;   // last '%' position in pattern
    let mut star_ti = usize::MAX;   // text position when '%' was encountered

    while ti < n {
        if pi < m && (pat[pi] == '_' || pat[pi] == text[ti]) {
            // Literal or '_' match — advance both
            ti += 1;
            pi += 1;
        } else if pi < m && pat[pi] == '%' {
            // '%' — save backtrack point, advance only pattern
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            // Mismatch but can backtrack to last '%'
            // '%' matches one more text char (star_ti advances)
            star_ti += 1;
            ti = star_ti;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }

    // Consume trailing '%'s in pattern
    while pi < m && pat[pi] == '%' {
        pi += 1;
    }

    pi == m
}
```

This correctly handles: `%`, `_`, multiple `%`, `%` at start/end/middle,
empty string with `%`, and all edge cases.

### apply_and / apply_not helpers

```rust
// Used by BETWEEN (combines two comparison results)
fn apply_and(l: Value, r: Value) -> Value { ... }  // same logic as eval_and but without row

fn apply_not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        Value::Null => Value::Null,
        other => other,  // unreachable in correct use
    }
}
```

---

## Implementation phases

### Phase 1 — DbError variants (axiomdb-core)
1. Add `DivisionByZero`, `Overflow`, `ColumnIndexOutOfBounds` to `error.rs`.

### Phase 2 — Cargo.toml (axiomdb-sql)
1. Add `axiomdb-types = { workspace = true }` to dependencies.

### Phase 3 — expr.rs
1. Define `Expr` with all variants.
2. Define `BinaryOp` (14 variants).
3. Define `UnaryOp` (2 variants).
4. Derive `Debug, Clone, PartialEq` on all.

### Phase 4 — eval.rs (core evaluator)
1. Implement private helpers: `like_match`, `coerce_numeric`, `compare_values`,
   `eval_arithmetic`, `eval_comparison`, `eval_concat`, `eval_unary`,
   `eval_binary`, `eval_and`, `eval_or`, `eval_in`, `apply_and`, `apply_not`.
2. Implement public `eval(expr, row)`.
3. Implement public `is_truthy(value)`.
4. Unit tests inline for `like_match` and `coerce_numeric`.

### Phase 5 — lib.rs
```rust
pub mod eval;
pub mod expr;

pub use eval::{eval, is_truthy};
pub use expr::{BinaryOp, Expr, UnaryOp};
```

### Phase 6 — Integration tests
File: `crates/axiomdb-sql/tests/integration_eval.rs`

Tests grouped by category:
```
Literals and column references (4 tests)
Arithmetic — all ops, all types, coercion (12 tests)
Arithmetic errors — overflow, div/zero, type mismatch (6 tests)
Comparisons — all 6 ops, null propagation (10 tests)
NULL propagation — arithmetic, comparison (6 tests)
AND truth table — all 9 combinations (9 tests)
OR truth table — all 9 combinations (9 tests)
NOT — true, false, null (3 tests)
IS NULL — positive, negative, negated (4 tests)
BETWEEN — match, no match, null propagation (5 tests)
LIKE — patterns with %, _, literal, null, negated (10 tests)
IN — match, no-match, null-in-list, null-expr, negated (8 tests)
String concat — ok, null, type error (3 tests)
is_truthy — all Value variants (7 tests)
Nested expressions — realistic WHERE clauses (5 tests)
```

Total: ~101 tests.

---

## Anti-patterns to avoid

- **DO NOT** evaluate both sides of AND/OR before checking for short-circuit —
  the right side may have side effects (or errors) that should be avoided.
- **DO NOT** use `==` to compare `Value::Null == Value::Null` — always use
  `matches!(v, Value::Null)` or `.is_null()`.
- **DO NOT** use Rust's `PartialOrd` on `Value` directly — implement
  `compare_values` explicitly to handle cross-type and NULL cases.
- **DO NOT** forget `NOT IN` is `NOT (x IN list)` — apply `apply_not` after
  `eval_in`, correctly handling `NOT NULL = NULL`.
- **DO NOT** use `like_match` on bytes — always convert to `Vec<char>` first
  for correct Unicode `_` matching (one `_` = one Unicode char, not one byte).
- **DO NOT** add `unwrap()` anywhere in `src/`.

---

## Risks

| Risk | Mitigation |
|---|---|
| AND/OR short-circuit with NULL is wrong | Explicit 9-case truth table test for each |
| `NULL = NULL → TRUE` (common mistake) | Dedicated acceptance criterion + test |
| `NOT IN` with NULL in list returns wrong value | `eval_in` then `apply_not` pattern; test explicitly |
| `like_match` infinite loop on `%%` | Algorithm advances pattern by 1 on `%`; trailing `%` consumed at end |
| Integer overflow silent (no panic in release) | Use `checked_add/sub/mul` everywhere; test with MAX values |
| Unicode `_` matches byte instead of char | `text.chars().collect::<Vec<char>>()` before `like_match` |
| `f64::NAN` comparison returns wrong bool | `compare_values` rejects NaN with TypeMismatch (NaN never stored) |
