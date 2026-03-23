//! Integration tests for the basic executor (Phase 4.5).
//!
//! All tests exercise the full pipeline:
//!   parse(sql) → analyze(stmt, storage, snap) → execute(stmt, storage, txn)
//!
//! Storage is `MemoryStorage` (no disk I/O). WAL is written to a temp directory.

use axiomdb_catalog::{CatalogBootstrap, CatalogReader};
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Runs a SQL string through the full pipeline and returns the `QueryResult`.
fn run(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> QueryResult {
    run_result(sql, storage, txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

/// Runs a SQL string and returns the result or error.
fn run_result(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

/// Creates a fresh `MemoryStorage` + `TxnManager` with catalog initialized.
fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.into_path().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

/// Extracts rows from a `QueryResult::Rows`.
fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Extracts affected count from a `QueryResult::Affected`.
fn affected_count(result: QueryResult) -> u64 {
    match result {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

// ── SELECT without FROM ───────────────────────────────────────────────────────

#[test]
fn test_select_literal_int() {
    let (mut storage, mut txn) = setup();
    let result = run("SELECT 1", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn test_select_expr_without_from() {
    let (mut storage, mut txn) = setup();
    let result = run("SELECT 1 + 2", &mut storage, &mut txn);
    let r = rows(result);
    assert_eq!(r[0][0], Value::Int(3));
}

#[test]
fn test_select_alias_without_from() {
    let (mut storage, mut txn) = setup();
    let result = run("SELECT 42 AS answer", &mut storage, &mut txn);
    if let QueryResult::Rows { columns, rows } = result {
        assert_eq!(columns[0].name, "answer");
        assert_eq!(rows[0][0], Value::Int(42));
    } else {
        panic!("expected Rows");
    }
}

// ── CREATE TABLE + basic DDL ──────────────────────────────────────────────────

#[test]
fn test_create_table() {
    let (mut storage, mut txn) = setup();
    let result = run(
        "CREATE TABLE users (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    assert_eq!(result, QueryResult::Empty);

    // Table must be visible in catalog.
    let snap = txn.snapshot();
    let reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_table("public", "users").unwrap().is_some());
}

#[test]
fn test_create_table_if_not_exists() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    // Second create with IF NOT EXISTS must not error.
    let result = run(
        "CREATE TABLE IF NOT EXISTS t (id INT)",
        &mut storage,
        &mut txn,
    );
    assert_eq!(result, QueryResult::Empty);
}

#[test]
fn test_create_table_duplicate_error() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result("CREATE TABLE t (id INT)", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TableAlreadyExists { .. }),
        "got {err:?}"
    );
}

#[test]
fn test_drop_table() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("DROP TABLE t", &mut storage, &mut txn);

    let err = run_result("SELECT * FROM t", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }), "got {err:?}");
}

#[test]
fn test_drop_table_if_exists() {
    let (mut storage, mut txn) = setup();
    let result = run("DROP TABLE IF EXISTS nonexistent", &mut storage, &mut txn);
    assert_eq!(result, QueryResult::Empty);
}

// ── INSERT ────────────────────────────────────────────────────────────────────

#[test]
fn test_insert_and_scan() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    let aff = affected_count(run(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 1);

    let r = rows(run("SELECT * FROM users", &mut storage, &mut txn));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("Alice".into()));
}

#[test]
fn test_insert_multi_row() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val TEXT)", &mut storage, &mut txn);
    let aff = affected_count(run(
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 3);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 3);
}

#[test]
fn test_insert_named_columns() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    // Insert with reversed column order.
    run(
        "INSERT INTO t (name, id) VALUES ('Charlie', 3)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run("SELECT id, name FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(3));
    assert_eq!(r[0][1], Value::Text("Charlie".into()));
}

#[test]
fn test_insert_missing_column_is_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t (id) VALUES (4)", &mut storage, &mut txn);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(4));
    assert_eq!(r[0][1], Value::Null);
}

#[test]
fn test_insert_unknown_column_error() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result(
        "INSERT INTO t (id, ghost) VALUES (1, 'x')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got {err:?}");
}

// ── SELECT with WHERE ─────────────────────────────────────────────────────────

#[test]
fn test_select_with_where() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT * FROM users WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Text("Bob".into()));
}

#[test]
fn test_select_where_no_match() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);

    let r = rows(run("SELECT * FROM t WHERE id = 99", &mut storage, &mut txn));
    assert_eq!(r.len(), 0);
}

#[test]
fn test_select_column_projection() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
    );

    let result = run("SELECT name FROM users", &mut storage, &mut txn);
    if let QueryResult::Rows { columns, rows } = result {
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "name");
        assert_eq!(rows[0][0], Value::Text("Alice".into()));
    } else {
        panic!("expected Rows");
    }
}

#[test]
fn test_select_column_meta_wildcard() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL, val TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'x')", &mut storage, &mut txn);

    if let QueryResult::Rows { columns, .. } = run("SELECT * FROM t", &mut storage, &mut txn) {
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert!(!columns[0].nullable); // NOT NULL
        assert_eq!(columns[1].name, "val");
        assert!(columns[1].nullable);
    } else {
        panic!("expected Rows");
    }
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

#[test]
fn test_update_with_where() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        &mut storage,
        &mut txn,
    );

    let aff = affected_count(run(
        "UPDATE users SET name = 'Robert' WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 1);

    let r = rows(run(
        "SELECT name FROM users WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("Robert".into()));

    // Other rows unchanged.
    let all = rows(run("SELECT * FROM users", &mut storage, &mut txn));
    assert_eq!(all.len(), 3);
}

#[test]
fn test_update_no_match() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);

    let aff = affected_count(run(
        "UPDATE t SET id = 99 WHERE id = 999",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 0);
}

#[test]
fn test_update_unknown_column_error() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);

    let err = run_result(
        "UPDATE t SET ghost = 1 WHERE id = 1",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got {err:?}");
}

// ── DELETE ────────────────────────────────────────────────────────────────────

#[test]
fn test_delete_with_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2), (3)", &mut storage, &mut txn);

    let aff = affected_count(run("DELETE FROM t WHERE id = 2", &mut storage, &mut txn));
    assert_eq!(aff, 1);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 2);
    let ids: Vec<&Value> = r.iter().map(|row| &row[0]).collect();
    assert!(!ids.contains(&&Value::Int(2)));
}

#[test]
fn test_delete_without_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2), (3)", &mut storage, &mut txn);

    let aff = affected_count(run("DELETE FROM t", &mut storage, &mut txn));
    assert_eq!(aff, 3);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 0);
}

// ── Transaction control ───────────────────────────────────────────────────────

#[test]
fn test_explicit_txn_begin_commit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (42)", &mut storage, &mut txn);
    run("COMMIT", &mut storage, &mut txn);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_explicit_txn_rollback() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (42)", &mut storage, &mut txn);
    run("ROLLBACK", &mut storage, &mut txn);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 0, "rolled-back insert must not be visible");
}

#[test]
fn test_read_own_writes_in_txn() {
    // INSERT then SELECT in the same explicit transaction must see the row.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    run("BEGIN", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (7)", &mut storage, &mut txn);
    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    run("COMMIT", &mut storage, &mut txn);

    assert_eq!(
        r.len(),
        1,
        "read-your-own-writes must work within a transaction"
    );
    assert_eq!(r[0][0], Value::Int(7));
}

#[test]
fn test_autocommit_each_statement_independent() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    // Two autocommit inserts.
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2)", &mut storage, &mut txn);

    let r = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 2);
}

// ── Error paths ───────────────────────────────────────────────────────────────

#[test]
fn test_select_nonexistent_table() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT * FROM ghost", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }), "got {err:?}");
}

#[test]
fn test_order_by_returns_not_implemented() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result("SELECT * FROM t ORDER BY id", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");
}

#[test]
fn test_limit_returns_not_implemented() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result("SELECT * FROM t LIMIT 1", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");
}

// ── Full round-trip ───────────────────────────────────────────────────────────

#[test]
fn test_full_crud_roundtrip() {
    let (mut storage, mut txn) = setup();

    // CREATE
    run(
        "CREATE TABLE employees (id INT NOT NULL, name TEXT, salary INT)",
        &mut storage,
        &mut txn,
    );

    // INSERT 5 rows
    for i in 1..=5i32 {
        run(
            &format!("INSERT INTO employees VALUES ({i}, 'emp{i}', {})", i * 1000),
            &mut storage,
            &mut txn,
        );
    }

    // SELECT — 5 rows
    let r = rows(run("SELECT * FROM employees", &mut storage, &mut txn));
    assert_eq!(r.len(), 5);

    // UPDATE 2 rows
    let aff = affected_count(run(
        "UPDATE employees SET salary = 9999 WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 1);
    let aff2 = affected_count(run(
        "UPDATE employees SET salary = 9999 WHERE id = 4",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff2, 1);

    // SELECT — verify updates
    let updated = rows(run(
        "SELECT salary FROM employees WHERE id = 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(updated[0][0], Value::Int(9999));

    // DELETE 1 row
    let del = affected_count(run(
        "DELETE FROM employees WHERE id = 3",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(del, 1);

    // SELECT — 4 remaining
    let final_rows = rows(run("SELECT * FROM employees", &mut storage, &mut txn));
    assert_eq!(final_rows.len(), 4);

    // DROP TABLE
    run("DROP TABLE employees", &mut storage, &mut txn);

    // SELECT → TableNotFound
    let err = run_result("SELECT * FROM employees", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }), "got {err:?}");
}

// ── JOIN tests ────────────────────────────────────────────────────────────────

fn setup_join_tables(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE users (id INT NOT NULL, name TEXT)",
        storage,
        txn,
    );
    run(
        "CREATE TABLE orders (id INT NOT NULL, user_id INT, total INT)",
        storage,
        txn,
    );
    run("INSERT INTO users VALUES (1, 'Alice')", storage, txn);
    run("INSERT INTO users VALUES (2, 'Bob')", storage, txn);
    run("INSERT INTO users VALUES (3, 'Carol')", storage, txn);
    run("INSERT INTO orders VALUES (10, 1, 100)", storage, txn);
    run("INSERT INTO orders VALUES (11, 1, 200)", storage, txn);
    run("INSERT INTO orders VALUES (12, 2, 50)", storage, txn);
    // order 13 has no matching user
    run("INSERT INTO orders VALUES (13, 99, 300)", storage, txn);
}

#[test]
fn test_inner_join_basic() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id",
        &mut storage,
        &mut txn,
    ));
    // 3 matching pairs: (Alice,100), (Alice,200), (Bob,50)
    assert_eq!(r.len(), 3);
    let names: Vec<&Value> = r.iter().map(|row| &row[0]).collect();
    assert_eq!(
        names
            .iter()
            .filter(|&&v| v == &Value::Text("Alice".into()))
            .count(),
        2
    );
    assert_eq!(
        names
            .iter()
            .filter(|&&v| v == &Value::Text("Bob".into()))
            .count(),
        1
    );
}

#[test]
fn test_inner_join_where_filter() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE o.total > 100",
        &mut storage,
        &mut txn,
    ));
    // Only (Alice, 200)
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Int(200));
}

#[test]
fn test_inner_join_select_star() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let result = run(
        "SELECT * FROM users u JOIN orders o ON u.id = o.user_id",
        &mut storage,
        &mut txn,
    );
    if let QueryResult::Rows { columns, rows } = result {
        // users has 2 cols, orders has 3 cols → 5 total
        assert_eq!(columns.len(), 5);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[2].name, "id"); // orders.id
        assert_eq!(rows.len(), 3);
    } else {
        panic!("expected Rows");
    }
}

#[test]
fn test_left_join_unmatched_left() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT u.name, o.total FROM users u LEFT JOIN orders o ON u.id = o.user_id",
        &mut storage,
        &mut txn,
    ));
    // 3 (Alice) + 1 (Bob) + 1 (Carol with NULL) = 4 output rows
    assert_eq!(r.len(), 4);
    // Carol has no orders → total should be NULL
    let carol_row = r
        .iter()
        .find(|row| row[0] == Value::Text("Carol".into()))
        .unwrap();
    assert_eq!(carol_row[1], Value::Null, "Carol's total must be NULL");
}

#[test]
fn test_left_join_column_meta_nullable() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let result = run(
        "SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id",
        &mut storage,
        &mut txn,
    );
    if let QueryResult::Rows { columns, .. } = result {
        // users columns: nullable as per catalog (NOT NULL for id → false)
        assert!(!columns[0].nullable, "users.id is NOT NULL");
        // orders columns: must be nullable=true because of LEFT JOIN
        assert!(
            columns[2].nullable,
            "orders.id must be nullable after LEFT JOIN"
        );
        assert!(
            columns[3].nullable,
            "orders.user_id must be nullable after LEFT JOIN"
        );
    } else {
        panic!("expected Rows");
    }
}

#[test]
fn test_right_join_unmatched_right() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT u.name, o.total FROM users u RIGHT JOIN orders o ON u.id = o.user_id",
        &mut storage,
        &mut txn,
    ));
    // 3 matched + order 13 (user_id=99, no matching user) → 4 rows
    assert_eq!(r.len(), 4);
    // The unmatched order has NULL for u.name
    let unmatched = r.iter().find(|row| row[0] == Value::Null).unwrap();
    assert_eq!(
        unmatched[1],
        Value::Int(300),
        "unmatched order total must be 300"
    );
}

#[test]
fn test_cross_join() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE a (x INT)", &mut storage, &mut txn);
    run("CREATE TABLE b (y INT)", &mut storage, &mut txn);
    run("INSERT INTO a VALUES (1), (2)", &mut storage, &mut txn);
    run(
        "INSERT INTO b VALUES (10), (20), (30)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT a.x, b.y FROM a CROSS JOIN b",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 6, "CROSS JOIN 2×3 should produce 6 rows");
}

#[test]
fn test_three_table_join() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE orders (id INT, user_id INT, product_id INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE products (id INT, label TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products VALUES (100, 'Widget')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO orders VALUES (10, 1, 100)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT u.name, p.label FROM users u JOIN orders o ON u.id = o.user_id JOIN products p ON o.product_id = p.id",
        &mut storage, &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("Alice".into()));
    assert_eq!(r[0][1], Value::Text("Widget".into()));
}

#[test]
fn test_full_outer_join_not_implemented() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result(
        "SELECT * FROM t t1 FULL OUTER JOIN t t2 ON t1.id = t2.id",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::NotImplemented { .. }), "got {err:?}");
}
