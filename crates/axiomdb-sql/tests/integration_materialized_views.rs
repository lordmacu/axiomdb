mod common;

use axiomdb_core::error::DbError;
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
fn create_materialized_view_populates_rows_and_metadata() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE sales (region TEXT, total INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO sales VALUES ('north', 10), ('north', 15), ('south', 7)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE MATERIALIZED VIEW mv_sales AS SELECT region, SUM(total) AS total FROM sales GROUP BY region",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let data = rows(
        run_ctx(
            "SELECT region, total FROM mv_sales ORDER BY region",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        data,
        vec![
            vec![Value::Text("north".into()), Value::Int(25)],
            vec![Value::Text("south".into()), Value::Int(7)],
        ]
    );

    let full_tables = rows(
        run_ctx(
            "SHOW FULL TABLES",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(
        full_tables
            .iter()
            .any(|row| row[0] == Value::Text("mv_sales".into())
                && row[1] == Value::Text("MATERIALIZED VIEW".into())),
        "SHOW FULL TABLES must expose mv_sales as MATERIALIZED VIEW: {full_tables:?}"
    );

    let is_rows = rows(
        run_ctx(
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_NAME = 'mv_sales'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        is_rows,
        vec![vec![
            Value::Text("mv_sales".into()),
            Value::Text("MATERIALIZED VIEW".into())
        ]]
    );

    match run_ctx(
        "SHOW CREATE TABLE mv_sales",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[0].name, "View");
            assert_eq!(columns[1].name, "Create View");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Text("mv_sales".into()));
            let ddl = match &rows[0][1] {
                Value::Text(text) => text.clone(),
                other => panic!("expected DDL text, got {other:?}"),
            };
            assert!(
                ddl.contains("CREATE MATERIALIZED VIEW `mv_sales` AS SELECT region, SUM(total) AS total FROM sales GROUP BY region"),
                "unexpected SHOW CREATE TABLE output: {ddl}"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn refresh_materialized_view_rebuilds_contents() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for sql in [
        "CREATE TABLE sales (region TEXT, total INT)",
        "INSERT INTO sales VALUES ('north', 10), ('south', 7)",
        "CREATE MATERIALIZED VIEW mv_sales AS SELECT region, SUM(total) AS total FROM sales GROUP BY region",
        "INSERT INTO sales VALUES ('north', 5), ('east', 3)",
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    }

    let before = rows(
        run_ctx(
            "SELECT region, total FROM mv_sales ORDER BY region",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        before,
        vec![
            vec![Value::Text("north".into()), Value::Int(10)],
            vec![Value::Text("south".into()), Value::Int(7)],
        ]
    );

    run_ctx(
        "REFRESH MATERIALIZED VIEW mv_sales",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let after = rows(
        run_ctx(
            "SELECT region, total FROM mv_sales ORDER BY region",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        after,
        vec![
            vec![Value::Text("east".into()), Value::Int(3)],
            vec![Value::Text("north".into()), Value::Int(15)],
            vec![Value::Text("south".into()), Value::Int(7)],
        ]
    );
}

#[test]
fn drop_materialized_view_rejects_plain_tables() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE plain_t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "DROP MATERIALIZED VIEW plain_t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    match err {
        DbError::InvalidValue { reason } => {
            assert!(reason.contains("not a materialized view"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
