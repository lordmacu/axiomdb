//! Phase 11.21e (partial) — boolean combinators in JSONPath filter exprs.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn scalar(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Value {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected Rows for {sql:?}");
    };
    rows[0][0].clone()
}

fn as_text(v: &Value) -> String {
    match v {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => format!("{other:?}"),
    }
}

#[test]
fn and_two_comparisons() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Items with age >= 18 AND age < 65.
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"age\":10},{\"age\":30},{\"age\":70},{\"age\":25}]', \
            '$[?(@.age >= 18 && @.age < 65)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"age\":30"));
    assert!(text.contains("\"age\":25"));
    assert!(!text.contains("\"age\":10"));
    assert!(!text.contains("\"age\":70"));
}

#[test]
fn or_two_comparisons() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"x\":1},{\"x\":5},{\"x\":10}]', \
            '$[?(@.x < 2 || @.x > 9)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"x\":1"));
    assert!(text.contains("\"x\":10"));
    assert!(!text.contains("\"x\":5"));
}

#[test]
fn not_filter() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"ok\":true},{\"ok\":false}]', \
            '$[?(!(@.ok == true))]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"ok\":false"));
    assert!(!text.contains("\"ok\":true"));
}

#[test]
fn parenthesized_precedence() {
    let (mut s, mut t, mut b, mut c) = setup();
    // (x > 0 && y > 0) || tag == "keep"
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"x\":1,\"y\":1},{\"x\":-1,\"y\":1,\"tag\":\"keep\"},{\"x\":-1,\"y\":-1}]', \
            '$[?((@.x > 0 && @.y > 0) || @.tag == \"keep\")]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"x\":1"));
    assert!(text.contains("\"keep\""));
    assert_eq!(text.matches("\"x\":-1,\"y\":-1").count(), 0);
}

#[test]
fn existence_and_comparison_mix() {
    let (mut s, mut t, mut b, mut c) = setup();
    // @.priority exists AND @.score > 50
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"priority\":1,\"score\":60},{\"score\":70},{\"priority\":2,\"score\":10}]', \
            '$[?(@.priority && @.score > 50)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"score\":60"));
    assert!(!text.contains("\"score\":70"));
    assert!(!text.contains("\"score\":10"));
}

// ── Phase 11.21f (partial): path-vs-path comparison ────────────────────────

#[test]
fn compare_two_paths_equal() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"a\":1,\"b\":1},{\"a\":1,\"b\":2}]', \
            '$[?(@.a == @.b)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"a\":1,\"b\":1"));
    assert!(!text.contains("\"a\":1,\"b\":2"));
}

#[test]
fn compare_two_paths_lt() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"x\":1,\"y\":2},{\"x\":3,\"y\":1}]', \
            '$[?(@.x < @.y)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"x\":1"));
    assert!(!text.contains("\"x\":3"));
}

#[test]
fn missing_rhs_path_filters_out() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"a\":1,\"b\":1},{\"a\":1}]', \
            '$[?(@.a == @.b)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"b\":1"));
    assert_eq!(text.matches("\"a\":1}").count(), 0);
}

// ── Phase 11.21g (partial): arithmetic in filter RHS ───────────────────────

#[test]
fn arith_path_times_literal() {
    let (mut s, mut t, mut b, mut c) = setup();
    // price > cost * 1.5
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"price\":20,\"cost\":10},{\"price\":12,\"cost\":10}]', \
            '$[?(@.price > @.cost * 1.5)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"price\":20"));
    assert!(!text.contains("\"price\":12"));
}

#[test]
fn arith_path_plus_path() {
    let (mut s, mut t, mut b, mut c) = setup();
    // total >= a + b
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"a\":1,\"b\":2,\"total\":5},{\"a\":1,\"b\":2,\"total\":2}]', \
            '$[?(@.total >= @.a + @.b)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"total\":5"));
    assert!(!text.contains("\"total\":2"));
}

#[test]
fn arith_division() {
    let (mut s, mut t, mut b, mut c) = setup();
    // ratio == n / d
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"n\":10,\"d\":4,\"ratio\":2.5},{\"n\":10,\"d\":4,\"ratio\":1.0}]', \
            '$[?(@.ratio == @.n / @.d)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"ratio\":2.5"));
    assert!(!text.contains("\"ratio\":1.0"));
}

#[test]
fn arith_precedence_mul_over_add() {
    let (mut s, mut t, mut b, mut c) = setup();
    // x == 2 + 3 * 4  -> 14
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"x\":14},{\"x\":20}]', \
            '$[?(@.x == 2 + 3 * 4)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"x\":14"));
    assert!(!text.contains("\"x\":20"));
}

#[test]
fn arith_modulo() {
    let (mut s, mut t, mut b, mut c) = setup();
    // even-only via @.n % 2 == 0
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"n\":2},{\"n\":3},{\"n\":4}]', \
            '$[?(@.n % 2 == 0)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"n\":2"));
    assert!(text.contains("\"n\":4"));
    assert!(!text.contains("\"n\":3"));
}

#[test]
fn arith_div_by_zero_filters_out() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY(\
            '[{\"a\":10,\"b\":0},{\"a\":10,\"b\":2}]', \
            '$[?(@.a / @.b > 0)]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"b\":2"));
    assert_eq!(text.matches("\"b\":0").count(), 0);
}
