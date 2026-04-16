//! Phase 21.9 — LATERAL subquery JOIN integration tests.
//!
//! Covers: basic LATERAL inner join, LEFT JOIN, CROSS JOIN, LATERAL as first
//! FROM source, chained LATERAL subqueries, outer reference access, and
//! rejection of RIGHT/FULL LATERAL JOINs.

mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

/// Insertion-sort rows by the i-th column (ascending) without requiring Ord on Value.
/// We compare by_i64, which panics if the value is not an integer.
fn sort_by_col(rows: &mut [Vec<Value>], col: usize) {
    for i in 1..rows.len() {
        for j in 0..rows.len() - i {
            let a = as_i64(&rows[j][col]);
            let b = as_i64(&rows[j + 1][col]);
            if a > b {
                rows.swap(j, j + 1);
            }
        }
    }
}

// ── basic_lateral_inner ─────────────────────────────────────────────────────

#[test]
fn basic_lateral_inner() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    // t(id): 1, 2, 3
    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // other(t_id, val): (1,10), (2,20)
    run_ctx(
        "CREATE TABLE other (t_id INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO other VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // SELECT t.id, sub.val
    // FROM t, LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub
    // Expected: (1, 11), (2, 12)
    // Note: t.id=3 has no matching row in `other`, so it is excluded (INNER join)
    let result = run_ctx(
        "SELECT t.id, sub.val FROM t, LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 2, "expected 2 rows, got {r:?}");
    // Sort by t.id for deterministic assertion
    let mut sorted = r.clone();
    sort_by_col(&mut sorted, 0);
    assert_eq!(as_i64(&sorted[0][0]), 1);
    assert_eq!(as_i64(&sorted[0][1]), 11);
    assert_eq!(as_i64(&sorted[1][0]), 2);
    assert_eq!(as_i64(&sorted[1][1]), 12);
}

// ── lateral_left_join_null_pad ───────────────────────────────────────────────

#[test]
fn lateral_left_join_null_pad() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    run_ctx(
        "CREATE TABLE other (t_id INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO other VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // LEFT JOIN: t.id=3 has no match, so sub.val is NULL
    let result = run_ctx(
        "SELECT t.id, sub.val FROM t LEFT JOIN LATERAL (SELECT t.id + 10 AS val FROM other o WHERE o.t_id = t.id) sub ON true",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 3, "expected 3 rows, got {r:?}");
    let mut sorted = r.clone();
    sort_by_col(&mut sorted, 0);

    assert_eq!(as_i64(&sorted[0][0]), 1);
    assert_eq!(as_i64(&sorted[0][1]), 11);
    assert_eq!(as_i64(&sorted[1][0]), 2);
    assert_eq!(as_i64(&sorted[1][1]), 12);
    assert_eq!(as_i64(&sorted[2][0]), 3);
    assert!(
        matches!(&sorted[2][1], Value::Null),
        "expected NULL for t.id=3, got {:?}",
        sorted[2][1]
    );
}

// ── lateral_cross_join ───────────────────────────────────────────────────────

#[test]
fn lateral_cross_join() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // CROSS JOIN LATERAL: each left row joined with all subquery results
    // SELECT t.id, sub.x
    // FROM t CROSS JOIN LATERAL (SELECT 1 AS x UNION ALL SELECT 2) sub
    // Expected: (1,1),(1,2),(2,1),(2,2),(3,1),(3,2)
    let result = run_ctx(
        "SELECT t.id, sub.x FROM t CROSS JOIN LATERAL (SELECT 1 AS x UNION ALL SELECT 2) sub",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 6, "expected 6 rows, got {r:?}");

    let mut ids: Vec<i64> = r.iter().map(|row| as_i64(&row[0])).collect();
    ids.sort();
    // Each of 3 left rows appears twice (once per x value)
    assert_eq!(ids, vec![1, 1, 2, 2, 3, 3]);

    let mut xs: Vec<i64> = r.iter().map(|row| as_i64(&row[1])).collect();
    xs.sort();
    assert_eq!(xs, vec![1, 1, 1, 2, 2, 2]);
}

// ── lateral_first_from ───────────────────────────────────────────────────────

#[test]
fn lateral_first_from() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    // LATERAL as first FROM source (no left tables)
    // SELECT * FROM LATERAL (SELECT 1 AS x, 2 AS y) AS init
    // Expected: (1, 2)
    let result = run_ctx(
        "SELECT * FROM LATERAL (SELECT 1 AS x, 2 AS y) AS init",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 1, "expected 1 row, got {r:?}");
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_i64(&r[0][1]), 2);
}

// ── lateral_chain ───────────────────────────────────────────────────────────

#[test]
fn lateral_chain() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    // Two LATERAL in chain, second references first
    // SELECT * FROM (VALUES (1)) t(v), LATERAL (SELECT t.v + 10 AS a) s1, LATERAL (SELECT s1.a * 2 AS b) s2
    // Expected: (1, 11, 22)
    let result = run_ctx(
        "SELECT * FROM (VALUES (1)) t(v), LATERAL (SELECT t.v + 10 AS a) s1, LATERAL (SELECT s1.a * 2 AS b) s2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 1, "expected 1 row, got {r:?}");
    assert_eq!(as_i64(&r[0][0]), 1); // v = 1
    assert_eq!(as_i64(&r[0][1]), 11); // a = t.v + 10
    assert_eq!(as_i64(&r[0][2]), 22); // b = s1.a * 2
}

// ── lateral_outer_ref ───────────────────────────────────────────────────────

#[test]
fn lateral_outer_ref() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx(
        "CREATE TABLE t (a INT, b INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO t VALUES (1, 2), (3, 4)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // Subquery references multiple columns from left table
    // SELECT sub.sum_ab FROM t, LATERAL (SELECT t.a + t.b AS sum_ab) sub
    // Expected: (3), (7)
    let result = run_ctx(
        "SELECT sub.sum_ab FROM t, LATERAL (SELECT t.a + t.b AS sum_ab) sub",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("query should succeed");

    let r = rows(result);
    assert_eq!(r.len(), 2, "expected 2 rows, got {r:?}");
    let mut sums: Vec<i64> = r.iter().map(|row| as_i64(&row[0])).collect();
    sums.sort();
    assert_eq!(sums, vec![3, 7]);
}

// ── lateral_right_rejected ───────────────────────────────────────────────────

#[test]
fn lateral_right_rejected() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx("INSERT INTO t VALUES (1)", &mut s, &mut t, &mut b, &mut c);

    // RIGHT JOIN LATERAL → NotImplemented
    let err = run_ctx(
        "SELECT t.id FROM t RIGHT JOIN LATERAL (SELECT 1) sub ON true",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();

    let msg = format!("{err:?}");
    assert!(
        msg.contains("NotImplemented") || msg.contains("RIGHT"),
        "expected NotImplemented error, got: {msg}"
    );
}

// ── lateral_full_rejected ───────────────────────────────────────────────────

#[test]
fn lateral_full_rejected() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx("INSERT INTO t VALUES (1)", &mut s, &mut t, &mut b, &mut c);

    // FULL JOIN LATERAL → NotImplemented
    let err = run_ctx(
        "SELECT t.id FROM t FULL JOIN LATERAL (SELECT 1) sub ON true",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();

    let msg = format!("{err:?}");
    assert!(
        msg.contains("NotImplemented") || msg.contains("FULL"),
        "expected NotImplemented error, got: {msg}"
    );
}

// ── lateral_update_join ─────────────────────────────────────────────────────

#[test]
fn lateral_update_join() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    // target(id, val): rows to update
    run_ctx(
        "CREATE TABLE target (id INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO target VALUES (1, 0), (2, 0), (3, 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // multiplier(tid, factor): only id=1 and id=2 have a factor
    run_ctx(
        "CREATE TABLE multiplier (tid INT, factor INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO multiplier VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // UPDATE target t JOIN LATERAL (...) sub ON true SET t.val = sub.computed
    // sub computes target.id * factor for matching rows
    run_ctx(
        "UPDATE target t JOIN LATERAL (SELECT m.factor * t.id AS computed FROM multiplier m WHERE m.tid = t.id) sub ON true SET t.val = sub.computed",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("UPDATE with LATERAL JOIN should succeed");

    // Verify: id=1 → val=10, id=2 → val=40, id=3 → val=0 (no match, not updated)
    let result = run_ctx(
        "SELECT id, val FROM target ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("SELECT after UPDATE");
    let r = rows(result);
    assert_eq!(r.len(), 3, "expected 3 rows, got {r:?}");
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_i64(&r[0][1]), 10); // 1 * 10
    assert_eq!(as_i64(&r[1][0]), 2);
    assert_eq!(as_i64(&r[1][1]), 40); // 2 * 20
    assert_eq!(as_i64(&r[2][0]), 3);
    assert_eq!(as_i64(&r[2][1]), 0); // no match in multiplier → not updated
}

// ── lateral_delete_join ─────────────────────────────────────────────────────

#[test]
fn lateral_delete_join() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx(
        "CREATE TABLE target (id INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO target VALUES (1, 100), (2, 200), (3, 300)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // sentinel: only ids 1 and 2 should be deleted
    run_ctx(
        "CREATE TABLE to_delete (tid INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run_ctx(
        "INSERT INTO to_delete VALUES (1), (2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // DELETE t FROM target t JOIN LATERAL (...) sub ON true
    run_ctx(
        "DELETE t FROM target t JOIN LATERAL (SELECT d.tid AS match_id FROM to_delete d WHERE d.tid = t.id) sub ON true",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("DELETE with LATERAL JOIN should succeed");

    // Only id=3 should remain
    let result = run_ctx("SELECT id, val FROM target", &mut s, &mut t, &mut b, &mut c)
        .expect("SELECT after DELETE");
    let r = rows(result);
    assert_eq!(r.len(), 1, "expected 1 row remaining, got {r:?}");
    assert_eq!(as_i64(&r[0][0]), 3);
    assert_eq!(as_i64(&r[0][1]), 300);
}

// ── regression_non_lateral_subquery ─────────────────────────────────────────

#[test]
fn regression_non_lateral_subquery() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();

    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c);
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );

    // Existing subquery JOIN without LATERAL still works.
    // sub only exposes column x (= id + 10); use sub.x - 10 in the ON condition.
    // t.id=1 → sub.x=11, t.id=2 → sub.x=12, t.id=3 → sub.x=13.
    let result = run_ctx(
        "SELECT t.id, sub.x FROM t JOIN (SELECT id + 10 AS x FROM t) sub ON t.id = sub.x - 10",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect("non-LATERAL subquery JOIN should work");

    let r = rows(result);
    assert_eq!(r.len(), 3, "expected 3 rows, got {r:?}");
    let mut sorted = r.clone();
    sort_by_col(&mut sorted, 0);
    assert_eq!(as_i64(&sorted[0][0]), 1);
    assert_eq!(as_i64(&sorted[0][1]), 11);
    assert_eq!(as_i64(&sorted[1][0]), 2);
    assert_eq!(as_i64(&sorted[1][1]), 12);
    assert_eq!(as_i64(&sorted[2][0]), 3);
    assert_eq!(as_i64(&sorted[2][1]), 13);
}
