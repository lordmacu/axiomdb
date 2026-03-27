use super::*;
use axiomdb_types::Value;

use crate::{BinaryOp, Expr};

// ── like_match ────────────────────────────────────────────────────────────────

#[test]
fn test_like_exact_match() {
    assert!(like_match("hello", "hello"));
    assert!(!like_match("hello", "world"));
}

#[test]
fn test_like_percent_any() {
    assert!(like_match("hello", "%"));
    assert!(like_match("", "%"));
    assert!(like_match("hello", "%ello"));
    assert!(like_match("hello", "hell%"));
    assert!(like_match("hello", "%ell%"));
    assert!(like_match("hello", "%"));
    assert!(!like_match("hello", "world%"));
}

#[test]
fn test_like_underscore() {
    // _ matches exactly one char
    assert!(like_match("hello", "h_llo")); // h + _ + llo
    assert!(like_match("hello", "_____")); // 5 underscores = 5 chars ✓
    assert!(like_match("hello", "h___o")); // h + 3 underscores + o = h+e+l+l+o ✓
    assert!(!like_match("hello", "h____o")); // 6 pattern positions vs 5 text chars
    assert!(!like_match("hello", "____")); // 4 underscores vs 5 chars
    assert!(!like_match("hi", "___")); // 3 underscores vs 2 chars
}

#[test]
fn test_like_multiple_percent() {
    assert!(like_match("abcdef", "%b%d%"));
    assert!(like_match("abcdef", "a%f"));
    assert!(!like_match("abcdef", "%z%"));
}

#[test]
fn test_like_empty_string() {
    assert!(like_match("", "%"));
    assert!(like_match("", "%%"));
    assert!(!like_match("", "_"));
    assert!(like_match("", ""));
    assert!(!like_match("a", ""));
}

#[test]
fn test_like_unicode() {
    // '_' must match one Unicode char, not one byte
    assert!(like_match("こんにちは", "_んにちは"));
    assert!(like_match("こんにちは", "%にちは"));
    assert!(like_match("🦀rust", "%rust"));
}

// ── coerce_for_op integration ─────────────────────────────────────────────

#[test]
fn test_eval_int_plus_bigint() {
    // Int(1) + BigInt(999) — coerce_for_op widens Int to BigInt.
    let expr = Expr::BinaryOp {
        op: BinaryOp::Add,
        left: Box::new(Expr::Literal(Value::Int(1))),
        right: Box::new(Expr::Literal(Value::BigInt(999))),
    };
    assert_eq!(eval(&expr, &[]).unwrap(), Value::BigInt(1000));
}

#[test]
fn test_eval_int_eq_real() {
    // Int(5) = Real(5.0) — coerce_for_op widens Int to Real for comparison.
    let expr = Expr::BinaryOp {
        op: BinaryOp::Eq,
        left: Box::new(Expr::Literal(Value::Int(5))),
        right: Box::new(Expr::Literal(Value::Real(5.0))),
    };
    assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
}

#[test]
fn test_eval_int_lt_real() {
    let expr = Expr::BinaryOp {
        op: BinaryOp::Lt,
        left: Box::new(Expr::Literal(Value::Int(3))),
        right: Box::new(Expr::Literal(Value::Real(3.5))),
    };
    assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
}

#[test]
fn test_eval_int_plus_decimal() {
    // Int(2) + Decimal(314, 2) = Decimal(514, 2) = 5.14
    let expr = Expr::BinaryOp {
        op: BinaryOp::Add,
        left: Box::new(Expr::Literal(Value::Int(2))),
        right: Box::new(Expr::Literal(Value::Decimal(314, 2))),
    };
    assert_eq!(eval(&expr, &[]).unwrap(), Value::Decimal(514, 2));
}

#[test]
fn test_eval_incompatible_types_error() {
    let expr = Expr::BinaryOp {
        op: BinaryOp::Add,
        left: Box::new(Expr::Literal(Value::Int(1))),
        right: Box::new(Expr::Literal(Value::Text("a".into()))),
    };
    assert!(eval(&expr, &[]).is_err());
}

// ── is_truthy ────────────────────────────────────────────────────────────────

#[test]
fn test_is_truthy() {
    assert!(is_truthy(&Value::Bool(true)));
    assert!(!is_truthy(&Value::Bool(false)));
    assert!(!is_truthy(&Value::Null));
    assert!(!is_truthy(&Value::Int(1)));
    assert!(!is_truthy(&Value::Text("true".into())));
}
