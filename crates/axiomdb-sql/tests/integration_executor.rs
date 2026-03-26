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
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
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
fn test_order_by_works() {
    // ORDER BY is now implemented — this test verifies it works, not that it errors.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (3), (1), (2)", &mut storage, &mut txn);
    let r = rows(run(
        "SELECT * FROM t ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn test_limit_works() {
    // LIMIT is now implemented.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
        &mut storage,
        &mut txn,
    );
    let r = rows(run(
        "SELECT * FROM t ORDER BY id ASC LIMIT 3",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
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

// ── GROUP BY / Aggregate tests ────────────────────────────────────────────────

fn setup_employees(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE employees (id INT NOT NULL, name TEXT, dept TEXT, salary INT)",
        storage,
        txn,
    );
    run(
        "INSERT INTO employees VALUES (1, 'Alice', 'eng', 90000)",
        storage,
        txn,
    );
    run(
        "INSERT INTO employees VALUES (2, 'Bob', 'eng', 80000)",
        storage,
        txn,
    );
    run(
        "INSERT INTO employees VALUES (3, 'Carol', 'eng', 70000)",
        storage,
        txn,
    );
    run(
        "INSERT INTO employees VALUES (4, 'Dave', 'sales', 60000)",
        storage,
        txn,
    );
    run(
        "INSERT INTO employees VALUES (5, 'Eve', 'sales', 55000)",
        storage,
        txn,
    );
}

#[test]
fn test_group_by_count_star() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let result = run(
        "SELECT dept, COUNT(*) FROM employees GROUP BY dept",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r.len(), 2); // eng and sales
                            // eng → 3, sales → 2
    let mut counts: Vec<(String, i64)> = r
        .iter()
        .map(|row| {
            let dept = match &row[0] {
                Value::Text(s) => s.clone(),
                _ => panic!("expected Text"),
            };
            let cnt = match row[1] {
                Value::BigInt(n) => n,
                _ => panic!("expected BigInt"),
            };
            (dept, cnt)
        })
        .collect();
    counts.sort_by_key(|(d, _)| d.clone());
    assert_eq!(counts, vec![("eng".into(), 3), ("sales".into(), 2)]);
}

#[test]
fn test_group_by_sum_and_avg() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, SUM(salary), AVG(salary) FROM employees GROUP BY dept",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    // Find eng row
    let eng = r
        .iter()
        .find(|row| row[0] == Value::Text("eng".into()))
        .unwrap();
    assert_eq!(eng[1], Value::Int(240000), "eng sum = 90000+80000+70000");
    if let Value::Real(avg) = eng[2] {
        assert!((avg - 80000.0).abs() < 1.0, "eng avg ≈ 80000");
    } else {
        panic!("expected Real for avg");
    }
}

#[test]
fn test_group_by_min_max() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, MIN(salary), MAX(salary) FROM employees GROUP BY dept",
        &mut storage,
        &mut txn,
    ));
    let eng = r
        .iter()
        .find(|row| row[0] == Value::Text("eng".into()))
        .unwrap();
    assert_eq!(eng[1], Value::Int(70000)); // MIN
    assert_eq!(eng[2], Value::Int(90000)); // MAX
}

#[test]
fn test_group_by_null_key_grouped() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, dept TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'eng')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, NULL)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (3, NULL)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, COUNT(*) FROM t GROUP BY dept",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2); // 'eng' group + NULL group
    let null_group = r.iter().find(|row| row[0] == Value::Null).unwrap();
    assert_eq!(null_group[1], Value::BigInt(2)); // 2 NULLs → 1 group of 2
}

#[test]
fn test_ungrouped_count_star() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT COUNT(*) FROM employees",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(5));
}

#[test]
fn test_ungrouped_count_empty_table() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);

    // Empty table → COUNT(*) returns 1 row with (0), not 0 rows.
    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(
        r.len(),
        1,
        "empty table must still return 1 row for COUNT(*)"
    );
    assert_eq!(r[0][0], Value::BigInt(0));
}

#[test]
fn test_count_col_skips_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, mgr INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 100)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, NULL)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (3, NULL)", &mut storage, &mut txn);

    let r = rows(run("SELECT COUNT(mgr) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(1)); // only 1 non-NULL manager
}

#[test]
fn test_sum_all_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, NULL)", &mut storage, &mut txn);

    let r = rows(run("SELECT SUM(val) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Null, "SUM of all NULLs must be NULL");
}

#[test]
fn test_having_filter() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, COUNT(*) FROM employees GROUP BY dept HAVING COUNT(*) > 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1); // only eng has 3 > 2
    assert_eq!(r[0][0], Value::Text("eng".into()));
}

#[test]
fn test_having_with_sum() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, SUM(salary) FROM employees GROUP BY dept HAVING SUM(salary) > 200000",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1); // only eng: 240000 > 200000
    assert_eq!(r[0][0], Value::Text("eng".into()));
}

#[test]
fn test_select_star_with_group_by_error() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    let err = run_result("SELECT * FROM t GROUP BY id", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::TypeMismatch { .. }), "got {err:?}");
}

// ── ORDER BY / LIMIT tests ────────────────────────────────────────────────────

fn setup_order_table(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE scores (id INT, name TEXT, score INT)",
        storage,
        txn,
    );
    run("INSERT INTO scores VALUES (3, 'Carol', 85)", storage, txn);
    run("INSERT INTO scores VALUES (1, 'Alice', 92)", storage, txn);
    run("INSERT INTO scores VALUES (4, 'Dave', NULL)", storage, txn);
    run("INSERT INTO scores VALUES (2, 'Bob', 78)", storage, txn);
    run("INSERT INTO scores VALUES (5, 'Eve', NULL)", storage, txn);
}

#[test]
fn test_order_by_asc() {
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT id FROM scores ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    let ids: Vec<&Value> = r.iter().map(|row| &row[0]).collect();
    assert_eq!(
        ids,
        vec![
            &Value::Int(1),
            &Value::Int(2),
            &Value::Int(3),
            &Value::Int(4),
            &Value::Int(5)
        ]
    );
}

#[test]
fn test_order_by_desc() {
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT id FROM scores ORDER BY id DESC",
        &mut storage,
        &mut txn,
    ));
    let ids: Vec<&Value> = r.iter().map(|row| &row[0]).collect();
    assert_eq!(
        ids,
        vec![
            &Value::Int(5),
            &Value::Int(4),
            &Value::Int(3),
            &Value::Int(2),
            &Value::Int(1)
        ]
    );
}

#[test]
fn test_order_by_text() {
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT name FROM scores ORDER BY name ASC",
        &mut storage,
        &mut txn,
    ));
    let names: Vec<&Value> = r.iter().map(|row| &row[0]).collect();
    // Alphabetical: Alice, Bob, Carol, Dave, Eve
    assert_eq!(names[0], &Value::Text("Alice".into()));
    assert_eq!(names[4], &Value::Text("Eve".into()));
}

#[test]
fn test_order_by_nulls_asc_default() {
    // ASC default: NULLs LAST
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT score FROM scores ORDER BY score ASC",
        &mut storage,
        &mut txn,
    ));
    // NULLs should be at the END
    let last = r.last().unwrap();
    assert_eq!(last[0], Value::Null, "ASC default: NULLs must be LAST");
    let second_last = &r[r.len() - 2];
    assert_eq!(second_last[0], Value::Null);
    // First values should be non-NULL
    assert_ne!(r[0][0], Value::Null);
}

#[test]
fn test_order_by_nulls_desc_default() {
    // DESC default: NULLs FIRST
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT score FROM scores ORDER BY score DESC",
        &mut storage,
        &mut txn,
    ));
    // NULLs should be at the START
    assert_eq!(r[0][0], Value::Null, "DESC default: NULLs must be FIRST");
    assert_eq!(r[1][0], Value::Null);
}

#[test]
fn test_order_by_nulls_first_explicit() {
    // ASC NULLS FIRST: NULLs before non-NULLs
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT score FROM scores ORDER BY score ASC NULLS FIRST",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[1][0], Value::Null);
    assert_ne!(r[2][0], Value::Null);
}

#[test]
fn test_order_by_nulls_last_explicit() {
    // DESC NULLS LAST: NULLs after non-NULLs
    let (mut storage, mut txn) = setup();
    setup_order_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT score FROM scores ORDER BY score DESC NULLS LAST",
        &mut storage,
        &mut txn,
    ));
    assert_ne!(r[0][0], Value::Null);
    assert_eq!(r[r.len() - 1][0], Value::Null);
    assert_eq!(r[r.len() - 2][0], Value::Null);
}

#[test]
fn test_multi_column_order_by() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (dept TEXT, salary INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('eng', 90000)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('eng', 70000)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('sales', 80000)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT dept, salary FROM t ORDER BY dept ASC, salary DESC",
        &mut storage,
        &mut txn,
    ));
    // eng rows first (ASC), within eng: 90000 before 70000 (DESC)
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Int(90000));
    assert_eq!(r[1][0], Value::Text("eng".into()));
    assert_eq!(r[1][1], Value::Int(70000));
    assert_eq!(r[2][0], Value::Text("sales".into()));
}

#[test]
fn test_limit_only() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    for i in 1..=10i32 {
        run(
            &format!("INSERT INTO t VALUES ({i})"),
            &mut storage,
            &mut txn,
        );
    }
    let r = rows(run(
        "SELECT * FROM t ORDER BY id ASC LIMIT 3",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn test_limit_offset() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    for i in 1..=10i32 {
        run(
            &format!("INSERT INTO t VALUES ({i})"),
            &mut storage,
            &mut txn,
        );
    }
    let r = rows(run(
        "SELECT * FROM t ORDER BY id ASC LIMIT 3 OFFSET 5",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(6));
    assert_eq!(r[2][0], Value::Int(8));
}

#[test]
fn test_limit_zero() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2), (3)", &mut storage, &mut txn);
    let r = rows(run("SELECT * FROM t LIMIT 0", &mut storage, &mut txn));
    assert_eq!(r.len(), 0);
}

#[test]
fn test_offset_beyond_end() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2)", &mut storage, &mut txn);
    // Use LIMIT + OFFSET (parser requires LIMIT before OFFSET).
    let r = rows(run(
        "SELECT * FROM t ORDER BY id LIMIT 100 OFFSET 100",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 0);
}

#[test]
fn test_order_by_with_group_by() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (dept TEXT, val INT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES ('b', 1)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES ('a', 2)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES ('c', 3)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT dept, COUNT(*) FROM t GROUP BY dept ORDER BY dept ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("a".into()));
    assert_eq!(r[1][0], Value::Text("b".into()));
    assert_eq!(r[2][0], Value::Text("c".into()));
}

// ── DISTINCT tests ────────────────────────────────────────────────────────────

#[test]
fn test_distinct_single_column() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (val INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1), (2), (1), (3), (2)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT DISTINCT val FROM t ORDER BY val ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn test_distinct_multi_column() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (dept TEXT, role TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('eng', 'dev')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('eng', 'dev')",
        &mut storage,
        &mut txn,
    ); // duplicate
    run(
        "INSERT INTO t VALUES ('eng', 'mgr')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('sales', 'dev')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT DISTINCT dept, role FROM t ORDER BY dept ASC, role ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3); // (eng,dev), (eng,mgr), (sales,dev)
}

#[test]
fn test_distinct_null_dedup() {
    // Two NULLs → only one NULL row in result.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (val INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (NULL), (1), (NULL), (2)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT DISTINCT val FROM t ORDER BY val ASC NULLS LAST",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3); // (1,), (2,), (NULL,)
    assert_eq!(r[2][0], Value::Null);
    assert_ne!(r[0][0], Value::Null);
}

#[test]
fn test_distinct_star() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (a INT, b TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'x')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'x')", &mut storage, &mut txn); // duplicate
    run("INSERT INTO t VALUES (2, 'y')", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT DISTINCT * FROM t ORDER BY a ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
}

#[test]
fn test_distinct_empty_table() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (val INT)", &mut storage, &mut txn);
    let r = rows(run("SELECT DISTINCT val FROM t", &mut storage, &mut txn));
    assert_eq!(r.len(), 0);
}

#[test]
fn test_distinct_no_duplicates() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2), (3)", &mut storage, &mut txn);
    let r = rows(run(
        "SELECT DISTINCT id FROM t ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
}

#[test]
fn test_distinct_with_where() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (dept TEXT, val INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('eng', 1), ('eng', 2), ('sales', 3)",
        &mut storage,
        &mut txn,
    );

    // WHERE filters to 'eng' rows, DISTINCT deduplicates dept → 1 unique dept
    let r = rows(run(
        "SELECT DISTINCT dept FROM t WHERE dept = 'eng'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("eng".into()));
}

#[test]
fn test_distinct_with_limit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (val INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (3),(1),(3),(2),(1)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT DISTINCT val FROM t ORDER BY val ASC LIMIT 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
}

#[test]
fn test_distinct_scalar() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT DISTINCT 1", &mut storage, &mut txn));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

// ── CASE WHEN tests ───────────────────────────────────────────────────────────

fn setup_case_table(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run("CREATE TABLE t (id INT, val INT, label TEXT)", storage, txn);
    run("INSERT INTO t VALUES (1, 120000, 'Alice')", storage, txn);
    run("INSERT INTO t VALUES (2, 60000, 'Bob')", storage, txn);
    run("INSERT INTO t VALUES (3, 30000, 'Carol')", storage, txn);
}

#[test]
fn test_case_when_searched_basic() {
    let (mut storage, mut txn) = setup();
    setup_case_table(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, CASE WHEN val > 100000 THEN 'senior' WHEN val > 50000 THEN 'mid' ELSE 'junior' END FROM t ORDER BY id ASC",
        &mut storage, &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][1], Value::Text("senior".into())); // 120000 > 100000
    assert_eq!(r[1][1], Value::Text("mid".into())); // 60000 > 50000
    assert_eq!(r[2][1], Value::Text("junior".into())); // 30000 → ELSE
}

#[test]
fn test_case_when_no_else_returns_null() {
    let (mut storage, mut txn) = setup();
    setup_case_table(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT CASE WHEN id = 99 THEN 'found' END FROM t ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    for row in &r {
        assert_eq!(row[0], Value::Null, "no match + no ELSE → NULL");
    }
}

#[test]
fn test_case_when_null_condition_not_truthy() {
    // CASE WHEN NULL THEN 1 ELSE 0 END → 0 (NULL is UNKNOWN, not truthy)
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT CASE WHEN NULL THEN 1 ELSE 0 END",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(0));
}

#[test]
fn test_case_simple_form() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, status TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'active')", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (2, 'inactive')",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (3, 'other')", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, CASE status WHEN 'active' THEN 1 WHEN 'inactive' THEN 0 ELSE -1 END FROM t ORDER BY id ASC",
        &mut storage, &mut txn,
    ));
    assert_eq!(r[0][1], Value::Int(1));
    assert_eq!(r[1][1], Value::Int(0));
    assert_eq!(r[2][1], Value::Int(-1));
}

#[test]
fn test_case_simple_null_no_match() {
    // CASE NULL WHEN NULL THEN 1 END → NULL (NULL ≠ NULL in simple CASE)
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT CASE NULL WHEN NULL THEN 1 END",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn test_case_then_null() {
    // THEN can produce NULL
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT CASE WHEN 1 = 1 THEN NULL ELSE 1 END",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn test_case_in_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, type TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, 'x')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE CASE type WHEN 'x' THEN 1 ELSE 0 END = 1 ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}

#[test]
fn test_case_in_order_by() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, dept TEXT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1, 'sales')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, 'eng')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (3, 'hr')", &mut storage, &mut txn);

    // Order: eng=1 first, sales=2 second, others=3 last
    let r = rows(run(
        "SELECT id FROM t ORDER BY CASE dept WHEN 'eng' THEN 1 WHEN 'sales' THEN 2 ELSE 3 END ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(2)); // eng
    assert_eq!(r[1][0], Value::Int(1)); // sales
    assert_eq!(r[2][0], Value::Int(3)); // hr
}

#[test]
fn test_case_nested() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT CASE WHEN 1 > 0 THEN CASE WHEN 2 > 1 THEN 'both' ELSE 'a_only' END ELSE 'none' END",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("both".into()));
}

#[test]
fn test_case_no_when_parse_error() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT CASE END", &mut storage, &mut txn).unwrap_err();
    assert!(matches!(err, DbError::ParseError { .. }), "got {err:?}");
}

// ── INSERT ... SELECT tests ───────────────────────────────────────────────────

#[test]
fn test_insert_select_copy_all() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE src (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO src VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let aff = affected_count(run(
        "INSERT INTO dst SELECT * FROM src",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 3);

    let r = rows(run(
        "SELECT * FROM dst ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][1], Value::Text("Alice".into()));
}

#[test]
fn test_insert_select_with_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE src (id INT, val INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO src VALUES (1, 10), (2, 50), (3, 100)",
        &mut storage,
        &mut txn,
    );
    run("CREATE TABLE dst (id INT, val INT)", &mut storage, &mut txn);

    let aff = affected_count(run(
        "INSERT INTO dst SELECT * FROM src WHERE val > 20",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 2);
    let r = rows(run(
        "SELECT * FROM dst ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn test_insert_select_named_columns() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE src (a INT, b TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO src VALUES (1, 'x'), (2, 'y')",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE dst (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );

    // Map src.a → dst.name (only named column)
    run(
        "INSERT INTO dst (name) SELECT b FROM src ORDER BY a ASC",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id, name FROM dst ORDER BY name ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Null, "id not in column list → NULL");
    assert_eq!(r[0][1], Value::Text("x".into()));
}

#[test]
fn test_insert_select_with_limit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE src (id INT)", &mut storage, &mut txn);
    for i in 1..=10i32 {
        run(
            &format!("INSERT INTO src VALUES ({i})"),
            &mut storage,
            &mut txn,
        );
    }
    run("CREATE TABLE dst (id INT)", &mut storage, &mut txn);

    let aff = affected_count(run(
        "INSERT INTO dst SELECT * FROM src ORDER BY id ASC LIMIT 3",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 3);

    let r = rows(run(
        "SELECT * FROM dst ORDER BY id ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn test_insert_select_aggregation() {
    let (mut storage, mut txn) = setup();
    setup_employees(&mut storage, &mut txn); // reuse existing helper
    run(
        "CREATE TABLE summary (dept TEXT, cnt INT)",
        &mut storage,
        &mut txn,
    );

    let aff = affected_count(run(
        "INSERT INTO summary (dept, cnt) SELECT dept, COUNT(*) FROM employees GROUP BY dept",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(aff, 2); // eng + sales

    let r = rows(run(
        "SELECT * FROM summary ORDER BY dept ASC",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    let eng = r
        .iter()
        .find(|row| row[0] == Value::Text("eng".into()))
        .unwrap();
    // COUNT(*) returns BigInt; coerced to Int when stored in the INT column.
    assert!(
        eng[1] == Value::Int(3) || eng[1] == Value::BigInt(3),
        "eng count must be 3, got {:?}",
        eng[1]
    );
}

#[test]
fn test_insert_select_mvcc_no_self_read() {
    // INSERT INTO t SELECT * FROM t where t already has 2 rows.
    // MVCC: the SELECT sees snapshot at BEGIN — new rows not visible.
    // After commit: t has 4 rows (2 original + 2 copies), not infinite.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1), (2)", &mut storage, &mut txn);

    let aff = affected_count(run("INSERT INTO t SELECT * FROM t", &mut storage, &mut txn));
    assert_eq!(aff, 2, "should insert exactly the 2 pre-existing rows");

    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(
        r[0][0],
        Value::BigInt(4),
        "total rows: 2 original + 2 copies"
    );
}

// ── 4.14: AUTO_INCREMENT + LAST_INSERT_ID ────────────────────────────────────

#[test]
fn test_auto_increment_basic() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, name TEXT)",
        &mut storage,
        &mut txn,
    );

    // First insert: id should be auto-assigned to 1
    let r = run(
        "INSERT INTO t (name) VALUES ('Alice')",
        &mut storage,
        &mut txn,
    );
    match r {
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            assert_eq!(count, 1);
            assert_eq!(last_insert_id, Some(1));
        }
        other => panic!("expected Affected, got {other:?}"),
    }

    // Second insert: id = 2
    let r = run(
        "INSERT INTO t (name) VALUES ('Bob')",
        &mut storage,
        &mut txn,
    );
    match r {
        QueryResult::Affected { last_insert_id, .. } => assert_eq!(last_insert_id, Some(2)),
        other => panic!("{other:?}"),
    }

    // Row values should be correct
    let r = rows(run(
        "SELECT id, name FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
}

#[test]
fn test_auto_increment_explicit_null() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val INT)",
        &mut storage,
        &mut txn,
    );

    // Explicit NULL should trigger auto-generation
    let r = run("INSERT INTO t VALUES (NULL, 100)", &mut storage, &mut txn);
    match r {
        QueryResult::Affected { last_insert_id, .. } => assert_eq!(last_insert_id, Some(1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_auto_increment_explicit_value_no_advance() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val INT)",
        &mut storage,
        &mut txn,
    );

    // Explicit non-NULL id should NOT set LAST_INSERT_ID
    let r = run("INSERT INTO t VALUES (99, 1)", &mut storage, &mut txn);
    match r {
        QueryResult::Affected { last_insert_id, .. } => assert_eq!(last_insert_id, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_auto_increment_multi_row() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val INT)",
        &mut storage,
        &mut txn,
    );

    // Multi-row INSERT: LAST_INSERT_ID = first generated id
    let r = run(
        "INSERT INTO t VALUES (NULL, 1), (NULL, 2), (NULL, 3)",
        &mut storage,
        &mut txn,
    );
    match r {
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            assert_eq!(count, 3);
            assert_eq!(last_insert_id, Some(1)); // first generated
        }
        other => panic!("{other:?}"),
    }

    let r = rows(run("SELECT id FROM t ORDER BY id", &mut storage, &mut txn));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

#[test]
fn test_last_insert_id_function() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT, val INT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (NULL, 42)", &mut storage, &mut txn);

    let r = rows(run("SELECT LAST_INSERT_ID()", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(1));

    run("INSERT INTO t VALUES (NULL, 99)", &mut storage, &mut txn);
    let r = rows(run("SELECT lastval()", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(2));
}

#[test]
fn test_serial_synonym() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT SERIAL, val TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t (val) VALUES ('x')", &mut storage, &mut txn);

    let r = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1));
}

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

// ── 4.16: SQL full test suite ─────────────────────────────────────────────────
// Fills coverage gaps: LIKE, BETWEEN, IN, IS NULL, constraints, expressions,
// scalar functions, NULL semantics, error cases.

// ── LIKE / NOT LIKE ───────────────────────────────────────────────────────────

#[test]
fn test_like_basic() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (name TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('Alice'), ('Bob'), ('Alfred')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT name FROM t WHERE name LIKE 'Al%' ORDER BY name",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("Alfred".into()));
    assert_eq!(r[1][0], Value::Text("Alice".into()));
}

#[test]
fn test_like_single_char_wildcard() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (v TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('cat'), ('car'), ('card')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT v FROM t WHERE v LIKE 'ca_' ORDER BY v",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2); // cat, car (not card — 4 chars)
}

#[test]
fn test_not_like() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (v TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('hello'), ('world'), ('help')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT v FROM t WHERE v NOT LIKE 'hel%' ORDER BY v",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("world".into()));
}

// ── BETWEEN ───────────────────────────────────────────────────────────────────

#[test]
fn test_between_inclusive() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (n INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1),(2),(3),(4),(5)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT n FROM t WHERE n BETWEEN 2 AND 4 ORDER BY n",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(4));
}

#[test]
fn test_not_between() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (n INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1),(3),(5)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT n FROM t WHERE n NOT BETWEEN 2 AND 4 ORDER BY n",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(5));
}

// ── IN list ───────────────────────────────────────────────────────────────────

#[test]
fn test_in_list() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE id IN (1, 3) ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}

#[test]
fn test_not_in_list() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1),(2),(3)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id FROM t WHERE id NOT IN (1, 3)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

// ── IS NULL / IS NOT NULL ─────────────────────────────────────────────────────

#[test]
fn test_is_null_filter() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, NULL)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE val IS NULL ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn test_is_not_null_filter() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 'a'), (2, NULL)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE val IS NOT NULL",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

// ── Constraints ───────────────────────────────────────────────────────────────

#[test]
fn test_not_null_constraint_violated() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );

    // NOT NULL: stored as `nullable=false` in catalog but not yet enforced at insert time.
    // Documents current behavior (succeeds without error).
    run(
        "INSERT INTO t VALUES (NULL, 'Alice')",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Null); // NULL accepted (constraint not enforced yet)
}

#[test]
fn test_unique_constraint_enforced() {
    // UNIQUE column constraint creates a B-Tree unique index at CREATE TABLE time
    // (Phase 6.5). Duplicate values are rejected at INSERT time.
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);
    // Second insert with same id should fail with UniqueViolation.
    let err = run_result("INSERT INTO t VALUES (1, 'Bob')", &mut storage, &mut txn)
        .expect_err("expected UniqueViolation");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got: {err}"
    );
    // Only the first row should exist.
    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(1));
}

#[test]
fn test_check_constraint_parsed_but_not_yet_enforced() {
    // CHECK constraints are parsed and stored in the AST but not yet evaluated
    // at INSERT time (gap from 4.3b — enforcement deferred to a later subphase).
    // This test verifies the current behavior: INSERT with CHECK-violating values
    // succeeds silently.
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut storage,
        &mut txn,
    );
    // Succeeds (CHECK not enforced yet)
    run("INSERT INTO t VALUES (-1)", &mut storage, &mut txn);
    let r = rows(run("SELECT age FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(-1));
}

#[test]
fn test_check_constraint_passes() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (0)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (42)", &mut storage, &mut txn);

    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(2));
}

// ── UPDATE with arithmetic expression ────────────────────────────────────────

#[test]
fn test_update_with_arithmetic() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, score INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        &mut storage,
        &mut txn,
    );

    run(
        "UPDATE t SET score = score + 5 WHERE id <= 2",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT score FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(15));
    assert_eq!(r[1][0], Value::Int(25));
    assert_eq!(r[2][0], Value::Int(30)); // unchanged
}

#[test]
fn test_update_multiple_columns() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, a INT, b INT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 10, 20)", &mut storage, &mut txn);

    run(
        "UPDATE t SET a = 99, b = 88 WHERE id = 1",
        &mut storage,
        &mut txn,
    );

    let r = rows(run("SELECT a, b FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(99));
    assert_eq!(r[0][1], Value::Int(88));
}

// ── NULL semantics in arithmetic ─────────────────────────────────────────────

#[test]
fn test_null_arithmetic_propagation() {
    let (mut storage, mut txn) = setup();
    // NULL + anything = NULL
    let r = rows(run(
        "SELECT NULL + 1, 1 + NULL, NULL * 0",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_null_comparison_returns_null() {
    let (mut storage, mut txn) = setup();
    // NULL = NULL → NULL (not TRUE)
    let r = rows(run(
        "SELECT NULL = NULL, NULL <> NULL, NULL > 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_three_valued_logic_and() {
    let (mut storage, mut txn) = setup();
    // FALSE AND NULL = FALSE (short-circuit)
    // TRUE AND NULL = NULL
    let r = rows(run(
        "SELECT FALSE AND NULL, TRUE AND NULL, NULL AND FALSE",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Bool(false));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Bool(false));
}

#[test]
fn test_three_valued_logic_or() {
    let (mut storage, mut txn) = setup();
    // TRUE OR NULL = TRUE, FALSE OR NULL = NULL
    let r = rows(run(
        "SELECT TRUE OR NULL, FALSE OR NULL, NULL OR TRUE",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Bool(true));
}

// ── String concatenation ─────────────────────────────────────────────────────

#[test]
fn test_string_concat() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT 'Hello' || ', ' || 'World'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("Hello, World".into()));
}

#[test]
fn test_string_concat_with_column() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (fname TEXT, lname TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('Alice', 'Smith')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT fname || ' ' || lname FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("Alice Smith".into()));
}

// ── CAST ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cast_text_to_int_valid() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT CAST('42' AS INT)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_cast_text_to_bigint() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT CAST('999' AS BIGINT)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(999));
}

#[test]
fn test_cast_int_to_text_not_supported_in_strict() {
    // CAST uses strict mode; Int→Text conversion is not allowed in strict mode.
    // Use CAST with supported conversions (Text→Int is supported, Int→Text is not).
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT CAST(123 AS TEXT)", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::InvalidCoercion { .. })));
}

#[test]
fn test_cast_invalid_text_to_int() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT CAST('abc' AS INT)", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::InvalidCoercion { .. })));
}

// ── Scalar functions ──────────────────────────────────────────────────────────

#[test]
fn test_abs_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT ABS(-42), ABS(7), ABS(0)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(42));
    assert_eq!(r[0][1], Value::Int(7));
    assert_eq!(r[0][2], Value::Int(0));
}

#[test]
fn test_length_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT LENGTH('hello'), LENGTH(''), LENGTH(NULL)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(5)); // LENGTH returns Int
    assert_eq!(r[0][1], Value::Int(0));
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_upper_lower() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT UPPER('hello'), LOWER('WORLD')",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("HELLO".into()));
    assert_eq!(r[0][1], Value::Text("world".into()));
}

#[test]
fn test_substr_function() {
    let (mut storage, mut txn) = setup();
    // SUBSTR(str, start, len) — 1-based
    let r = rows(run(
        "SELECT SUBSTR('hello world', 7, 5)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("world".into()));
}

#[test]
fn test_trim_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT TRIM('  hello  ')", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Text("hello".into()));
}

#[test]
fn test_coalesce_returns_first_non_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT COALESCE(NULL, NULL, 42, 99)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_coalesce_all_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT COALESCE(NULL, NULL)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn test_round_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT ROUND(3.7), ROUND(3.2), ROUND(-1.5)",
        &mut storage,
        &mut txn,
    ));
    // Rounding behavior: nearest integer
    assert!(matches!(r[0][0], Value::Real(v) if (v - 4.0).abs() < 0.001));
    assert!(matches!(r[0][1], Value::Real(v) if (v - 3.0).abs() < 0.001));
}

#[test]
fn test_floor_ceil_functions() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT FLOOR(3.9), CEIL(3.1)", &mut storage, &mut txn));
    assert!(matches!(r[0][0], Value::Real(v) if (v - 3.0).abs() < 0.001));
    assert!(matches!(r[0][1], Value::Real(v) if (v - 4.0).abs() < 0.001));
}

#[test]
fn test_now_returns_non_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT NOW()", &mut storage, &mut txn));
    assert!(!matches!(r[0][0], Value::Null));
}

// ── Division by zero ──────────────────────────────────────────────────────────

#[test]
fn test_division_by_zero_error() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT 1 / 0", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::DivisionByZero)));
}

#[test]
fn test_modulo_by_zero_error() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT 5 % 0", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::DivisionByZero)));
}

// ── Complex WHERE with AND/OR/NOT ─────────────────────────────────────────────

#[test]
fn test_complex_where_and_or() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (a INT, b INT, c INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (1,2,3),(4,5,6),(7,8,9),(10,11,12)",
        &mut storage,
        &mut txn,
    );

    // (a < 5 OR a > 8) AND b != 11
    let r = rows(run(
        "SELECT a FROM t WHERE (a < 5 OR a > 8) AND b <> 11 ORDER BY a",
        &mut storage,
        &mut txn,
    ));
    // (a < 5 OR a > 8) AND b <> 11:
    //   a=1:  (T OR F) AND 2≠11=T  → included
    //   a=4:  (T OR F) AND 5≠11=T  → included
    //   a=7:  (F OR F) AND ...=F   → excluded
    //   a=10: (F OR T) AND 11≠11=F → excluded
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(4));
}

#[test]
fn test_not_operator_in_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (active BOOL)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (TRUE), (FALSE), (TRUE)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT COUNT(*) FROM t WHERE NOT active",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::BigInt(1));
}

// ── SELECT with computed expressions ─────────────────────────────────────────

#[test]
fn test_select_arithmetic_expression() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (price INT, qty INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (10, 3), (5, 7)",
        &mut storage,
        &mut txn,
    );

    // ORDER BY alias not supported yet — use expression directly
    let r = rows(run(
        "SELECT price * qty FROM t ORDER BY price * qty",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    // 5*7=35, 10*3=30 → ORDER BY ascending: 30, 35
    assert_eq!(r[0][0], Value::Int(30));
    assert_eq!(r[1][0], Value::Int(35));
}

#[test]
fn test_select_with_alias() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (x INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (5)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT x * 2 AS doubled FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(10));
}

// ── HAVING with aggregate condition ──────────────────────────────────────────

#[test]
fn test_having_count_greater_than() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE orders (customer_id INT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO orders VALUES (1,100),(1,200),(2,50),(3,75),(3,25),(3,100)",
        &mut storage,
        &mut txn,
    );

    // Customers with more than 2 orders
    let r = rows(run(
        "SELECT customer_id FROM orders GROUP BY customer_id HAVING COUNT(*) > 2 ORDER BY customer_id",
        &mut storage, &mut txn
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}

#[test]
fn test_having_sum_filter() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE sales (dept INT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO sales VALUES (1,100),(1,150),(2,50),(2,30)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT dept FROM sales GROUP BY dept HAVING SUM(amount) > 200 ORDER BY dept",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1)); // dept 1: 100+150=250 > 200 ✓
}

// ── INSERT DEFAULT values ─────────────────────────────────────────────────────

#[test]
fn test_insert_with_column_default() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, status TEXT DEFAULT 'pending')",
        &mut storage,
        &mut txn,
    );
    // Insert without specifying status — should use default
    run("INSERT INTO t (id) VALUES (1)", &mut storage, &mut txn);

    let r = rows(run("SELECT id, status FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1));
    // status defaults to NULL when not specified via col list (no DEFAULT in executor yet)
    // This test verifies the current behavior (NULL for omitted columns)
    assert_eq!(r[0][1], Value::Null);
}

// ── Full round-trip: CREATE → INSERT → SELECT → UPDATE → DELETE ──────────────

#[test]
fn test_full_sql_suite_roundtrip() {
    let (mut storage, mut txn) = setup();

    // Schema
    run("CREATE TABLE products (id INT AUTO_INCREMENT, name TEXT NOT NULL, price INT, stock INT DEFAULT 0)", &mut storage, &mut txn);
    run(
        "CREATE TABLE categories (id INT, name TEXT UNIQUE)",
        &mut storage,
        &mut txn,
    );

    // Insert
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Widget', 100, 50)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Gadget', 200, 25)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Doohickey', 50, 100)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO categories VALUES (1, 'Electronics')",
        &mut storage,
        &mut txn,
    );

    // Select with WHERE + ORDER
    let r = rows(run(
        "SELECT name, price FROM products WHERE price > 75 ORDER BY price",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Int(100));
    assert_eq!(r[1][1], Value::Int(200));

    // Aggregate
    let r = rows(run(
        "SELECT COUNT(*), AVG(price), MAX(stock) FROM products",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::BigInt(3));
    // AVG(100+200+50)/3 = 116.67
    assert!(matches!(r[0][1], Value::Real(_)));
    assert_eq!(r[0][2], Value::Int(100));

    // Update
    run(
        "UPDATE products SET price = price - 10 WHERE price > 100",
        &mut storage,
        &mut txn,
    );
    let r = rows(run(
        "SELECT price FROM products WHERE name = 'Gadget'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(190));

    // Delete
    run(
        "DELETE FROM products WHERE stock > 75",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT COUNT(*) FROM products", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(2));

    // Note: UNIQUE constraints not yet enforced at executor level (deferred)
    // so duplicate inserts currently succeed. Verified above via other tests.

    // Truncate + auto_increment reset
    run("TRUNCATE TABLE products", &mut storage, &mut txn);
    run(
        "INSERT INTO products (name, price) VALUES ('Reset', 1)",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT id FROM products", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1)); // reset to 1 after TRUNCATE
}

// ── Phase 4.25c: strict mode + warnings ──────────────────────────────────────

use axiomdb_sql::{
    bloom::BloomRegistry, execute_with_ctx, session::SessionContext, SessionContext as SC,
};

fn run_ctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn setup_ctx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.into_path().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

#[test]
fn test_strict_mode_default_is_on() {
    let ctx = SessionContext::new();
    assert!(ctx.strict_mode, "strict_mode must default to true");
}

#[test]
fn test_set_strict_mode_off_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(!ctx.strict_mode, "strict_mode should be OFF after SET");
}

#[test]
fn test_set_strict_mode_on_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET strict_mode = ON",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(ctx.strict_mode, "strict_mode should be ON after SET");
}

#[test]
fn test_set_strict_mode_default_restores_on() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET strict_mode = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "SET strict_mode = DEFAULT must restore true"
    );
}

#[test]
fn test_set_sql_mode_empty_string_disables_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET sql_mode = ''",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(!ctx.strict_mode, "SET sql_mode = '' must disable strict");
}

#[test]
fn test_set_sql_mode_strict_trans_tables_enables_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = 'STRICT_TRANS_TABLES'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "STRICT_TRANS_TABLES token must enable strict"
    );
}

#[test]
fn test_set_sql_mode_default_restores_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "SET sql_mode = DEFAULT must restore strict ON"
    );
}

#[test]
fn test_set_sql_mode_ansi_quotes_with_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "ANSI_QUOTES,STRICT_TRANS_TABLES must enable strict"
    );
}

#[test]
fn test_strict_mode_on_insert_lossy_string_to_int_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    assert!(ctx.strict_mode);

    run_ctx(
        "CREATE TABLE t_strict (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "INSERT INTO t_strict VALUES ('42abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(result.is_err(), "strict ON: '42abc' into INT must error");
    // No row should have been inserted.
    let rows_after = rows(
        run_ctx(
            "SELECT * FROM t_strict",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(
        rows_after.is_empty(),
        "no row should be stored on strict error"
    );
}

#[test]
fn test_strict_mode_off_insert_lossy_string_stores_and_warns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_perm (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_perm VALUES ('42abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1, "should have exactly one warning");
    let w = &ctx.warnings[0];
    assert_eq!(w.code, 1265);
    assert!(
        w.message.contains("age") && w.message.contains("row 1"),
        "warning message: {}",
        w.message
    );

    let r = rows(
        run_ctx(
            "SELECT age FROM t_perm",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(42), "permissive result must be 42");
}

#[test]
fn test_strict_mode_off_insert_non_numeric_stores_zero_and_warns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_zero (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_zero VALUES ('abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1);
    let r = rows(
        run_ctx(
            "SELECT age FROM t_zero",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(0), "non-numeric string -> permissive 0");
}

#[test]
fn test_strict_mode_off_multirow_insert_row_numbers_in_warnings() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_multi (n INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_multi VALUES ('42abc'), ('7x')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 2, "two rows, two warnings");
    assert!(
        ctx.warnings[0].message.contains("row 1"),
        "first warning: {}",
        ctx.warnings[0].message
    );
    assert!(
        ctx.warnings[1].message.contains("row 2"),
        "second warning: {}",
        ctx.warnings[1].message
    );
}

#[test]
fn test_strict_mode_off_update_warns_on_permissive_fallback() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_upd (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t_upd VALUES (10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "UPDATE t_upd SET age = '9x'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1, "update must emit one warning");
    assert_eq!(ctx.warnings[0].code, 1265);

    let r = rows(
        run_ctx(
            "SELECT age FROM t_upd",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(9), "permissive update result must be 9");
}

#[test]
fn test_strict_mode_off_cast_still_errors() {
    // Non-assignment coercion (CAST) must remain unaffected by strict_mode.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT CAST('abc' AS SIGNED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // CAST should still error even with strict OFF.
    assert!(
        result.is_err(),
        "CAST('abc' AS INT) must still error with strict OFF"
    );
}

// ── Phase 4.9b: Sort-Based GROUP BY ──────────────────────────────────────────

/// Convenience wrapper: run SQL through the ctx path, panic on error.
fn rctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

/// Sets up a table with a single-column index on `dept` and inserts rows.
fn setup_indexed_dept_ctx(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) {
    rctx(
        "CREATE TABLE emp_idx (id INT NOT NULL, dept TEXT NOT NULL, salary INT NOT NULL)",
        storage,
        txn,
        bloom,
        ctx,
    );
    rctx(
        "CREATE INDEX idx_dept ON emp_idx (dept)",
        storage,
        txn,
        bloom,
        ctx,
    );
    // eng × 3, hr × 1, sales × 2 — inserted in alphabetical dept order so
    // insertion order matches index order (validates presorted assumption).
    for (id, dept, salary) in [
        (1, "eng", 90000),
        (2, "eng", 80000),
        (3, "eng", 70000),
        (4, "hr", 65000),
        (5, "sales", 60000),
        (6, "sales", 55000),
    ] {
        rctx(
            &format!("INSERT INTO emp_idx VALUES ({id}, '{dept}', {salary})"),
            storage,
            txn,
            bloom,
            ctx,
        );
    }
}

/// Sets up a table with a composite index on `(region, dept)`.
fn setup_composite_index_ctx(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) {
    rctx(
        "CREATE TABLE emp_comp (id INT NOT NULL, region TEXT NOT NULL, dept TEXT NOT NULL, salary INT NOT NULL)",
        storage,
        txn,
        bloom,
        ctx,
    );
    rctx(
        "CREATE INDEX idx_region_dept ON emp_comp (region, dept)",
        storage,
        txn,
        bloom,
        ctx,
    );
    for (id, region, dept, salary) in [
        (1, "east", "eng", 90000),
        (2, "east", "eng", 85000),
        (3, "east", "sales", 70000),
        (4, "west", "eng", 80000),
        (5, "west", "hr", 65000),
        (6, "west", "hr", 60000),
    ] {
        rctx(
            &format!("INSERT INTO emp_comp VALUES ({id}, '{region}', '{dept}', {salary})"),
            storage,
            txn,
            bloom,
            ctx,
        );
    }
}

#[test]
fn test_sorted_group_by_equality_lookup() {
    // IndexLookup equality path: GROUP BY on the same column → sorted strategy.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // dept = 'eng' uses IndexLookup (non-unique → treated as range), GROUP BY dept.
    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept = 'eng' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Only one group (eng).
    assert_eq!(r.len(), 1, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[0][1], Value::BigInt(3));
}

#[test]
fn test_sorted_group_by_raw_scan_order() {
    // Verify that the dept index actually returns rows in alphabetical dept order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept FROM emp_idx WHERE dept >= 'a' AND dept <= 'z'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Must come back in index (alphabetical dept) order: eng, eng, eng, hr, sales, sales.
    assert_eq!(r.len(), 6, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[1][0], Value::Text("eng".into()));
    assert_eq!(r[2][0], Value::Text("eng".into()));
    assert_eq!(r[3][0], Value::Text("hr".into()));
    assert_eq!(r[4][0], Value::Text("sales".into()));
    assert_eq!(r[5][0], Value::Text("sales".into()));
}

#[test]
fn test_sorted_group_by_single_index_count() {
    // Index on (dept) + two-sided range triggers IndexRange → sorted strategy
    // (strategy only chosen in choose_group_by_strategy_ctx, i.e. the ctx path).
    // The planner requires AND for range detection; single-sided >= falls to Scan.
    // Sorted path emits groups in index key order (alphabetical for TEXT).
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Sorted path emits groups in index (alphabetical) order: eng, hr, sales.
    assert_eq!(r.len(), 3, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::BigInt(1));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_single_index_sum() {
    // SUM under the sorted path (ctx path) must aggregate correctly.
    // Sorted path emits groups in index key order: eng, hr, sales.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, SUM(salary) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // eng: 90000+80000+70000=240000; hr: 65000; sales: 60000+55000=115000
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Int(240000));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::Int(65000));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::Int(115000));
}

#[test]
fn test_sorted_group_by_composite_prefix_full() {
    // Index on (region, dept) + GROUP BY region, dept → full prefix match.
    // Sorted path emits groups in (region, dept) index order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_composite_index_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT region, dept, COUNT(*) FROM emp_comp WHERE region >= 'a' AND region <= 'z' GROUP BY region, dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Index order: east/eng(2), east/sales(1), west/eng(1), west/hr(2)
    assert_eq!(r.len(), 4);
    assert_eq!(r[0][0], Value::Text("east".into()));
    assert_eq!(r[0][1], Value::Text("eng".into()));
    assert_eq!(r[0][2], Value::BigInt(2));
    assert_eq!(r[1][0], Value::Text("east".into()));
    assert_eq!(r[1][1], Value::Text("sales".into()));
    assert_eq!(r[1][2], Value::BigInt(1));
    assert_eq!(r[2][0], Value::Text("west".into()));
    assert_eq!(r[2][1], Value::Text("eng".into()));
    assert_eq!(r[2][2], Value::BigInt(1));
    assert_eq!(r[3][0], Value::Text("west".into()));
    assert_eq!(r[3][1], Value::Text("hr".into()));
    assert_eq!(r[3][2], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_composite_prefix_partial() {
    // Index on (region, dept) + GROUP BY region only → leading prefix match.
    // Sorted path emits groups in region index order: east, west.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_composite_index_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT region, COUNT(*) FROM emp_comp WHERE region >= 'a' AND region <= 'z' GROUP BY region",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("east".into()));
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("west".into()));
    assert_eq!(r[1][1], Value::BigInt(3));
}

#[test]
fn test_hash_group_by_without_index_still_correct() {
    // Plain table scan (no index) → hash strategy; must produce correct results.
    // Hash iteration order is non-deterministic — sort manually to verify values.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // Setup employees via plain run() — no index, will use hash strategy.
    run(
        "CREATE TABLE emp_hash (id INT NOT NULL, dept TEXT, salary INT)",
        &mut storage,
        &mut txn,
    );
    for (id, dept, salary) in [
        (1, "eng", 90000),
        (2, "eng", 80000),
        (3, "eng", 70000),
        (4, "sales", 60000),
        (5, "sales", 55000),
    ] {
        run(
            &format!("INSERT INTO emp_hash VALUES ({id}, '{dept}', {salary})"),
            &mut storage,
            &mut txn,
        );
    }

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_hash GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    let mut counts: Vec<(String, i64)> = r
        .iter()
        .map(|row| {
            let dept = match &row[0] {
                Value::Text(s) => s.clone(),
                v => panic!("expected Text, got {v:?}"),
            };
            let cnt = match row[1] {
                Value::BigInt(n) => n,
                ref v => panic!("expected BigInt, got {v:?}"),
            };
            (dept, cnt)
        })
        .collect();
    counts.sort_by_key(|(d, _)| d.clone());
    assert_eq!(counts, vec![("eng".into(), 3), ("sales".into(), 2)]);
}

#[test]
fn test_sorted_group_by_having() {
    // HAVING must filter correctly under the sorted path.
    // hr has count=1 → filtered out; eng(3) and sales(2) remain in index order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept HAVING COUNT(*) > 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("sales".into()));
    assert_eq!(r[1][1], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_group_concat() {
    // GROUP_CONCAT must accumulate correctly under the sorted path.
    // The inner ORDER BY is within the aggregate (operates on per-group input rows).
    // Sorted path emits groups in index order: eng, hr, sales.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, GROUP_CONCAT(salary ORDER BY salary DESC SEPARATOR ',') FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Text("90000,80000,70000".into()));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::Text("65000".into()));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::Text("60000,55000".into()));
}
