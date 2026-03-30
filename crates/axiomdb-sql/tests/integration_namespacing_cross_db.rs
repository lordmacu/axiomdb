//! Integration tests for cross-database name resolution (Phase 22b.3b).
//!
//! Covers: explicit `database.schema.table` resolution across SELECT, INSERT,
//! UPDATE, DELETE, and CREATE TABLE.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

#[test]
fn test_cross_db_three_part_select() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice'), (2, 'bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id, name FROM axiomdb.public.users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("alice".into()));
}

#[test]
fn test_cross_db_three_part_insert() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE target (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE source (id INT, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO source VALUES (10, 'x'), (20, 'y')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO axiomdb.public.target SELECT * FROM source",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM target",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(2));
}

#[test]
fn test_cross_db_three_part_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE items (id INT, score INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO items VALUES (1, 10), (2, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE other",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE other", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let count = affected_count(
        run_ctx(
            "UPDATE axiomdb.public.items SET score = 99",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(count, 2);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT score FROM items",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert!(rows.iter().all(|r| r[0] == Value::Int(99)));
}

#[test]
fn test_cross_db_three_part_delete() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE logs (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO logs VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE other",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE other", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let count = affected_count(
        run_ctx(
            "DELETE FROM axiomdb.public.logs WHERE id > 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(count, 2);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM logs",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(1));
}

#[test]
fn test_cross_db_database_not_found_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let err = run_ctx(
        "SELECT * FROM ghost.public.events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::DatabaseNotFound { .. }),
        "expected DatabaseNotFound, got {err:?}"
    );
}

#[test]
fn test_cross_db_explicit_overrides_use() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (100), (200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM axiomdb.public.t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(1));

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::BigInt(2));
}

#[test]
fn test_cross_db_unqualified_still_works() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE x (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO x VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM x",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Int(42));

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM public.x",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn test_cross_db_create_table_three_part() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "CREATE TABLE analytics.public.events (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1, 'test')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT name FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows[0][0], Value::Text("test".into()));
}

// ── Phase 22b.4: schema namespacing ──────────────────────────────────────────
