//! Phase 21.3b — WITH RECURSIVE executor.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn rows(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
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

#[test]
fn counter_1_to_10_union_all() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = run_ctx(
        "WITH RECURSIVE counter(n) AS (\
            SELECT 1 \
            UNION ALL \
            SELECT n + 1 FROM counter WHERE n < 10\
         ) SELECT n FROM counter",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    assert_eq!(r.len(), 10);
    for (i, row) in r.iter().enumerate() {
        assert_eq!(as_i64(&row[0]), (i + 1) as i64);
    }
}

#[test]
fn counter_base_only_when_step_excluded() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = run_ctx(
        "WITH RECURSIVE counter(n) AS (\
            SELECT 1 \
            UNION ALL \
            SELECT n + 1 FROM counter WHERE n < 0\
         ) SELECT n FROM counter",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 1);
}

#[test]
fn counter_union_dedup_collapses_duplicates() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Without ALL, duplicate (n=5 twice) collapses.
    let res = run_ctx(
        "WITH RECURSIVE counter(n) AS (\
            SELECT 1 \
            UNION \
            SELECT n + 1 FROM counter WHERE n < 5\
         ) SELECT n FROM counter",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    assert_eq!(r.len(), 5);
}

#[test]
fn outer_where_applies_to_recursive_result() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = run_ctx(
        "WITH RECURSIVE counter(n) AS (\
            SELECT 1 \
            UNION ALL \
            SELECT n + 1 FROM counter WHERE n < 10\
         ) SELECT n FROM counter WHERE n > 7",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    assert_eq!(r.len(), 3);
    assert_eq!(as_i64(&r[0][0]), 8);
    assert_eq!(as_i64(&r[2][0]), 10);
}

#[test]
fn tree_descendants_two_levels() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE emp (id INT, name TEXT, manager_id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // 1 → 2 → 3 (3 reports to 2, 2 reports to 1).
    run_ctx(
        "INSERT INTO emp VALUES (1, 'alice', 0), (2, 'bob', 1), (3, 'carol', 2), (4, 'dave', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let res = run_ctx(
        "WITH RECURSIVE sub(id, name, manager_id, depth) AS (\
            SELECT id, name, manager_id, 0 FROM emp WHERE id = 1 \
            UNION ALL \
            SELECT e.id, e.name, e.manager_id, s.depth + 1 \
              FROM emp e JOIN sub s ON e.manager_id = s.id\
         ) SELECT id FROM sub ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    // Alice (1) + Bob (2) + Carol (3) + Dave (4) = 4 rows.
    assert_eq!(r.len(), 4);
}

#[test]
fn max_recursion_exceeded_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    // No stop condition — runs forever until MAX_RECURSION caps it.
    let err = run_ctx(
        "WITH RECURSIVE counter(n) AS (\
            SELECT 1 \
            UNION ALL \
            SELECT n + 1 FROM counter\
         ) SELECT n FROM counter",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("MAX_RECURSION"), "got {msg}");
}

#[test]
fn non_recursive_cte_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx("CREATE TABLE t (id INT)", &mut s, &mut t, &mut b, &mut c).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let res = run_ctx(
        "WITH a AS (SELECT id FROM t WHERE id > 1) SELECT * FROM a",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(res);
    assert_eq!(r.len(), 2);
}
