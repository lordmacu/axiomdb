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

fn affected(r: QueryResult) -> u64 {
    match r {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

fn file_str(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

// ── basic write ───────────────────────────────────────────────────────────────

#[test]
fn test_into_outfile_literal_tab_default() {
    let path = "/tmp/axm_test_outfile_tab.tsv";
    let r = run(&format!("SELECT 1, 'hello' INTO OUTFILE '{path}'"));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content, "1\thello\n");
}

#[test]
fn test_into_outfile_custom_separator() {
    let path = "/tmp/axm_test_outfile_sep.csv";
    let r = run(&format!(
        "SELECT 1, 2, 3 INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
    ));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content, "1,2,3\n");
}

#[test]
fn test_into_outfile_enclosed_by() {
    let path = "/tmp/axm_test_outfile_enc.csv";
    let r = run(&format!(
        "SELECT 'alice', 'bob' INTO OUTFILE '{path}' FIELDS TERMINATED BY ',' ENCLOSED BY '\"'"
    ));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content, "\"alice\",\"bob\"\n");
}

#[test]
fn test_into_outfile_optionally_enclosed_by() {
    let path = "/tmp/axm_test_outfile_opt_enc.csv";
    let r = run(&format!(
        "SELECT 'x', 'y' INTO OUTFILE '{path}' FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"'"
    ));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content, "\"x\",\"y\"\n");
}

#[test]
fn test_into_outfile_enclosure_doubled_in_value() {
    let path = "/tmp/axm_test_outfile_double_enc.csv";
    // Use | as enclosure; value contains | which must be doubled inside the enclosure
    let sql =
        format!("SELECT 'a|b' INTO OUTFILE '{path}' FIELDS TERMINATED BY ',' ENCLOSED BY '|'");
    let r = run(&sql);
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    // |a||b|
    assert_eq!(content.trim(), "|a||b|");
}

#[test]
fn test_into_outfile_lines_terminated_crlf() {
    let path = "/tmp/axm_test_outfile_crlf.csv";
    let r = run(&format!(
        "SELECT 1 INTO OUTFILE '{path}' FIELDS TERMINATED BY ',' LINES TERMINATED BY '\\r\\n'"
    ));
    assert_eq!(affected(r), 1);
    let bytes = std::fs::read(path).unwrap();
    // last two bytes should be \r\n
    assert!(bytes.ends_with(b"\r\n"), "got bytes: {bytes:?}");
}

#[test]
fn test_into_outfile_null_value() {
    let path = "/tmp/axm_test_outfile_null.csv";
    let r = run(&format!("SELECT NULL INTO OUTFILE '{path}'"));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content.trim(), r"\N");
}

#[test]
fn test_into_outfile_bool_true_false() {
    let path = "/tmp/axm_test_outfile_bool.csv";
    let r = run(&format!(
        "SELECT TRUE, FALSE INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
    ));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content.trim(), "1,0");
}

#[test]
fn test_into_outfile_empty_result_empty_file() {
    let path = "/tmp/axm_test_outfile_empty.csv";
    // Use a condition that's always false to get 0 rows
    let stmts = &[
        "CREATE TABLE axm_outfile_empty (id INT)",
        &format!("SELECT id FROM axm_outfile_empty INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 0);
    let content = file_str(path);
    assert_eq!(content, "");
}

#[test]
fn test_into_outfile_multiple_rows() {
    let path = "/tmp/axm_test_outfile_multirow.csv";
    let stmts = &[
        "CREATE TABLE axm_outfile_mr (id INT, name VARCHAR(20))",
        "INSERT INTO axm_outfile_mr VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &format!(
            "SELECT id, name FROM axm_outfile_mr ORDER BY id INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
        ),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 3);
    let content = file_str(path);
    assert_eq!(content, "1,alice\n2,bob\n3,carol\n");
}

#[test]
fn test_into_outfile_where_filter() {
    let path = "/tmp/axm_test_outfile_where.csv";
    let stmts = &[
        "CREATE TABLE axm_outfile_where (id INT, val INT)",
        "INSERT INTO axm_outfile_where VALUES (1, 10), (2, 20), (3, 30)",
        &format!(
            "SELECT id FROM axm_outfile_where WHERE val > 15 ORDER BY id INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
        ),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 2);
    let content = file_str(path);
    assert_eq!(content, "2\n3\n");
}

#[test]
fn test_into_outfile_order_by() {
    let path = "/tmp/axm_test_outfile_order.csv";
    let stmts = &[
        "CREATE TABLE axm_outfile_order (id INT, score INT)",
        "INSERT INTO axm_outfile_order VALUES (3, 100), (1, 300), (2, 200)",
        &format!("SELECT score FROM axm_outfile_order ORDER BY score INTO OUTFILE '{path}'"),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 3);
    let content = file_str(path);
    assert_eq!(content, "100\n200\n300\n");
}

#[test]
fn test_into_outfile_limit() {
    let path = "/tmp/axm_test_outfile_limit.csv";
    let stmts = &[
        "CREATE TABLE axm_outfile_limit (id INT)",
        "INSERT INTO axm_outfile_limit VALUES (1), (2), (3), (4), (5)",
        &format!(
            "SELECT id FROM axm_outfile_limit ORDER BY id LIMIT 2 INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
        ),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 2);
    let content = file_str(path);
    assert_eq!(content, "1\n2\n");
}

#[test]
fn test_into_outfile_overwrites_existing() {
    let path = "/tmp/axm_test_outfile_overwrite.csv";
    // write initial content
    std::fs::write(path, "old content\n").unwrap();
    let r = run(&format!(
        "SELECT 'new' INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
    ));
    assert_eq!(affected(r), 1);
    let content = file_str(path);
    assert_eq!(content, "new\n");
}

#[test]
fn test_into_outfile_multi_char_sep_error() {
    let e = run_err("SELECT 1 INTO OUTFILE '/tmp/axm_err_sep.csv' FIELDS TERMINATED BY 'ab'");
    assert!(
        matches!(e, axiomdb_core::error::DbError::InvalidValue { .. }),
        "got: {e:?}"
    );
}

#[test]
fn test_into_outfile_no_outfile_unchanged() {
    // Normal SELECT still works as Rows, not Affected
    let r = run("SELECT 1, 2, 3");
    assert!(matches!(r, QueryResult::Rows { .. }));
}

#[test]
fn test_into_outfile_int_column() {
    let path = "/tmp/axm_test_outfile_int.csv";
    let stmts = &[
        "CREATE TABLE axm_outfile_int (n INT)",
        "INSERT INTO axm_outfile_int VALUES (42), (-7)",
        &format!(
            "SELECT n FROM axm_outfile_int ORDER BY n INTO OUTFILE '{path}' FIELDS TERMINATED BY ','"
        ),
    ];
    let r = run_multi(stmts);
    assert_eq!(affected(r), 2);
    let content = file_str(path);
    assert_eq!(content, "-7\n42\n");
}
