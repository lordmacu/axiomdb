//! Tests for the statement-fingerprinting cache (Attack 2).
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-statement-fingerprinting.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-statement-fingerprinting.md`
//!
//! Step 2.1 (this file at first): unit tests for the AST literal walker
//! `extract_literals` and its round-trip with `substitute_params`.
//! Subsequent steps add shape_hash tests, cache-API tests, and
//! end-to-end tests that drive `Db::run_inner`.

use axiomdb_sql::{
    parse,
    statement_cache::{extract_literals, substitute_params},
};

#[test]
fn extract_then_substitute_roundtrips_simple_insert() {
    let original = parse(
        "INSERT INTO t VALUES (1, 'hello', 3.14, TRUE, NULL)",
        None,
    )
    .unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(
        extracted.len(),
        5,
        "5 literals: 1 INT, 1 TEXT, 1 REAL, 1 BOOL, 1 NULL"
    );
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original, "round-trip must match original AST");
}

#[test]
fn extract_handles_select_where_binary_op() {
    let original = parse(
        "SELECT id FROM t WHERE id = 42 AND name = 'alice'",
        None,
    )
    .unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 2);
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_multi_row_values() {
    let original = parse(
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        None,
    )
    .unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert_eq!(extracted.len(), 6, "3 rows × 2 cols");
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn extract_handles_no_literals() {
    // SELECT * has no literals; extracted should be empty.
    let original = parse("SELECT * FROM t", None).unwrap();
    let mut stmt = original.clone();
    let extracted = extract_literals(&mut stmt);
    assert!(extracted.is_empty());
    let restored = substitute_params(stmt, &extracted).unwrap();
    assert_eq!(restored, original);
}
