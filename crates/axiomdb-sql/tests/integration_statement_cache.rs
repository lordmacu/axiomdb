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
    statement_cache::{extract_literals, shape_hash, substitute_params},
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

// ── Step 2.2: shape_hash ─────────────────────────────────────────────────

fn prepare_shape(sql: &str) -> (axiomdb_sql::ast::Stmt, Vec<axiomdb_types::Value>) {
    let mut stmt = parse(sql, None).unwrap();
    let extracted = extract_literals(&mut stmt);
    (stmt, extracted)
}

#[test]
fn shape_hash_equal_for_same_shape_different_literals() {
    // Same INSERT, different literal values → same shape → same hash.
    let (s1, _) = prepare_shape("INSERT INTO t VALUES (1, 'a')");
    let (s2, _) = prepare_shape("INSERT INTO t VALUES (99, 'zzz')");
    assert_eq!(
        shape_hash(&s1),
        shape_hash(&s2),
        "different literals must collapse to the same shape hash"
    );
}

#[test]
fn shape_hash_distinct_for_different_table() {
    let (s1, _) = prepare_shape("INSERT INTO t1 VALUES (1)");
    let (s2, _) = prepare_shape("INSERT INTO t2 VALUES (1)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_column_list() {
    let (s1, _) = prepare_shape("INSERT INTO t(a, b) VALUES (1, 2)");
    let (s2, _) = prepare_shape("INSERT INTO t(a, c) VALUES (1, 2)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_values_count() {
    let (s1, _) = prepare_shape("INSERT INTO t VALUES (1, 2)");
    let (s2, _) = prepare_shape("INSERT INTO t VALUES (1, 2, 3)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}

#[test]
fn shape_hash_distinct_for_different_in_list_length() {
    let (s1, _) = prepare_shape("SELECT * FROM t WHERE id IN (1, 2)");
    let (s2, _) = prepare_shape("SELECT * FROM t WHERE id IN (1, 2, 3)");
    assert_ne!(shape_hash(&s1), shape_hash(&s2));
}
