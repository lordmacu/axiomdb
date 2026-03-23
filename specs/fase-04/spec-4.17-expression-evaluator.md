# Spec: 4.17 + 4.17b — Expression Evaluator + NULL Semantics

## What to build (not how)

A runtime expression evaluator that takes an `Expr` tree and a row context
(`&[Value]`) and returns a `Value`. This is the component the executor calls
for every row to evaluate WHERE predicates, SELECT projections, and any
expression in the query.

Subfases 4.17 and 4.17b are implemented together — a correct evaluator is
inseparable from correct NULL (3-valued logic) semantics.

---

## Expr type

Lives in `axiomdb-sql/src/expr.rs`. This is the expression tree type that
the parser (Phase 4.1–4.4) will produce and the evaluator consumes.

```rust
/// SQL expression tree node.
///
/// Built by the parser (Phase 4.1–4.4) and evaluated by [`eval`].
/// Every node evaluates to a [`Value`]; NULL propagates per SQL semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    // ── Terminals ─────────────────────────────────────────────────────────
    /// A literal SQL value: 42, 'hello', TRUE, NULL, 3.14, etc.
    Literal(Value),

    /// A column reference resolved to a row index by the semantic analyzer
    /// (Phase 4.18). `col_idx` is the position in the `row: &[Value]` slice.
    /// `name` is preserved for error messages.
    Column { col_idx: usize, name: String },

    // ── Unary operators ───────────────────────────────────────────────────
    UnaryOp {
        op: UnaryOp,
        operand: Box<Expr>,
    },

    // ── Binary operators ──────────────────────────────────────────────────
    BinaryOp {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },

    // ── Special SQL forms ─────────────────────────────────────────────────
    /// `expr IS [NOT] NULL`
    IsNull { expr: Box<Expr>, negated: bool },

    /// `expr [NOT] BETWEEN low AND high`
    /// Semantically equivalent to `expr >= low AND expr <= high`,
    /// with full NULL propagation.
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },

    /// `expr [NOT] LIKE pattern`
    /// `%` = any sequence, `_` = exactly one character. Case-sensitive.
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool },

    /// `expr [NOT] IN (v1, v2, ...)`
    /// Short-circuits: TRUE if any match, NULL if no match but NULL in list,
    /// FALSE if no match and no NULLs.
    In { expr: Box<Expr>, list: Vec<Expr>, negated: bool },

    // ── Function call ─────────────────────────────────────────────────────
    /// `func_name(arg1, arg2, ...)`
    /// Implementations registered in Phase 4.19.
    Function { name: String, args: Vec<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,   // arithmetic negation: -expr
    Not,   // boolean NOT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // Arithmetic
    Add, Sub, Mul, Div, Mod,
    // Comparison
    Eq, NotEq, Lt, LtEq, Gt, GtEq,
    // Boolean
    And, Or,
    // String
    Concat,  // expr || expr
}
```

---

## Evaluator API

Lives in `axiomdb-sql/src/eval.rs`.

```rust
/// Evaluates `expr` in the context of `row`.
///
/// `row[i]` is the value of column `i` in the current tuple. Column
/// references in the expression must have been resolved to indices by the
/// semantic analyzer (Phase 4.18) before calling this function.
///
/// ## NULL semantics (3-valued logic)
///
/// SQL uses TRUE / FALSE / UNKNOWN (represented here as `Value::Null`).
/// The evaluator propagates NULL per the rules documented in this spec.
/// Use [`is_truthy`] to convert the result to a Rust `bool` for filtering.
///
/// ## Errors
/// - [`DbError::DivisionByZero`] — integer or decimal division by zero.
/// - [`DbError::TypeMismatch`] — incompatible types in arithmetic or concat.
/// - [`DbError::ColumnIndexOutOfBounds`] — col_idx >= row.len().
pub fn eval(expr: &Expr, row: &[Value]) -> Result<Value, DbError>

/// Returns `true` only for `Value::Bool(true)`.
///
/// Used by the executor to filter rows in WHERE clauses:
/// - `NULL` (UNKNOWN) → `false` (row excluded)
/// - `Value::Bool(false)` → `false` (row excluded)
/// - `Value::Bool(true)` → `true` (row included)
/// - Any other Value → `false` (type error in predicate → exclude)
pub fn is_truthy(value: &Value) -> bool
```

---

## NULL semantics — complete 3-valued logic table

### Arithmetic with NULL
Any arithmetic operation with a NULL operand yields NULL:
```
NULL + 1     = NULL
NULL * 0     = NULL   (even though math says 0)
NULL / 2     = NULL
1 - NULL     = NULL
```

### Comparison with NULL
Any comparison involving NULL yields NULL (UNKNOWN), **including NULL = NULL**:
```
NULL = NULL  = NULL   ← the most common mistake; use IS NULL instead
NULL = 1     = NULL
NULL > 5     = NULL
1 = 1        = TRUE
1 > 2        = FALSE
```

### IS NULL (immune to NULL propagation)
```
NULL IS NULL      = TRUE
1    IS NULL      = FALSE
NULL IS NOT NULL  = FALSE
1    IS NOT NULL  = TRUE
```

### NOT
```
NOT TRUE  = FALSE
NOT FALSE = TRUE
NOT NULL  = NULL
```

### AND (short-circuit, FALSE dominates)
```
TRUE  AND TRUE  = TRUE
TRUE  AND FALSE = FALSE
TRUE  AND NULL  = NULL
FALSE AND TRUE  = FALSE
FALSE AND FALSE = FALSE
FALSE AND NULL  = FALSE   ← FALSE short-circuits; NULL is irrelevant
NULL  AND TRUE  = NULL
NULL  AND FALSE = FALSE   ← FALSE short-circuits
NULL  AND NULL  = NULL
```

### OR (short-circuit, TRUE dominates)
```
TRUE  OR TRUE  = TRUE
TRUE  OR FALSE = TRUE
TRUE  OR NULL  = TRUE    ← TRUE short-circuits; NULL is irrelevant
FALSE OR TRUE  = TRUE
FALSE OR FALSE = FALSE
FALSE OR NULL  = NULL
NULL  OR TRUE  = TRUE    ← TRUE short-circuits
NULL  OR FALSE = NULL
NULL  OR NULL  = NULL
```

### IN list
```
1    IN (1, 2, NULL)  = TRUE    ← match found; NULL irrelevant
3    IN (1, 2, NULL)  = NULL    ← no match; NULL in list → UNKNOWN
3    IN (1, 2)        = FALSE   ← no match; no NULL → FALSE
NULL IN (1, 2)        = NULL    ← expr is NULL → UNKNOWN
NULL IN (1, 2, NULL)  = NULL    ← expr is NULL → UNKNOWN
```
`NOT IN` follows De Morgan: `x NOT IN list = NOT (x IN list)`.

### BETWEEN
`expr BETWEEN low AND high` is exactly `expr >= low AND expr <= high`.
Full NULL propagation applies through both comparisons.
```
NULL BETWEEN 1 AND 10  = NULL
5    BETWEEN 1 AND NULL = NULL
5    BETWEEN 1 AND 10  = TRUE
```

### LIKE
```
NULL LIKE '%'       = NULL
'hi' LIKE NULL      = NULL
'hello' LIKE 'hell_' = TRUE
'hello' LIKE '%ell%' = TRUE
'hello' LIKE 'HELL%' = FALSE  (case-sensitive)
```

---

## Type coercion in arithmetic

Implicit numeric promotions applied before arithmetic ops:

| Left | Right | Result type |
|---|---|---|
| Int | Int | Int |
| Int | BigInt | BigInt |
| BigInt | Int | BigInt |
| Int/BigInt | Real | Real |
| Real | Int/BigInt | Real |
| Int | Decimal | Decimal |
| BigInt | Decimal | Decimal |
| Decimal | Decimal | Decimal (scale = max(s1,s2) for +/-; s1+s2 for *) |

No implicit Text → numeric coercion. For `1 + 'hello'` → `DbError::TypeMismatch`.
No implicit numeric → Text coercion. For `'a' || 1` → `DbError::TypeMismatch`.

Concat (`||`) requires both operands to be Text. Result is Text.

Integer overflow: `i32::MAX + 1` → `DbError::Overflow`. No automatic promotion to BigInt (explicit CAST required). Same for BigInt overflow.

Division: integer division truncates toward zero (same as Rust `/`).
Division by zero: `DbError::DivisionByZero` for any numeric type.

---

## LIKE pattern matching algorithm

Pattern characters:
- `%` — matches any sequence of zero or more characters
- `_` — matches exactly one character
- All other characters — literal match (case-sensitive)

Algorithm: iterative with backtracking O(n·m), handles all patterns including
multiple `%` without exponential blowup. Full Unicode support (operates on
`char` sequences, not bytes).

---

## New DbError variants

```rust
#[error("division by zero")]
DivisionByZero,

#[error("integer overflow in expression")]
Overflow,

#[error("column index {idx} out of bounds (row has {len} columns)")]
ColumnIndexOutOfBounds { idx: usize, len: usize },
```

---

## Inputs / Outputs

| Operation | Input | Output | Errors |
|---|---|---|---|
| `eval` | `&Expr`, `&[Value]` | `Value` | DivisionByZero, Overflow, TypeMismatch, ColumnIndexOutOfBounds |
| `is_truthy` | `&Value` | `bool` | — (infallible) |

---

## Use cases

1. **WHERE with literal**: `eval(BinaryOp{Eq, Column{0}, Literal(Int(42))}, &[Int(3)])` → `Bool(false)`

2. **WHERE with NULL column**: `eval(BinaryOp{Eq, Column{0}, Literal(Int(1))}, &[Null])` → `Null` (UNKNOWN) → `is_truthy` → `false` → row excluded

3. **NULL = NULL**: `eval(BinaryOp{Eq, Literal(Null), Literal(Null)}, &[])` → `Null`

4. **IS NULL on NULL**: `eval(IsNull{Column{0}, false}, &[Null])` → `Bool(true)`

5. **AND short-circuit FALSE**: `eval(BinaryOp{And, Literal(Bool(false)), Literal(Null)}, &[])` → `Bool(false)` (not NULL)

6. **OR short-circuit TRUE**: `eval(BinaryOp{Or, Literal(Bool(true)), Literal(Null)}, &[])` → `Bool(true)`

7. **LIKE match**: `eval(Like{Column{0}, Literal(Text("%ell%")), false}, &[Text("hello")])` → `Bool(true)`

8. **LIKE with NULL**: `eval(Like{Literal(Null), Literal(Text("%")), false}, &[])` → `Null`

9. **IN with match**: `eval(In{Column{0}, [Literal(Int(2)), Literal(Int(3))], false}, &[Int(2)])` → `Bool(true)`

10. **IN without match but NULL in list**: `eval(In{Column{0}, [Literal(Int(5)), Literal(Null)], false}, &[Int(1)])` → `Null`

11. **BETWEEN**: `eval(Between{Column{0}, Literal(Int(1)), Literal(Int(10)), false}, &[Int(5)])` → `Bool(true)`

12. **Type coercion Int+BigInt**: `eval(BinaryOp{Add, Literal(Int(1)), Literal(BigInt(2))}, &[])` → `BigInt(3)`

13. **Division by zero**: `eval(BinaryOp{Div, Literal(Int(1)), Literal(Int(0))}, &[])` → `Err(DivisionByZero)`

14. **String concat**: `eval(BinaryOp{Concat, Literal(Text("foo")), Literal(Text("bar"))}, &[])` → `Text("foobar")`

15. **Arithmetic NULL propagation**: `eval(BinaryOp{Mul, Literal(Null), Literal(Int(0))}, &[])` → `Null`

16. **Nested expression**: `WHERE age > 18 AND name LIKE 'A%'` evaluates correctly for rows with NULL age.

---

## Acceptance criteria

- [ ] `Expr` enum has all variants: Literal, Column, UnaryOp, BinaryOp, IsNull, Between, Like, In, Function
- [ ] `BinaryOp` has all 14 variants (Add, Sub, Mul, Div, Mod, Eq, NotEq, Lt, LtEq, Gt, GtEq, And, Or, Concat)
- [ ] `UnaryOp` has Neg and Not
- [ ] `eval(Literal(v), &[])` returns `v`
- [ ] `eval(Column{0}, &[Int(42)])` returns `Int(42)`
- [ ] `eval(Column{5}, &[Int(1)])` returns `Err(ColumnIndexOutOfBounds)`
- [ ] All arithmetic ops: correct results for Int, BigInt, Real, Decimal
- [ ] Integer overflow returns `Err(Overflow)`
- [ ] Division by zero returns `Err(DivisionByZero)`
- [ ] Type coercion Int→BigInt, Int/BigInt→Real applied in arithmetic
- [ ] `eval(BinaryOp{Add, Null, anything})` returns `Null`
- [ ] `eval(BinaryOp{Eq, Null, Null})` returns `Null` (not TRUE)
- [ ] `eval(IsNull{Null, false})` returns `Bool(true)`
- [ ] `eval(IsNull{Int(1), false})` returns `Bool(false)`
- [ ] `eval(BinaryOp{And, Bool(false), Null})` returns `Bool(false)` (short-circuit)
- [ ] `eval(BinaryOp{And, Bool(true), Null})` returns `Null`
- [ ] `eval(BinaryOp{Or, Bool(true), Null})` returns `Bool(true)` (short-circuit)
- [ ] `eval(BinaryOp{Or, Bool(false), Null})` returns `Null`
- [ ] `eval(BinaryOp{Not, Null})` — via UnaryOp Not — returns `Null`
- [ ] LIKE: `%` matches any sequence including empty
- [ ] LIKE: `_` matches exactly one character
- [ ] LIKE: NULL expr or pattern → Null
- [ ] LIKE: case-sensitive matching
- [ ] IN: match found → `Bool(true)` regardless of NULL in list
- [ ] IN: no match, NULL in list → `Null`
- [ ] IN: no match, no NULL → `Bool(false)`
- [ ] IN: NULL expr → `Null`
- [ ] NOT IN: semantically `NOT (expr IN list)` with correct NULL propagation
- [ ] BETWEEN: equivalent to `>= AND <=` with NULL propagation
- [ ] Concat: Text || Text → Text; non-Text → TypeMismatch
- [ ] `is_truthy(Bool(true))` = true; all other Values = false
- [ ] Full AND/OR truth table (9 combinations each) passes
- [ ] No `unwrap()` in `src/`

---

## ⚠️ DEFERRED

- Function body evaluation — `Function` variant is parsed but dispatched to Phase 4.19 implementations
- `CASE WHEN` — Phase 4.24 (separate Expr variant)
- Subquery expressions — Phase 4.11
- Window function expressions — Phase 13.2
- ILIKE (case-insensitive LIKE) — Phase 5.9 (session charset)
- ESCAPE clause in LIKE — Phase 4.x
- Pattern compilation/caching for repeated LIKE evaluations — Phase 8 (SIMD)

---

## Out of scope

- Parsing SQL into Expr — Phase 4.1–4.4
- Resolving column names to indices — Phase 4.18 (semantic analyzer)
- Type coercion matrix beyond numeric promotion — Phase 4.18b
- CAST expressions — Phase 4.12b

---

## Dependencies

- `axiomdb-types`: `Value`, `DataType` (for coercion)
- `axiomdb-core`: `DbError` (adds `DivisionByZero`, `Overflow`, `ColumnIndexOutOfBounds`)
- `axiomdb-sql/Cargo.toml`: add `axiomdb-types = { workspace = true }`
