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
fn create_aggregate_persists_catalog_metadata() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE AGGREGATE median(FLOAT) (SFUNC = median_state, STYPE = FLOAT[], FINALFUNC = median_final)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let agg = reader
        .get_aggregate("public", "median", 1)
        .unwrap()
        .unwrap();
    assert_eq!(agg.name, "median");
    assert_eq!(agg.sfunc, "median_state");
    assert_eq!(agg.stype, "FLOAT[]");
    assert_eq!(agg.finalfunc.as_deref(), Some("median_final"));
}

#[test]
fn custom_median_aggregate_runs_end_to_end() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (grp INT, v FLOAT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE AGGREGATE median(FLOAT) (SFUNC = median_state, STYPE = FLOAT[], FINALFUNC = median_final)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 1.0), (1, 9.0), (1, 5.0), (2, 2.0), (2, 4.0)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT grp, median(v) AS m FROM t GROUP BY grp ORDER BY grp",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Real(5.0)],
            vec![Value::Int(2), Value::Real(3.0)],
        ]
    );
}

#[test]
fn custom_aggregate_requires_catalog_definition() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (v FLOAT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1.0), (2.0)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "SELECT median(v) FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::NotImplemented { .. }
                | DbError::InvalidValue { .. }
                | DbError::TypeMismatch { .. }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn drop_aggregate_removes_definition() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE AGGREGATE median(FLOAT) (SFUNC = median_state, STYPE = FLOAT[], FINALFUNC = median_final)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "DROP AGGREGATE median(FLOAT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    assert!(reader
        .get_aggregate("public", "median", 1)
        .unwrap()
        .is_none());
}

#[test]
fn create_aggregate_rejects_unknown_helper_combo() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "CREATE AGGREGATE badmed(FLOAT) (SFUNC = nope_state, STYPE = FLOAT[], FINALFUNC = median_final)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}
