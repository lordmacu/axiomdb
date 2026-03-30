//! Integration tests for join and aggregate execution in the core executor.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

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
