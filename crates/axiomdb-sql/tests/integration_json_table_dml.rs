//! Phase 11.20d4 — JSON_TABLE as UPDATE / DELETE source.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn exec(
    sql: &str,
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) {
    run_ctx(sql, s, t, b, c).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
}

fn rows(
    sql: &str,
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let res = run_ctx(sql, s, t, b, c).unwrap();
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

fn seed(s: &mut MemoryStorage, t: &mut TxnManager, b: &mut BloomRegistry, c: &mut SessionContext) {
    exec(
        "CREATE TABLE orders (id INT, priority INT, tag TEXT)",
        s,
        t,
        b,
        c,
    );
    exec(
        "INSERT INTO orders VALUES (1, 0, 'a'), (2, 0, 'b'), (3, 0, 'c')",
        s,
        t,
        b,
        c,
    );
}

// ── UPDATE ──────────────────────────────────────────────────────────────────

#[test]
fn update_join_json_table_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    exec(
        r#"UPDATE orders o
             JOIN JSON_TABLE('[{"id":1,"pri":5},{"id":3,"pri":9}]', '$[*]'
                    COLUMNS (id INT PATH '$.id', pri INT PATH '$.pri')) AS j
               ON o.id = j.id
             SET o.priority = j.pri"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id, priority FROM orders ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_i64(&r[0][1]), 5);
    assert_eq!(as_i64(&r[1][1]), 0);
    assert_eq!(as_i64(&r[2][1]), 9);
}

#[test]
fn update_cross_apply_correlated_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE u (id INT, payload TEXT, tag TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        r#"INSERT INTO u VALUES
             (1, '{"flags":["urgent"]}', ''),
             (2, '{"flags":["cold"]}', ''),
             (3, '{"flags":["urgent_fire"]}', '')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        r#"UPDATE u
             CROSS APPLY JSON_TABLE(u.payload, '$.flags[*]'
                    COLUMNS (flag TEXT PATH '$')) AS j
             SET u.tag = j.flag
             WHERE j.flag LIKE 'urgent%'"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id, tag FROM u ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r[0][1], Value::Text("urgent".into()));
    assert_eq!(r[1][1], Value::Text("".into()));
    assert_eq!(r[2][1], Value::Text("urgent_fire".into()));
}

#[test]
fn update_left_join_json_table_no_match_unchanged() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    // LEFT JOIN with JSON that matches only id=2. Other rows get NULL pri
    // from the JT side; SET priority = COALESCE(j.pri, priority) keeps them.
    exec(
        r#"UPDATE orders o
             LEFT JOIN JSON_TABLE('[{"id":2,"pri":77}]', '$[*]'
                    COLUMNS (id INT PATH '$.id', pri INT PATH '$.pri')) AS j
               ON o.id = j.id
             SET o.priority = COALESCE(j.pri, o.priority)"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id, priority FROM orders ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_i64(&r[0][1]), 0);
    assert_eq!(as_i64(&r[1][1]), 77);
    assert_eq!(as_i64(&r[2][1]), 0);
}

// ── DELETE ──────────────────────────────────────────────────────────────────

#[test]
fn delete_join_json_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    exec(
        r#"DELETE o FROM orders o
             JOIN JSON_TABLE('[1, 3]', '$[*]' COLUMNS (id INT PATH '$')) AS j
               ON o.id = j.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id FROM orders ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 2);
}

#[test]
fn delete_cross_apply_correlated() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE u (id INT, tags TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        r#"INSERT INTO u VALUES
             (1, '["keep"]'),
             (2, '["drop","x"]'),
             (3, '["keep","x"]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Delete rows whose tags include 'drop'.
    exec(
        r#"DELETE u FROM u
             CROSS APPLY JSON_TABLE(u.tags, '$[*]'
                    COLUMNS (tag TEXT PATH '$')) AS j
             WHERE j.tag = 'drop'"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id FROM u ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_i64(&r[1][0]), 3);
}

// ── NESTED / PASSING ────────────────────────────────────────────────────────

#[test]
fn update_passing_outer_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE cfg (id INT, lo INT, matched_max INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO cfg VALUES (1, 2, 0), (2, 5, 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        r#"UPDATE cfg c
             CROSS APPLY JSON_TABLE('[1,2,3,4,5,6]', '$[?(@ > $threshold)]'
                    PASSING c.lo AS threshold
                    COLUMNS (v INT PATH '$')) AS j
             SET c.matched_max = j.v
             WHERE j.v = 6"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id, matched_max FROM cfg ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Both rows match v=6 (lo=2 admits 3..6; lo=5 admits 6 only).
    assert_eq!(as_i64(&r[0][1]), 6);
    assert_eq!(as_i64(&r[1][1]), 6);
}

// ── Rejections ──────────────────────────────────────────────────────────────

#[test]
fn update_right_join_jt_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        r#"UPDATE orders o
             RIGHT JOIN JSON_TABLE(o.tag, '$[*]' COLUMNS (v TEXT PATH '$')) AS j
               ON TRUE
             SET o.priority = 1"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("RIGHT") || msg.contains("FULL"),
        "expected RIGHT rejection, got {msg}"
    );
}

// ── Regression ─────────────────────────────────────────────────────────────

#[test]
fn regression_update_with_subquery_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    exec(
        r#"UPDATE orders o
             JOIN (SELECT 2 AS id, 42 AS pri) q ON q.id = o.id
             SET o.priority = q.pri"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id, priority FROM orders WHERE id = 2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_i64(&r[0][1]), 42);
}

#[test]
fn regression_delete_with_subquery_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    exec(
        r#"DELETE o FROM orders o
             JOIN (SELECT 1 AS id) q ON q.id = o.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT id FROM orders ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 2);
    assert_eq!(as_i64(&r[1][0]), 3);
}
