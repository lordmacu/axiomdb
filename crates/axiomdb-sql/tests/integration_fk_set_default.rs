//! Integration tests for `ON DELETE SET DEFAULT` and `ON UPDATE SET DEFAULT`.
//!
//! Covers GAP-C.4.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn rows_of(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected rows");
    };
    rows
}

#[test]
fn on_delete_set_default_applies_column_default() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (\
            id INT PRIMARY KEY, \
            parent_id INT DEFAULT 99, \
            FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE SET DEFAULT\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1, 'p1'), (99, 'fallback')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (10, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "DELETE FROM parent WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = rows_of(
        "SELECT id, parent_id FROM child",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Int(99) | Value::BigInt(99)));
}

#[test]
fn on_update_set_default_applies_column_default() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (\
            id INT PRIMARY KEY, \
            parent_id INT DEFAULT 77, \
            FOREIGN KEY (parent_id) REFERENCES parent(id) ON UPDATE SET DEFAULT\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1, 'p1'), (77, 'fallback')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (10, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "UPDATE parent SET id = 2 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = rows_of(
        "SELECT id, parent_id FROM child",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0][1], Value::Int(77) | Value::BigInt(77)),
        "parent_id must be set to column DEFAULT (77), got {:?}",
        rows[0][1],
    );
}

#[test]
fn on_delete_set_default_without_column_default_falls_back_to_null() {
    // When the child column has no DEFAULT, SET DEFAULT degrades to NULL
    // (matches PostgreSQL behavior; MySQL rejects CREATE TABLE if the default
    // value is missing, but AxiomDB favors PG semantics here).
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE parent (id INT PRIMARY KEY)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (\
            id INT PRIMARY KEY, \
            parent_id INT, \
            FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE SET DEFAULT\
         )",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (10, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "DELETE FROM parent WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = rows_of(
        "SELECT id, parent_id FROM child",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Null));
}
