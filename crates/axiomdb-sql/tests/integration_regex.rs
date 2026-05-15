mod common;

use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use common::{rows, run, run_result, setup};

// ── helpers ──────────────────────────────────────────────────────────────────

fn eval(sql: &str) -> Value {
    let (mut storage, mut txn) = setup();
    let result = run(&format!("SELECT {sql}"), &mut storage, &mut txn);
    rows(result)
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

fn eval_err(sql: &str) -> DbError {
    let (mut storage, mut txn) = setup();
    run_result(&format!("SELECT {sql}"), &mut storage, &mut txn)
        .expect_err("expected error but got Ok")
}

// ── Tilde operators: basic ────────────────────────────────────────────────────

#[test]
fn tilde_match_case_sensitive() {
    assert_eq!(eval("'hello' ~ 'h.*'"), Value::Bool(true));
}

#[test]
fn tilde_no_match() {
    assert_eq!(eval("'hello' ~ 'world'"), Value::Bool(false));
}

#[test]
fn tilde_star_match_case_insensitive() {
    assert_eq!(eval("'Hello' ~* 'hello'"), Value::Bool(true));
}

#[test]
fn tilde_star_with_case_sensitive_would_fail() {
    assert_eq!(eval("'Hello' ~ 'hello'"), Value::Bool(false));
}

#[test]
fn bang_tilde_negates_match() {
    assert_eq!(eval("'hello' !~ 'world'"), Value::Bool(true));
}

#[test]
fn bang_tilde_negates_true_to_false() {
    assert_eq!(eval("'hello' !~ 'h.*'"), Value::Bool(false));
}

#[test]
fn bang_tilde_star_case_insensitive_negation() {
    // 'Hello' matches 'HELLO' case-insensitively, so !~* is false
    assert_eq!(eval("'Hello' !~* 'HELLO'"), Value::Bool(false));
}

#[test]
fn bang_tilde_star_no_match_so_negation_is_true() {
    assert_eq!(eval("'hello' !~* 'world'"), Value::Bool(true));
}

// ── NULL propagation ──────────────────────────────────────────────────────────

#[test]
fn null_left_returns_null() {
    assert_eq!(eval("NULL ~ 'x'"), Value::Null);
}

#[test]
fn null_right_returns_null() {
    assert_eq!(eval("'hello' ~ NULL"), Value::Null);
}

#[test]
fn both_null_returns_null() {
    assert_eq!(eval("NULL ~* NULL"), Value::Null);
}

// ── Edge cases ───────────────────────────────────────────────────────────────

#[test]
fn empty_string_matches_empty_anchor() {
    assert_eq!(eval("'' ~ '^$'"), Value::Bool(true));
}

#[test]
fn empty_pattern_matches_any_string() {
    assert_eq!(eval("'foo' ~ ''"), Value::Bool(true));
}

#[test]
fn invalid_pattern_returns_error() {
    let err = eval_err("'x' ~ '['");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("invalid regex pattern")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn unicode_text_matches() {
    assert_eq!(eval("'über' ~ 'ü'"), Value::Bool(true));
}

// ── Unary Tilde (bitwise NOT) regression ─────────────────────────────────────

#[test]
fn unary_tilde_still_works_as_bitnot() {
    // ~0 in MySQL/SQL = bitwise NOT of 0 = 18446744073709551615 (u64::MAX) as i64 = -1
    assert_eq!(eval("~0"), Value::BigInt(-1));
}

#[test]
fn unary_tilde_of_one() {
    assert_eq!(eval("~1"), Value::BigInt(-2));
}

// ── WHERE clause usage ────────────────────────────────────────────────────────

#[test]
fn tilde_in_where_clause() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE tbl_regex (name TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO tbl_regex VALUES ('alice'), ('bob'), ('charlie')",
        &mut storage,
        &mut txn,
    );
    let res = run(
        "SELECT name FROM tbl_regex WHERE name ~ '^a'",
        &mut storage,
        &mut txn,
    );
    let result_rows = rows(res);
    assert_eq!(result_rows.len(), 1);
    assert_eq!(result_rows[0][0], Value::Text("alice".into()));
}

#[test]
fn tilde_star_in_where_clause_filters_case_insensitively() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE tbl_regex2 (name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO tbl_regex2 VALUES ('Alice'), ('BOB'), ('charlie')",
        &mut storage,
        &mut txn,
    );
    let res = run(
        "SELECT name FROM tbl_regex2 WHERE name ~* '^a'",
        &mut storage,
        &mut txn,
    );
    let result_rows = rows(res);
    assert_eq!(result_rows.len(), 1);
    assert_eq!(result_rows[0][0], Value::Text("Alice".into()));
}

// ── REGEXP_LIKE ───────────────────────────────────────────────────────────────

#[test]
fn regexp_like_basic_match() {
    assert_eq!(eval("REGEXP_LIKE('hello', 'h.*')"), Value::Bool(true));
}

#[test]
fn regexp_like_no_match() {
    assert_eq!(
        eval("REGEXP_LIKE('foo123', '^[a-z]+$')"),
        Value::Bool(false)
    );
}

#[test]
fn regexp_like_case_insensitive_flag() {
    assert_eq!(
        eval("REGEXP_LIKE('Hello World', 'hello', 'i')"),
        Value::Bool(true)
    );
}

#[test]
fn regexp_like_null_text_returns_null() {
    assert_eq!(eval("REGEXP_LIKE(NULL, 'x')"), Value::Null);
}

#[test]
fn regexp_like_null_pattern_returns_null() {
    assert_eq!(eval("REGEXP_LIKE('hello', NULL)"), Value::Null);
}

#[test]
fn regexp_like_null_flags_treated_as_no_flags() {
    assert_eq!(
        eval("REGEXP_LIKE('hello', 'HELLO', NULL)"),
        Value::Bool(false)
    );
}

#[test]
fn regexp_like_wrong_arity_one_arg() {
    let err = eval_err("REGEXP_LIKE('a')");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("REGEXP_LIKE requires 2 or 3")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn regexp_like_wrong_arity_four_args() {
    let err = eval_err("REGEXP_LIKE('a', 'b', 'i', 'extra')");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("REGEXP_LIKE requires 2 or 3")),
        "unexpected error: {err:?}"
    );
}

// ── REGEXP_REPLACE ────────────────────────────────────────────────────────────

#[test]
fn regexp_replace_first_occurrence_only() {
    assert_eq!(
        eval("REGEXP_REPLACE('foo bar', 'o+', 'X')"),
        Value::Text("fX bar".into())
    );
}

#[test]
fn regexp_replace_global_flag() {
    assert_eq!(
        eval("REGEXP_REPLACE('aaa', 'a', 'b', 'g')"),
        Value::Text("bbb".into())
    );
}

#[test]
fn regexp_replace_case_insensitive_flag() {
    // [a-z] with 'i' matches both upper and lowercase letters, 'g' replaces all.
    // 'Foo Bar' — F, o, o, B, a, r all match, spaces don't.
    assert_eq!(
        eval("REGEXP_REPLACE('Foo Bar', '[a-z]', '_', 'gi')"),
        Value::Text("___ ___".into())
    );
}

#[test]
fn regexp_replace_backreference() {
    // Use [0-9] instead of \d because MySQL-style string parsing treats \d as 'd'.
    assert_eq!(
        eval("REGEXP_REPLACE('2024-01-15', '([0-9]{4})-([0-9]{2})-([0-9]{2})', '$3/$2/$1')"),
        Value::Text("15/01/2024".into())
    );
}

#[test]
fn regexp_replace_null_text_returns_null() {
    assert_eq!(eval("REGEXP_REPLACE(NULL, 'x', 'y')"), Value::Null);
}

#[test]
fn regexp_replace_null_pattern_returns_null() {
    assert_eq!(eval("REGEXP_REPLACE('hello', NULL, 'y')"), Value::Null);
}

#[test]
fn regexp_replace_null_replacement_returns_null() {
    assert_eq!(eval("REGEXP_REPLACE('hello', 'x', NULL)"), Value::Null);
}

#[test]
fn regexp_replace_null_flags_treated_as_no_flags() {
    assert_eq!(
        eval("REGEXP_REPLACE('hello', 'l', 'L', NULL)"),
        Value::Text("heLlo".into())
    );
}

#[test]
fn regexp_replace_wrong_arity_two_args() {
    let err = eval_err("REGEXP_REPLACE('a', 'b')");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("REGEXP_REPLACE requires 3 or 4")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn regexp_replace_invalid_pattern() {
    let err = eval_err("REGEXP_REPLACE('a', '[', 'x')");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("invalid regex pattern")),
        "unexpected error: {err:?}"
    );
}
