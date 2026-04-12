//! Integration tests for `CREATE TABLE ... IMMUTABLE` (Phase 13.9).
//!
//! IMMUTABLE tables accept INSERTs but reject every UPDATE and DELETE at the
//! executor layer. Errors surface as `DbError::ImmutableTable` with SQLSTATE
//! 42000.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE journal (id INT, note TEXT) IMMUTABLE",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO journal VALUES (1, 'opening balance'), (2, 'first entry')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

#[test]
fn immutable_table_accepts_inserts() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM journal ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn immutable_table_rejects_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let err = run_ctx(
        "UPDATE journal SET note = 'x' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("UPDATE on immutable table must fail");
    assert!(
        matches!(&err, DbError::ImmutableTable { operation, table } if operation == "UPDATE" && table == "journal"),
        "unexpected error: {err:?}",
    );
}

#[test]
fn immutable_table_rejects_delete() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let err = run_ctx(
        "DELETE FROM journal WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("DELETE on immutable table must fail");
    assert!(
        matches!(&err, DbError::ImmutableTable { operation, table } if operation == "DELETE" && table == "journal"),
        "unexpected error: {err:?}",
    );
}

#[test]
fn immutable_table_rejects_multi_table_update_join() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    // Add a helper table to JOIN against.
    run_ctx(
        "CREATE TABLE other (id INT, new_note TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO other VALUES (1, 'updated')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = run_ctx(
        "UPDATE journal JOIN other ON other.id = journal.id \
         SET journal.note = other.new_note",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("UPDATE JOIN on immutable table must fail");
    assert!(
        matches!(&err, DbError::ImmutableTable { .. }),
        "unexpected error: {err:?}",
    );
}

#[test]
fn immutable_table_rejects_multi_table_delete_join() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE other (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO other VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = run_ctx(
        "DELETE journal FROM journal JOIN other ON other.id = journal.id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("DELETE JOIN on immutable table must fail");
    assert!(
        matches!(&err, DbError::ImmutableTable { .. }),
        "unexpected error: {err:?}",
    );
}

#[test]
fn immutable_table_rejects_truncate() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let err = run_ctx(
        "TRUNCATE TABLE journal",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("TRUNCATE on immutable table must fail");
    assert!(
        matches!(&err, DbError::ImmutableTable { operation, .. } if operation == "TRUNCATE"),
        "unexpected error: {err:?}",
    );
}

#[test]
fn non_immutable_table_still_allows_update_and_delete() {
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
        "UPDATE t SET id = 2 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "DELETE FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}
