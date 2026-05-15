mod common;

use axiomdb_types::Value;

fn sql(query: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(query, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    match r {
        axiomdb_sql::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn sql_cols(query: &str) -> (Vec<axiomdb_sql::result::ColumnMeta>, Vec<Vec<Value>>) {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(query, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    match r {
        axiomdb_sql::QueryResult::Rows { columns, rows, .. } => (columns, rows),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn sql_multi(stmts: &[&str]) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let mut result = Vec::new();
    for stmt in stmts {
        let r = common::run_ctx(stmt, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        if let axiomdb_sql::QueryResult::Rows { rows, .. } = r {
            result = rows;
        }
    }
    result
}

fn sql_err(query: &str) -> axiomdb_core::error::DbError {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(query, &mut storage, &mut txn, &mut bloom, &mut ctx)
        .expect_err("expected error")
}

// ── Basic literal array, no FROM ──────────────────────────────────────────────

#[test]
fn unnest_select_literal_array_no_from() {
    let r = sql("SELECT UNNEST(ARRAY[1,2,3]) AS n");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn unnest_select_text_array_no_from() {
    let r = sql("SELECT UNNEST(ARRAY['a','b','c']) AS tag");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("a".into()));
    assert_eq!(r[2][0], Value::Text("c".into()));
}

#[test]
fn unnest_select_single_element() {
    let r = sql("SELECT UNNEST(ARRAY[42]) AS n");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(42));
}

// ── Default column names ──────────────────────────────────────────────────────

#[test]
fn unnest_select_no_alias_default_name() {
    let (cols, rows) = sql_cols("SELECT UNNEST(ARRAY[1,2])");
    assert_eq!(cols[0].name, "unnest");
    assert_eq!(rows.len(), 2);
}

#[test]
fn unnest_select_second_unnest_default_name() {
    let (cols, rows) = sql_cols("SELECT UNNEST(ARRAY[1,2]), UNNEST(ARRAY['a','b'])");
    assert_eq!(cols[0].name, "unnest");
    assert_eq!(cols[1].name, "unnest_1");
    assert_eq!(rows.len(), 2);
}

// ── Scalar + UNNEST ───────────────────────────────────────────────────────────

#[test]
fn unnest_select_scalar_repeats() {
    let r = sql("SELECT 42, UNNEST(ARRAY['a','b','c']) AS tag");
    assert_eq!(r.len(), 3);
    assert!(matches!(r[0][0], Value::Int(42) | Value::BigInt(42)));
    assert_eq!(r[0][1], Value::Text("a".into()));
    assert_eq!(r[2][1], Value::Text("c".into()));
}

// ── Multiple UNNESTs — zip semantics ─────────────────────────────────────────

#[test]
fn unnest_select_two_unnests_zip() {
    let r = sql("SELECT UNNEST(ARRAY[1,2,3]) AS n, UNNEST(ARRAY['a','b','c']) AS s");
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("a".into()));
    assert_eq!(r[2][0], Value::Int(3));
    assert_eq!(r[2][1], Value::Text("c".into()));
}

#[test]
fn unnest_select_two_unnests_different_lengths() {
    // The executor requires equal-length arrays for zip — mismatched lengths → error
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST(ARRAY[1,2,3]) AS a, UNNEST(ARRAY['x','y']) AS b",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    );
    assert!(r.is_err(), "expected error for mismatched array lengths");
}

// ── NULL and empty arrays ─────────────────────────────────────────────────────

#[test]
fn unnest_select_null_array_zero_rows() {
    let r = sql("SELECT UNNEST(NULL::INT[]) AS n");
    assert_eq!(r.len(), 0);
}

#[test]
fn unnest_select_empty_array_zero_rows() {
    let r = sql("SELECT UNNEST(ARRAY[]::INT[]) AS n");
    assert_eq!(r.len(), 0);
}

// ── With real table ───────────────────────────────────────────────────────────

#[test]
fn unnest_select_from_table() {
    let r = sql_multi(&[
        "CREATE TABLE posts (id INT, tags TEXT[])",
        "INSERT INTO posts VALUES (1, ARRAY['rust','db'])",
        "INSERT INTO posts VALUES (2, ARRAY['sql'])",
        "SELECT id, UNNEST(tags) AS tag FROM posts ORDER BY id, tag",
    ]);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0], vec![Value::Int(1), Value::Text("db".into())]);
    assert_eq!(r[1], vec![Value::Int(1), Value::Text("rust".into())]);
    assert_eq!(r[2], vec![Value::Int(2), Value::Text("sql".into())]);
}

#[test]
fn unnest_select_where_on_base_table() {
    let r = sql_multi(&[
        "CREATE TABLE t (id INT, arr INT[])",
        "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, ARRAY[30])",
        "SELECT id, UNNEST(arr) AS n FROM t WHERE id = 1 ORDER BY n",
    ]);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Int(10));
    assert_eq!(r[1][1], Value::Int(20));
}

#[test]
fn unnest_select_order_by_unnest_col() {
    let r = sql("SELECT UNNEST(ARRAY[3,1,2]) AS n ORDER BY n");
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn unnest_select_limit() {
    let r = sql("SELECT UNNEST(ARRAY[1,2,3,4,5]) AS n LIMIT 3");
    assert_eq!(r.len(), 3);
}

// ── CTE and subquery ──────────────────────────────────────────────────────────

#[test]
fn unnest_select_in_cte() {
    let r = sql_multi(&[
        "CREATE TABLE posts (id INT, tags TEXT[])",
        "INSERT INTO posts VALUES (1, ARRAY['rust','db','sql'])",
        "WITH expanded AS (SELECT id, UNNEST(tags) AS tag FROM posts) \
         SELECT * FROM expanded WHERE tag = 'db'",
    ]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Text("db".into()));
}

#[test]
fn unnest_select_in_subquery() {
    let r = sql_multi(&[
        "CREATE TABLE t (arr INT[])",
        "INSERT INTO t VALUES (ARRAY[10,20,30])",
        "SELECT * FROM (SELECT UNNEST(arr) AS n FROM t) AS sub ORDER BY n",
    ]);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(10));
    assert_eq!(r[2][0], Value::Int(30));
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn unnest_select_zero_arg_error() {
    // Parser rejects UNNEST() before our validator; any error is acceptable
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx("SELECT UNNEST() AS n", &mut storage, &mut txn, &mut bloom, &mut ctx);
    assert!(r.is_err(), "expected error for zero-arg UNNEST");
}

#[test]
fn unnest_select_multi_arg_error() {
    // Parser rejects UNNEST(a, b) in expression context before our validator;
    // any error is acceptable — the important thing is that it errors out
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(
        "SELECT UNNEST(ARRAY[1], ARRAY[2]) AS n",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    );
    assert!(r.is_err(), "expected error for multi-arg UNNEST in SELECT list");
}
