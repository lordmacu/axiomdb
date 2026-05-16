/// Integration tests for Phase 20.13 — SQL range types
///
/// Tests cover: int4range, int8range, numrange, daterange, tsrange,
/// constructors, operators (@>, <@, &&, +, *, -), scalar functions,
/// CREATE TABLE with range columns, INSERT, SELECT, UPDATE.
mod common;

use axiomdb_types::Value;
use common::*;

// ── Constructor function tests ────────────────────────────────────────────────

#[test]
fn test_int4range_constructor_default_bounds() {
    let (mut s, mut t) = setup();
    // int4range(lower, upper) defaults to [lower, upper)
    let r = rows(run("SELECT int4range(1, 5)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[1,5)");
}

#[test]
fn test_int4range_constructor_inclusive_bounds() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(1, 5, '[]')", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    // [1,5] discrete → canonicalized to [1,6)
    assert_eq!(r[0][0].to_string(), "[1,6)");
}

#[test]
fn test_int4range_constructor_exclusive_lower() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(1, 5, '()')", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    // (1,5) → [2,5) after canonicalization
    assert_eq!(r[0][0].to_string(), "[2,5)");
}

#[test]
fn test_int4range_empty() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(5, 5)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "empty");
}

#[test]
fn test_int8range_constructor() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int8range(100, 200)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[100,200)");
}

#[test]
fn test_numrange_constructor() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT numrange(1.5, 3.5)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    let v = r[0][0].to_string();
    assert!(v.starts_with('[') && v.ends_with(')'), "got: {v}");
}

// ── Scalar functions ──────────────────────────────────────────────────────────

#[test]
fn test_lower_upper_bounded() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT lower(int4range(1, 5)), upper(int4range(1, 5))",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Int(5));
}

#[test]
fn test_lower_upper_unbounded() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT lower(int4range(NULL, 5)), upper(int4range(1, NULL))",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[0][1], Value::Null);
}

#[test]
fn test_isempty_false() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT isempty(int4range(1, 5))", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Bool(false));
}

#[test]
fn test_isempty_true() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT isempty(int4range(3, 3))", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn test_lower_does_not_break_string_lower() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT lower('HELLO')", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("hello".into()));
}

// ── Containment operator @> ───────────────────────────────────────────────────

#[test]
fn test_contains_element_true() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(1, 10) @> 5", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn test_contains_element_false() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(1, 10) @> 10", &mut s, &mut t));
    // [1,10) exclusive upper → 10 is NOT contained
    assert_eq!(r[0][0], Value::Bool(false));
}

#[test]
fn test_contains_range() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 10) @> int4range(2, 5)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn test_contained_by_element() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT 5 <@ int4range(1, 10)", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Bool(true));
}

// ── Overlap operator && ───────────────────────────────────────────────────────

#[test]
fn test_overlap_true() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 5) && int4range(4, 8)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn test_overlap_false() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 5) && int4range(5, 8)",
        &mut s,
        &mut t,
    ));
    // [1,5) and [5,8) — 5 is not in [1,5)
    assert_eq!(r[0][0], Value::Bool(false));
}

// ── Set operators + * - ──────────────────────────────────────────────────────

#[test]
fn test_union_adjacent() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 5) + int4range(5, 10)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[1,10)");
}

#[test]
fn test_intersection() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 8) * int4range(4, 12)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[4,8)");
}

#[test]
fn test_difference() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 10) - int4range(1, 5)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[5,10)");
}

// ── Equality / comparison ─────────────────────────────────────────────────────

#[test]
fn test_range_equality() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 5) = int4range(1, 5)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
}

#[test]
fn test_range_not_equal() {
    let (mut s, mut t) = setup();
    let r = rows(run(
        "SELECT int4range(1, 5) <> int4range(1, 6)",
        &mut s,
        &mut t,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
}

// ── Cast ──────────────────────────────────────────────────────────────────────

#[test]
fn test_cast_text_to_int4range() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT CAST('[1,5)' AS INT4RANGE)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[1,5)");
}

#[test]
fn test_cast_text_empty_range() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT CAST('empty' AS INT4RANGE)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "empty");
}

// ── CREATE TABLE + INSERT + SELECT ────────────────────────────────────────────

#[test]
fn test_create_table_with_range_column() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE reservations (id INT PRIMARY KEY, slot INT4RANGE)",
        &mut s,
        &mut t,
    );
    run(
        "INSERT INTO reservations VALUES (1, int4range(9, 17))",
        &mut s,
        &mut t,
    );
    let r = rows(run(
        "SELECT slot FROM reservations WHERE id = 1",
        &mut s,
        &mut t,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].to_string(), "[9,17)");
}

#[test]
fn test_range_in_where_clause() {
    let (mut s, mut t) = setup();
    run(
        "CREATE TABLE slots (id INT PRIMARY KEY, period INT4RANGE)",
        &mut s,
        &mut t,
    );
    run(
        "INSERT INTO slots VALUES (1, int4range(1, 10))",
        &mut s,
        &mut t,
    );
    run(
        "INSERT INTO slots VALUES (2, int4range(20, 30))",
        &mut s,
        &mut t,
    );
    run(
        "INSERT INTO slots VALUES (3, int4range(5, 15))",
        &mut s,
        &mut t,
    );

    // Find ranges that contain 7
    let r = rows(run(
        "SELECT id FROM slots WHERE period @> 7",
        &mut s,
        &mut t,
    ));
    let ids: Vec<i32> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!("expected Int"),
        })
        .collect();
    assert!(ids.contains(&1), "slot 1 [1,10) should contain 7");
    assert!(ids.contains(&3), "slot 3 [5,15) should contain 7");
    assert!(!ids.contains(&2), "slot 2 [20,30) should not contain 7");
}

#[test]
fn test_range_null_upper_unbounded() {
    let (mut s, mut t) = setup();
    let r = rows(run("SELECT int4range(5, NULL)", &mut s, &mut t));
    assert_eq!(r.len(), 1);
    let v = r[0][0].to_string();
    assert!(v.starts_with("[5,") && v.ends_with(')'), "got: {v}");
}

#[test]
fn test_int4range_canonical_inclusive_upper() {
    let (mut s, mut t) = setup();
    // [1,4] with discrete canonicalization → [1,5)
    let r = rows(run("SELECT int4range(1, 4, '[]')", &mut s, &mut t));
    assert_eq!(r[0][0].to_string(), "[1,5)");
}
