//! Phase 11.23a — `JSON_SCHEMA_VALID(schema, doc)` Draft-07 subset.

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

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}
fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

#[test]
fn type_string_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\"}', '\"hi\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn type_string_fails_on_number() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\"}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn type_integer_accepts_whole_float() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"integer\"}', '3.0')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn type_array_union() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":[\"integer\",\"null\"]}', 'null')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn required_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"object\",\"required\":[\"a\"]}', '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn required_missing_fails() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"object\",\"required\":[\"a\"]}', '{\"b\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn properties_recurse() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"properties\":{\"a\":{\"type\":\"integer\",\"minimum\":0}}}', \
            '{\"a\":-1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn additional_properties_false_rejects_extra() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"properties\":{\"a\":{\"type\":\"integer\"}},\"additionalProperties\":false}', \
            '{\"a\":1,\"b\":2}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn minimum_maximum() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"minimum\":0,\"maximum\":10}', '5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"minimum\":0,\"maximum\":10}', '11')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn exclusive_bounds() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"exclusiveMinimum\":0}', '0')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn min_max_length() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"minLength\":3}', '\"ab\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn array_items_homogeneous() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}', '[1,2,3]')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}', '[1,\"x\"]')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn enum_check() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"enum\":[\"red\",\"green\"]}', '\"blue\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"enum\":[\"red\",\"green\"]}', '\"red\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn const_check() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"const\":42}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn true_schema_accepts_all() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('true', '{\"x\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn false_schema_rejects_all() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('false', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(NULL, '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn jsonb_input() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            CAST('{\"type\":\"object\",\"required\":[\"a\"]}' AS JSONB), \
            CAST('{\"a\":1}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

// ── Phase 11.23d partial: pattern, logical combinators, uniqueItems ────────

#[test]
fn pattern_matches() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"pattern\":\"^[a-z]+$\"}', '\"abc\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"pattern\":\"^[a-z]+$\"}', '\"AB\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn all_of_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"allOf\":[{\"minimum\":0},{\"maximum\":10}]}', '5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"allOf\":[{\"minimum\":0},{\"maximum\":10}]}', '15')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn any_of_short_circuits() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"anyOf\":[{\"type\":\"string\"},{\"type\":\"integer\"}]}', '42')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"anyOf\":[{\"type\":\"string\"},{\"type\":\"integer\"}]}', '[]')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn one_of_exactly_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"oneOf\":[{\"type\":\"integer\"},{\"type\":\"string\"}]}', '42')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_true(&v));
    // Matches both integer and minimum:0 → 2 hits → fails
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"oneOf\":[{\"type\":\"integer\"},{\"minimum\":0}]}', '5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn not_combinator() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"not\":{\"type\":\"string\"}}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"not\":{\"type\":\"string\"}}', '\"hi\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn unique_items() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"uniqueItems\":true}', '[1,2,3]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"uniqueItems\":true}', '[1,2,1]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

// ── Phase 11.23e partial: patternProperties, propertyNames, dependencies,
// if/then/else, format ──────────────────────────────────────────────────────

#[test]
fn pattern_properties_matches() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"patternProperties\":{\"^x_\":{\"type\":\"integer\"}}}', \
            '{\"x_a\":1,\"y_b\":\"ok\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn pattern_properties_fails_non_matching_type() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"patternProperties\":{\"^x_\":{\"type\":\"integer\"}}}', \
            '{\"x_a\":\"not-int\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn property_names_schema() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"propertyNames\":{\"maxLength\":3}}', \
            '{\"ok\":1,\"too_long\":2}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn dependencies_required_keys() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"dependencies\":{\"credit_card\":[\"billing_address\"]}}', \
            '{\"credit_card\":\"4242\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"dependencies\":{\"credit_card\":[\"billing_address\"]}}', \
            '{\"credit_card\":\"4242\",\"billing_address\":\"x\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn if_then_branch() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"if\":{\"type\":\"string\"},\"then\":{\"minLength\":3}}', \
            '\"ab\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"if\":{\"type\":\"string\"},\"then\":{\"minLength\":3}}', \
            '\"abcd\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn if_else_branch() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"if\":{\"type\":\"string\"},\"else\":{\"minimum\":10}}', \
            '5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn format_email() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"format\":\"email\"}', '\"a@b.co\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"format\":\"email\"}', '\"bogus\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn format_uuid() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"type\":\"string\",\"format\":\"uuid\"}', \
            '\"550e8400-e29b-41d4-a716-446655440000\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn format_date_and_datetime() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"format\":\"date\"}', '\"2026-04-13\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"format\":\"date-time\"}', '\"2026-04-13T10:00:00Z\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn format_ipv4_ipv6() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"format\":\"ipv4\"}', '\"10.0.0.1\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"format\":\"ipv6\"}', '\"::1\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"format\":\"ipv4\"}', '\"not-an-ip\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}
