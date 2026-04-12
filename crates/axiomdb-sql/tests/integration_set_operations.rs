//! Integration tests for set operations: UNION, INTERSECT, EXCEPT (+ ALL variants).
//!
//! Covers GAP-B.1 (UNION) regression + GAP-B.8 (INTERSECT / EXCEPT).

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup_with_data() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE a (x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE b (x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // a: 1, 2, 2, 3
    run_ctx(
        "INSERT INTO a VALUES (1), (2), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // b: 2, 3, 3, 4
    run_ctx(
        "INSERT INTO b VALUES (2), (3), (3), (4)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn run_rows(sql: &str) -> Vec<i64> {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_with_data();
    let QueryResult::Rows { rows, .. } =
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap()
    else {
        panic!("expected rows for: {sql}");
    };
    let mut vals: Vec<i64> = rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i as i64,
            Value::BigInt(i) => *i,
            other => panic!("unexpected value {:?}", other),
        })
        .collect();
    vals.sort();
    vals
}

#[test]
fn union_all_keeps_duplicates() {
    assert_eq!(
        run_rows("SELECT x FROM a UNION ALL SELECT x FROM b"),
        vec![1, 2, 2, 2, 3, 3, 3, 4],
    );
}

#[test]
fn union_dedupes() {
    assert_eq!(
        run_rows("SELECT x FROM a UNION SELECT x FROM b"),
        vec![1, 2, 3, 4],
    );
}

#[test]
fn intersect_distinct() {
    // a distinct={1,2,3}; b distinct={2,3,4}; ∩ = {2,3}
    assert_eq!(
        run_rows("SELECT x FROM a INTERSECT SELECT x FROM b"),
        vec![2, 3],
    );
}

#[test]
fn intersect_all_keeps_min_counts() {
    // a: 1×1, 2×2, 3×1; b: 2×1, 3×2, 4×1; min → 2×1, 3×1
    assert_eq!(
        run_rows("SELECT x FROM a INTERSECT ALL SELECT x FROM b"),
        vec![2, 3],
    );
}

#[test]
fn except_distinct() {
    // a distinct={1,2,3}; b distinct={2,3,4}; a − b = {1}
    assert_eq!(run_rows("SELECT x FROM a EXCEPT SELECT x FROM b"), vec![1],);
}

#[test]
fn except_all_preserves_remaining_counts() {
    // a: 1×1, 2×2, 3×1; b: 2×1, 3×2, 4×1
    // a − b counts → 1×1, 2×1, 3×0, 4×0 ⇒ [1, 2]
    assert_eq!(
        run_rows("SELECT x FROM a EXCEPT ALL SELECT x FROM b"),
        vec![1, 2],
    );
}

#[test]
fn chained_union_and_intersect() {
    // SELECT 1 UNION SELECT 2 INTERSECT SELECT 2
    // Left-associative chain: (1 UNION 2) INTERSECT 2 = {2}
    assert_eq!(
        run_rows("SELECT 1 UNION SELECT 2 INTERSECT SELECT 2"),
        vec![2],
    );
}

#[test]
fn triple_intersect() {
    assert_eq!(
        run_rows("SELECT x FROM a INTERSECT SELECT x FROM b INTERSECT SELECT 3"),
        vec![3],
    );
}

#[test]
fn except_then_union() {
    // (a EXCEPT b) = {1}; UNION SELECT 5 = {1,5}
    assert_eq!(
        run_rows("SELECT x FROM a EXCEPT SELECT x FROM b UNION SELECT 5"),
        vec![1, 5],
    );
}

#[test]
fn column_count_mismatch_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_with_data();
    let err = run_ctx(
        "SELECT x FROM a INTERSECT SELECT x, x FROM b",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("mismatched arity must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("same number of columns"),
        "unexpected error: {msg}",
    );
}
