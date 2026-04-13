//! Integration tests for Phase 11.20b — single-level `NESTED PATH` in
//! `JSON_TABLE(...)`.
//!
//! Covers:
//!   - basic shred with parent × children cartesian,
//!   - LEFT-OUTER NULL padding when a parent match has zero children,
//!   - per-level ordinality counters (parent counter increments per match,
//!     child counter resets per parent and counts child matches),
//!   - nested `EXISTS PATH` and `DEFAULT ON EMPTY`,
//!   - `NESTED '...'` shortcut (omitting the `PATH` keyword),
//!   - WHERE predicates referencing both parent and child columns,
//!   - deferred cases: multi-sibling NESTED, depth ≥ 2.

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

#[test]
fn basic_nested_shred_parents_times_children() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, item_name, qty FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A","qty":2},{"name":"B","qty":3}]},
              {"id":2,"items":[{"name":"C","qty":1}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    item_name TEXT PATH '$.name',
                    qty       INT  PATH '$.qty'
                )
            )
        ) AS t ORDER BY inv_id, item_name"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![Value::Int(1), Value::Text("A".into()), Value::Int(2)]
    );
    assert_eq!(
        rows[1],
        vec![Value::Int(1), Value::Text("B".into()), Value::Int(3)]
    );
    assert_eq!(
        rows[2],
        vec![Value::Int(2), Value::Text("C".into()), Value::Int(1)]
    );
}

#[test]
fn left_outer_null_pad_on_empty_children() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, item_name FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A"}]},
              {"id":2,"items":[]},
              {"id":3,"items":[{"name":"B"}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    item_name TEXT PATH '$.name'
                )
            )
        ) AS t ORDER BY inv_id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Text("A".into())]);
    // id=2 has empty items → LEFT OUTER pad
    assert_eq!(rows[1], vec![Value::Int(2), Value::Null]);
    assert_eq!(rows[2], vec![Value::Int(3), Value::Text("B".into())]);
}

#[test]
fn parent_with_missing_items_key_pads_null() {
    // `$.items[*]` on an object without "items" yields zero matches →
    // LEFT-OUTER behaves same as empty-array case.
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, item_name FROM JSON_TABLE(
            '[{"id":10},{"id":20,"items":[{"name":"X"}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    item_name TEXT PATH '$.name'
                )
            )
        ) AS t ORDER BY inv_id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(10), Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(20), Value::Text("X".into())]);
}

#[test]
fn per_level_ordinality_parent_and_child() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT ord_inv, inv_id, ord_item, item_name FROM JSON_TABLE(
            '[{"id":10,"items":[{"name":"A"},{"name":"B"}]},
              {"id":20,"items":[{"name":"C"}]}]',
            '$[*]' COLUMNS (
                ord_inv FOR ORDINALITY,
                inv_id  INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    ord_item FOR ORDINALITY,
                    item_name TEXT PATH '$.name'
                )
            )
        ) AS t ORDER BY ord_inv, ord_item"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::BigInt(1),
            Value::Int(10),
            Value::BigInt(1),
            Value::Text("A".into())
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::BigInt(1),
            Value::Int(10),
            Value::BigInt(2),
            Value::Text("B".into())
        ]
    );
    // ord_inv=2 (next parent) ord_item=1 (reset)
    assert_eq!(
        rows[2],
        vec![
            Value::BigInt(2),
            Value::Int(20),
            Value::BigInt(1),
            Value::Text("C".into())
        ]
    );
}

#[test]
fn nested_exists_path_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, has_qty FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A","qty":2},{"name":"B"}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    has_qty BOOLEAN EXISTS PATH '$.qty'
                )
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Bool(true)]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Bool(false)]);
}

#[test]
fn nested_default_on_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, qty FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A"},{"name":"B","qty":5}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    qty INT PATH '$.qty' DEFAULT 0 ON EMPTY
                )
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(0)]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Int(5)]);
}

#[test]
fn nested_without_path_keyword() {
    // SQL:2016 + MariaDB shortcut: `NESTED '<jsonpath>' COLUMNS(...)` without PATH.
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, item_name FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A"}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED '$.items[*]' COLUMNS (
                    item_name TEXT PATH '$.name'
                )
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Text("A".into())]);
}

#[test]
fn where_across_parent_and_child_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, item_name, qty FROM JSON_TABLE(
            '[{"id":1,"items":[{"name":"A","qty":1},{"name":"B","qty":5}]},
              {"id":2,"items":[{"name":"C","qty":10}]}]',
            '$[*]' COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.items[*]' COLUMNS (
                    item_name TEXT PATH '$.name',
                    qty       INT  PATH '$.qty'
                )
            )
        ) AS t WHERE qty >= 5 ORDER BY inv_id, item_name"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![Value::Int(1), Value::Text("B".into()), Value::Int(5)]
    );
    assert_eq!(
        rows[1],
        vec![Value::Int(2), Value::Text("C".into()), Value::Int(10)]
    );
}

#[test]
fn duplicate_column_name_across_levels_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT 1 FROM JSON_TABLE('[]', '$[*]' COLUMNS (
            id INT PATH '$.id',
            NESTED PATH '$.x[*]' COLUMNS (
                id INT PATH '$'
            )
        )) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(err.is_err(), "expected duplicate-name error, got {err:?}");
}

#[test]
fn multi_sibling_nested_not_yet_implemented() {
    // 11.20c scope — reject with explicit message.
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT 1 FROM JSON_TABLE('[]', '$[*]' COLUMNS (
            NESTED PATH '$.a[*]' COLUMNS (v1 INT PATH '$'),
            NESTED PATH '$.b[*]' COLUMNS (v2 INT PATH '$')
        )) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let e = err.unwrap_err();
    let s = format!("{e:?}");
    assert!(s.contains("11.20c"), "expected 11.20c pointer, got {s}");
}

#[test]
fn multi_level_nested_not_yet_implemented() {
    // 11.20c scope — reject depth ≥ 2.
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT 1 FROM JSON_TABLE('[]', '$[*]' COLUMNS (
            NESTED PATH '$.a[*]' COLUMNS (
                NESTED PATH '$.b[*]' COLUMNS (v INT PATH '$')
            )
        )) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let e = err.unwrap_err();
    let s = format!("{e:?}");
    assert!(s.contains("11.20c"), "expected 11.20c pointer, got {s}");
}
