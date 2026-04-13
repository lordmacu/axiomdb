//! Phase 11.20d2 — JSON_TABLE as the first FROM entry combined with JOINs,
//! plus CROSS APPLY / OUTER APPLY parser sugar.

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

fn seed_users(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) {
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1,'alice'),(2,'bob'),(3,'carol')",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
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

// ── JSON_TABLE first + JOIN real table ──────────────────────────────────────

#[test]
fn jt_first_inner_join_real_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT j.id, u.name
             FROM JSON_TABLE('[{"id":1},{"id":3},{"id":99}]',
                             '$[*]' COLUMNS (id INT PATH '$.id')) AS j
             JOIN users u ON u.id = j.id
             ORDER BY j.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_text(&rows[0][1]), "alice");
    assert_eq!(as_i64(&rows[1][0]), 3);
    assert_eq!(as_text(&rows[1][1]), "carol");
}

#[test]
fn jt_first_left_join_subquery() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT j.v, q.c
             FROM JSON_TABLE('[{"v":10},{"v":20}]',
                             '$[*]' COLUMNS (v INT PATH '$.v')) AS j
             LEFT JOIN (SELECT 10 AS c) q ON q.c = j.v
             ORDER BY j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 10);
    assert_eq!(as_i64(&rows[0][1]), 10);
    assert_eq!(as_i64(&rows[1][0]), 20);
    assert!(matches!(rows[1][1], Value::Null));
}

#[test]
fn jt_first_join_jt_second() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT a.k, b.v
             FROM JSON_TABLE('[{"k":1},{"k":2}]',
                             '$[*]' COLUMNS (k INT PATH '$.k')) AS a
             JOIN JSON_TABLE('[{"k":2,"v":"x"},{"k":3,"v":"y"}]',
                             '$[*]' COLUMNS (k INT PATH '$.k',
                                             v TEXT PATH '$.v')) AS b
                ON a.k = b.k"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0][0]), 2);
    assert_eq!(as_text(&rows[0][1]), "x");
}

// ── CROSS APPLY / OUTER APPLY ───────────────────────────────────────────────

#[test]
fn cross_apply_vs_join_on_true_equivalence() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let apply_rows = run(
        r#"SELECT u.id, j.v
             FROM users u
             CROSS APPLY JSON_TABLE('[10,20]', '$[*]'
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY u.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let join_rows = run(
        r#"SELECT u.id, j.v
             FROM users u
             JOIN JSON_TABLE('[10,20]', '$[*]'
                    COLUMNS (v INT PATH '$')) AS j ON TRUE
             ORDER BY u.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(apply_rows, join_rows);
    assert_eq!(apply_rows.len(), 6);
}

#[test]
fn cross_apply_json_table_non_correlated_product() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT u.id, j.v
             FROM users u
             CROSS APPLY JSON_TABLE('[1,2,3]', '$[*]'
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY u.id, j.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 9);
}

#[test]
fn outer_apply_preserves_left_on_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT u.id, j.v
             FROM users u
             OUTER APPLY JSON_TABLE('[]', '$[*]'
                          COLUMNS (v INT PATH '$')) AS j
             ORDER BY u.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert!(matches!(row[1], Value::Null));
    }
}

#[test]
fn cross_apply_regular_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    run_ctx("CREATE TABLE nums (n INT)", &mut s, &mut t, &mut b, &mut c).unwrap();
    run_ctx(
        "INSERT INTO nums VALUES (10),(20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let rows = run(
        "SELECT u.id, n.n FROM users u CROSS APPLY nums n ORDER BY u.id, n.n",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 6);
}

#[test]
fn cross_apply_rejects_on_clause() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        r#"SELECT u.id FROM users u
             CROSS APPLY JSON_TABLE('[1]', '$[*]'
                          COLUMNS (v INT PATH '$')) AS j ON TRUE"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("APPLY") && (msg.contains("ON") || msg.contains("USING")),
        "expected APPLY-ON rejection, got: {msg}"
    );
}

// ── WHERE / ORDER BY / LIMIT / GROUP BY over joined output ──────────────────

#[test]
fn jt_first_where_filter() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT j.id
             FROM JSON_TABLE('[{"id":1},{"id":2},{"id":3}]',
                             '$[*]' COLUMNS (id INT PATH '$.id')) AS j
             JOIN users u ON u.id = j.id
             WHERE u.name IN ('alice','carol')
             ORDER BY j.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_i64(&rows[1][0]), 3);
}

#[test]
fn jt_first_order_by_limit() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT j.id, u.name
             FROM JSON_TABLE('[{"id":1},{"id":2},{"id":3}]',
                             '$[*]' COLUMNS (id INT PATH '$.id')) AS j
             JOIN users u ON u.id = j.id
             ORDER BY j.id DESC
             LIMIT 2"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 3);
    assert_eq!(as_i64(&rows[1][0]), 2);
}

#[test]
fn jt_first_group_by_aggregate() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT j.id, COUNT(*) AS n
             FROM JSON_TABLE('[{"id":1},{"id":1},{"id":2}]',
                             '$[*]' COLUMNS (id INT PATH '$.id')) AS j
             JOIN users u ON u.id = j.id
             GROUP BY j.id
             ORDER BY j.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_i64(&rows[0][1]), 2);
    assert_eq!(as_i64(&rows[1][0]), 2);
    assert_eq!(as_i64(&rows[1][1]), 1);
}

// ── NESTED PATH + PASSING still work when JT is first source ───────────────

#[test]
fn jt_first_nested_path_with_join() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed_users(&mut s, &mut t, &mut b, &mut c);
    let rows = run(
        r#"SELECT j.uid, j.tag, u.name
             FROM JSON_TABLE(
                    '[{"uid":1,"tags":["x","y"]},{"uid":2,"tags":["z"]}]',
                    '$[*]' COLUMNS (
                       uid INT PATH '$.uid',
                       NESTED PATH '$.tags[*]' COLUMNS (tag TEXT PATH '$')
                    )
                 ) AS j
             JOIN users u ON u.id = j.uid
             ORDER BY j.uid, j.tag"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(as_i64(&rows[0][0]), 1);
    assert_eq!(as_text(&rows[0][1]), "x");
    assert_eq!(as_i64(&rows[1][0]), 1);
    assert_eq!(as_text(&rows[1][1]), "y");
    assert_eq!(as_i64(&rows[2][0]), 2);
    assert_eq!(as_text(&rows[2][1]), "z");
}
