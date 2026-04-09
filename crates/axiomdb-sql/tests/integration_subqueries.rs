//! Integration tests for subqueries (Phase 4.11).
//!
//! Covers: scalar subqueries, IN subquery, EXISTS/NOT EXISTS, correlated
//! subqueries, derived tables, NULL semantics, and cardinality violation.
//!
//! Full pipeline: parse → analyze → execute with MemoryStorage + WAL.

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn run(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> QueryResult {
    run_result(sql, storage, txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn run_result(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.snapshot();
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn single_value(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> Value {
    let r = rows(run(sql, storage, txn));
    assert_eq!(r.len(), 1, "expected 1 row, got {}", r.len());
    assert_eq!(r[0].len(), 1, "expected 1 column, got {}", r[0].len());
    r[0][0].clone()
}

/// Initialises a test schema with two tables:
///
/// users (id INT, name TEXT, role TEXT)
/// orders (id INT, user_id INT, total INT)
fn setup_schema(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "CREATE TABLE users (id INT, name TEXT, role TEXT)",
        storage,
        txn,
    );
    run(
        "CREATE TABLE orders (id INT, user_id INT, total INT)",
        storage,
        txn,
    );
}

fn insert_users(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run(
        "INSERT INTO users VALUES (1, 'Alice', 'admin')",
        storage,
        txn,
    );
    run("INSERT INTO users VALUES (2, 'Bob', 'user')", storage, txn);
    run("INSERT INTO users VALUES (3, 'Carol', 'vip')", storage, txn);
}

fn insert_orders(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    run("INSERT INTO orders VALUES (10, 1, 500)", storage, txn);
    run("INSERT INTO orders VALUES (11, 1, 1500)", storage, txn);
    run("INSERT INTO orders VALUES (12, 2, 200)", storage, txn);
    // user 3 (Carol) has no orders
}

// ── Uncorrelated scalar subquery ──────────────────────────────────────────────

#[test]
fn scalar_subquery_in_where() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // MAX(user_id) = 2 → only orders with user_id > 1 → order 11 (user 1 with total 1500) and 12
    // Actually: MAX(user_id in orders) = 2, so user_id > 2 → no rows
    // Let's use: user_id = (SELECT MIN(user_id) FROM orders) → user_id = 1
    let r = rows(run(
        "SELECT id FROM orders WHERE user_id = (SELECT MIN(user_id) FROM orders)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    // Orders 10 and 11 have user_id=1
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert!(ids.contains(&10));
    assert!(ids.contains(&11));
}

#[test]
fn scalar_subquery_in_select_list() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // SELECT name, (SELECT MAX(total) FROM orders) AS max_total FROM users WHERE id = 1
    let r = rows(run(
        "SELECT name, (SELECT MAX(total) FROM orders) FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("Alice".into()));
    assert_eq!(r[0][1], Value::Int(1500));
}

#[test]
fn scalar_subquery_zero_rows_returns_null() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);

    // Empty table → subquery returns 0 rows → NULL
    let v = single_value(
        "SELECT (SELECT id FROM users WHERE id = 9999)",
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Null);
}

#[test]
fn scalar_subquery_multiple_rows_returns_cardinality_violation() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);

    // users has 3 rows → scalar subquery must fail
    let err = run_result("SELECT (SELECT id FROM users)", &mut storage, &mut txn);
    match err {
        Err(DbError::CardinalityViolation { count }) => assert_eq!(count, 3),
        other => panic!("expected CardinalityViolation, got {other:?}"),
    }
}

// ── IN subquery ───────────────────────────────────────────────────────────────

#[test]
fn in_subquery_match_returns_true() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // Users whose id appears in orders.user_id → users 1 and 2
    let r = rows(run(
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
}

#[test]
fn in_subquery_no_match_returns_false() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // Carol (id=3) is not in orders → NOT IN matches her
    let r = rows(run(
        "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}

#[test]
fn in_subquery_null_in_result_propagates_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (val INT)", &mut storage, &mut txn);
    // Insert NULL and some values
    run("INSERT INTO t VALUES (1)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (NULL)", &mut storage, &mut txn);

    // SELECT 5 IN (SELECT val FROM t) → no match + NULL in set → NULL (UNKNOWN)
    let v = single_value("SELECT 5 IN (SELECT val FROM t)", &mut storage, &mut txn);
    assert_eq!(v, Value::Null);

    // SELECT 1 IN (SELECT val FROM t) → match found → TRUE (ignores NULL)
    let v = single_value("SELECT 1 IN (SELECT val FROM t)", &mut storage, &mut txn);
    assert_eq!(v, Value::Bool(true));
}

// ── EXISTS / NOT EXISTS ───────────────────────────────────────────────────────

#[test]
fn exists_correlated_returns_true_when_rows_found() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // Users that have at least one order
    let r = rows(run(
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
}

#[test]
fn not_exists_correlated() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // Users with no orders → Carol (id=3)
    let r = rows(run(
        "SELECT id FROM users WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}

#[test]
fn exists_never_null() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE empty_t (id INT)", &mut storage, &mut txn);

    // EXISTS on empty table → FALSE, never NULL
    let v = single_value(
        "SELECT EXISTS (SELECT 1 FROM empty_t)",
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Bool(false));

    // NOT EXISTS on empty table → TRUE
    let v = single_value(
        "SELECT NOT EXISTS (SELECT 1 FROM empty_t)",
        &mut storage,
        &mut txn,
    );
    // NOT EXISTS parses as Exists { negated: true }, result = TRUE
    assert_eq!(v, Value::Bool(true));
}

// ── Correlated scalar in SELECT list ─────────────────────────────────────────

#[test]
fn correlated_scalar_in_select_list() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.user_id = users.id) FROM users ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    // Alice (id=1) has 2 orders
    assert_eq!(r[0][1], Value::BigInt(2));
    // Bob (id=2) has 1 order
    assert_eq!(r[1][1], Value::BigInt(1));
    // Carol (id=3) has 0 orders
    assert_eq!(r[2][1], Value::BigInt(0));
}

// ── Derived table in FROM ─────────────────────────────────────────────────────

#[test]
fn derived_table_basic() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // SELECT * from a derived table that filters orders
    let r = rows(run(
        "SELECT id FROM (SELECT id, total FROM orders WHERE total > 400) AS big_orders",
        &mut storage,
        &mut txn,
    ));
    // Orders with total > 400: order 10 (500) and 11 (1500)
    assert_eq!(r.len(), 2);
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert!(ids.contains(&10));
    assert!(ids.contains(&11));
}

#[test]
fn derived_table_with_group_by_inside() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // Derived table aggregates; outer query selects from it
    let r = rows(run(
        "SELECT user_id FROM (SELECT user_id, SUM(total) FROM orders GROUP BY user_id) AS totals WHERE user_id = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

// ── Derived table in JOIN ─────────────────────────────────────────────────────

#[test]
fn derived_table_in_join_basic() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT big.total \
         FROM users \
         JOIN (SELECT user_id, total FROM orders WHERE total > 400) AS big \
         ON big.user_id = users.id \
         ORDER BY big.total",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(r, vec![vec![Value::Int(500)], vec![Value::Int(1500)]]);
}

#[test]
fn derived_table_in_left_join_preserves_unmatched_rows() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT users.id, stats.order_count \
         FROM users \
         LEFT JOIN ( \
             SELECT user_id, COUNT(*) AS order_count \
             FROM orders \
             GROUP BY user_id \
         ) AS stats ON stats.user_id = users.id \
         ORDER BY users.id",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::BigInt(2)],
            vec![Value::Int(2), Value::BigInt(1)],
            vec![Value::Int(3), Value::Null],
        ]
    );
}

#[test]
fn derived_table_in_right_join_preserves_unmatched_rows() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);
    run(
        "INSERT INTO orders VALUES (13, 99, 50)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT users.id, stats.user_id, stats.order_count \
         FROM users \
         RIGHT JOIN ( \
             SELECT user_id, COUNT(*) AS order_count \
             FROM orders \
             GROUP BY user_id \
         ) AS stats ON stats.user_id = users.id \
         ORDER BY stats.user_id",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(1), Value::BigInt(2)],
            vec![Value::Int(2), Value::Int(2), Value::BigInt(1)],
            vec![Value::Null, Value::Int(99), Value::BigInt(1)],
        ]
    );
}

#[test]
fn derived_table_in_full_join_preserves_unmatched_rows_from_both_sides() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);
    run(
        "INSERT INTO orders VALUES (13, 99, 50)",
        &mut storage,
        &mut txn,
    );

    let mut r = rows(run(
        "SELECT users.id, stats.user_id, stats.order_count \
         FROM users \
         FULL JOIN ( \
             SELECT user_id, COUNT(*) AS order_count \
             FROM orders \
             GROUP BY user_id \
         ) AS stats ON stats.user_id = users.id",
        &mut storage,
        &mut txn,
    ));

    r.sort_by_key(|row| {
        let left = match &row[0] {
            Value::Int(n) => *n,
            Value::Null => i32::MAX - 1,
            other => panic!("expected INT/NULL for users.id, got {other:?}"),
        };
        let right = match &row[1] {
            Value::Int(n) => *n,
            Value::Null => i32::MAX,
            other => panic!("expected INT/NULL for stats.user_id, got {other:?}"),
        };
        (left, right)
    });

    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(1), Value::BigInt(2)],
            vec![Value::Int(2), Value::Int(2), Value::BigInt(1)],
            vec![Value::Int(3), Value::Null, Value::Null],
            vec![Value::Null, Value::Int(99), Value::BigInt(1)],
        ]
    );
}

#[test]
fn derived_table_in_join_using_resolves_names() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users2 (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "CREATE TABLE archived_users (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users2 VALUES (1, 'Alice'), (2, 'Bob')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO archived_users VALUES (1, 'Alicia'), (3, 'Carol')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT old_users.name \
         FROM users2 \
         JOIN (SELECT id, name FROM archived_users) AS old_users USING (id)",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(r, vec![vec![Value::Text("Alicia".into())]]);
}

#[test]
fn derived_table_in_join_alias_wildcard_returns_derived_columns() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    let result = run_result(
        "SELECT stats.* \
         FROM users \
         JOIN ( \
             SELECT user_id, COUNT(*) AS order_count \
             FROM orders \
             GROUP BY user_id \
         ) AS stats ON stats.user_id = users.id \
         ORDER BY stats.user_id",
        &mut storage,
        &mut txn,
    )
    .unwrap();

    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(
                columns.len(),
                2,
                "stats.* should expose only derived columns"
            );
            assert_eq!(columns[0].name, "user_id");
            assert_eq!(columns[1].name, "order_count");
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1), Value::BigInt(2)],
                    vec![Value::Int(2), Value::BigInt(1)],
                ]
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn derived_table_join_chain_mixes_base_and_derived_sources() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    let r = rows(run(
        "SELECT users.id, stats.order_count, admins.role \
         FROM users \
         LEFT JOIN ( \
             SELECT user_id, COUNT(*) AS order_count \
             FROM orders \
             GROUP BY user_id \
         ) AS stats ON stats.user_id = users.id \
         LEFT JOIN ( \
             SELECT id, role \
             FROM users \
             WHERE role = 'admin' \
         ) AS admins ON admins.id = users.id \
         ORDER BY users.id",
        &mut storage,
        &mut txn,
    ));

    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::BigInt(2), Value::Text("admin".into())],
            vec![Value::Int(2), Value::BigInt(1), Value::Null],
            vec![Value::Int(3), Value::Null, Value::Null],
        ]
    );
}

// ── Nested subqueries (2 levels) ──────────────────────────────────────────────

#[test]
fn nested_uncorrelated_subquery() {
    let (mut storage, mut txn) = setup();
    setup_schema(&mut storage, &mut txn);
    insert_users(&mut storage, &mut txn);
    insert_orders(&mut storage, &mut txn);

    // WHERE user_id IN (subquery with its own scalar subquery in WHERE)
    let r = rows(run(
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > (SELECT AVG(total) FROM orders))",
        &mut storage,
        &mut txn,
    ));
    // AVG(500, 1500, 200) = 733, orders with total > 733: order 11 (user_id=1)
    // So: users with id IN {1} → Alice
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}
