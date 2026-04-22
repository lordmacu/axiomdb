mod common;

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use common::{rows, run, run_result, setup};

fn setup_sales() -> (axiomdb_storage::MemoryStorage, axiomdb_wal::TxnManager) {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE sales (region TEXT, product TEXT, month TEXT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO sales VALUES \
         ('north','a','Jan',10), \
         ('north','a','Feb',20), \
         ('north','b','Jan',7), \
         ('south','a','Jan',15), \
         ('south','a','Mar',5)",
        &mut storage,
        &mut txn,
    );
    (storage, txn)
}

#[test]
fn pivot_basic_two_values() {
    let (mut storage, mut txn) = setup_sales();
    let result = rows(run(
        "SELECT * \
         FROM sales PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p \
         ORDER BY region, product",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        result,
        vec![
            vec![
                Value::Text("north".into()),
                Value::Text("a".into()),
                Value::Int(10),
                Value::Int(20),
            ],
            vec![
                Value::Text("north".into()),
                Value::Text("b".into()),
                Value::Int(7),
                Value::Null,
            ],
            vec![
                Value::Text("south".into()),
                Value::Text("a".into()),
                Value::Int(15),
                Value::Null,
            ],
        ]
    );
}

#[test]
fn pivot_multiple_grouping_columns_and_outer_projection() {
    let (mut storage, mut txn) = setup_sales();
    let result = rows(run(
        "SELECT region, Feb \
         FROM sales PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p \
         ORDER BY region, product",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        result,
        vec![
            vec![Value::Text("north".into()), Value::Int(20)],
            vec![Value::Text("north".into()), Value::Null],
            vec![Value::Text("south".into()), Value::Null],
        ]
    );
}

#[test]
fn pivot_without_passthrough_columns_returns_single_row() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE monthly_amounts (month TEXT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO monthly_amounts VALUES ('Jan', 10), ('Jan', 2), ('Feb', 5)",
        &mut storage,
        &mut txn,
    );

    let result = rows(run(
        "SELECT * \
         FROM monthly_amounts PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb'))",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(result, vec![vec![Value::Int(12), Value::Int(5)]]);
}

#[test]
fn pivot_can_be_joined_as_derived_table() {
    let (mut storage, mut txn) = setup_sales();
    run(
        "CREATE TABLE regions (name TEXT, label TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO regions VALUES ('north', 'North'), ('south', 'South')",
        &mut storage,
        &mut txn,
    );

    let result = rows(run(
        "SELECT r.label, p.Jan, p.Feb \
         FROM regions r \
         JOIN sales PIVOT (SUM(amount) FOR month IN ('Jan', 'Feb')) AS p \
           ON r.name = p.region \
         WHERE p.product = 'a' \
         ORDER BY r.label",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        result,
        vec![
            vec![Value::Text("North".into()), Value::Int(10), Value::Int(20)],
            vec![Value::Text("South".into()), Value::Int(15), Value::Null],
        ]
    );
}

#[test]
fn pivot_rejects_output_name_collision() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (region TEXT, Jan INT, month TEXT, amount INT)",
        &mut storage,
        &mut txn,
    );
    let err = run_result(
        "SELECT * FROM t PIVOT (SUM(amount) FOR month IN ('Jan'))",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected pivot name collision");

    match err {
        DbError::Other(msg) => assert!(msg.contains("duplicate PIVOT output column")),
        other => panic!("expected duplicate-column error, got {other:?}"),
    }
}

#[test]
fn pivot_rejects_non_literal_in_values_list() {
    let (mut storage, mut txn) = setup_sales();
    let err = run_result(
        "SELECT * FROM sales PIVOT (SUM(amount) FOR month IN (LOWER('Jan')))",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected parse error for non-literal pivot value");

    let msg = err.to_string();
    assert!(msg.contains("PIVOT IN values must be literals"));
}
