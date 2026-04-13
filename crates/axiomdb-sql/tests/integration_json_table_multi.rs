//! Integration tests for Phase 11.20c — multi-sibling NESTED (UNION
//! semantics) and multi-level NESTED (recursive tree walk) in
//! `JSON_TABLE(...)`.

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

// ── Multi-sibling NESTED (UNION) ─────────────────────────────────────────────

#[test]
fn multi_sibling_union_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, price, tag FROM JSON_TABLE(
            '[{"id":1,"prices":[10,20],"tags":["a","b","c"]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.prices[*]' COLUMNS (price INT PATH '$'),
                NESTED PATH '$.tags[*]'   COLUMNS (tag TEXT PATH '$')
            )
        ) AS t ORDER BY COALESCE(price, 1000), COALESCE(tag, 'z')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // |prices|=2 + |tags|=3 = 5 rows.
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(10), Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Int(20), Value::Null]);
    assert_eq!(
        rows[2],
        vec![Value::Int(1), Value::Null, Value::Text("a".into())]
    );
    assert_eq!(
        rows[3],
        vec![Value::Int(1), Value::Null, Value::Text("b".into())]
    );
    assert_eq!(
        rows[4],
        vec![Value::Int(1), Value::Null, Value::Text("c".into())]
    );
}

#[test]
fn multi_sibling_both_empty_yields_two_left_outer_pads() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, price, tag FROM JSON_TABLE(
            '[{"id":1,"prices":[],"tags":[]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.prices[*]' COLUMNS (price INT PATH '$'),
                NESTED PATH '$.tags[*]'   COLUMNS (tag TEXT PATH '$')
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Null, Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Null, Value::Null]);
}

#[test]
fn multi_sibling_one_empty_one_populated() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, price, tag FROM JSON_TABLE(
            '[{"id":1,"prices":[7],"tags":[]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.prices[*]' COLUMNS (price INT PATH '$'),
                NESTED PATH '$.tags[*]'   COLUMNS (tag TEXT PATH '$')
            )
        ) AS t ORDER BY COALESCE(price, 1000)"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // prices has 1 match → 1 row; tags empty → 1 LEFT-OUTER pad row.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(7), Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Null, Value::Null]);
}

// ── Multi-level NESTED ───────────────────────────────────────────────────────

#[test]
fn multi_level_basic_shred() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, line_id, part FROM JSON_TABLE(
            '[{"id":1,"lines":[
               {"lid":"L1","parts":["P1","P2"]},
               {"lid":"L2","parts":["P3"]}
            ]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.lines[*]' COLUMNS (
                    line_id TEXT PATH '$.lid',
                    NESTED PATH '$.parts[*]' COLUMNS (
                        part TEXT PATH '$'
                    )
                )
            )
        ) AS t ORDER BY line_id, part"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("L1".into()),
            Value::Text("P1".into())
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Int(1),
            Value::Text("L1".into()),
            Value::Text("P2".into())
        ]
    );
    assert_eq!(
        rows[2],
        vec![
            Value::Int(1),
            Value::Text("L2".into()),
            Value::Text("P3".into())
        ]
    );
}

#[test]
fn multi_level_inner_empty_left_outer_pad() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, line_id, part FROM JSON_TABLE(
            '[{"id":1,"lines":[
               {"lid":"L1","parts":["P1"]},
               {"lid":"L2","parts":[]}
            ]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.lines[*]' COLUMNS (
                    line_id TEXT PATH '$.lid',
                    NESTED PATH '$.parts[*]' COLUMNS (
                        part TEXT PATH '$'
                    )
                )
            )
        ) AS t ORDER BY line_id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("L1".into()),
            Value::Text("P1".into())
        ]
    );
    // L2 has empty parts → inner LEFT-OUTER pad
    assert_eq!(
        rows[1],
        vec![Value::Int(1), Value::Text("L2".into()), Value::Null]
    );
}

#[test]
fn multi_level_outer_empty_single_null_pad() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, line_id, part FROM JSON_TABLE(
            '[{"id":1,"lines":[]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.lines[*]' COLUMNS (
                    line_id TEXT PATH '$.lid',
                    NESTED PATH '$.parts[*]' COLUMNS (
                        part TEXT PATH '$'
                    )
                )
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Outer lines empty → 1 LEFT-OUTER pad row with everything NULL below
    // the inv_id.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Null, Value::Null]);
}

#[test]
fn multi_level_ordinality_resets_per_level() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT line_ord, part_ord FROM JSON_TABLE(
            '[{"lines":[
               {"parts":["A","B"]},
               {"parts":["C"]}
            ]}]',
            '$[*]' COLUMNS (
                NESTED PATH '$.lines[*]' COLUMNS (
                    line_ord FOR ORDINALITY,
                    NESTED PATH '$.parts[*]' COLUMNS (
                        part_ord FOR ORDINALITY
                    )
                )
            )
        ) AS t ORDER BY line_ord, part_ord"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // line_ord: 1,1,2  | part_ord: 1,2,1
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::BigInt(1), Value::BigInt(1)]);
    assert_eq!(rows[1], vec![Value::BigInt(1), Value::BigInt(2)]);
    assert_eq!(rows[2], vec![Value::BigInt(2), Value::BigInt(1)]);
}

#[test]
fn multi_level_three_deep() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT a, b_val, c_val FROM JSON_TABLE(
            '[{"a":1,"bs":[
               {"bv":"X","cs":[{"cv":"p"},{"cv":"q"}]},
               {"bv":"Y","cs":[{"cv":"r"}]}
            ]}]',
            '$[*]' COLUMNS (
                a INT PATH '$.a',
                NESTED PATH '$.bs[*]' COLUMNS (
                    b_val TEXT PATH '$.bv',
                    NESTED PATH '$.cs[*]' COLUMNS (
                        c_val TEXT PATH '$.cv'
                    )
                )
            )
        ) AS t ORDER BY b_val, c_val"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("X".into()),
            Value::Text("p".into())
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Int(1),
            Value::Text("X".into()),
            Value::Text("q".into())
        ]
    );
    assert_eq!(
        rows[2],
        vec![
            Value::Int(1),
            Value::Text("Y".into()),
            Value::Text("r".into())
        ]
    );
}

// ── Mixed: multi-sibling where one branch also has a multi-level ────────────

#[test]
fn mixed_sibling_and_multi_level() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, line_id, part, tag FROM JSON_TABLE(
            '[{"id":1,
               "lines":[{"lid":"L1","parts":["P1"]}],
               "tags":["t1","t2"]
             }]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.lines[*]' COLUMNS (
                    line_id TEXT PATH '$.lid',
                    NESTED PATH '$.parts[*]' COLUMNS (
                        part TEXT PATH '$'
                    )
                ),
                NESTED PATH '$.tags[*]' COLUMNS (
                    tag TEXT PATH '$'
                )
            )
        ) AS t ORDER BY COALESCE(line_id, 'zz'), COALESCE(tag, 'zz')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // lines branch → 1 row: (L1, P1, NULL)
    // tags branch  → 2 rows: (NULL, NULL, t1), (NULL, NULL, t2)
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("L1".into()),
            Value::Text("P1".into()),
            Value::Null
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Int(1),
            Value::Null,
            Value::Null,
            Value::Text("t1".into())
        ]
    );
    assert_eq!(
        rows[2],
        vec![
            Value::Int(1),
            Value::Null,
            Value::Null,
            Value::Text("t2".into())
        ]
    );
}

#[test]
fn depth_limit_rejected_above_32() {
    // Build a pathologically deep NESTED chain to hit the compile-time guard.
    // Depth 33 here (1 row-path + 33 nested levels).
    let (mut s, mut t, mut b, mut c) = setup();
    let mut sql = String::from("SELECT 1 FROM JSON_TABLE('[]', '$[*]' COLUMNS (");
    for _ in 0..33 {
        sql.push_str("NESTED PATH '$.a[*]' COLUMNS (");
    }
    sql.push_str("v INT PATH '$'");
    for _ in 0..33 {
        sql.push_str(")");
    }
    sql.push_str(")) AS t");
    let err = run_ctx(&sql, &mut s, &mut t, &mut b, &mut c);
    assert!(err.is_err(), "expected depth-limit error, got {err:?}");
}
