//! Phase 21.23 — Advanced SQL acceptance/regression suite.
//!
//! This file is intentionally interaction-focused: it exercises already
//! implemented advanced SQL features together in realistic multi-statement
//! flows instead of re-testing every parser edge case from each subphase.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::{Checkpointer, TxnManager};

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn query_rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    rows(run_ctx(sql, storage, txn, bloom, ctx).unwrap())
}

#[test]
fn cte_and_recursive_cte_acceptance_flow_returns_expected_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE employees (id INT, manager_id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO employees VALUES \
         (1, 0, 'alice'), \
         (2, 1, 'bob'), \
         (3, 2, 'carol'), \
         (4, 2, 'dave')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let root_rows = query_rows(
        "WITH roots AS (SELECT id, name FROM employees WHERE manager_id = 0) \
         SELECT id, name FROM roots",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        root_rows,
        vec![vec![Value::Int(1), Value::Text("alice".into())]]
    );

    let recursive_rows = query_rows(
        "WITH RECURSIVE org(id, depth) AS (\
            SELECT id, 0 FROM employees WHERE id = 1 \
            UNION ALL \
            SELECT e.id, org.depth + 1 \
              FROM employees e JOIN org ON e.manager_id = org.id\
         ) \
         SELECT id, depth FROM org ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        recursive_rows,
        vec![
            vec![Value::Int(1), Value::Int(0)],
            vec![Value::Int(2), Value::Int(1)],
            vec![Value::Int(3), Value::Int(2)],
            vec![Value::Int(4), Value::Int(2)],
        ]
    );
}

#[test]
fn merge_and_savepoint_acceptance_flow_preserves_pre_savepoint_changes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE dst (id INT, qty INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO dst VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 20), (2, 5)) AS s(id, qty) \
         ON d.id = s.id \
         WHEN MATCHED THEN UPDATE SET qty = s.qty \
         WHEN NOT MATCHED THEN INSERT (id, qty) VALUES (s.id, s.qty)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SAVEPOINT after_first_merge",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "MERGE INTO dst AS d USING (VALUES (1, 99), (3, 9)) AS s(id, qty) \
         ON d.id = s.id \
         WHEN MATCHED THEN UPDATE SET qty = s.qty \
         WHEN NOT MATCHED THEN INSERT (id, qty) VALUES (s.id, s.qty)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ROLLBACK TO SAVEPOINT after_first_merge",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let rows = query_rows(
        "SELECT id, qty FROM dst ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(2), Value::Int(5)],
        ]
    );
}

#[test]
fn cursor_over_cte_acceptance_flow_fetches_and_closes_on_commit() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE sales (region TEXT, amount INT)",
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

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "DECLARE regional CURSOR FOR \
         WITH totals AS (\
             SELECT region, SUM(amount) AS total FROM sales GROUP BY region\
         ) \
         SELECT region, total FROM totals ORDER BY region",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let first = query_rows(
        "FETCH 1 FROM regional",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        first,
        vec![vec![Value::Text("north".into()), Value::Int(25)]]
    );

    let rest = query_rows(
        "FETCH ALL FROM regional",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rest, vec![vec![Value::Text("south".into()), Value::Int(7)]]);

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let err = run_ctx(
        "FETCH NEXT FROM regional",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    match err {
        DbError::InvalidValue { reason } => {
            assert!(reason.contains("not found"), "got {reason}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

#[test]
fn checkpoint_acceptance_flow_matches_transaction_rules() {
    let (mut storage, mut txn, mut bloom, mut writer_ctx) = setup();
    let mut admin_ctx = SessionContext::new();

    assert_eq!(Checkpointer::last_checkpoint_lsn(&storage).unwrap(), 0);

    match run_ctx(
        "CHECKPOINT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut admin_ctx,
    )
    .unwrap()
    {
        QueryResult::Empty => {}
        other => panic!("expected Empty result, got {other:?}"),
    }
    assert!(Checkpointer::last_checkpoint_lsn(&storage).unwrap() > 0);

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut writer_ctx).unwrap();
    let writer_txn_id = writer_ctx.conn_txn.as_ref().unwrap().txn_id;

    let err = run_ctx(
        "CHECKPOINT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut admin_ctx,
    )
    .unwrap_err();
    match err {
        DbError::TransactionAlreadyActive { txn_id } => assert_eq!(txn_id, writer_txn_id),
        other => panic!("unexpected error: {other:?}"),
    }

    run_ctx(
        "ROLLBACK",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut writer_ctx,
    )
    .unwrap();
}

#[test]
fn grouping_sets_acceptance_flow_returns_subtotals_and_grand_total() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE gs_sales (region TEXT, yr INT, amount INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO gs_sales VALUES \
         ('N', 2022, 10), ('N', 2022, 20), ('N', 2023, 15), \
         ('S', 2022, 5), ('S', 2023, 25), ('S', 2023, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = query_rows(
        "SELECT region, yr, SUM(amount) \
         FROM gs_sales \
         GROUP BY GROUPING SETS ((region, yr), (region), ()) \
         ORDER BY region, yr",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(rows.len(), 7, "expected 7 grouping-set rows, got {rows:?}");

    let grand_total = rows
        .iter()
        .find(|row| row[0] == Value::Null && row[1] == Value::Null)
        .expect("missing grand-total row");
    assert_eq!(grand_total[2], Value::Int(85));

    let north_subtotal = rows
        .iter()
        .find(|row| row[0] == Value::Text("N".into()) && row[1] == Value::Null)
        .expect("missing north subtotal");
    assert_eq!(north_subtotal[2], Value::Int(45));
}
