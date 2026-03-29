//! Integration tests for SHOW, DESCRIBE, TRUNCATE, and ALTER TABLE executor behavior.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

// ── 4.20: SHOW TABLES / SHOW COLUMNS / DESCRIBE ──────────────────────────────

#[test]
fn test_show_tables_empty() {
    let (mut storage, mut txn) = setup();
    // Fresh DB — catalog tables are system tables, user tables = 0
    let r = rows(run("SHOW TABLES", &mut storage, &mut txn));
    assert_eq!(r.len(), 0, "no user tables yet");
}

#[test]
fn test_show_tables_after_create() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE users (id INT)", &mut storage, &mut txn);
    run("CREATE TABLE orders (id INT)", &mut storage, &mut txn);

    let r = rows(run("SHOW TABLES", &mut storage, &mut txn));
    assert_eq!(r.len(), 2);
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            v => panic!("expected Text, got {v:?}"),
        })
        .collect();
    assert!(names.contains(&"users".to_string()));
    assert!(names.contains(&"orders".to_string()));
}

#[test]
fn test_show_tables_from_schema() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let r = rows(run("SHOW TABLES FROM public", &mut storage, &mut txn));
    assert_eq!(r.len(), 1);
}

#[test]
fn test_describe_basic() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run("DESCRIBE users", &mut storage, &mut txn));
    assert_eq!(r.len(), 2);

    // id column
    assert_eq!(r[0][0], Value::Text("id".into())); // Field
    assert_eq!(r[0][1], Value::Text("INT".into())); // Type
    assert_eq!(r[0][2], Value::Text("NO".into())); // Null

    // name column
    assert_eq!(r[1][0], Value::Text("name".into()));
    assert_eq!(r[1][1], Value::Text("TEXT".into()));
    assert_eq!(r[1][2], Value::Text("YES".into())); // nullable by default
}

#[test]
fn test_show_columns_same_as_describe() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (x INT)", &mut storage, &mut txn);

    let r1 = rows(run("DESCRIBE t", &mut storage, &mut txn));
    let r2 = rows(run("SHOW COLUMNS FROM t", &mut storage, &mut txn));
    assert_eq!(r1, r2);
}

#[test]
fn test_describe_auto_increment_extra() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val TEXT)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run("DESCRIBE t", &mut storage, &mut txn));
    // id column Extra should be "auto_increment"
    assert_eq!(r[0][5], Value::Text("auto_increment".into()));
    // val column Extra should be ""
    assert_eq!(r[1][5], Value::Text("".into()));
}

#[test]
fn test_describe_nonexistent_table() {
    let (mut storage, mut txn) = setup();
    let err = run_result("DESCRIBE nonexistent", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::TableNotFound { .. })));
}

// ── 4.21: TRUNCATE TABLE ─────────────────────────────────────────────────────

#[test]
fn test_truncate_deletes_all_rows() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2), (3)", &mut storage, &mut txn);

    run("TRUNCATE TABLE t", &mut storage, &mut txn);

    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(0));
}

#[test]
fn test_truncate_returns_zero_count() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2)", &mut storage, &mut txn);

    let r = run("TRUNCATE TABLE t", &mut storage, &mut txn);
    match r {
        QueryResult::Affected { count, .. } => assert_eq!(count, 0), // MySQL convention
        other => panic!("expected Affected, got {other:?}"),
    }
}

#[test]
fn test_truncate_resets_auto_increment() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (NULL, 1), (NULL, 2)",
        &mut storage,
        &mut txn,
    );

    run("TRUNCATE TABLE t", &mut storage, &mut txn);

    // After truncate, next insert should start from 1 again
    run("INSERT INTO t VALUES (NULL, 99)", &mut storage, &mut txn);
    let r = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn test_truncate_empty_table_noop() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    // Should not error
    let r = run("TRUNCATE TABLE t", &mut storage, &mut txn);
    assert!(matches!(r, QueryResult::Affected { count: 0, .. }));
}

#[test]
fn test_truncate_nonexistent_table() {
    let (mut storage, mut txn) = setup();
    let err = run_result("TRUNCATE TABLE nonexistent", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::TableNotFound { .. })));
}

// ── 4.22: ALTER TABLE ─────────────────────────────────────────────────────────

#[test]
fn test_alter_add_column_catalog() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run("ALTER TABLE t ADD COLUMN age INT", &mut storage, &mut txn);

    let r = rows(run("DESCRIBE t", &mut storage, &mut txn));
    assert_eq!(r.len(), 3);
    let col_names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(col_names, vec!["id", "name", "age"]);
}

#[test]
fn test_alter_add_column_existing_rows_get_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, 'Bob')", &mut storage, &mut txn);

    run("ALTER TABLE t ADD COLUMN score INT", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, name, score FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][2], Value::Null); // score = NULL for existing rows
    assert_eq!(r[1][2], Value::Null);
}

#[test]
fn test_alter_add_column_with_default() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2)", &mut storage, &mut txn);

    run(
        "ALTER TABLE t ADD COLUMN status TEXT DEFAULT 'active'",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id, status FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][1], Value::Text("active".into()));
    assert_eq!(r[1][1], Value::Text("active".into()));
}

#[test]
fn test_alter_add_column_duplicate_name() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);

    let err = run_result("ALTER TABLE t ADD COLUMN name TEXT", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::ColumnAlreadyExists { .. })));
}

#[test]
fn test_alter_add_column_then_insert_and_select() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("ALTER TABLE t ADD COLUMN val TEXT", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, 'hello')", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, val FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Null); // old row: val = NULL
    assert_eq!(r[1][1], Value::Text("hello".into())); // new row
}

#[test]
fn test_alter_drop_column_removes_from_catalog() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, name TEXT, age INT)",
        &mut storage,
        &mut txn,
    );
    run("ALTER TABLE t DROP COLUMN age", &mut storage, &mut txn);

    let r = rows(run("DESCRIBE t", &mut storage, &mut txn));
    assert_eq!(r.len(), 2);
    let col_names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(col_names, vec!["id", "name"]);
}

#[test]
fn test_alter_drop_column_data_remains_correct() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, name TEXT, age INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (1, 'Alice', 30)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (2, 'Bob', 25)",
        &mut storage,
        &mut txn,
    );

    run("ALTER TABLE t DROP COLUMN age", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, name FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("Alice".into()));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[1][1], Value::Text("Bob".into()));
}

#[test]
fn test_alter_drop_column_nonexistent() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    let err = run_result(
        "ALTER TABLE t DROP COLUMN nonexistent",
        &mut storage,
        &mut txn,
    );
    assert!(matches!(err, Err(DbError::ColumnNotFound { .. })));
}

#[test]
fn test_alter_drop_column_if_exists_nonexistent() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    // Should succeed silently
    run(
        "ALTER TABLE t DROP COLUMN IF EXISTS nonexistent",
        &mut storage,
        &mut txn,
    );
}

#[test]
fn test_alter_rename_column() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);

    run(
        "ALTER TABLE t RENAME COLUMN name TO full_name",
        &mut storage,
        &mut txn,
    );

    // DESCRIBE shows new name
    let r = rows(run("DESCRIBE t", &mut storage, &mut txn));
    let col_names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(col_names.contains(&"full_name".to_string()));
    assert!(!col_names.contains(&"name".to_string()));
}

#[test]
fn test_alter_rename_column_nonexistent() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    let err = run_result(
        "ALTER TABLE t RENAME COLUMN nonexistent TO x",
        &mut storage,
        &mut txn,
    );
    assert!(matches!(err, Err(DbError::ColumnNotFound { .. })));
}

#[test]
fn test_alter_rename_column_to_existing_name() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);

    let err = run_result(
        "ALTER TABLE t RENAME COLUMN id TO name",
        &mut storage,
        &mut txn,
    );
    assert!(matches!(err, Err(DbError::ColumnAlreadyExists { .. })));
}

#[test]
fn test_alter_rename_table() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE users (id INT)", &mut storage, &mut txn);
    run("INSERT INTO users VALUES (1)", &mut storage, &mut txn);

    run(
        "ALTER TABLE users RENAME TO customers",
        &mut storage,
        &mut txn,
    );

    // SHOW TABLES shows new name
    let r = rows(run("SHOW TABLES", &mut storage, &mut txn));
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(names.contains(&"customers".to_string()));
    assert!(!names.contains(&"users".to_string()));
}

#[test]
fn test_alter_rename_table_to_existing() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE a (id INT)", &mut storage, &mut txn);
    run("CREATE TABLE b (id INT)", &mut storage, &mut txn);

    let err = run_result("ALTER TABLE a RENAME TO b", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::TableAlreadyExists { .. })));
}

#[test]
fn test_alter_nonexistent_table() {
    let (mut storage, mut txn) = setup();

    let err = run_result(
        "ALTER TABLE nonexistent ADD COLUMN x INT",
        &mut storage,
        &mut txn,
    );
    assert!(matches!(err, Err(DbError::TableNotFound { .. })));
}
