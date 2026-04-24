mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;
use common::{run_ctx, setup_ctx};

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn explicit_column_collation_overrides_binary_session_comparisons() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET AXIOM_COMPAT = 'standard'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE users (name TEXT COLLATE utf8mb4_unicode_ci)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES ('José')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT COUNT(*) FROM users WHERE name LIKE 'jose'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows(result), vec![vec![Value::BigInt(1)]]);
}

#[test]
fn expr_collate_can_force_binary_comparison_inside_mysql_mode() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET AXIOM_COMPAT = 'mysql'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE users (name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES ('José')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let relaxed = run_ctx(
        "SELECT COUNT(*) FROM users WHERE name LIKE 'jose'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows(relaxed), vec![vec![Value::BigInt(1)]]);

    let forced_binary = run_ctx(
        "SELECT COUNT(*) FROM users WHERE name COLLATE utf8mb4_bin LIKE 'jose'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows(forced_binary), vec![vec![Value::BigInt(0)]]);
}

#[test]
fn database_default_collation_is_inherited_by_unqualified_text_columns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET AXIOM_COMPAT = 'mysql'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE DATABASE appdb COLLATE utf8mb4_bin",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE appdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "CREATE TABLE users (name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES ('José')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT COUNT(*) FROM users WHERE name LIKE 'jose'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows(result), vec![vec![Value::BigInt(0)]]);
}
