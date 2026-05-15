mod common;

use axiomdb_sql::QueryResult;

fn run(sql: &str) -> QueryResult {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn run_multi(stmts: &[&str]) -> QueryResult {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let mut last = QueryResult::Affected {
        count: 0,
        last_insert_id: None,
    };
    for sql in stmts {
        last = common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    }
    last
}

fn run_err(sql: &str) -> axiomdb_core::error::DbError {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).expect_err("expected error")
}

fn rows(r: QueryResult) -> Vec<Vec<axiomdb_types::Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn affected(r: QueryResult) -> u64 {
    match r {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

fn col_names(r: &QueryResult) -> Vec<String> {
    match r {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── COPY TO PARQUET tests ─────────────────────────────────────────────────────

#[test]
fn test_copy_to_parquet_creates_file() {
    let path = "/tmp/axm_pq_basic.parquet";
    let r = run_multi(&[
        "CREATE TABLE axm_pq_basic (id INT, name VARCHAR(20))",
        "INSERT INTO axm_pq_basic VALUES (1, 'alice'), (2, 'bob')",
        &format!("COPY axm_pq_basic TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    assert_eq!(affected(r), 2);
    assert!(std::path::Path::new(path).exists(), "file should exist");
}

#[test]
fn test_copy_to_parquet_empty_table() {
    let path = "/tmp/axm_pq_empty.parquet";
    let r = run_multi(&[
        "CREATE TABLE axm_pq_empty (x INT)",
        &format!("COPY axm_pq_empty TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    assert_eq!(affected(r), 0);
    assert!(std::path::Path::new(path).exists());
}

#[test]
fn test_copy_to_parquet_snappy() {
    let path = "/tmp/axm_pq_snappy.parquet";
    let r = run_multi(&[
        "CREATE TABLE axm_pq_snappy (id INT, val TEXT)",
        "INSERT INTO axm_pq_snappy VALUES (1, 'hello')",
        &format!("COPY axm_pq_snappy TO '{path}' WITH (FORMAT PARQUET, COMPRESSION SNAPPY)"),
    ]);
    assert_eq!(affected(r), 1);
    assert!(std::path::Path::new(path).exists());
}

#[test]
fn test_copy_to_parquet_all_basic_types() {
    let path = "/tmp/axm_pq_types.parquet";
    let r = run_multi(&[
        "CREATE TABLE axm_pq_types (a INT, b BIGINT, c REAL, d BOOL, e TEXT)",
        "INSERT INTO axm_pq_types VALUES (1, 9999999999, 3.14, TRUE, 'hello')",
        &format!("COPY axm_pq_types TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    assert_eq!(affected(r), 1);
    assert!(std::path::Path::new(path).exists());
}

// ── READ_PARQUET tests ────────────────────────────────────────────────────────

#[test]
fn test_read_parquet_basic() {
    let path = "/tmp/axm_pq_read_basic.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_src (id INT, name TEXT)",
        "INSERT INTO axm_pq_src VALUES (1, 'alice'), (2, 'bob')",
        &format!("COPY axm_pq_src TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT * FROM READ_PARQUET('{path}') ORDER BY id"));
    let data = rows(r);
    assert_eq!(data.len(), 2);
}

#[test]
fn test_read_parquet_empty_file() {
    let path = "/tmp/axm_pq_read_empty.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_empty2 (x INT)",
        &format!("COPY axm_pq_empty2 TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT * FROM READ_PARQUET('{path}')"));
    let data = rows(r);
    assert_eq!(data.len(), 0, "empty parquet should return 0 rows");
}

#[test]
fn test_read_parquet_where_filter() {
    let path = "/tmp/axm_pq_where.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_where (id INT, score INT)",
        "INSERT INTO axm_pq_where VALUES (1, 80), (2, 95), (3, 70)",
        &format!("COPY axm_pq_where TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!(
        "SELECT * FROM READ_PARQUET('{path}') WHERE score > 85"
    ));
    let data = rows(r);
    assert_eq!(data.len(), 1);
}

#[test]
fn test_read_parquet_order_by() {
    let path = "/tmp/axm_pq_order.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_order (id INT)",
        "INSERT INTO axm_pq_order VALUES (3), (1), (2)",
        &format!("COPY axm_pq_order TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT * FROM READ_PARQUET('{path}') ORDER BY id"));
    let data = rows(r);
    assert_eq!(data.len(), 3);
    // rows are ordered: 1, 2, 3
    use axiomdb_types::Value;
    assert_eq!(data[0][0], Value::Int(1));
    assert_eq!(data[1][0], Value::Int(2));
    assert_eq!(data[2][0], Value::Int(3));
}

#[test]
fn test_read_parquet_limit() {
    let path = "/tmp/axm_pq_limit.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_limit (id INT)",
        "INSERT INTO axm_pq_limit VALUES (1), (2), (3), (4), (5)",
        &format!("COPY axm_pq_limit TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!(
        "SELECT * FROM READ_PARQUET('{path}') ORDER BY id LIMIT 2"
    ));
    let data = rows(r);
    assert_eq!(data.len(), 2);
}

#[test]
fn test_read_parquet_alias() {
    let path = "/tmp/axm_pq_alias.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_alias_src (id INT)",
        "INSERT INTO axm_pq_alias_src VALUES (42)",
        &format!("COPY axm_pq_alias_src TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT p.id FROM READ_PARQUET('{path}') AS p"));
    let data = rows(r);
    assert_eq!(data.len(), 1);
    use axiomdb_types::Value;
    assert_eq!(data[0][0], Value::Int(42));
}

#[test]
fn test_read_parquet_column_aliases() {
    let path = "/tmp/axm_pq_col_alias.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_cola (a INT, b TEXT)",
        "INSERT INTO axm_pq_cola VALUES (1, 'x')",
        &format!("COPY axm_pq_cola TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    // Rename columns via alias list
    let r = run(&format!(
        "SELECT renamed_a FROM READ_PARQUET('{path}') AS p(renamed_a, renamed_b)"
    ));
    let names = col_names(&r);
    assert!(
        names.contains(&"renamed_a".to_string()),
        "column should be renamed"
    );
    let data = rows(r);
    assert_eq!(data.len(), 1);
}

#[test]
fn test_read_parquet_in_cte() {
    let path = "/tmp/axm_pq_cte.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_cte_src (id INT, val INT)",
        "INSERT INTO axm_pq_cte_src VALUES (1, 10), (2, 20)",
        &format!("COPY axm_pq_cte_src TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!(
        "WITH raw AS (SELECT * FROM READ_PARQUET('{path}')) SELECT * FROM raw ORDER BY id"
    ));
    let data = rows(r);
    assert_eq!(data.len(), 2);
}

#[test]
fn test_read_parquet_join_real_table() {
    let path = "/tmp/axm_pq_join.parquet";
    // Write the parquet file first.
    run_multi(&[
        "CREATE TABLE axm_pq_scores_src (user_id INT, score INT)",
        "INSERT INTO axm_pq_scores_src VALUES (1, 95), (2, 80)",
        &format!("COPY axm_pq_scores_src TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    // Now run the JOIN query in a session that also has the users table.
    let r = run_multi(&[
        "CREATE TABLE axm_pq_users (id INT, name TEXT)",
        "INSERT INTO axm_pq_users VALUES (1, 'alice'), (2, 'bob')",
        &format!(
            "SELECT u.name, s.score FROM axm_pq_users u JOIN READ_PARQUET('{path}') AS s ON u.id = s.user_id ORDER BY u.id"
        ),
    ]);
    let data = rows(r);
    assert_eq!(data.len(), 2);
    use axiomdb_types::Value;
    assert_eq!(data[0][0], Value::Text("alice".into()));
    assert_eq!(data[1][0], Value::Text("bob".into()));
}

#[test]
fn test_parquet_round_trip_int_text() {
    let path = "/tmp/axm_pq_roundtrip.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_rt (id INT, name TEXT, active BOOL)",
        "INSERT INTO axm_pq_rt VALUES (1, 'alice', TRUE), (2, 'bob', FALSE)",
        &format!("COPY axm_pq_rt TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT * FROM READ_PARQUET('{path}') ORDER BY id"));
    let data = rows(r);
    assert_eq!(data.len(), 2);
    use axiomdb_types::Value;
    assert_eq!(data[0][0], Value::Int(1));
    assert_eq!(data[0][1], Value::Text("alice".into()));
    assert_eq!(data[0][2], Value::Bool(true));
    assert_eq!(data[1][0], Value::Int(2));
    assert_eq!(data[1][1], Value::Text("bob".into()));
    assert_eq!(data[1][2], Value::Bool(false));
}

#[test]
fn test_parquet_null_values() {
    let path = "/tmp/axm_pq_null.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_null (id INT, val TEXT)",
        "INSERT INTO axm_pq_null VALUES (1, NULL), (2, 'present')",
        &format!("COPY axm_pq_null TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT * FROM READ_PARQUET('{path}') ORDER BY id"));
    let data = rows(r);
    assert_eq!(data.len(), 2);
    use axiomdb_types::Value;
    assert_eq!(data[0][1], Value::Null, "first row val should be Null");
    assert_eq!(data[1][1], Value::Text("present".into()));
}

#[test]
fn test_parquet_count_star() {
    let path = "/tmp/axm_pq_count.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_count (id INT)",
        "INSERT INTO axm_pq_count VALUES (1), (2), (3)",
        &format!("COPY axm_pq_count TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    let r = run(&format!("SELECT COUNT(*) FROM READ_PARQUET('{path}')"));
    let data = rows(r);
    assert_eq!(data.len(), 1);
    use axiomdb_types::Value;
    assert_eq!(data[0][0], Value::BigInt(3));
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_read_parquet_file_not_found() {
    let e = run_err("SELECT * FROM READ_PARQUET('/tmp/axm_nonexistent_zzz.parquet')");
    assert!(
        matches!(e, axiomdb_core::error::DbError::Io(_)),
        "expected Io error, got: {e:?}"
    );
}

#[test]
fn test_read_parquet_no_arg_parse_error() {
    let e = run_err("SELECT * FROM READ_PARQUET()");
    assert!(
        matches!(e, axiomdb_core::error::DbError::ParseError { .. }),
        "expected ParseError, got: {e:?}"
    );
}

#[test]
fn test_read_parquet_column_alias_mismatch() {
    let path = "/tmp/axm_pq_mismatch.parquet";
    run_multi(&[
        "CREATE TABLE axm_pq_mismatch (a INT, b INT)",
        "INSERT INTO axm_pq_mismatch VALUES (1, 2)",
        &format!("COPY axm_pq_mismatch TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    // Provide 3 aliases for a 2-column file — should error.
    let e = run_err(&format!(
        "SELECT * FROM READ_PARQUET('{path}') AS p(x, y, z)"
    ));
    assert!(
        matches!(e, axiomdb_core::error::DbError::InvalidValue { .. }),
        "expected InvalidValue, got: {e:?}"
    );
}

#[test]
fn test_copy_to_parquet_bad_path() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    for sql in &[
        "CREATE TABLE axm_pq_badpath (id INT)",
        "INSERT INTO axm_pq_badpath VALUES (1)",
    ] {
        common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    }
    let e = common::run_ctx(
        "COPY axm_pq_badpath TO '/nonexistent/dir/x.parquet' WITH (FORMAT PARQUET)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("expected Io error for bad path");
    assert!(
        matches!(e, axiomdb_core::error::DbError::Io(_)),
        "expected Io error, got: {e:?}"
    );
}
