//! Integration tests for ORDER BY, DISTINCT, CASE, INSERT ... SELECT, and AUTO_INCREMENT.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

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
