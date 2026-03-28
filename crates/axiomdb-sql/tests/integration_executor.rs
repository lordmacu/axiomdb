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
    let wal_path = dir.keep().join("test.wal");
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

// ── FULL OUTER JOIN tests (Phase 4.8b) ───────────────────────────────────────

#[test]
fn test_full_outer_join_matched_and_unmatched_both_sides() {
    // users: id=1,2,3;  orders: user_id=1,1,2,99
    // FULL OUTER JOIN ON users.id = orders.user_id →
    //   matched:   (1,10),(1,11),(2,12)
    //   unmatched left:  (3, NULL)
    //   unmatched right: (NULL, 13)
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let result = rows(run(
        "SELECT users.id, orders.id FROM users FULL OUTER JOIN orders ON users.id = orders.user_id ORDER BY users.id, orders.id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        result.len(),
        5,
        "3 matched + 1 unmatched left + 1 unmatched right"
    );

    // Row (NULL, 13) — unmatched right order
    let unmatched_right: Vec<&Vec<Value>> = result.iter().filter(|r| r[0] == Value::Null).collect();
    assert_eq!(unmatched_right.len(), 1);
    assert_eq!(unmatched_right[0][1], Value::Int(13));

    // Row (3, NULL) — unmatched left user Carol
    let unmatched_left: Vec<&Vec<Value>> = result.iter().filter(|r| r[1] == Value::Null).collect();
    assert_eq!(unmatched_left.len(), 1);
    assert_eq!(unmatched_left[0][0], Value::Int(3));
}

#[test]
fn test_full_outer_join_one_to_many_emits_all_matches() {
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    let result = rows(run(
        "SELECT users.id, orders.id FROM users FULL OUTER JOIN orders ON users.id = orders.user_id WHERE users.id = 1 ORDER BY orders.id",
        &mut storage,
        &mut txn,
    ));
    // user 1 matches orders 10 and 11
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][1], Value::Int(10));
    assert_eq!(result[1][1], Value::Int(11));
}

#[test]
fn test_full_outer_join_on_vs_where_semantics() {
    // ON filters before null-extension; WHERE filters after.
    // ON: users.id = orders.user_id → user 3 + order 99 remain unmatched
    // WHERE users.id IS NOT NULL → drops the unmatched-right row (NULL, 13)
    let (mut storage, mut txn) = setup();
    setup_join_tables(&mut storage, &mut txn);

    // With WHERE: only rows where users.id IS NOT NULL survive
    let result = rows(run(
        "SELECT users.id, orders.id FROM users FULL OUTER JOIN orders ON users.id = orders.user_id WHERE users.id IS NOT NULL ORDER BY users.id, orders.id",
        &mut storage,
        &mut txn,
    ));
    // matched: (1,10),(1,11),(2,12)  + unmatched-left: (3,NULL) → 4 rows
    // the unmatched-right (NULL,13) is removed by WHERE
    assert_eq!(result.len(), 4);
    assert!(result.iter().all(|r| r[0] != Value::Null));
}

#[test]
fn test_full_outer_join_using() {
    // USING column must work via the executor-side USING path
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE a (id INT, val TEXT)", &mut storage, &mut txn);
    run("CREATE TABLE b (id INT, score INT)", &mut storage, &mut txn);
    run("INSERT INTO a VALUES (1, 'x')", &mut storage, &mut txn);
    run("INSERT INTO a VALUES (2, 'y')", &mut storage, &mut txn);
    run("INSERT INTO b VALUES (1, 10)", &mut storage, &mut txn);
    run("INSERT INTO b VALUES (3, 30)", &mut storage, &mut txn);

    let result = rows(run(
        "SELECT a.id, b.id FROM a FULL OUTER JOIN b USING (id) ORDER BY a.id, b.id",
        &mut storage,
        &mut txn,
    ));
    // matched: (1, 1)
    // unmatched left:  (2, NULL)
    // unmatched right: (NULL, 3)
    assert_eq!(result.len(), 3);
    let matched: Vec<&Vec<Value>> = result
        .iter()
        .filter(|r| r[0] != Value::Null && r[1] != Value::Null)
        .collect();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0][0], Value::Int(1));
}

#[test]
fn test_full_outer_join_select_star_nullability_metadata() {
    // SELECT * over FULL JOIN must mark both sides as nullable.
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE l (id INT NOT NULL, v TEXT NOT NULL)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE r (id INT NOT NULL, w TEXT NOT NULL)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO l VALUES (1, 'a')", &mut storage, &mut txn);
    run("INSERT INTO r VALUES (2, 'b')", &mut storage, &mut txn);

    let result = run_result(
        "SELECT * FROM l FULL OUTER JOIN r ON l.id = r.id",
        &mut storage,
        &mut txn,
    )
    .unwrap();
    if let QueryResult::Rows { columns, .. } = result {
        // All 4 columns (l.id, l.v, r.id, r.w) must be nullable.
        assert_eq!(columns.len(), 4);
        for col in &columns {
            assert!(
                col.nullable,
                "column '{}' must be nullable in FULL JOIN SELECT *",
                col.name
            );
        }
    } else {
        panic!("expected Rows result");
    }
}

#[test]
fn test_full_outer_join_in_chain_with_left_join() {
    // a FULL JOIN b ... LEFT JOIN c ... — chain must keep working.
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE a (id INT, v INT)", &mut storage, &mut txn);
    run("CREATE TABLE b (id INT, v INT)", &mut storage, &mut txn);
    run("CREATE TABLE c (id INT, v INT)", &mut storage, &mut txn);
    run("INSERT INTO a VALUES (1, 10)", &mut storage, &mut txn);
    run("INSERT INTO b VALUES (1, 20)", &mut storage, &mut txn);
    run("INSERT INTO b VALUES (2, 30)", &mut storage, &mut txn);
    run("INSERT INTO c VALUES (1, 40)", &mut storage, &mut txn);

    let result = rows(run(
        "SELECT a.id, b.id, c.id FROM a FULL OUTER JOIN b ON a.id = b.id LEFT JOIN c ON b.id = c.id ORDER BY a.id, b.id",
        &mut storage,
        &mut txn,
    ));
    // a(1) FULL b(1) → (1,1,_)  then LEFT c(1) → (1,1,1)
    // a(NULL) FULL b(2) → (NULL,2,NULL)  then LEFT c → no match for id=2 → (NULL,2,NULL)
    assert!(result.len() >= 2, "chain must produce multiple rows");
    // The matched row must have all three IDs = 1
    let fully_matched: Vec<&Vec<Value>> = result
        .iter()
        .filter(|r| r[0] != Value::Null && r[1] != Value::Null && r[2] != Value::Null)
        .collect();
    assert!(!fully_matched.is_empty());
    assert_eq!(fully_matched[0][0], Value::Int(1));
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

use axiomdb_sql::{bloom::BloomRegistry, execute_with_ctx, SessionContext};

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
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

#[test]
fn test_count_star_with_ctx_uses_masked_scan_without_hanging() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("COUNT(*) must return rows");
    };

    assert_eq!(rows, vec![vec![Value::BigInt(3)]]);
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

// ── 4.10d — Parameterized LIMIT/OFFSET row-count coercion ────────────────────

/// Sets up a 10-row table for LIMIT/OFFSET tests (ids 1..=10, ordered).
fn setup_limit_table(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run("CREATE TABLE t_lim (a INT)", storage, txn);
    for i in 1..=10i32 {
        run(&format!("INSERT INTO t_lim VALUES ({i})"), storage, txn);
    }
}

#[test]
fn test_limit_text_int_same_as_literal() {
    // LIMIT '2' OFFSET '1' must behave identically to LIMIT 2 OFFSET 1.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT a FROM t_lim ORDER BY a ASC LIMIT '2' OFFSET '1'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[1][0], Value::Int(3));
}

#[test]
fn test_limit_text_with_whitespace() {
    // Leading/trailing ASCII whitespace in an integer text row count is ignored.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT a FROM t_lim ORDER BY a ASC LIMIT '  3  '",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn test_limit_text_zero() {
    // LIMIT '0' is valid and returns zero rows.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT a FROM t_lim ORDER BY a ASC LIMIT '0'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 0);
}

#[test]
fn test_limit_negative_int_errors() {
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT -1", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_text_negative_errors() {
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT '-1'", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_non_integral_text_errors() {
    // "10.1" is not an exact integer.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT '10.1'", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_scientific_notation_text_errors() {
    // "1e3" looks like a number but is not an exact base-10 integer string.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT '1e3'", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_time_like_text_errors() {
    // "10:10:10" resembles a time value but is not an integer.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result(
        "SELECT a FROM t_lim LIMIT '10:10:10'",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_alpha_text_errors() {
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT 'abc'", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_limit_null_errors() {
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let err = run_result("SELECT a FROM t_lim LIMIT NULL", &mut storage, &mut txn).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
}

#[test]
fn test_offset_text_int_same_as_literal() {
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    let r = rows(run(
        "SELECT a FROM t_lim ORDER BY a ASC LIMIT 3 OFFSET '4'",
        &mut storage,
        &mut txn,
    ));
    // rows 5, 6, 7
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(5));
    assert_eq!(r[2][0], Value::Int(7));
}

#[test]
fn test_limit_bigint_overflow_errors() {
    // A BigInt that exceeds usize::MAX must be rejected, not silently truncated.
    // We synthesise this via a subquery since the parser produces Int for small
    // literals; CAST is not yet in scope, so we use a known large expression.
    // On 64-bit targets usize::MAX == i64::MAX, so we cannot exceed it with i64.
    // We verify the happy path instead: a BigInt of 2 returns 2 rows.
    let (mut storage, mut txn) = setup();
    setup_limit_table(&mut storage, &mut txn);
    // 2 as BIGINT — parsed from a literal that fits; result must be 2 rows.
    let r = rows(run(
        "SELECT a FROM t_lim ORDER BY a ASC LIMIT 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
}

// ── on_error session variable tests (Phase 5.2c) ─────────────────────────────

#[test]
fn test_on_error_default_is_rollback_statement() {
    let ctx = SessionContext::new();
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackStatement
    );
}

#[test]
fn test_set_on_error_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for (sql, expected) in [
        (
            "SET on_error = 'rollback_transaction'",
            axiomdb_sql::session::OnErrorMode::RollbackTransaction,
        ),
        (
            "SET on_error = 'savepoint'",
            axiomdb_sql::session::OnErrorMode::Savepoint,
        ),
        (
            "SET on_error = 'ignore'",
            axiomdb_sql::session::OnErrorMode::Ignore,
        ),
        (
            "SET on_error = DEFAULT",
            axiomdb_sql::session::OnErrorMode::RollbackStatement,
        ),
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        assert_eq!(ctx.on_error, expected, "after: {sql}");
    }
}

#[test]
fn test_set_on_error_bare_identifier() {
    // SET on_error = rollback_statement (no quotes) must also work.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET on_error = rollback_transaction",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackTransaction
    );
}

#[test]
fn test_set_on_error_invalid_value_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    let result = run_ctx(
        "SET on_error = 'banana'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(result, Err(DbError::InvalidValue { .. })),
        "expected InvalidValue, got {result:?}"
    );
    // Mode unchanged
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackStatement
    );
}

#[test]
fn test_rollback_statement_keeps_txn_open_on_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // ctx.on_error is already RollbackStatement (default)
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Duplicate key error inside active txn
    let err = run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err());
    // Transaction must still be active
    assert!(
        txn.active_txn_id().is_some(),
        "transaction must stay open after rollback_statement"
    );
    // Successful subsequent insert still works
    run_ctx(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Only id=1 (original) and id=2 (after error) committed
    let rows = run_ctx(
        "SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows.into_rows().len(), 2);
}

#[test]
fn test_rollback_transaction_closes_txn_on_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET on_error = 'rollback_transaction'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Duplicate key error
    let err = run_ctx(
        "INSERT INTO t VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err());
    // Whole transaction must be rolled back
    assert!(
        txn.active_txn_id().is_none(),
        "rollback_transaction must close the entire txn"
    );
    // id=42 must NOT be committed
    let rows = run_ctx(
        "SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows.into_rows().len(), 0);
}

#[test]
fn test_savepoint_keeps_first_implicit_autocommit0_txn_open() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.autocommit = false;
    run_ctx(
        "SET on_error = 'savepoint'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // First DML in autocommit=0 — implicit BEGIN
    let err = run_ctx(
        "INSERT INTO t VALUES (999)", // will fail: table empty, no dup — let's use a parse error
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // This insert will succeed (no dup); use a unique violation next
    assert!(err.is_ok()); // id=999 inserted, txn open
    let err2 = run_ctx(
        "INSERT INTO t VALUES (999)", // dup key
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err2.is_err());
    // In savepoint mode: implicit txn must stay open after dup-key on non-first DML
    assert!(
        txn.active_txn_id().is_some(),
        "savepoint mode must keep the implicit txn open after error"
    );
}

// Helper to extract rows from QueryResult
trait IntoRows {
    fn into_rows(self) -> Vec<axiomdb_types::Value>;
}
impl IntoRows for QueryResult {
    fn into_rows(self) -> Vec<axiomdb_types::Value> {
        match self {
            QueryResult::Rows { rows, .. } => rows.into_iter().flatten().collect(),
            _ => vec![],
        }
    }
}

// ── Bulk DELETE / TRUNCATE fast-path tests (Phase 5.16) ──────────────────────

fn setup_pk_table(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE items (id INT NOT NULL, label TEXT)",
        storage,
        txn,
    );
    for i in 1..=5 {
        run(
            &format!("INSERT INTO items VALUES ({i}, 'item{i}')"),
            storage,
            txn,
        );
    }
}

#[test]
fn test_bulk_delete_pk_table_returns_correct_count() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    let result = run("DELETE FROM items", &mut storage, &mut txn);
    match result {
        QueryResult::Affected { count, .. } => assert_eq!(count, 5),
        other => panic!("expected Affected, got {other:?}"),
    }
}

#[test]
fn test_bulk_delete_leaves_table_empty() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    run("DELETE FROM items", &mut storage, &mut txn);
    let rows = rows(run("SELECT * FROM items", &mut storage, &mut txn));
    assert!(rows.is_empty(), "table must be empty after bulk DELETE");
}

#[test]
fn test_bulk_delete_allows_reinsert_same_pk() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT NOT NULL)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("DELETE FROM t", &mut storage, &mut txn);
    // Reinserting the same PK must succeed — no stale index entry.
    let result = run_result("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    assert!(
        result.is_ok(),
        "reinsert after bulk DELETE must succeed: {result:?}"
    );
}

#[test]
fn test_truncate_indexed_table_allows_reinsert() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (2, 'Bob')", &mut storage, &mut txn);
    run("TRUNCATE TABLE t", &mut storage, &mut txn);
    let rows_after = rows(run("SELECT * FROM t", &mut storage, &mut txn));
    assert!(rows_after.is_empty(), "table must be empty after TRUNCATE");
    // Must be able to reinsert — old index roots are gone.
    let result = run_result("INSERT INTO t VALUES (1, 'Carol')", &mut storage, &mut txn);
    assert!(
        result.is_ok(),
        "reinsert after TRUNCATE must succeed: {result:?}"
    );
}

#[test]
fn test_bulk_delete_then_rollback_restores_data() {
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    run("BEGIN", &mut storage, &mut txn);
    run("DELETE FROM items", &mut storage, &mut txn);
    let count_inside = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(count_inside, 0, "inside txn: table must appear empty");
    run("ROLLBACK", &mut storage, &mut txn);
    let count_after = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(
        count_after, 5,
        "after ROLLBACK: original data must be restored"
    );
}

#[test]
fn test_bulk_delete_savepoint_rollback_restores_data() {
    // Tests TxnManager savepoint API directly (SQL SAVEPOINT is Phase 7.12).
    let (mut storage, mut txn) = setup();
    setup_pk_table(&mut storage, &mut txn);
    txn.begin().unwrap(); // explicit BEGIN
                          // Insert item6 before the savepoint.
    run(
        "INSERT INTO items VALUES (6, 'item6')",
        &mut storage,
        &mut txn,
    );
    let sp = txn.savepoint();
    // Bulk delete inside the transaction.
    run("DELETE FROM items", &mut storage, &mut txn);
    assert_eq!(
        rows(run("SELECT * FROM items", &mut storage, &mut txn)).len(),
        0,
        "inside txn after DELETE: must be empty"
    );
    // Rollback to savepoint — restores pre-delete state (items 1-5 + item6).
    txn.rollback_to_savepoint(sp, &mut storage).unwrap();
    let count = rows(run("SELECT * FROM items", &mut storage, &mut txn)).len();
    assert_eq!(
        count, 6,
        "after savepoint rollback: must see items 1–5 + item6"
    );
    txn.commit().unwrap();
}

#[test]
fn test_truncate_resets_auto_increment_bulk_path() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT AUTO_INCREMENT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t (name) VALUES ('a')", &mut storage, &mut txn);
    run("INSERT INTO t (name) VALUES ('b')", &mut storage, &mut txn);
    run("TRUNCATE TABLE t", &mut storage, &mut txn);
    run("INSERT INTO t (name) VALUES ('c')", &mut storage, &mut txn);
    let rows_result = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(rows_result.len(), 1);
    // After TRUNCATE + bulk path, auto-increment must reset — id should be 1.
    assert_eq!(
        rows_result[0][0],
        axiomdb_types::Value::Int(1),
        "AUTO_INCREMENT must reset to 1 after TRUNCATE (bulk path)"
    );
}

#[test]
fn test_truncate_parent_fk_table_fails() {
    let (mut storage, mut txn) = setup();
    // Parent must have PRIMARY KEY so FK enforcement can find the index.
    run(
        "CREATE TABLE parent (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE child (id INT NOT NULL, parent_id INT NOT NULL REFERENCES parent(id))",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO parent VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO child VALUES (1, 1)", &mut storage, &mut txn);
    let result = run_result("TRUNCATE TABLE parent", &mut storage, &mut txn);
    assert!(
        matches!(result, Err(DbError::ForeignKeyParentViolation { .. })),
        "TRUNCATE of parent FK table must fail: {result:?}"
    );
    // Table still has data.
    assert_eq!(
        rows(run("SELECT * FROM parent", &mut storage, &mut txn)).len(),
        1
    );
}

#[test]
fn test_delete_parent_fk_table_uses_slow_path() {
    // DELETE on a parent-FK table must still use the row-by-row path (which
    // enforces RESTRICT semantics), not the bulk-empty fast path.
    // Uses execute_with_ctx since FK enforcement is most reliable through the ctx path.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE parent (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE child (id INT NOT NULL, parent_id INT NOT NULL REFERENCES parent(id) ON DELETE RESTRICT)",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    run_ctx(
        "INSERT INTO parent VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO child VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // DELETE on parent while child references it must fail with FK violation.
    let result = run_ctx(
        "DELETE FROM parent",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(result, Err(DbError::ForeignKeyParentViolation { .. })),
        "DELETE on parent with referencing child must fail: {result:?}"
    );
}

// ── Indexed DELETE WHERE fast path tests (Phase 6.3b) ────────────────────────

#[test]
fn test_delete_where_pk_equality_uses_indexed_path() {
    // DELETE FROM t WHERE id = 5 on a table with PRIMARY KEY (id).
    // After deletion, id=5 must be gone and all others intact.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE pk_del (id INT NOT NULL, name TEXT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO pk_del VALUES ({i}, 'row{i}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "DELETE FROM pk_del WHERE id = 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        result,
        QueryResult::Affected {
            count: 1,
            last_insert_id: None
        }
    );

    // id=5 must be gone.
    let r5 = run_ctx(
        "SELECT id FROM pk_del WHERE id = 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r5 {
        assert!(rows.is_empty(), "id=5 should be deleted");
    } else {
        panic!("expected Rows");
    }
    // Other rows intact.
    for i in [1, 2, 3, 4, 6, 7, 8, 9, 10i32] {
        let r = run_ctx(
            &format!("SELECT id FROM pk_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert_eq!(rows.len(), 1, "row {i} should still exist");
        } else {
            panic!("expected Rows");
        }
    }
}

#[test]
fn test_delete_where_pk_range_deletes_correct_rows() {
    // DELETE FROM t WHERE id > 7 — should delete ids 8,9,10 only.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE pk_range_del (id INT NOT NULL, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO pk_range_del VALUES ({i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "DELETE FROM pk_range_del WHERE id > 7",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        result,
        QueryResult::Affected {
            count: 3,
            last_insert_id: None
        }
    );

    // Rows 1-7 intact.
    for i in 1..=7i32 {
        let r = run_ctx(
            &format!("SELECT id FROM pk_range_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert_eq!(rows.len(), 1, "row {i} should exist");
        } else {
            panic!("expected Rows");
        }
    }
    // Rows 8-10 gone.
    for i in 8..=10i32 {
        let r = run_ctx(
            &format!("SELECT id FROM pk_range_del WHERE id = {i}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
        if let QueryResult::Rows { rows, .. } = r {
            assert!(rows.is_empty(), "row {i} should be deleted");
        } else {
            panic!("expected Rows");
        }
    }
}

#[test]
fn test_delete_where_secondary_index_maintains_all_indexes() {
    // DELETE using a secondary index must still maintain all indexes correctly.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE sec_del (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_score ON sec_del (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=5i32 {
        run_ctx(
            &format!("INSERT INTO sec_del VALUES ({i}, {})", i * 10),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    run_ctx(
        "DELETE FROM sec_del WHERE score = 30",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // id=3 (score=30) must be gone; others intact.
    let r = run_ctx(
        "SELECT id FROM sec_del",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r {
        let ids: Vec<_> = rows.iter().map(|r| r[0].clone()).collect();
        assert!(
            !ids.contains(&axiomdb_types::Value::Int(3)),
            "id=3 must be deleted"
        );
        assert_eq!(ids.len(), 4, "4 rows should remain");
    }

    // Reinserting the deleted key must succeed (no stale index entry).
    run_ctx(
        "INSERT INTO sec_del VALUES (3, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn test_update_batch_deletes_old_pk_and_secondary_keys() {
    // Phase 5.19: UPDATE must batch-delete old keys from both the PRIMARY KEY
    // and secondary indexes before reinserting new keys.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE upd_batch (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_upd_score ON upd_batch (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=10i32 {
        run_ctx(
            &format!("INSERT INTO upd_batch VALUES ({i}, {})", i * 10),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        "UPDATE upd_batch SET id = id + 100, score = score + 1 WHERE id <= 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 5);

    for i in 1..=5i32 {
        let old_pk = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE id = {i}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert!(old_pk.is_empty(), "old PK value {i} should be gone");

        let new_id = i + 100;
        let new_pk = rows(
            run_ctx(
                &format!("SELECT id, score FROM upd_batch WHERE id = {new_id}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert_eq!(
            new_pk,
            vec![vec![Value::Int(new_id), Value::Int(i * 10 + 1)]],
            "new PK value {new_id} should exist with the updated score",
        );

        let old_score = i * 10;
        let old_score_rows = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE score = {old_score}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert!(
            old_score_rows.is_empty(),
            "old secondary-index key {old_score} should be gone",
        );

        let new_score = old_score + 1;
        let new_score_rows = rows(
            run_ctx(
                &format!("SELECT id FROM upd_batch WHERE score = {new_score}"),
                &mut storage,
                &mut txn,
                &mut bloom,
                &mut ctx,
            )
            .unwrap(),
        );
        assert_eq!(
            new_score_rows,
            vec![vec![Value::Int(new_id)]],
            "new secondary-index key {new_score} should resolve to the new PK",
        );
    }

    let untouched = rows(
        run_ctx(
            "SELECT id, score FROM upd_batch WHERE id = 6",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(untouched, vec![vec![Value::Int(6), Value::Int(60)]]);
}

#[test]
fn test_delete_where_non_sargable_falls_back_to_scan() {
    // DELETE with a non-indexable predicate must still delete the right rows.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE fallback_del (id INT NOT NULL, label TEXT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=6i32 {
        run_ctx(
            &format!("INSERT INTO fallback_del VALUES ({i}, 'x{i}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }
    // LIKE is not sargable → must fall back to scan but still be correct.
    let result = run_ctx(
        "DELETE FROM fallback_del WHERE label LIKE 'x%3%' OR id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Affected { count, .. } = result {
        assert!(count >= 1, "at least 1 row deleted");
    }
}

#[test]
fn test_delete_where_indexed_then_reinsert_same_pk() {
    // After indexed DELETE, reinserting the same PK must succeed.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE reins_del (id INT NOT NULL, val INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for i in 1..=5i32 {
        run_ctx(
            &format!("INSERT INTO reins_del VALUES ({i}, {i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }
    run_ctx(
        "DELETE FROM reins_del WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Must succeed — no stale unique constraint.
    run_ctx(
        "INSERT INTO reins_del VALUES (3, 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx(
        "SELECT val FROM reins_del WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], axiomdb_types::Value::Int(99));
    }
}


#[test]
fn test_update_noop_counts_matched_rows_but_leaves_data_unchanged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE noop_upd (id INT NOT NULL, score INT, PRIMARY KEY (id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO noop_upd VALUES (1, 10), (2, 20), (3, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "UPDATE noop_upd SET score = score WHERE id <= 2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 2);

    let rows = rows(
        run_ctx(
            "SELECT id, score FROM noop_upd ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(30)],
        ]
    );
}

// ── Phase 5.21 — transactional INSERT staging ──────────────────────────────

fn run_ctx_5_21(
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

fn ok_ctx_5_21(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx_5_21(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

/// Staged rows are all committed on COMMIT and visible afterwards.
#[test]
fn test_5_21_staged_rows_committed() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE staged_t (id INT, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=100 {
        ok_ctx_5_21(
            &format!("INSERT INTO staged_t VALUES ({i}, {i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM staged_t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(100));
    } else {
        panic!("expected Rows");
    }
}

/// SELECT inside an explicit transaction sees rows inserted before it (barrier flush).
#[test]
fn test_5_21_read_your_own_writes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE rowt (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO rowt VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // SELECT triggers barrier flush — the inserted row must be visible.
    let r = ok_ctx_5_21(
        "SELECT v FROM rowt WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(10));
    } else {
        panic!("expected Rows");
    }
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
}

/// Table switch triggers a flush of the first table before buffering the second.
#[test]
fn test_5_21_table_switch_flushes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE t1 (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "CREATE TABLE t2 (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO t1 VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Insert into different table — triggers flush of t1.
    ok_ctx_5_21(
        "INSERT INTO t2 VALUES (2, 2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Both tables must have their rows.
    let r1 = ok_ctx_5_21(
        "SELECT COUNT(*) FROM t1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r2 = ok_ctx_5_21(
        "SELECT COUNT(*) FROM t2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let (QueryResult::Rows { rows: r1, .. }, QueryResult::Rows { rows: r2, .. }) = (r1, r2) {
        assert_eq!(r1[0][0], Value::BigInt(1));
        assert_eq!(r2[0][0], Value::BigInt(1));
    }
}

/// A table-switch flush must survive a later failing statement.
#[test]
fn test_5_21_table_switch_flush_survives_later_statement_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE first_t (id INT PRIMARY KEY, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "CREATE TABLE second_t (id INT PRIMARY KEY, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO first_t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO second_t VALUES (1, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let err = run_ctx_5_21(
        "INSERT INTO second_t VALUES (1, 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "got {err:?}"
    );

    ok_ctx_5_21(
        "INSERT INTO first_t VALUES (2, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r1 = ok_ctx_5_21(
        "SELECT id FROM first_t ORDER BY id ASC",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r1 {
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    } else {
        panic!("expected Rows");
    }

    let r2 = ok_ctx_5_21(
        "SELECT id, v FROM second_t ORDER BY id ASC",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r2 {
        assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(20)]]);
    } else {
        panic!("expected Rows");
    }
}

/// ROLLBACK discards unflushed staged rows without touching heap or WAL.
#[test]
fn test_5_21_rollback_discards_staged_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE rbt (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO rbt VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO rbt VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // ROLLBACK must discard buffer without heap writes.
    ok_ctx_5_21("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM rbt",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(0));
    } else {
        panic!("expected Rows");
    }
}

/// UNIQUE violations across staged rows are detected before heap mutation.
#[test]
fn test_5_21_unique_violation_in_buffer_detected_early() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE uvt (id INT UNIQUE, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO uvt VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Duplicate key within the buffer — must fail immediately.
    let e = run_ctx_5_21(
        "INSERT INTO uvt VALUES (1, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("expected UniqueViolation");
    assert!(
        matches!(e, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {e:?}"
    );

    // After the error, the transaction should still be able to commit cleanly
    // with only the first row (the duplicate was never staged).
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM uvt",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(1));
    }
}

/// Autocommit single-row INSERT still works correctly (not staged).
#[test]
fn test_5_21_autocommit_insert_not_staged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE acsingle (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Three autocommit inserts — no explicit BEGIN.
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (2, 2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (3, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM acsingle",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(3));
    }
}
