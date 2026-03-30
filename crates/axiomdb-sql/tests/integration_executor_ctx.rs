//! Integration tests for executor behavior that depends on `SessionContext`.

mod common;

use axiomdb_sql::{QueryResult, SessionContext};
use axiomdb_types::Value;

use common::*;

#[test]
fn test_count_star_with_ctx_uses_masked_scan_without_hanging() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t (id INT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT COUNT(*) FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("COUNT(*) must return rows");
    };

    assert_eq!(rows, vec![vec![Value::BigInt(3)]]);
}

#[test]
fn test_strict_mode_default_is_on() {
    let ctx = SessionContext::new();
    assert!(ctx.strict_mode, "strict_mode must default to true");
}

#[test]
fn test_set_strict_mode_off_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(!ctx.strict_mode, "strict_mode should be OFF after SET");
}

#[test]
fn test_set_strict_mode_on_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET strict_mode = ON",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(ctx.strict_mode, "strict_mode should be ON after SET");
}

#[test]
fn test_set_strict_mode_default_restores_on() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET strict_mode = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "SET strict_mode = DEFAULT must restore true"
    );
}

#[test]
fn test_set_sql_mode_empty_string_disables_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET sql_mode = ''",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(!ctx.strict_mode, "SET sql_mode = '' must disable strict");
}

#[test]
fn test_set_sql_mode_strict_trans_tables_enables_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = 'STRICT_TRANS_TABLES'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "STRICT_TRANS_TABLES token must enable strict"
    );
}

#[test]
fn test_set_sql_mode_default_restores_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "SET sql_mode = DEFAULT must restore strict ON"
    );
}

#[test]
fn test_set_sql_mode_ansi_quotes_with_strict() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.strict_mode = false;
    run_ctx(
        "SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert!(
        ctx.strict_mode,
        "ANSI_QUOTES,STRICT_TRANS_TABLES must enable strict"
    );
}

#[test]
fn test_strict_mode_on_insert_lossy_string_to_int_errors() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    assert!(ctx.strict_mode);

    run_ctx(
        "CREATE TABLE t_strict (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "INSERT INTO t_strict VALUES ('42abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(result.is_err(), "strict ON: '42abc' into INT must error");
    // No row should have been inserted.
    let rows_after = rows(
        run_ctx(
            "SELECT * FROM t_strict",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(
        rows_after.is_empty(),
        "no row should be stored on strict error"
    );
}

#[test]
fn test_strict_mode_off_insert_lossy_string_stores_and_warns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_perm (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_perm VALUES ('42abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1, "should have exactly one warning");
    let w = &ctx.warnings[0];
    assert_eq!(w.code, 1265);
    assert!(
        w.message.contains("age") && w.message.contains("row 1"),
        "warning message: {}",
        w.message
    );

    let r = rows(
        run_ctx(
            "SELECT age FROM t_perm",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(42), "permissive result must be 42");
}

#[test]
fn test_strict_mode_off_insert_non_numeric_stores_zero_and_warns() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_zero (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_zero VALUES ('abc')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1);
    let r = rows(
        run_ctx(
            "SELECT age FROM t_zero",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(0), "non-numeric string -> permissive 0");
}

#[test]
fn test_strict_mode_off_multirow_insert_row_numbers_in_warnings() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_multi (n INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "INSERT INTO t_multi VALUES ('42abc'), ('7x')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 2, "two rows, two warnings");
    assert!(
        ctx.warnings[0].message.contains("row 1"),
        "first warning: {}",
        ctx.warnings[0].message
    );
    assert!(
        ctx.warnings[1].message.contains("row 2"),
        "second warning: {}",
        ctx.warnings[1].message
    );
}

#[test]
fn test_strict_mode_off_update_warns_on_permissive_fallback() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE TABLE t_upd (age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t_upd VALUES (10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    ctx.clear_warnings();
    run_ctx(
        "UPDATE t_upd SET age = '9x'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(ctx.warnings.len(), 1, "update must emit one warning");
    assert_eq!(ctx.warnings[0].code, 1265);

    let r = rows(
        run_ctx(
            "SELECT age FROM t_upd",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(9), "permissive update result must be 9");
}

#[test]
fn test_strict_mode_off_cast_still_errors() {
    // Non-assignment coercion (CAST) must remain unaffected by strict_mode.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET strict_mode = OFF",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT CAST('abc' AS SIGNED)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // CAST should still error even with strict OFF.
    assert!(
        result.is_err(),
        "CAST('abc' AS INT) must still error with strict OFF"
    );
}
