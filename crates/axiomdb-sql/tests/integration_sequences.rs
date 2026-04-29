mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn create_sequence_persists_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE order_seq START WITH 10 INCREMENT BY 5 MINVALUE 1 MAXVALUE 100 CACHE 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_sequence("public", "order_seq")
        .unwrap()
        .expect("sequence must be persisted");
    assert_eq!(def.start_value, 10);
    assert_eq!(def.last_value, 10);
    assert_eq!(def.increment, 5);
    assert_eq!(def.min_value, 1);
    assert_eq!(def.max_value, 100);
    assert!(!def.is_called);
}

#[test]
fn create_sequence_if_not_exists_is_idempotent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s START WITH 7",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE SEQUENCE IF NOT EXISTS s START WITH 99",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_sequence("public", "s").unwrap().unwrap();
    assert_eq!(def.start_value, 7);
}

#[test]
fn duplicate_create_sequence_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn drop_sequence_removes_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "DROP SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_sequence("public", "s").unwrap().is_none());
}

#[test]
fn drop_sequence_if_exists_ignores_missing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "DROP SEQUENCE IF EXISTS missing",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn invalid_sequence_options_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "CREATE SEQUENCE bad INCREMENT BY 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn drop_table_name_as_sequence_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = common::run_ctx(
        "DROP SEQUENCE t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn nextval_advances_sequence() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let first = rows(
        common::run_ctx(
            "SELECT NEXTVAL('s')",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    let second = rows(
        common::run_ctx(
            "SELECT NEXTVAL('s')",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(first, vec![vec![Value::BigInt(1)]]);
    assert_eq!(second, vec![vec![Value::BigInt(2)]]);
}

#[test]
fn nextval_advances_per_output_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let out = rows(
        common::run_ctx(
            "SELECT NEXTVAL('s') FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        out,
        vec![
            vec![Value::BigInt(1)],
            vec![Value::BigInt(2)],
            vec![Value::BigInt(3)]
        ]
    );
}

#[test]
fn currval_requires_session_nextval_first() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = common::run_ctx(
        "SELECT CURRVAL('s')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));

    common::run_ctx(
        "SELECT NEXTVAL('s')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let out = rows(
        common::run_ctx(
            "SELECT CURRVAL('s')",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(out, vec![vec![Value::BigInt(1)]]);
}

#[test]
fn rollback_does_not_reuse_nextval_value() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let first = rows(
        common::run_ctx(
            "SELECT NEXTVAL('s')",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    common::run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let second = rows(
        common::run_ctx(
            "SELECT NEXTVAL('s')",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );

    assert_eq!(first, vec![vec![Value::BigInt(1)]]);
    assert_eq!(second, vec![vec![Value::BigInt(2)]]);
}

#[test]
fn nextval_enforces_maxvalue() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE SEQUENCE s START WITH 1 MAXVALUE 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "SELECT NEXTVAL('s')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = common::run_ctx(
        "SELECT NEXTVAL('s')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}
