/// Integration tests for subfase 39.21 — Aggregate Hash Execution
///
/// Covers: GroupTablePrimitive (INT GROUP BY), GroupTableGeneric (TEXT / multi-col),
/// NULL group, AVG with all-NULL input, non-aggregate column in SELECT
/// (non_agg_col_indices path), HAVING with non-agg column, and mixed-type SUM.
mod common;

use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::{rows, run, setup};

// ── group by INT column (GroupTablePrimitive path) ────────────────────────────

#[test]
fn test_group_by_int_count_avg() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, age INT NOT NULL, score DOUBLE NOT NULL)",
        &mut s,
        &mut t,
    );
    for i in 1i32..=100 {
        let age = 20 + (i % 5); // 5 distinct ages: 21..25
        let score = i as f64;
        run(
            &format!("INSERT INTO t VALUES ({i}, {age}, {score})"),
            &mut s,
            &mut t,
        );
    }
    let result = run(
        "SELECT age, COUNT(*) AS c, AVG(score) AS a FROM t GROUP BY age ORDER BY age",
        &mut s,
        &mut t,
    );
    let r = rows(result);
    assert_eq!(r.len(), 5, "expected 5 distinct ages");
    for row in &r {
        assert_eq!(row[1], Value::BigInt(20), "COUNT should be 20 per group");
    }
}

// ── COUNT(*) on empty table emits one row ─────────────────────────────────────

#[test]
fn test_count_star_empty_table() {
    let (mut s, mut t) = setup();
    run("CREATE TABLE empty (id INT NOT NULL)", &mut s, &mut t);
    let r = rows(run("SELECT COUNT(*) FROM empty", &mut s, &mut t));
    assert_eq!(r, vec![vec![Value::BigInt(0)]]);
}

// ── MIN / MAX — no GROUP BY ───────────────────────────────────────────────────

#[test]
fn test_min_max_no_group_by() {
    let (mut s, mut t) = setup();
    run("CREATE TABLE t (v INT NOT NULL)", &mut s, &mut t);
    for v in [5i32, 2, 8, 1, 9, 3] {
        run(&format!("INSERT INTO t VALUES ({v})"), &mut s, &mut t);
    }
    let r = rows(run("SELECT MIN(v), MAX(v) FROM t", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Int(9));
}

// ── HAVING SUM filter ─────────────────────────────────────────────────────────

#[test]
fn test_having_sum_filter() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE emp (dept TEXT NOT NULL, salary INT NOT NULL)",
        &mut s,
        &mut t,
    );
    for (dept, sal) in [("eng", 1200), ("eng", 900), ("hr", 400), ("hr", 300)] {
        run(
            &format!("INSERT INTO emp VALUES ('{dept}', {sal})"),
            &mut s,
            &mut t,
        );
    }
    let r = rows(run(
        "SELECT dept, SUM(salary) FROM emp GROUP BY dept HAVING SUM(salary) > 1000 ORDER BY dept",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1, "only eng (total 2100) passes HAVING");
    assert_eq!(r[0][0], Value::Text("eng".into()));
    let sum_val = match r[0][1] {
        Value::Int(v) => v as i64,
        Value::BigInt(v) => v,
        ref other => panic!("unexpected sum type: {other:?}"),
    };
    assert_eq!(sum_val, 2100);
}

// ── NULL values in GROUP BY column form their own group ───────────────────────

#[test]
fn test_null_group_by_column() {
    let (mut s, mut t) = setup();
    run("CREATE TABLE t (age INT)", &mut s, &mut t);
    run("INSERT INTO t VALUES (NULL)", &mut s, &mut t);
    run("INSERT INTO t VALUES (NULL)", &mut s, &mut t);
    run("INSERT INTO t VALUES (25)", &mut s, &mut t);
    let r = rows(run(
        "SELECT age, COUNT(*) FROM t GROUP BY age ORDER BY age",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 2, "NULL group + age=25 group");
    let total: i64 = r
        .iter()
        .map(|row| match row[1] {
            Value::BigInt(n) => n,
            Value::Int(n) => n as i64,
            _ => 0,
        })
        .sum();
    assert_eq!(total, 3);
}

// ── AVG with all-NULL input returns NULL ──────────────────────────────────────

#[test]
fn test_avg_all_null_returns_null() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (grp INT NOT NULL, score DOUBLE)",
        &mut s,
        &mut t,
    );
    run("INSERT INTO t VALUES (1, NULL)", &mut s, &mut t);
    run("INSERT INTO t VALUES (1, NULL)", &mut s, &mut t);
    let r = rows(run("SELECT AVG(score) FROM t", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Null, "AVG of all-NULL must return NULL");
}

// ── Non-aggregate column in SELECT exercises non_agg_col_indices path ─────────

#[test]
fn test_non_agg_col_in_select_with_group_by() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (name TEXT NOT NULL, age INT NOT NULL)",
        &mut s,
        &mut t,
    );
    run("INSERT INTO t VALUES ('Alice', 30)", &mut s, &mut t);
    run("INSERT INTO t VALUES ('Bob', 30)", &mut s, &mut t);
    run("INSERT INTO t VALUES ('Carol', 40)", &mut s, &mut t);
    let r = rows(run(
        "SELECT name, age, COUNT(*) FROM t GROUP BY age ORDER BY age",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 2);
    // age=40 group has exactly 1 row (Carol).
    let row_40 = r.iter().find(|row| row[1] == Value::Int(40)).unwrap();
    assert_eq!(row_40[2], Value::BigInt(1));
}

// ── SUM accumulation (INT) ────────────────────────────────────────────────────

#[test]
fn test_sum_int() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (cat INT NOT NULL, v INT NOT NULL)",
        &mut s,
        &mut t,
    );
    for i in 1i32..=10 {
        run(&format!("INSERT INTO t VALUES (1, {i})"), &mut s, &mut t);
    }
    let r = rows(run(
        "SELECT cat, SUM(v) FROM t GROUP BY cat",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    let sum_val = match r[0][1] {
        Value::Int(v) => v as i64,
        Value::BigInt(v) => v,
        ref other => panic!("unexpected type: {other:?}"),
    };
    assert_eq!(sum_val, 55);
}

// ── Multi-column GROUP BY uses GroupTableGeneric ──────────────────────────────

#[test]
fn test_multi_column_group_by() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (a INT NOT NULL, b TEXT NOT NULL, v INT NOT NULL)",
        &mut s,
        &mut t,
    );
    run("INSERT INTO t VALUES (1, 'x', 10)", &mut s, &mut t);
    run("INSERT INTO t VALUES (1, 'y', 20)", &mut s, &mut t);
    run("INSERT INTO t VALUES (2, 'x', 30)", &mut s, &mut t);
    run("INSERT INTO t VALUES (1, 'x', 5)", &mut s, &mut t);
    let r = rows(run(
        "SELECT a, b, SUM(v) FROM t GROUP BY a, b ORDER BY a, b",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 3);
    // (1, 'x') → 15
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("x".into()));
    let sum_val = match r[0][2] {
        Value::Int(v) => v as i64,
        Value::BigInt(v) => v,
        ref other => panic!("{other:?}"),
    };
    assert_eq!(sum_val, 15);
}

// ── TEXT GROUP BY uses GroupTableGeneric ──────────────────────────────────────

#[test]
fn test_text_group_by() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (dept TEXT NOT NULL, sal INT NOT NULL)",
        &mut s,
        &mut t,
    );
    run("INSERT INTO t VALUES ('eng', 100)", &mut s, &mut t);
    run("INSERT INTO t VALUES ('hr', 200)", &mut s, &mut t);
    run("INSERT INTO t VALUES ('eng', 150)", &mut s, &mut t);
    let r = rows(run(
        "SELECT dept, COUNT(*) FROM t GROUP BY dept ORDER BY dept",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::BigInt(2));
}

// ── HAVING referencing a non-aggregate column ─────────────────────────────────

#[test]
fn test_having_non_agg_column() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE t (age INT NOT NULL, score INT NOT NULL)",
        &mut s,
        &mut t,
    );
    for age in [20i32, 30, 40] {
        run(
            &format!("INSERT INTO t VALUES ({age}, 100)"),
            &mut s,
            &mut t,
        );
        run(
            &format!("INSERT INTO t VALUES ({age}, 200)"),
            &mut s,
            &mut t,
        );
    }
    let r = rows(run(
        "SELECT age, COUNT(*) FROM t GROUP BY age HAVING age > 25 ORDER BY age",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 2, "ages 30 and 40 pass HAVING age > 25");
    assert!(r.iter().all(|row| row[1] == Value::BigInt(2)));
}
