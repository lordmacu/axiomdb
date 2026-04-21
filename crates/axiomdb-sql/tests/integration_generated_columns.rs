//! Parser and executor coverage for generated columns.

mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    ast::{ColumnConstraint, GeneratedColumnKind, Stmt},
    parse, QueryResult,
};
use axiomdb_types::Value;

#[test]
fn parses_stored_generated_column() {
    let stmt = parse(
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED)",
        None,
    )
    .unwrap();

    let Stmt::CreateTable(create) = stmt else {
        panic!("expected CREATE TABLE");
    };
    let generated = create.columns[1]
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            ColumnConstraint::Generated { expr, kind } => Some((expr, kind)),
            _ => None,
        })
        .expect("expected generated constraint");
    assert!(matches!(generated.1, GeneratedColumnKind::Stored));
}

#[test]
fn parses_virtual_generated_column_for_explicit_runtime_rejection() {
    let stmt = parse(
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) VIRTUAL)",
        None,
    )
    .unwrap();

    let Stmt::CreateTable(create) = stmt else {
        panic!("expected CREATE TABLE");
    };
    let generated = create.columns[1]
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            ColumnConstraint::Generated { expr, kind } => Some((expr, kind)),
            _ => None,
        })
        .expect("expected generated constraint");
    assert!(matches!(generated.1, GeneratedColumnKind::Virtual));
}

#[test]
fn create_table_persists_stored_generated_metadata() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let table = reader.get_table("public", "t").unwrap().unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    let total = columns.iter().find(|col| col.name == "total").unwrap();
    assert_eq!(total.generated_expr.as_deref(), Some("a + b"));
    assert!(total.generated_stored);
}

#[test]
fn virtual_generated_columns_are_not_implemented_yet() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) VIRTUAL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("VIRTUAL generated columns must be rejected for now");
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "unexpected error: {err}",
    );
    assert!(
        format!("{err}").contains("virtual generated columns"),
        "unexpected error: {err}",
    );
}

#[test]
fn generated_column_rejects_default_and_self_reference() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED DEFAULT 0)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("generated columns cannot declare DEFAULT");
    assert!(
        format!("{err}").contains("DEFAULT"),
        "unexpected error: {err}"
    );

    let err = common::run_ctx(
        "CREATE TABLE t2 (a INT, b INT GENERATED ALWAYS AS (b + 1) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("generated columns cannot reference themselves");
    assert!(format!("{err}").contains("self"), "unexpected error: {err}",);
}

#[test]
fn generated_column_rejects_auto_increment_and_on_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let err = common::run_ctx(
        "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED AUTO_INCREMENT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("generated columns cannot declare AUTO_INCREMENT");
    assert!(
        format!("{err}").contains("AUTO_INCREMENT"),
        "unexpected error: {err}",
    );

    let err = common::run_ctx(
        "CREATE TABLE t2 (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED ON UPDATE CURRENT_TIMESTAMP)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("generated columns cannot declare ON UPDATE");
    assert!(
        format!("{err}").contains("ON UPDATE"),
        "unexpected error: {err}",
    );
}

#[test]
fn alter_table_generated_columns_are_not_implemented_yet() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "ALTER TABLE t ADD COLUMN b INT GENERATED ALWAYS AS (a + 1) STORED",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("ALTER generated columns must be rejected for now");
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "unexpected error: {err}",
    );
    assert!(
        format!("{err}").contains("ALTER TABLE generated columns"),
        "unexpected error: {err}",
    );
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn insert_values_computes_stored_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(5)]]);
}

#[test]
fn insert_default_for_generated_column_computes_value() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (4, 7, DEFAULT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(11)]]);
}

#[test]
fn insert_literal_for_generated_column_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "INSERT INTO t VALUES (4, 7, 999)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("explicit generated value must be rejected");
    assert!(
        format!("{err}").contains("generated column"),
        "unexpected error: {err}",
    );
}

#[test]
fn insert_select_computes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE src (a INT, b INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE t (a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO src VALUES (8, 9)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t (a, b) SELECT a, b FROM src",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(17)]]);
}

#[test]
fn update_base_column_recomputes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "UPDATE t SET a = 10 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(13)]]);
}

#[test]
fn update_primary_key_table_recomputes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t (id, a, b) VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "UPDATE t SET a = 10 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT total FROM t WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(13)]]);
}

#[test]
fn update_generated_column_literal_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "UPDATE t SET total = 99 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("generated column assignment must be rejected");
    assert!(
        format!("{err}").contains("generated column"),
        "unexpected error: {err}",
    );
}

#[test]
fn returning_sees_generated_column_values() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "INSERT INTO t (id, a, b) VALUES (1, 2, 3) RETURNING *",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(5)
        ]]
    );

    let got = rows(
        common::run_ctx(
            "UPDATE t SET a = 10 WHERE id = 1 RETURNING *",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![
            Value::Int(1),
            Value::Int(10),
            Value::Int(3),
            Value::Int(13),
        ]]
    );
}

#[test]
fn constraints_and_indexes_see_generated_column_values() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (a INT, total INT GENERATED ALWAYS AS (a * 2) STORED, CHECK (total < 10))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX t_total ON t (total)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t (a) VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "INSERT INTO t (a) VALUES (5)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("CHECK should see generated value 10");
    assert!(
        format!("{err}").contains("CHECK"),
        "unexpected error: {err}",
    );

    let err = common::run_ctx(
        "INSERT INTO t (a) VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("UNIQUE index should see generated value 4");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "unexpected error: {err}",
    );
}

#[test]
fn on_conflict_do_update_recomputes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX t_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t (id, a, b) VALUES (1, 10, 20) \
         ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT a, b, total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(10), Value::Int(3), Value::Int(13)]]
    );
}

#[test]
fn odku_recomputes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX t_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t (id, a, b) VALUES (1, 10, 20) \
         ON DUPLICATE KEY UPDATE a = VALUES(a)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT a, b, total FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(10), Value::Int(3), Value::Int(13)]]
    );
}

#[test]
fn merge_update_recomputes_generated_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE src (id INT, a INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE dst (id INT, a INT, b INT, total INT GENERATED ALWAYS AS (a + b) STORED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO src VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO dst VALUES (1, 2, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "MERGE INTO dst AS d USING src AS s ON d.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = s.a",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SELECT a, b, total FROM dst",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(10), Value::Int(3), Value::Int(13)]]
    );
}
