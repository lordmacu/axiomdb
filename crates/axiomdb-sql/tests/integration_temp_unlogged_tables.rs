mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_with_defaults, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::setup_ctx;

fn run_with_ctx(
    storage: &MemoryStorage,
    txn: &TxnManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
    sql: &str,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = if let Some(ref ct) = ctx.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };
    let analyzed = analyze_with_defaults(
        stmt,
        storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn scalar_i64(row: &Value) -> i64 {
    match row {
        Value::Int(v) => i64::from(*v),
        Value::BigInt(v) => *v,
        other => panic!("expected integer scalar, got {other:?}"),
    }
}

#[test]
fn test_show_create_table_reconstructs_temporary_and_unlogged_prefixes() {
    let (storage, txn, bloom, mut ctx) = setup_ctx();

    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TEMP TABLE temp_show (id INT)",
    )
    .unwrap();
    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE UNLOGGED TABLE unlogged_show (id INT)",
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "SHOW CREATE TABLE temp_show",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    match &rows[0][1] {
        Value::Text(ddl) => assert!(
            ddl.starts_with("CREATE TEMPORARY TABLE"),
            "unexpected TEMP SHOW CREATE: {ddl}"
        ),
        other => panic!("expected text DDL, got {other:?}"),
    }

    let QueryResult::Rows { rows, .. } = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "SHOW CREATE TABLE unlogged_show",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    match &rows[0][1] {
        Value::Text(ddl) => assert!(
            ddl.starts_with("CREATE UNLOGGED TABLE"),
            "unexpected UNLOGGED SHOW CREATE: {ddl}"
        ),
        other => panic!("expected text DDL, got {other:?}"),
    }
}

#[test]
fn test_information_schema_hides_foreign_temp_tables() {
    let (storage, txn, bloom, mut ctx_a) = setup_ctx();
    let mut ctx_b = SessionContext::new();

    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx_a,
        "CREATE TEMP TABLE session_only (id INT PRIMARY KEY)",
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx_a,
        "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_NAME = 'session_only'",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(scalar_i64(&rows[0][0]), 1);

    let QueryResult::Rows { rows, .. } = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx_a,
        "SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS \
         WHERE TABLE_NAME = 'session_only' AND CONSTRAINT_TYPE = 'PRIMARY KEY'",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(scalar_i64(&rows[0][0]), 1);

    let QueryResult::Rows { rows, .. } = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx_a,
        "SELECT COUNT(*) FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_NAME = 'session_only' AND COLUMN_NAME = 'id'",
    )
    .unwrap() else {
        panic!("expected rows")
    };
    assert_eq!(scalar_i64(&rows[0][0]), 1);

    for sql in [
        "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_NAME = 'session_only'",
        "SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS \
         WHERE TABLE_NAME = 'session_only' AND CONSTRAINT_TYPE = 'PRIMARY KEY'",
        "SELECT COUNT(*) FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_NAME = 'session_only' AND COLUMN_NAME = 'id'",
    ] {
        let QueryResult::Rows { rows, .. } =
            run_with_ctx(&storage, &txn, &bloom, &mut ctx_b, sql).unwrap()
        else {
            panic!("expected rows")
        };
        assert_eq!(scalar_i64(&rows[0][0]), 0, "sql: {sql}");
    }
}

#[test]
fn test_foreign_keys_on_or_referencing_temp_unlogged_tables_are_rejected() {
    let (storage, txn, bloom, mut ctx) = setup_ctx();

    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TABLE parents (id INT PRIMARY KEY)",
    )
    .unwrap();
    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TEMP TABLE temp_parent (id INT PRIMARY KEY)",
    )
    .unwrap();
    run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE UNLOGGED TABLE unlogged_parent (id INT PRIMARY KEY)",
    )
    .unwrap();

    let err = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TEMP TABLE temp_child (id INT PRIMARY KEY, parent_id INT REFERENCES parents(id))",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");

    let err = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE UNLOGGED TABLE unlogged_child (id INT PRIMARY KEY, parent_id INT REFERENCES parents(id))",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");

    let err = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TABLE refs_temp_parent (id INT PRIMARY KEY, parent_id INT REFERENCES temp_parent(id))",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");

    let err = run_with_ctx(
        &storage,
        &txn,
        &bloom,
        &mut ctx,
        "CREATE TABLE refs_unlogged_parent (id INT PRIMARY KEY, parent_id INT REFERENCES unlogged_parent(id))",
    )
    .unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");
}
