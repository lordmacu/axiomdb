//! Phase 11.20d3 — LATERAL-correlated JSON_TABLE on the right side of a
//! JOIN / CROSS APPLY / OUTER APPLY. `doc` and/or PASSING expressions
//! reference outer columns; JSON_TABLE is re-materialized per outer row.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let res = run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    match res {
        QueryResult::Rows { rows, .. } => rows,
        _ => Vec::new(),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn as_text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s.as_str(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn seed_orders(
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) {
    run_ctx("CREATE TABLE orders (id INT, payload TEXT)", s, t, b, c).unwrap();
    run_ctx(
        r#"INSERT INTO orders VALUES
           (1, '{"items":[{"qty":1},{"qty":2}]}'),
           (2, '{"items":[{"qty":10}]}'),
           (3, '{"items":[]}')"#,
        s,
        t,
        b,
        c,
    )
    .unwrap();
}

// ── Correlated doc ──────────────────────────────────────────────────────────

#[test]
fn cross_apply_correlated_doc_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT o.id, j.qty
             FROM orders o
             CROSS APPLY JSON_TABLE(o.payload, '$.items[*]'
                          COLUMNS (qty INT PATH '$.qty')) AS j
             ORDER BY o.id, j.qty"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // id=1 → 2 rows (1, 2); id=2 → 1 row (10); id=3 → 0 rows (no APPLY match)
    assert_eq!(rows.len(), 3);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_i64(&rows[0][1]), 1);
    assert_eq!(as_i64(&rows[1][0]), 1);
    assert_eq!(as_i64(&rows[1][1]), 2);
    assert_eq!(as_i64(&rows[2][0]), 2);
    assert_eq!(as_i64(&rows[2][1]), 10);
}

#[test]
fn outer_apply_correlated_doc_empty_preserves_left() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT o.id, j.qty
             FROM orders o
             OUTER APPLY JSON_TABLE(o.payload, '$.items[*]'
                          COLUMNS (qty INT PATH '$.qty')) AS j
             ORDER BY o.id, j.qty"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // id=3 has empty items; OUTER APPLY preserves it with NULL.
    assert_eq!(rows.len(), 4);
    let id3 = rows.iter().find(|r| as_i64(&r[0]) == 3).unwrap();
    assert!(matches!(id3[1], Value::Null));
}

#[test]
fn inner_join_correlated_doc_with_on_condition() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT o.id, j.qty
             FROM orders o
             JOIN JSON_TABLE(o.payload, '$.items[*]'
                    COLUMNS (qty INT PATH '$.qty')) AS j
               ON j.qty > 1
             ORDER BY o.id, j.qty"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // id=1 → qty=2 only; id=2 → qty=10; id=3 → none.
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_i64(&rows[0][1]), 2);
    assert_eq!(as_i64(&rows[1][0]), 2);
    assert_eq!(as_i64(&rows[1][1]), 10);
}

#[test]
fn left_join_correlated_doc_null_pad() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT o.id, j.qty
             FROM orders o
             LEFT JOIN JSON_TABLE(o.payload, '$.items[*]'
                    COLUMNS (qty INT PATH '$.qty')) AS j
               ON j.qty >= 100
             ORDER BY o.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // No row matches j.qty >= 100 — LEFT JOIN preserves all outer rows.
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(matches!(r[1], Value::Null));
    }
}

// ── Correlated PASSING ──────────────────────────────────────────────────────

#[test]
fn passing_outer_column_into_filter() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE cfg (id INT, lo INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO cfg VALUES (1, 2), (2, 5)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let rows = run(
        r#"SELECT c.id, j.v
             FROM cfg c
             CROSS APPLY JSON_TABLE('[1,2,3,4,5,6]', '$[?(@ > $threshold)]'
                          PASSING c.lo AS threshold
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY c.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // id=1 (lo=2) → 3,4,5,6 (4 rows); id=2 (lo=5) → 6 (1 row).
    assert_eq!(rows.len(), 5);
    let id1_count = rows.iter().filter(|r| as_i64(&r[0]) == 1).count();
    let id2_count = rows.iter().filter(|r| as_i64(&r[0]) == 2).count();
    assert_eq!(id1_count, 4);
    assert_eq!(id2_count, 1);
}

#[test]
fn passing_two_outer_columns_into_range_filter() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE ranges (id INT, lo INT, hi INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO ranges VALUES (1, 2, 4), (2, 5, 9)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let rows = run(
        r#"SELECT r.id, j.v
             FROM ranges r
             CROSS APPLY JSON_TABLE('[1,2,3,4,5,6,7,8,9]',
                          '$[?(@ >= $lo && @ <= $hi)]'
                          PASSING r.lo AS lo, r.hi AS hi
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY r.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // id=1 [2..4] → 3; id=2 [5..9] → 5.
    let id1: Vec<_> = rows.iter().filter(|r| as_i64(&r[0]) == 1).collect();
    let id2: Vec<_> = rows.iter().filter(|r| as_i64(&r[0]) == 2).collect();
    assert_eq!(id1.len(), 3);
    assert_eq!(id2.len(), 5);
}

// ── NULL / NESTED / WRAPPER ─────────────────────────────────────────────────

#[test]
fn correlated_doc_null_skips_inner_and_pads_left() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE u (id INT, tags TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO u VALUES (1, '[1,2]'), (2, NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // CROSS APPLY: id=2 row disappears.
    let inner = run(
        r#"SELECT u.id, j.v FROM u
             CROSS APPLY JSON_TABLE(u.tags, '$[*]'
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY u.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(inner.len(), 2);
    // OUTER APPLY: id=2 row preserved with NULL.
    let outer = run(
        r#"SELECT u.id, j.v FROM u
             OUTER APPLY JSON_TABLE(u.tags, '$[*]'
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY u.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(outer.len(), 3);
    let id2 = outer.iter().find(|r| as_i64(&r[0]) == 2).unwrap();
    assert!(matches!(id2[1], Value::Null));
}

#[test]
fn correlated_doc_with_nested_path() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE u (id INT, payload TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        r#"INSERT INTO u VALUES
           (1, '[{"tag":"a","subs":["x","y"]},{"tag":"b","subs":["z"]}]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let rows = run(
        r#"SELECT u.id, j.tag, j.sub
             FROM u
             CROSS APPLY JSON_TABLE(u.payload, '$[*]'
                          COLUMNS (
                            tag TEXT PATH '$.tag',
                            NESTED PATH '$.subs[*]' COLUMNS (sub TEXT PATH '$')
                          )) AS j
             ORDER BY j.tag, j.sub"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(as_text(&rows[0][1]), "a");
    assert_eq!(as_text(&rows[0][2]), "x");
    assert_eq!(as_text(&rows[1][1]), "a");
    assert_eq!(as_text(&rows[1][2]), "y");
    assert_eq!(as_text(&rows[2][1]), "b");
    assert_eq!(as_text(&rows[2][2]), "z");
}

// ── LATERAL keyword ─────────────────────────────────────────────────────────

#[test]
fn lateral_keyword_accepted_on_join() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT o.id, j.qty FROM orders o
             JOIN LATERAL JSON_TABLE(o.payload, '$.items[*]'
                    COLUMNS (qty INT PATH '$.qty')) AS j ON TRUE
             ORDER BY o.id, j.qty"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn lateral_keyword_accepted_on_first_from_non_correlated() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT j.v FROM LATERAL JSON_TABLE('[10,20]', '$[*]'
             COLUMNS (v INT PATH '$')) AS j
             ORDER BY j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 10);
    assert_eq!(as_i64(&rows[1][0]), 20);
}

// ── Rejections ──────────────────────────────────────────────────────────────

#[test]
fn right_join_correlated_jt_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        r#"SELECT o.id, j.qty FROM orders o
             RIGHT JOIN JSON_TABLE(o.payload, '$.items[*]'
                    COLUMNS (qty INT PATH '$.qty')) AS j ON TRUE"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("RIGHT") || msg.contains("FULL"),
        "expected RIGHT/FULL rejection, got {msg}"
    );
}

#[test]
fn full_join_correlated_jt_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_orders(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        r#"SELECT o.id, j.qty FROM orders o
             FULL JOIN JSON_TABLE(o.payload, '$.items[*]'
                    COLUMNS (qty INT PATH '$.qty')) AS j ON TRUE"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("RIGHT") || msg.contains("FULL"),
        "expected FULL rejection, got {msg}"
    );
}

#[test]
fn first_from_correlated_doc_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx("CREATE TABLE u (tags TEXT)", &mut s, &mut t, &mut b, &mut c).unwrap();
    // Column reference in doc has no outer scope when JSON_TABLE is first FROM.
    let err = run_ctx(
        r#"SELECT v FROM JSON_TABLE(u.tags, '$[*]' COLUMNS (v INT PATH '$')) AS j"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    // Either resolver-level error (u not in scope) or semantic rejection.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("correlated") || msg.contains("u") || msg.contains("column"),
        "expected correlation/column-not-found error, got {msg}"
    );
}
