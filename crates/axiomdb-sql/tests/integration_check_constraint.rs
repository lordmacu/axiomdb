//! Phase 21.6 — CHECK constraint enforcement.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

// ── Column-level inline CHECK ────────────────────────────────────────────────

#[test]
fn inline_check_rejects_violation_on_insert() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx("INSERT INTO t VALUES (10)", &mut s, &mut t, &mut b, &mut c).unwrap();
    let err = run_ctx("INSERT INTO t VALUES (-5)", &mut s, &mut t, &mut b, &mut c).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Check") || msg.contains("check") || msg.contains("violation"),
        "got {msg}"
    );
}

#[test]
fn inline_check_accepts_valid_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (0), (1), (99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let res = run_ctx("SELECT COUNT(*) FROM t", &mut s, &mut t, &mut b, &mut c).unwrap();
    match res {
        QueryResult::Rows { rows, .. } => {
            let n = match &rows[0][0] {
                Value::BigInt(n) => *n,
                Value::Int(n) => *n as i64,
                other => panic!("{other:?}"),
            };
            assert_eq!(n, 3);
        }
        _ => panic!("expected Rows"),
    }
}

// ── UPDATE also enforced ────────────────────────────────────────────────────

#[test]
fn check_enforced_on_update() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, age INT CHECK (age >= 0))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let err = run_ctx(
        "UPDATE t SET age = -1 WHERE id = 1",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Check") || msg.contains("check") || msg.contains("violation"),
        "got {msg}"
    );
}

// ── Table-level CHECK ───────────────────────────────────────────────────────

#[test]
fn table_level_check_across_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (lo INT, hi INT, CHECK (lo <= hi))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 5), (3, 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let err = run_ctx(
        "INSERT INTO t VALUES (10, 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("Check") || msg.contains("check"), "got {msg}");
}

// ── Named CHECK + ALTER ADD ─────────────────────────────────────────────────

#[test]
fn alter_add_check_validates_existing_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx("CREATE TABLE t (v INT)", &mut s, &mut t, &mut b, &mut c).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // Add a CHECK that all existing rows satisfy.
    run_ctx(
        "ALTER TABLE t ADD CONSTRAINT positive CHECK (v > 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // Future violations blocked.
    let err = run_ctx("INSERT INTO t VALUES (-1)", &mut s, &mut t, &mut b, &mut c).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("Check") || msg.contains("check"), "got {msg}");
}

#[test]
fn alter_add_check_rejects_when_existing_row_violates() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx("CREATE TABLE t (v INT)", &mut s, &mut t, &mut b, &mut c).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (-5), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let err = run_ctx(
        "ALTER TABLE t ADD CONSTRAINT positive CHECK (v > 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Check") || msg.contains("check") || msg.contains("violat"),
        "got {msg}"
    );
}

// ── NULL semantics (SQL standard: CHECK treats NULL as pass) ────────────────

#[test]
fn check_null_passes_per_standard() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (v INT CHECK (v > 0))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // NULL should be allowed — CHECK expression returns UNKNOWN, not FALSE.
    run_ctx(
        "INSERT INTO t VALUES (NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
}
