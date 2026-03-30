//! Integration tests for database catalog namespacing (Phase 22b.3a).
//!
//! Covers: CREATE/DROP DATABASE, USE, and catalog-backed SHOW DATABASES.

mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

#[test]
fn test_show_databases_lists_default_and_created_database() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SHOW DATABASES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };

    let names: Vec<String> = rows
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(name) => name.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["analytics".to_string(), "axiomdb".to_string()]);
}

#[test]
fn test_use_database_switches_unqualified_resolution_namespace() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1)",
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
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (10), (20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(2)]]);

    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn test_drop_database_cascades_owned_tables_without_touching_axiomdb() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (1)",
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
        "CREATE TABLE events (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO events VALUES (10), (20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("USE axiomdb", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    run_ctx(
        "DROP DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(!reader.database_exists("analytics").unwrap());
    assert!(reader
        .get_table_in_database("analytics", "public", "events")
        .unwrap()
        .is_none());
    assert!(reader
        .get_table_in_database("axiomdb", "public", "events")
        .unwrap()
        .is_some());

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM events",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(rows, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn test_drop_active_database_is_rejected() {
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
        "USE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "DROP DATABASE analytics",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::ActiveDatabaseDrop { ref name } if name == "analytics"));
}

// ── Phase 22b.3b: cross-database name resolution ─────────────────────────────
