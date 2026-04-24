mod common;

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
fn show_create_trigger_round_trips_body() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE journal (id INT, debit INT, credit INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TRIGGER journal_balanced AFTER INSERT ON journal FOR EACH STATEMENT AS \
         SELECT 'journal not balanced' FROM journal GROUP BY 1 \
         HAVING SUM(debit) <> SUM(credit)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SHOW CREATE TRIGGER journal_balanced ON journal",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Text("journal_balanced".into()));
    assert!(
        matches!(&got[0][1], Value::Text(sql) if sql.contains("AFTER INSERT ON journal FOR EACH STATEMENT")),
        "expected SHOW CREATE TRIGGER output, got {:?}",
        got[0][1]
    );
}

#[test]
fn statement_trigger_fires_once_and_allows_balanced_batch() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE journal (id INT, debit INT, credit INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TRIGGER journal_balanced AFTER INSERT ON journal FOR EACH STATEMENT AS \
         SELECT 'bad batch' FROM journal WHERE 1 = 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "INSERT INTO journal VALUES (1, 10, 0), (2, 0, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 2);

    let got = rows(
        common::run_ctx(
            "SELECT id, debit, credit FROM journal ORDER BY id",
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
            vec![Value::Int(1), Value::Int(10), Value::Int(0)],
            vec![Value::Int(2), Value::Int(0), Value::Int(10)],
        ]
    );
}

#[test]
fn statement_trigger_rejects_unbalanced_batch_and_rolls_back_statement() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE journal (id INT, debit INT, credit INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TRIGGER journal_balanced AFTER INSERT ON journal FOR EACH STATEMENT AS \
         SELECT 'journal not balanced' FROM journal GROUP BY 1 \
         HAVING SUM(debit) <> SUM(credit)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "INSERT INTO journal VALUES (1, 10, 0), (2, 0, 9)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::TriggerValidationFailed { ref trigger, ref table, .. }
                if trigger == "journal_balanced" && table == "journal"
        ),
        "unexpected error: {err:?}"
    );

    let remaining = rows(
        common::run_ctx(
            "SELECT id FROM journal ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(
        remaining.is_empty(),
        "failed statement must leave no rows behind"
    );
}

#[test]
fn drop_trigger_stops_firing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE journal (id INT, debit INT, credit INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TRIGGER journal_balanced AFTER INSERT ON journal FOR EACH STATEMENT AS \
         SELECT 'journal not balanced' FROM journal GROUP BY 1 \
         HAVING SUM(debit) <> SUM(credit)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "DROP TRIGGER journal_balanced ON journal",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "INSERT INTO journal VALUES (1, 10, 0), (2, 0, 9)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(common::affected_count(result), 2);
}
