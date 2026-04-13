//! Integration tests for Phase 11.20d1 — `JSON_TABLE` WRAPPER / QUOTES /
//! PASSING.
//!
//! Covers:
//!   - per-column `WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY] WRAPPER` and
//!     `WITHOUT WRAPPER`,
//!   - `KEEP QUOTES` vs `OMIT QUOTES ON SCALAR STRING` on TEXT columns,
//!   - top-level `PASSING expr AS name [, …]` bindings threaded into the
//!     row path, column paths, and NESTED PATH filters,
//!   - negative cases: OMIT QUOTES on non-TEXT, duplicate PASSING names,
//!     undeclared `$var` (filter excludes the row, no panic).

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

// ── WRAPPER ─────────────────────────────────────────────────────────────────

#[test]
fn with_unconditional_wrapper_emits_json_array_literal() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT tags FROM JSON_TABLE(
            '{"tags":["a","b","c"]}',
            '$' COLUMNS (
                tags JSON PATH '$.tags[*]' WITH UNCONDITIONAL ARRAY WRAPPER
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Json(j) => assert_eq!(j, "[\"a\",\"b\",\"c\"]"),
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn with_conditional_wrapper_single_array_unwraps() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT items FROM JSON_TABLE(
            '{"items":[1,2,3]}',
            '$' COLUMNS (
                items JSON PATH '$.items' WITH CONDITIONAL WRAPPER
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Json(j) => assert_eq!(j, "[1,2,3]"),
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn with_conditional_wrapper_single_scalar_wraps() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT x FROM JSON_TABLE(
            '{"x":42}',
            '$' COLUMNS (
                x JSON PATH '$.x' WITH CONDITIONAL WRAPPER
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Json(j) => assert_eq!(j, "[42]"),
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn without_wrapper_single_hit_passes_through() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT id FROM JSON_TABLE(
            '{"id":7}',
            '$' COLUMNS (
                id INT PATH '$.id' WITHOUT WRAPPER
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows, vec![vec![Value::Int(7)]]);
}

#[test]
fn without_wrapper_multi_hit_routes_to_on_error_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT v FROM JSON_TABLE(
            '{"arr":[1,2,3]}',
            '$' COLUMNS (
                v INT PATH '$.arr[*]' WITHOUT WRAPPER NULL ON ERROR
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows, vec![vec![Value::Null]]);
}

#[test]
fn without_wrapper_multi_hit_error_surfaces() {
    let (mut s, mut t, mut b, mut c) = setup();
    let bad = "SELECT v FROM JSON_TABLE(
        '{\"arr\":[1,2,3]}',
        '$' COLUMNS (
            v INT PATH '$.arr[*]' WITHOUT WRAPPER ERROR ON ERROR
        )
    ) AS t";
    let res = run_ctx(bad, &mut s, &mut t, &mut b, &mut c);
    assert!(res.is_err(), "expected error on WITHOUT + multi-hit");
}

// ── QUOTES ──────────────────────────────────────────────────────────────────

#[test]
fn keep_quotes_preserves_json_string_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT name FROM JSON_TABLE(
            '{"name":"Alice"}',
            '$' COLUMNS (
                name TEXT PATH '$.name' KEEP QUOTES ON SCALAR STRING
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Default single-scalar behavior (no WRAPPER) just yields the string
    // value; KEEP is the default, explicit or not.
    assert_eq!(rows, vec![vec![Value::Text("Alice".into())]]);
}

#[test]
fn omit_quotes_on_text_column_strips_outer_double_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT name FROM JSON_TABLE(
            '{"name":"Alice"}',
            '$' COLUMNS (
                name TEXT PATH '$.name' OMIT QUOTES ON SCALAR STRING
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows, vec![vec![Value::Text("Alice".into())]]);
}

// ── PASSING ─────────────────────────────────────────────────────────────────

#[test]
fn passing_scalar_into_filter_selects_matching_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT oid, price FROM JSON_TABLE(
            '{"items":[{"price":10},{"price":20},{"price":30}]}',
            '$.items[?(@.price > $min)]'
            PASSING 15 AS min
            COLUMNS (
                oid   FOR ORDINALITY,
                price INT PATH '$.price'
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Int(20));
    assert_eq!(rows[1][1], Value::Int(30));
}

#[test]
fn passing_two_variables_both_referenced() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT price FROM JSON_TABLE(
            '{"items":[{"price":5},{"price":15},{"price":25},{"price":35}]}',
            '$.items[?(@.price >= $lo && @.price <= $hi)]'
            PASSING 10 AS lo, 30 AS hi
            COLUMNS (
                price INT PATH '$.price'
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(15));
    assert_eq!(rows[1][0], Value::Int(25));
}

#[test]
fn passing_visible_inside_nested_path_filter() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT inv_id, part FROM JSON_TABLE(
            '[{"id":1,"parts":[{"n":"a","q":1},{"n":"b","q":5},{"n":"c","q":9}]}]',
            '$[*]'
            PASSING 3 AS qmin
            COLUMNS (
                inv_id INT PATH '$.id',
                NESTED PATH '$.parts[?(@.q > $qmin)]' COLUMNS (
                    part TEXT PATH '$.n'
                )
            )
        ) AS t ORDER BY part"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("b".into()));
    assert_eq!(rows[1][1], Value::Text("c".into()));
}

#[test]
fn passing_plus_wrapper_on_same_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT oid, matches FROM JSON_TABLE(
            '{"items":[{"price":5},{"price":15},{"price":25}]}',
            '$'
            PASSING 10 AS lo
            COLUMNS (
                oid FOR ORDINALITY,
                matches JSON PATH '$.items[?(@.price > $lo)].price'
                    WITH UNCONDITIONAL ARRAY WRAPPER
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][1] {
        Value::Json(j) => assert_eq!(j, "[15,25]"),
        other => panic!("expected Json array, got {other:?}"),
    }
}

#[test]
fn undeclared_passing_var_excludes_rows_silently() {
    let (mut s, mut t, mut b, mut c) = setup();
    // `$undeclared` is not in PASSING → the filter is false on every row,
    // yielding an empty set. No panic, no DbError.
    let rows = run(
        r#"SELECT price FROM JSON_TABLE(
            '{"items":[{"price":1},{"price":2}]}',
            '$.items[?(@.price > $undeclared)]'
            COLUMNS (
                price INT PATH '$.price'
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 0);
}

// ── Negative cases ──────────────────────────────────────────────────────────

#[test]
fn omit_quotes_on_non_text_column_rejected_at_parse_time() {
    let (mut s, mut t, mut b, mut c) = setup();
    let bad = "SELECT x FROM JSON_TABLE(
        '{\"x\":\"1\"}',
        '$' COLUMNS (
            x INT PATH '$.x' OMIT QUOTES
        )
    ) AS t";
    let res = run_ctx(bad, &mut s, &mut t, &mut b, &mut c);
    assert!(
        res.is_err(),
        "expected parse error for OMIT on non-TEXT column"
    );
    let err_str = format!("{:?}", res.err().unwrap());
    assert!(
        err_str.contains("OMIT QUOTES"),
        "error should mention OMIT QUOTES, got: {err_str}"
    );
}

#[test]
fn duplicate_passing_var_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    let bad = "SELECT v FROM JSON_TABLE(
        '{}',
        '$'
        PASSING 1 AS a, 2 AS a
        COLUMNS (v INT PATH '$.v')
    ) AS t";
    let res = run_ctx(bad, &mut s, &mut t, &mut b, &mut c);
    assert!(res.is_err(), "duplicate PASSING name should error");
    let err_str = format!("{:?}", res.err().unwrap());
    assert!(
        err_str.contains("duplicate PASSING"),
        "error should mention duplicate PASSING, got: {err_str}"
    );
}
