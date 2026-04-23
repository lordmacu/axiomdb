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
fn row_number_orders_rows_within_window() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for sql in [
        "CREATE TABLE scores (id INT PRIMARY KEY, team TEXT, points INT)",
        "INSERT INTO scores VALUES (1, 'a', 30), (2, 'a', 10), (3, 'b', 20)",
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    }

    let data = rows(
        run_ctx(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY points DESC) AS rn FROM scores ORDER BY id",
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
            vec![Value::Int(1), Value::BigInt(1)],
            vec![Value::Int(2), Value::BigInt(3)],
            vec![Value::Int(3), Value::BigInt(2)],
        ]
    );
}

#[test]
fn rank_and_dense_rank_reset_per_partition() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for sql in [
        "CREATE TABLE scores (id INT PRIMARY KEY, team TEXT, points INT)",
        "INSERT INTO scores VALUES (1, 'a', 10), (2, 'a', 10), (3, 'a', 5), (4, 'b', 7), (5, 'b', 7), (6, 'b', 1)",
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    }

    let data = rows(
        run_ctx(
            "SELECT id, team, RANK() OVER (PARTITION BY team ORDER BY points DESC) AS r, DENSE_RANK() OVER (PARTITION BY team ORDER BY points DESC) AS dr FROM scores ORDER BY id",
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
            vec![
                Value::Int(1),
                Value::Text("a".into()),
                Value::BigInt(1),
                Value::BigInt(1)
            ],
            vec![
                Value::Int(2),
                Value::Text("a".into()),
                Value::BigInt(1),
                Value::BigInt(1)
            ],
            vec![
                Value::Int(3),
                Value::Text("a".into()),
                Value::BigInt(3),
                Value::BigInt(2)
            ],
            vec![
                Value::Int(4),
                Value::Text("b".into()),
                Value::BigInt(1),
                Value::BigInt(1)
            ],
            vec![
                Value::Int(5),
                Value::Text("b".into()),
                Value::BigInt(1),
                Value::BigInt(1)
            ],
            vec![
                Value::Int(6),
                Value::Text("b".into()),
                Value::BigInt(3),
                Value::BigInt(2)
            ],
        ]
    );
}

#[test]
fn final_order_by_can_differ_from_window_order() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for sql in [
        "CREATE TABLE scores (id INT PRIMARY KEY, points INT)",
        "INSERT INTO scores VALUES (1, 30), (2, 10), (3, 20)",
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    }

    let data = rows(
        run_ctx(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY points DESC) AS rn FROM scores ORDER BY id ASC",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(data[0][1], Value::BigInt(1));
    assert_eq!(data[1][1], Value::BigInt(3));
    assert_eq!(data[2][1], Value::BigInt(2));
}

#[test]
fn nested_window_expression_is_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE scores (id INT PRIMARY KEY, points INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "SELECT ROW_NUMBER() OVER (ORDER BY points DESC) + 1 FROM scores",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    match err {
        DbError::NotImplemented { feature } => {
            assert!(feature.contains("nested inside other SELECT expressions"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn window_functions_in_where_are_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE scores (id INT PRIMARY KEY, points INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "SELECT id FROM scores WHERE ROW_NUMBER() OVER (ORDER BY points) = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    match err {
        DbError::NotImplemented { feature } => {
            assert!(feature.contains("window functions in WHERE"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
