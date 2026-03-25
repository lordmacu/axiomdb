//! Integration tests for subfase 4.25b — Structured Error Responses.
//!
//! Tests cover:
//! 1. ParseError carries a byte position.
//! 2. UniqueViolation carries the offending value.
//! 3. ErrorResponse extracts position and builds a detail for UniqueViolation.

use axiomdb_core::{error::DbError, error_response::ErrorResponse};
use axiomdb_sql::parse;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_err(sql: &str) -> DbError {
    parse(sql, None).expect_err("expected parse error")
}

// ── ParseError position ───────────────────────────────────────────────────────

#[test]
fn parse_error_carries_position() {
    // "SELECT * FORM t" — "FORM" starts at byte offset 9
    let e = parse_err("SELECT * FORM t");
    match e {
        DbError::ParseError { position, .. } => {
            assert!(position.is_some(), "ParseError should carry a position");
            let pos = position.unwrap();
            // The unexpected token "FORM" starts at byte 9
            assert!(pos > 0, "position should be > 0, got {pos}");
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

#[test]
fn parse_error_position_nonzero_for_mid_query_error() {
    // Error deep in the query — position should be well past byte 0
    let e = parse_err("SELECT id, name FROM users WHERE");
    match e {
        DbError::ParseError { position, .. } => {
            let pos = position.unwrap_or(0);
            assert!(
                pos > 20,
                "position for trailing error should be > 20, got {pos}"
            );
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

#[test]
fn parse_error_at_start_of_query() {
    // Completely invalid token at byte 0
    let e = parse_err("@@invalid");
    match e {
        DbError::ParseError { position, .. } => {
            // Position may be 0 or None for a lexer error at the very start
            let _ = position; // just confirm it compiles with the new field
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

// ── UniqueViolation value ─────────────────────────────────────────────────────

#[test]
fn unique_violation_carries_index_name_and_value() {
    let err = DbError::UniqueViolation {
        index_name: "users_email_idx".into(),
        value: Some("alice@example.com".into()),
    };
    match &err {
        DbError::UniqueViolation { index_name, value } => {
            assert_eq!(index_name, "users_email_idx");
            assert_eq!(value.as_deref(), Some("alice@example.com"));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn unique_violation_value_none_is_valid() {
    let err = DbError::UniqueViolation {
        index_name: "pk_users".into(),
        value: None,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("pk_users"),
        "error message should contain index name: {msg}"
    );
}

// ── ErrorResponse integration ─────────────────────────────────────────────────

#[test]
fn error_response_extracts_parse_error_position() {
    let err = DbError::ParseError {
        message: "unexpected token 'FORM'".into(),
        position: Some(9),
    };
    let resp = ErrorResponse::from_error(&err);
    assert_eq!(resp.position, Some(9));
    assert_eq!(resp.sqlstate, "42601");
}

#[test]
fn error_response_parse_error_no_position() {
    let err = DbError::ParseError {
        message: "something".into(),
        position: None,
    };
    let resp = ErrorResponse::from_error(&err);
    assert_eq!(resp.position, None);
}

#[test]
fn error_response_unique_violation_detail_contains_value() {
    let err = DbError::UniqueViolation {
        index_name: "users_email_idx".into(),
        value: Some("alice@example.com".into()),
    };
    let resp = ErrorResponse::from_error(&err);
    assert_eq!(resp.sqlstate, "23505");
    let detail = resp.detail.expect("UniqueViolation should have detail");
    assert!(
        detail.contains("alice@example.com"),
        "detail should contain the offending value: {detail}"
    );
    assert!(
        detail.contains("users_email_idx"),
        "detail should contain index name: {detail}"
    );
}

#[test]
fn error_response_unique_violation_no_value_has_detail() {
    let err = DbError::UniqueViolation {
        index_name: "pk_users".into(),
        value: None,
    };
    let resp = ErrorResponse::from_error(&err);
    assert_eq!(resp.sqlstate, "23505");
    let detail = resp
        .detail
        .expect("UniqueViolation should have detail even without value");
    assert!(
        detail.contains("pk_users"),
        "detail should contain index name: {detail}"
    );
}

#[test]
fn error_response_unique_violation_hint_present() {
    let err = DbError::UniqueViolation {
        index_name: "users_email_idx".into(),
        value: Some("bob@example.com".into()),
    };
    let resp = ErrorResponse::from_error(&err);
    assert!(resp.hint.is_some(), "UniqueViolation should have a hint");
}
