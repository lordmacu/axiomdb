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
