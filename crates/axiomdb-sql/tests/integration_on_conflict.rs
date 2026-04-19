//! Parser and executor coverage for PostgreSQL `INSERT ... ON CONFLICT`.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_with_defaults,
    ast::{InsertSource, OnConflictAction, Stmt},
    expr::Expr,
    parse, QueryResult,
};
use axiomdb_types::Value;

fn analyze_err(sql: &str) -> DbError {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let stmt = parse(sql, None).unwrap();
    let snap = if let Some(ref ct) = ctx.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };
    analyze_with_defaults(
        stmt,
        &storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )
    .expect_err("expected analyze error")
}

fn affected(result: QueryResult) -> u64 {
    match result {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn parses_on_conflict_do_nothing_forms() {
    for sql in [
        "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT INTO t VALUES (1) ON CONFLICT (id) DO NOTHING",
        "INSERT INTO t (id) SELECT 1 ON CONFLICT (id) DO NOTHING RETURNING id",
    ] {
        let stmt = parse(sql, None).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
        let Stmt::Insert(insert) = stmt else {
            panic!("expected INSERT for {sql:?}");
        };
        let clause = insert
            .on_conflict
            .unwrap_or_else(|| panic!("expected ON CONFLICT for {sql:?}"));
        assert!(matches!(clause.action, OnConflictAction::DoNothing));
    }
}

#[test]
fn parses_on_conflict_do_update_with_excluded() {
    let stmt = parse(
        "INSERT INTO t VALUES (1, 2) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v RETURNING *",
        None,
    )
    .unwrap();

    let Stmt::Insert(insert) = stmt else {
        panic!("expected INSERT");
    };
    let clause = insert.on_conflict.expect("expected ON CONFLICT");
    assert_eq!(clause.target_columns, vec!["id"]);
    let OnConflictAction::DoUpdate {
        assignments,
        where_clause,
    } = clause.action
    else {
        panic!("expected DO UPDATE");
    };
    assert!(where_clause.is_none());
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].column, "v");
    assert!(matches!(
        assignments[0].value,
        Expr::ExcludedValue { ref name, .. } if name == "v"
    ));
    assert_eq!(insert.returning.len(), 1);
}

#[test]
fn parses_on_conflict_do_update_where() {
    let stmt = parse(
        "INSERT INTO t (id, v) VALUES (1, 2) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v WHERE v < EXCLUDED.v",
        None,
    )
    .unwrap();

    let Stmt::Insert(insert) = stmt else {
        panic!("expected INSERT");
    };
    let clause = insert.on_conflict.expect("expected ON CONFLICT");
    let OnConflictAction::DoUpdate { where_clause, .. } = clause.action else {
        panic!("expected DO UPDATE");
    };
    assert!(where_clause.is_some());
}

#[test]
fn on_conflict_is_mutually_exclusive_with_replace() {
    let err = parse(
        "REPLACE INTO t VALUES (1) ON CONFLICT (id) DO NOTHING",
        None,
    )
    .expect_err("REPLACE + ON CONFLICT must fail");
    assert!(
        format!("{err}").contains("mutually exclusive"),
        "unexpected error: {err}",
    );
}

#[test]
fn on_conflict_do_update_requires_target() {
    let err = parse(
        "INSERT INTO t VALUES (1) ON CONFLICT DO UPDATE SET v = EXCLUDED.v",
        None,
    )
    .expect_err("DO UPDATE without target must fail");
    assert!(
        format!("{err}").contains("requires a conflict target"),
        "unexpected error: {err}",
    );
}

#[test]
fn insert_select_source_keeps_on_conflict_tail() {
    let stmt = parse(
        "INSERT INTO t (id) SELECT id FROM src ON CONFLICT (id) DO NOTHING",
        None,
    )
    .unwrap();
    let Stmt::Insert(insert) = stmt else {
        panic!("expected INSERT");
    };
    assert!(matches!(insert.source, InsertSource::Select(_)));
    assert!(insert.on_conflict.is_some());
}

#[test]
fn analyzer_rejects_missing_conflict_target_column() {
    let err = analyze_err(
        "INSERT INTO t VALUES (1, 2) \
         ON CONFLICT (missing) DO UPDATE SET v = EXCLUDED.v",
    );
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "{err}");
}

#[test]
fn analyzer_rejects_missing_excluded_column() {
    let err = analyze_err(
        "INSERT INTO t VALUES (1, 2) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.missing",
    );
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "{err}");
}

#[test]
fn analyzer_rejects_conflict_target_without_unique_index() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let stmt = parse(
        "INSERT INTO t VALUES (1, 2) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
        None,
    )
    .unwrap();
    let snap = if let Some(ref ct) = ctx.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };
    let err = analyze_with_defaults(
        stmt,
        &storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )
    .expect_err("expected missing unique index error");
    assert!(
        format!("{err}").contains("no matching unique"),
        "unexpected error: {err}",
    );
}

#[test]
fn do_nothing_without_target_skips_pk_and_unique_conflicts() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_v ON t (v)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    affected(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 10)",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );

    let skipped_pk = affected(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 11) ON CONFLICT DO NOTHING",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    let skipped_unique = affected(
        common::run_ctx(
            "INSERT INTO t VALUES (2, 10) ON CONFLICT DO NOTHING",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(skipped_pk, 0);
    assert_eq!(skipped_unique, 0);

    let rows = rows(
        common::run_ctx(
            "SELECT id, v FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn do_nothing_with_target_only_skips_matching_target() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_v ON t (v)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let skipped = affected(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 11) ON CONFLICT (id) DO NOTHING",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(skipped, 0);

    let err = common::run_ctx(
        "INSERT INTO t VALUES (2, 10) ON CONFLICT (id) DO NOTHING",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("conflict on v should not be skipped by ON CONFLICT (id)");
    assert!(
        format!("{err}").contains("unique"),
        "unexpected error: {err}",
    );
}

#[test]
fn do_nothing_null_key_components_do_not_conflict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (NULL, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let inserted = affected(
        common::run_ctx(
            "INSERT INTO t VALUES (NULL, 11) ON CONFLICT (id) DO NOTHING",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(inserted, 1);
}

#[test]
fn do_update_uses_target_and_excluded_values() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let count = affected(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 5) \
             ON CONFLICT (id) DO UPDATE SET v = v + EXCLUDED.v",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(count, 1);
    let rows = rows(
        common::run_ctx(
            "SELECT v FROM t WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(matches!(rows[0][0], Value::Int(15) | Value::BigInt(15)));
}

#[test]
fn do_update_where_false_skips_count_and_returning() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let out = rows(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 20) \
             ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v WHERE v > EXCLUDED.v \
             RETURNING id, v",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(out.is_empty());
    let rows = rows(
        common::run_ctx(
            "SELECT v FROM t WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(matches!(rows[0][0], Value::Int(10) | Value::BigInt(10)));
}

#[test]
fn returning_reports_inserted_and_updated_rows_only() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let out = rows(
        common::run_ctx(
            "INSERT INTO t VALUES (1, 20), (2, 30) \
             ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v \
             RETURNING id, v",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(out.len(), 2);
    assert!(matches!(out[0][0], Value::Int(1) | Value::BigInt(1)));
    assert!(matches!(out[0][1], Value::Int(20) | Value::BigInt(20)));
    assert!(matches!(out[1][0], Value::Int(2) | Value::BigInt(2)));
    assert!(matches!(out[1][1], Value::Int(30) | Value::BigInt(30)));
}

#[test]
fn same_target_row_updated_twice_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE UNIQUE INDEX idx_id ON t (id)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "INSERT INTO t VALUES (1, 20), (1, 30) \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("same target row should not be updated twice");
    assert!(
        format!("{err}").contains("same row twice"),
        "unexpected error: {err}",
    );
}
