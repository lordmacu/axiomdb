/// Integration tests for 5.9e / 5.9f / 5.9g:
/// - SHOW WARNINGS / SHOW ERRORS (5.9e)
/// - SHOW FULL TABLES
/// - SHOW FULL COLUMNS
/// - SHOW TABLE STATUS
/// - SHOW ENGINES
/// - SHOW CHARSET
/// - SHOW COLLATION
mod common;
use axiomdb_types::Value;
use common::{run, setup};

fn full_result(result: axiomdb_sql::QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        axiomdb_sql::QueryResult::Rows { columns, rows } => {
            (columns.into_iter().map(|c| c.name).collect(), rows)
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── 5.9e: SHOW WARNINGS / SHOW ERRORS ────────────────────────────────────────

#[test]
fn show_warnings_empty_returns_three_columns() {
    let (mut storage, mut txn) = setup();
    let result = run("SHOW WARNINGS", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0], "Level");
    assert_eq!(cols[1], "Code");
    assert_eq!(cols[2], "Message");
    assert_eq!(data.len(), 0, "fresh session has no warnings");
}

#[test]
fn show_errors_empty_returns_three_columns() {
    let (mut storage, mut txn) = setup();
    let result = run("SHOW ERRORS", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0], "Level");
    assert_eq!(cols[1], "Code");
    assert_eq!(cols[2], "Message");
    assert_eq!(data.len(), 0, "fresh session has no errors");
}

#[test]
fn show_warnings_limit_parses() {
    let (mut storage, mut txn) = setup();
    // SHOW WARNINGS LIMIT 1 must parse and return a result set (empty on fresh session).
    let result = run("SHOW WARNINGS LIMIT 1", &mut storage, &mut txn);
    let (cols, _) = full_result(result);
    assert_eq!(cols.len(), 3);
}

// ── 5.9f: SHOW FULL TABLES ───────────────────────────────────────────────────

#[test]
fn show_tables_basic_has_one_column() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t1 (id INT)", &mut storage, &mut txn);
    let result = run("SHOW TABLES", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 1, "SHOW TABLES should have 1 column");
    assert!(cols[0].starts_with("Tables_in_"));
    assert_eq!(data.len(), 1);
    assert_eq!(data[0][0], Value::Text("t1".into()));
}

#[test]
fn show_full_tables_has_table_type_column() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t1 (id INT)", &mut storage, &mut txn);
    run("CREATE TABLE t2 (id INT)", &mut storage, &mut txn);
    let result = run("SHOW FULL TABLES", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 2, "SHOW FULL TABLES should have 2 columns");
    assert!(cols[0].starts_with("Tables_in_"));
    assert_eq!(cols[1], "Table_type");
    assert_eq!(data.len(), 2);
    for row in &data {
        assert_eq!(row[1], Value::Text("BASE TABLE".into()));
    }
}

// ── 5.9f: SHOW FULL COLUMNS ──────────────────────────────────────────────────

#[test]
fn show_columns_basic_has_six_columns() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE emp (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    let result = run("SHOW COLUMNS FROM emp", &mut storage, &mut txn);
    let (cols, _) = full_result(result);
    assert_eq!(cols.len(), 6);
    assert_eq!(cols[0], "Field");
    assert_eq!(cols[1], "Type");
    assert_eq!(cols[2], "Null");
}

#[test]
fn show_full_columns_has_extra_columns() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE emp (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    let result = run("SHOW FULL COLUMNS FROM emp", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 9, "SHOW FULL COLUMNS should have 9 columns");
    assert_eq!(cols[6], "Collation");
    assert_eq!(cols[7], "Privileges");
    assert_eq!(cols[8], "Comment");
    // TEXT column should have utf8mb4_general_ci collation
    let name_row = data
        .iter()
        .find(|r| r[0] == Value::Text("name".into()))
        .unwrap();
    assert_eq!(name_row[6], Value::Text("utf8mb4_general_ci".into()));
    // INT column should have NULL collation
    let id_row = data
        .iter()
        .find(|r| r[0] == Value::Text("id".into()))
        .unwrap();
    assert_eq!(id_row[6], Value::Null);
}

// ── 5.9f: SHOW TABLE STATUS ───────────────────────────────────────────────────

#[test]
fn show_table_status_returns_18_columns() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE orders (id INT)", &mut storage, &mut txn);
    let result = run("SHOW TABLE STATUS", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols.len(), 18, "SHOW TABLE STATUS must return 18 columns");
    assert_eq!(cols[0], "Name");
    assert_eq!(cols[1], "Engine");
    assert_eq!(cols[14], "Collation");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0][0], Value::Text("orders".into()));
    assert_eq!(data[0][1], Value::Text("InnoDB".into()));
    assert_eq!(data[0][14], Value::Text("utf8mb4_general_ci".into()));
}

#[test]
fn show_table_status_from_schema() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t1 (id INT)", &mut storage, &mut txn);
    // SHOW TABLE STATUS FROM public should work.
    let result = run("SHOW TABLE STATUS FROM public", &mut storage, &mut txn);
    let (_, data) = full_result(result);
    assert!(!data.is_empty());
}

#[test]
fn show_table_status_like_filter() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE orders (id INT)", &mut storage, &mut txn);
    run("CREATE TABLE order_items (id INT)", &mut storage, &mut txn);
    run("CREATE TABLE products (id INT)", &mut storage, &mut txn);
    let result = run("SHOW TABLE STATUS LIKE 'order%'", &mut storage, &mut txn);
    let (_, data) = full_result(result);
    assert_eq!(data.len(), 2, "LIKE 'order%' should match 2 tables");
    for row in &data {
        let name = match &row[0] {
            Value::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert!(name.starts_with("order"), "name {name} should match order%");
    }
}

// ── 5.9g: SHOW ENGINES ────────────────────────────────────────────────────────

#[test]
fn show_engines_returns_innodb_row() {
    let (mut storage, mut txn) = setup();
    let result = run("SHOW ENGINES", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols[0], "Engine");
    assert_eq!(cols[1], "Support");
    assert_eq!(cols[3], "Transactions");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0][0], Value::Text("InnoDB".into()));
    assert_eq!(data[0][1], Value::Text("DEFAULT".into()));
    assert_eq!(data[0][3], Value::Text("YES".into())); // Transactions
}

// ── 5.9g: SHOW CHARSET ────────────────────────────────────────────────────────

#[test]
fn show_charset_has_utf8mb4() {
    let (mut storage, mut txn) = setup();
    let result = run("SHOW CHARSET", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols[0], "Charset");
    let has_utf8mb4 = data.iter().any(|r| r[0] == Value::Text("utf8mb4".into()));
    assert!(has_utf8mb4, "SHOW CHARSET must include utf8mb4");
}

// ── 5.9g: SHOW COLLATION ─────────────────────────────────────────────────────

#[test]
fn show_collation_has_general_ci() {
    let (mut storage, mut txn) = setup();
    let result = run("SHOW COLLATION", &mut storage, &mut txn);
    let (cols, data) = full_result(result);
    assert_eq!(cols[0], "Collation");
    let has_utf8mb4_ci = data
        .iter()
        .any(|r| r[0] == Value::Text("utf8mb4_general_ci".into()));
    assert!(
        has_utf8mb4_ci,
        "SHOW COLLATION must include utf8mb4_general_ci"
    );
}
