//! Integration tests for the expression evaluator (subfase 4.17 + 4.17b).
//!
//! Covers: literals, columns, all arithmetic ops, all comparison ops,
//! NULL propagation, full AND/OR truth tables, NOT, IS NULL, BETWEEN,
//! LIKE patterns, IN list semantics, string concat, is_truthy, nested exprs.

use nexusdb_core::DbError;
use nexusdb_sql::{eval, is_truthy, BinaryOp, Expr, UnaryOp};
use nexusdb_types::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn lit(v: Value) -> Expr {
    Expr::Literal(v)
}
fn null() -> Expr {
    Expr::null()
}
fn bool_lit(b: bool) -> Expr {
    Expr::bool(b)
}
fn int(n: i32) -> Expr {
    Expr::int(n)
}
fn bigint(n: i64) -> Expr {
    Expr::Literal(Value::BigInt(n))
}
fn real(f: f64) -> Expr {
    Expr::Literal(Value::Real(f))
}
fn text(s: &str) -> Expr {
    Expr::text(s)
}
fn col(idx: usize) -> Expr {
    Expr::Column {
        col_idx: idx,
        name: format!("col{idx}"),
    }
}
fn binop(op: BinaryOp, l: Expr, r: Expr) -> Expr {
    Expr::binop(op, l, r)
}
fn unop(op: UnaryOp, e: Expr) -> Expr {
    Expr::UnaryOp {
        op,
        operand: Box::new(e),
    }
}
fn is_null(e: Expr) -> Expr {
    Expr::IsNull {
        expr: Box::new(e),
        negated: false,
    }
}
fn is_not_null(e: Expr) -> Expr {
    Expr::IsNull {
        expr: Box::new(e),
        negated: true,
    }
}

fn ok(expr: Expr) -> Value {
    eval(&expr, &[]).expect("eval failed")
}

fn ok_row(expr: Expr, row: &[Value]) -> Value {
    eval(&expr, row).expect("eval failed")
}

fn err(expr: Expr) -> DbError {
    eval(&expr, &[]).expect_err("expected error")
}

// ── Literals and column references ────────────────────────────────────────────

#[test]
fn test_literal_int() {
    assert_eq!(ok(int(42)), Value::Int(42));
}

#[test]
fn test_literal_null() {
    assert_eq!(ok(null()), Value::Null);
}

#[test]
fn test_literal_bool() {
    assert_eq!(ok(bool_lit(true)), Value::Bool(true));
    assert_eq!(ok(bool_lit(false)), Value::Bool(false));
}

#[test]
fn test_column_reference() {
    let row = [Value::Int(7), Value::Text("hello".into()), Value::Null];
    assert_eq!(ok_row(col(0), &row), Value::Int(7));
    assert_eq!(ok_row(col(1), &row), Value::Text("hello".into()));
    assert_eq!(ok_row(col(2), &row), Value::Null);
}

#[test]
fn test_column_out_of_bounds() {
    let row = [Value::Int(1)];
    let e = eval(&col(5), &row).unwrap_err();
    assert!(matches!(
        e,
        DbError::ColumnIndexOutOfBounds { idx: 5, len: 1 }
    ));
}

// ── Arithmetic ────────────────────────────────────────────────────────────────

#[test]
fn test_add_int() {
    assert_eq!(ok(binop(BinaryOp::Add, int(3), int(4))), Value::Int(7));
}

#[test]
fn test_sub_int() {
    assert_eq!(ok(binop(BinaryOp::Sub, int(10), int(3))), Value::Int(7));
}

#[test]
fn test_mul_int() {
    assert_eq!(ok(binop(BinaryOp::Mul, int(3), int(4))), Value::Int(12));
}

#[test]
fn test_div_int_truncates() {
    assert_eq!(ok(binop(BinaryOp::Div, int(7), int(2))), Value::Int(3));
    assert_eq!(ok(binop(BinaryOp::Div, int(-7), int(2))), Value::Int(-3));
}

#[test]
fn test_mod_int() {
    assert_eq!(ok(binop(BinaryOp::Mod, int(10), int(3))), Value::Int(1));
}

#[test]
fn test_add_bigint_coercion() {
    // Int + BigInt → BigInt
    assert_eq!(
        ok(binop(BinaryOp::Add, int(1), bigint(i64::MAX - 1))),
        Value::BigInt(i64::MAX)
    );
}

#[test]
fn test_add_real_coercion() {
    // Int + Real → Real
    assert_eq!(
        ok(binop(BinaryOp::Add, int(1), real(0.5))),
        Value::Real(1.5)
    );
}

#[test]
fn test_negate_int() {
    assert_eq!(ok(unop(UnaryOp::Neg, int(5))), Value::Int(-5));
}

#[test]
fn test_concat_text() {
    assert_eq!(
        ok(binop(BinaryOp::Concat, text("foo"), text("bar"))),
        Value::Text("foobar".into())
    );
}

// ── Arithmetic errors ─────────────────────────────────────────────────────────

#[test]
fn test_div_by_zero_int() {
    let e = err(binop(BinaryOp::Div, int(1), int(0)));
    assert!(matches!(e, DbError::DivisionByZero));
}

#[test]
fn test_mod_by_zero() {
    let e = err(binop(BinaryOp::Mod, int(5), int(0)));
    assert!(matches!(e, DbError::DivisionByZero));
}

#[test]
fn test_int_overflow_add() {
    let e = err(binop(BinaryOp::Add, lit(Value::Int(i32::MAX)), int(1)));
    assert!(matches!(e, DbError::Overflow));
}

#[test]
fn test_int_overflow_negate_min() {
    let e = err(unop(UnaryOp::Neg, lit(Value::Int(i32::MIN))));
    assert!(matches!(e, DbError::Overflow));
}

#[test]
fn test_type_mismatch_arithmetic() {
    // Text + Int: coerce_for_op returns InvalidCoercion (no implicit Text→numeric at op time).
    let e = err(binop(BinaryOp::Add, text("a"), int(1)));
    assert!(matches!(e, DbError::InvalidCoercion { .. }));
}

#[test]
fn test_concat_type_mismatch() {
    let e = err(binop(BinaryOp::Concat, int(1), text("a")));
    assert!(matches!(e, DbError::TypeMismatch { .. }));
}

// ── Comparisons ───────────────────────────────────────────────────────────────

#[test]
fn test_eq_true() {
    assert_eq!(ok(binop(BinaryOp::Eq, int(5), int(5))), Value::Bool(true));
}

#[test]
fn test_eq_false() {
    assert_eq!(ok(binop(BinaryOp::Eq, int(5), int(6))), Value::Bool(false));
}

#[test]
fn test_lt_gt() {
    assert_eq!(ok(binop(BinaryOp::Lt, int(1), int(2))), Value::Bool(true));
    assert_eq!(ok(binop(BinaryOp::Gt, int(2), int(1))), Value::Bool(true));
    assert_eq!(ok(binop(BinaryOp::Lt, int(2), int(1))), Value::Bool(false));
}

#[test]
fn test_lteq_gteq() {
    assert_eq!(ok(binop(BinaryOp::LtEq, int(3), int(3))), Value::Bool(true));
    assert_eq!(ok(binop(BinaryOp::GtEq, int(3), int(3))), Value::Bool(true));
    assert_eq!(
        ok(binop(BinaryOp::LtEq, int(4), int(3))),
        Value::Bool(false)
    );
}

#[test]
fn test_noteq() {
    assert_eq!(
        ok(binop(BinaryOp::NotEq, int(1), int(2))),
        Value::Bool(true)
    );
    assert_eq!(
        ok(binop(BinaryOp::NotEq, int(1), int(1))),
        Value::Bool(false)
    );
}

#[test]
fn test_text_comparison() {
    assert_eq!(
        ok(binop(BinaryOp::Lt, text("apple"), text("banana"))),
        Value::Bool(true)
    );
    assert_eq!(
        ok(binop(BinaryOp::Eq, text("hello"), text("hello"))),
        Value::Bool(true)
    );
}

// ── NULL propagation ──────────────────────────────────────────────────────────

#[test]
fn test_null_plus_int_is_null() {
    assert_eq!(ok(binop(BinaryOp::Add, null(), int(1))), Value::Null);
}

#[test]
fn test_null_mul_zero_is_null() {
    // NULL * 0 = NULL (not 0 — SQL semantics)
    assert_eq!(ok(binop(BinaryOp::Mul, null(), int(0))), Value::Null);
}

#[test]
fn test_null_eq_null_is_null() {
    // The most common mistake: NULL = NULL should be NULL, not TRUE
    assert_eq!(ok(binop(BinaryOp::Eq, null(), null())), Value::Null);
}

#[test]
fn test_null_eq_int_is_null() {
    assert_eq!(ok(binop(BinaryOp::Eq, null(), int(1))), Value::Null);
}

#[test]
fn test_null_lt_int_is_null() {
    assert_eq!(ok(binop(BinaryOp::Lt, null(), int(5))), Value::Null);
}

#[test]
fn test_not_null_is_null() {
    assert_eq!(ok(unop(UnaryOp::Not, null())), Value::Null);
}

// ── AND truth table (all 9 combinations) ─────────────────────────────────────

#[test]
fn test_and_true_true() {
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(true), bool_lit(true))),
        Value::Bool(true)
    );
}
#[test]
fn test_and_true_false() {
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(true), bool_lit(false))),
        Value::Bool(false)
    );
}
#[test]
fn test_and_true_null() {
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(true), null())),
        Value::Null
    );
}
#[test]
fn test_and_false_true() {
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(false), bool_lit(true))),
        Value::Bool(false)
    );
}
#[test]
fn test_and_false_false() {
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(false), bool_lit(false))),
        Value::Bool(false)
    );
}
#[test]
fn test_and_false_null() {
    // FALSE AND NULL = FALSE (short-circuit: FALSE dominates)
    assert_eq!(
        ok(binop(BinaryOp::And, bool_lit(false), null())),
        Value::Bool(false)
    );
}
#[test]
fn test_and_null_true() {
    assert_eq!(
        ok(binop(BinaryOp::And, null(), bool_lit(true))),
        Value::Null
    );
}
#[test]
fn test_and_null_false() {
    // NULL AND FALSE = FALSE (FALSE dominates)
    assert_eq!(
        ok(binop(BinaryOp::And, null(), bool_lit(false))),
        Value::Bool(false)
    );
}
#[test]
fn test_and_null_null() {
    assert_eq!(ok(binop(BinaryOp::And, null(), null())), Value::Null);
}

// ── OR truth table (all 9 combinations) ──────────────────────────────────────

#[test]
fn test_or_true_true() {
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(true), bool_lit(true))),
        Value::Bool(true)
    );
}
#[test]
fn test_or_true_false() {
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(true), bool_lit(false))),
        Value::Bool(true)
    );
}
#[test]
fn test_or_true_null() {
    // TRUE OR NULL = TRUE (short-circuit: TRUE dominates)
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(true), null())),
        Value::Bool(true)
    );
}
#[test]
fn test_or_false_true() {
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(false), bool_lit(true))),
        Value::Bool(true)
    );
}
#[test]
fn test_or_false_false() {
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(false), bool_lit(false))),
        Value::Bool(false)
    );
}
#[test]
fn test_or_false_null() {
    assert_eq!(
        ok(binop(BinaryOp::Or, bool_lit(false), null())),
        Value::Null
    );
}
#[test]
fn test_or_null_true() {
    // NULL OR TRUE = TRUE (TRUE dominates)
    assert_eq!(
        ok(binop(BinaryOp::Or, null(), bool_lit(true))),
        Value::Bool(true)
    );
}
#[test]
fn test_or_null_false() {
    assert_eq!(
        ok(binop(BinaryOp::Or, null(), bool_lit(false))),
        Value::Null
    );
}
#[test]
fn test_or_null_null() {
    assert_eq!(ok(binop(BinaryOp::Or, null(), null())), Value::Null);
}

// ── NOT ───────────────────────────────────────────────────────────────────────

#[test]
fn test_not_true() {
    assert_eq!(ok(unop(UnaryOp::Not, bool_lit(true))), Value::Bool(false));
}
#[test]
fn test_not_false() {
    assert_eq!(ok(unop(UnaryOp::Not, bool_lit(false))), Value::Bool(true));
}
#[test]
fn test_not_null() {
    assert_eq!(ok(unop(UnaryOp::Not, null())), Value::Null);
}

// ── IS NULL ───────────────────────────────────────────────────────────────────

#[test]
fn test_is_null_on_null() {
    assert_eq!(ok(is_null(null())), Value::Bool(true));
}
#[test]
fn test_is_null_on_int() {
    assert_eq!(ok(is_null(int(42))), Value::Bool(false));
}
#[test]
fn test_is_not_null_on_null() {
    assert_eq!(ok(is_not_null(null())), Value::Bool(false));
}
#[test]
fn test_is_not_null_on_value() {
    assert_eq!(ok(is_not_null(int(1))), Value::Bool(true));
}

// ── BETWEEN ───────────────────────────────────────────────────────────────────

fn between(e: Expr, lo: Expr, hi: Expr) -> Expr {
    Expr::Between {
        expr: Box::new(e),
        low: Box::new(lo),
        high: Box::new(hi),
        negated: false,
    }
}

#[test]
fn test_between_in_range() {
    assert_eq!(ok(between(int(5), int(1), int(10))), Value::Bool(true));
}
#[test]
fn test_between_at_boundary() {
    assert_eq!(ok(between(int(1), int(1), int(10))), Value::Bool(true));
    assert_eq!(ok(between(int(10), int(1), int(10))), Value::Bool(true));
}
#[test]
fn test_between_out_of_range() {
    assert_eq!(ok(between(int(0), int(1), int(10))), Value::Bool(false));
    assert_eq!(ok(between(int(11), int(1), int(10))), Value::Bool(false));
}
#[test]
fn test_between_null_expr() {
    assert_eq!(ok(between(null(), int(1), int(10))), Value::Null);
}
#[test]
fn test_between_null_bound() {
    assert_eq!(ok(between(int(5), null(), int(10))), Value::Null);
    assert_eq!(ok(between(int(5), int(1), null())), Value::Null);
}

// ── LIKE ──────────────────────────────────────────────────────────────────────

fn like(e: Expr, p: Expr) -> Expr {
    Expr::Like {
        expr: Box::new(e),
        pattern: Box::new(p),
        negated: false,
    }
}
fn not_like(e: Expr, p: Expr) -> Expr {
    Expr::Like {
        expr: Box::new(e),
        pattern: Box::new(p),
        negated: true,
    }
}

#[test]
fn test_like_prefix() {
    assert_eq!(ok(like(text("hello"), text("hell%"))), Value::Bool(true));
}
#[test]
fn test_like_suffix() {
    assert_eq!(ok(like(text("hello"), text("%ello"))), Value::Bool(true));
}
#[test]
fn test_like_contains() {
    assert_eq!(ok(like(text("hello"), text("%ell%"))), Value::Bool(true));
}
#[test]
fn test_like_no_match() {
    assert_eq!(ok(like(text("hello"), text("world%"))), Value::Bool(false));
}
#[test]
fn test_like_underscore() {
    assert_eq!(ok(like(text("hello"), text("h_llo"))), Value::Bool(true));
    assert_eq!(ok(like(text("hello"), text("_____"))), Value::Bool(true));
    assert_eq!(ok(like(text("hello"), text("h___o"))), Value::Bool(true));
}
#[test]
fn test_like_case_sensitive() {
    assert_eq!(ok(like(text("Hello"), text("hello%"))), Value::Bool(false));
}
#[test]
fn test_like_null_text() {
    assert_eq!(ok(like(null(), text("%"))), Value::Null);
}
#[test]
fn test_like_null_pattern() {
    assert_eq!(ok(like(text("hello"), null())), Value::Null);
}
#[test]
fn test_not_like() {
    assert_eq!(
        ok(not_like(text("hello"), text("world%"))),
        Value::Bool(true)
    );
    assert_eq!(
        ok(not_like(text("hello"), text("hell%"))),
        Value::Bool(false)
    );
}
#[test]
fn test_not_like_null() {
    assert_eq!(ok(not_like(null(), text("%"))), Value::Null);
}

// ── IN ────────────────────────────────────────────────────────────────────────

fn in_list(e: Expr, items: Vec<Expr>) -> Expr {
    Expr::In {
        expr: Box::new(e),
        list: items,
        negated: false,
    }
}
fn not_in(e: Expr, items: Vec<Expr>) -> Expr {
    Expr::In {
        expr: Box::new(e),
        list: items,
        negated: true,
    }
}

#[test]
fn test_in_match_found() {
    let expr = in_list(int(2), vec![int(1), int(2), int(3)]);
    assert_eq!(ok(expr), Value::Bool(true));
}
#[test]
fn test_in_no_match_no_null() {
    let expr = in_list(int(5), vec![int(1), int(2), int(3)]);
    assert_eq!(ok(expr), Value::Bool(false));
}
#[test]
fn test_in_match_with_null_in_list() {
    // Match found → TRUE regardless of NULL in list
    let expr = in_list(int(1), vec![int(1), null(), int(3)]);
    assert_eq!(ok(expr), Value::Bool(true));
}
#[test]
fn test_in_no_match_null_in_list() {
    // No match, but NULL in list → UNKNOWN
    let expr = in_list(int(5), vec![int(1), null(), int(3)]);
    assert_eq!(ok(expr), Value::Null);
}
#[test]
fn test_in_null_expr() {
    // NULL IN (...) = NULL (UNKNOWN)
    let expr = in_list(null(), vec![int(1), int(2)]);
    assert_eq!(ok(expr), Value::Null);
}
#[test]
fn test_not_in_no_match_no_null() {
    // NOT (5 IN (1,2,3)) = NOT FALSE = TRUE
    let expr = not_in(int(5), vec![int(1), int(2), int(3)]);
    assert_eq!(ok(expr), Value::Bool(true));
}
#[test]
fn test_not_in_match() {
    // NOT (1 IN (1,2,3)) = NOT TRUE = FALSE
    let expr = not_in(int(1), vec![int(1), int(2), int(3)]);
    assert_eq!(ok(expr), Value::Bool(false));
}
#[test]
fn test_not_in_null_in_list_no_match() {
    // NOT (5 IN (1, NULL, 3)) = NOT NULL = NULL
    let expr = not_in(int(5), vec![int(1), null(), int(3)]);
    assert_eq!(ok(expr), Value::Null);
}

// ── is_truthy ────────────────────────────────────────────────────────────────

#[test]
fn test_is_truthy_true() {
    assert!(is_truthy(&Value::Bool(true)));
}
#[test]
fn test_is_truthy_false() {
    assert!(!is_truthy(&Value::Bool(false)));
}
#[test]
fn test_is_truthy_null() {
    assert!(!is_truthy(&Value::Null));
}
#[test]
fn test_is_truthy_int_one() {
    // Int(1) is NOT truthy — only Bool(true) is
    assert!(!is_truthy(&Value::Int(1)));
}
#[test]
fn test_is_truthy_text() {
    assert!(!is_truthy(&Value::Text("true".into())));
}

// ── Nested expressions ────────────────────────────────────────────────────────

#[test]
fn test_nested_where_age_and_name() {
    // WHERE age > 18 AND name LIKE 'A%'
    // row: [age=25, name="Alice"]
    let row = [Value::Int(25), Value::Text("Alice".into())];
    let expr = binop(
        BinaryOp::And,
        binop(BinaryOp::Gt, col(0), int(18)),
        Expr::Like {
            expr: Box::new(col(1)),
            pattern: Box::new(text("A%")),
            negated: false,
        },
    );
    assert_eq!(ok_row(expr, &row), Value::Bool(true));
}

#[test]
fn test_nested_null_age_excluded() {
    // WHERE age > 18 AND name LIKE 'A%'
    // row: [age=NULL, name="Alice"] → excluded (NULL AND TRUE = NULL → is_truthy=false)
    let row = [Value::Null, Value::Text("Alice".into())];
    let expr = binop(
        BinaryOp::And,
        binop(BinaryOp::Gt, col(0), int(18)),
        Expr::Like {
            expr: Box::new(col(1)),
            pattern: Box::new(text("A%")),
            negated: false,
        },
    );
    let result = ok_row(expr, &row);
    assert!(!is_truthy(&result), "row with NULL age must be excluded");
}

#[test]
fn test_nested_arithmetic_in_comparison() {
    // WHERE price * 1.1 < 100.0
    // row: [price=80]
    let row = [Value::Int(80)];
    let expr = binop(
        BinaryOp::Lt,
        binop(BinaryOp::Mul, col(0), real(1.1)),
        real(100.0),
    );
    assert_eq!(ok_row(expr, &row), Value::Bool(true));
}

#[test]
fn test_nested_or_with_is_null() {
    // WHERE price IS NULL OR price > 0
    // row: [price=NULL] → TRUE (IS NULL branch)
    let row = [Value::Null];
    let expr = binop(
        BinaryOp::Or,
        is_null(col(0)),
        binop(BinaryOp::Gt, col(0), int(0)),
    );
    assert_eq!(ok_row(expr, &row), Value::Bool(true));
}

#[test]
fn test_between_via_and_equivalence() {
    // BETWEEN 1 AND 10 is equivalent to >= 1 AND <= 10
    let row = [Value::Int(5)];
    let via_between = Expr::Between {
        expr: Box::new(col(0)),
        low: Box::new(int(1)),
        high: Box::new(int(10)),
        negated: false,
    };
    let via_and = binop(
        BinaryOp::And,
        binop(BinaryOp::GtEq, col(0), int(1)),
        binop(BinaryOp::LtEq, col(0), int(10)),
    );
    assert_eq!(ok_row(via_between, &row), ok_row(via_and, &row));
}
