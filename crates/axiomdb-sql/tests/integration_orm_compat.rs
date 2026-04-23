//! Phase 21.24 — ORM compatibility tier 2.
//!
//! These tests intentionally model a small "connect + introspect + migrate"
//! baseline for Prisma / ActiveRecord style clients while documenting the
//! remaining `GENERATED ... AS IDENTITY` gap and validating DEFERRABLE FKs.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run_ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    match run_ok(sql, storage, txn, bloom, ctx) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn orm_connect_and_metadata_probe_baseline() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ok(
        "SET foreign_key_checks = 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "SET unique_checks = 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "SET sql_notes = 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    run_ok(
        "CREATE TABLE orm_users (id INT SERIAL, email TEXT NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let inserted = rows(
        "INSERT INTO orm_users (email) VALUES ('alice@example.com') RETURNING id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(inserted, vec![vec![Value::Int(1)]]);

    let full_fields = rows(
        "SHOW FULL FIELDS FROM orm_users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(full_fields.len(), 2);
    assert_eq!(full_fields[0][0], Value::Text("id".into()));
    assert_eq!(full_fields[1][0], Value::Text("email".into()));
    assert_eq!(full_fields[1][6], Value::Text("utf8mb4_general_ci".into()));

    let full_tables = rows(
        "SHOW FULL TABLES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        full_tables.iter().any(|row| row
            == &vec![
                Value::Text("orm_users".into()),
                Value::Text("BASE TABLE".into())
            ]),
        "SHOW FULL TABLES must expose orm_users as BASE TABLE: {full_tables:?}"
    );

    let status = rows(
        "SHOW TABLE STATUS LIKE 'orm_users'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(status.len(), 1);
    assert_eq!(status[0][0], Value::Text("orm_users".into()));
    assert_eq!(status[0][1], Value::Text("InnoDB".into()));

    let create_rows = rows(
        "SHOW CREATE TABLE orm_users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    match &create_rows[0][1] {
        Value::Text(ddl) => {
            assert!(ddl.contains("CREATE TABLE"), "unexpected DDL: {ddl}");
            assert!(ddl.contains("AUTO_INCREMENT"), "unexpected DDL: {ddl}");
            assert!(
                ddl.contains("`email` TEXT NOT NULL"),
                "unexpected DDL: {ddl}"
            );
        }
        other => panic!("expected text DDL, got {other:?}"),
    }
}

#[test]
fn show_full_fields_is_alias_of_show_full_columns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ok(
        "CREATE TABLE orm_fields_alias (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let cols = rows(
        "SHOW FULL COLUMNS FROM orm_fields_alias",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let fields = rows(
        "SHOW FULL FIELDS FROM orm_fields_alias",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(fields, cols);
}

#[test]
fn orm_tier2_documents_identity_gap_and_deferrable_fk_baseline() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    let identity_err = run_ctx(
        "CREATE TABLE identity_probe (id INT GENERATED ALWAYS AS IDENTITY, email TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    match identity_err {
        DbError::ParseError { .. } | DbError::NotImplemented { .. } => {}
        other => panic!("unexpected identity error: {other:?}"),
    }

    run_ok(
        "CREATE TABLE parent_probe (id INT PRIMARY KEY)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    run_ok(
        "CREATE TABLE child_probe (\
            id INT, \
            parent_id INT, \
            CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent_probe(id) DEFERRABLE INITIALLY DEFERRED\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
}
