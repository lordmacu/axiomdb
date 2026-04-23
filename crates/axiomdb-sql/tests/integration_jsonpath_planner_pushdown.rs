//! Integration tests for Phase 11.21h — JSONPath planner pushdown to GIN.

mod common;

use axiomdb_types::Value;

fn explain_rows(sql: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, doc JSONB)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES \
            (1, CAST('{\"k\":1,\"flag\":true,\"a\":{\"b\":1}}' AS JSONB)), \
            (2, CAST('{\"flag\":false,\"a\":{\"b\":2}}' AS JSONB)), \
            (3, CAST('{\"other\":1}' AS JSONB))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::rows(common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap())
}

fn explain_rows_with_index(sql: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, doc JSONB)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE INDEX idx_doc_gin ON t USING GIN (doc)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES \
            (1, CAST('{\"k\":1,\"flag\":true,\"a\":{\"b\":1}}' AS JSONB)), \
            (2, CAST('{\"flag\":false,\"a\":{\"b\":2}}' AS JSONB)), \
            (3, CAST('{\"other\":1}' AS JSONB))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::rows(common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap())
}

#[test]
fn explain_at_question_simple_key_uses_gin() {
    let rows = explain_rows_with_index("EXPLAIN SELECT id FROM t WHERE doc @? '$.k'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3], Value::Text("gin".into()));
    assert_eq!(rows[0][5], Value::Text("idx_doc_gin".into()));
    assert_eq!(
        rows[0][9],
        Value::Text("Using GIN index; Using where".into())
    );
}

#[test]
fn explain_at_at_simple_key_uses_gin() {
    let rows = explain_rows_with_index("EXPLAIN SELECT id FROM t WHERE doc @@ '$.flag'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3], Value::Text("gin".into()));
    assert_eq!(rows[0][5], Value::Text("idx_doc_gin".into()));
    assert_eq!(
        rows[0][9],
        Value::Text("Using GIN index; Using where".into())
    );
}

#[test]
fn at_at_gin_path_still_rechecks_predicate() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, doc JSONB)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE INDEX idx_doc_gin ON t USING GIN (doc)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES \
            (1, CAST('{\"flag\":true}' AS JSONB)), \
            (2, CAST('{\"flag\":false}' AS JSONB)), \
            (3, CAST('{\"other\":1}' AS JSONB))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(
        common::run_ctx(
            "SELECT id FROM t WHERE doc @@ '$.flag' ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn explain_without_gin_index_falls_back_to_scan() {
    let rows = explain_rows("EXPLAIN SELECT id FROM t WHERE doc @? '$.k'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3], Value::Text("ALL".into()));
    assert_eq!(rows[0][5], Value::Null);
}

#[test]
fn explain_nested_jsonpath_falls_back_to_scan() {
    let rows = explain_rows_with_index("EXPLAIN SELECT id FROM t WHERE doc @? '$.a.b'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3], Value::Text("ALL".into()));
    assert_eq!(rows[0][5], Value::Null);
}
