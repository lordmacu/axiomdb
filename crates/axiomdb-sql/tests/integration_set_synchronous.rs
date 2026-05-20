//! Tests for Attack 6 — SET synchronous = STRICT|NORMAL|OFF|DEFAULT.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-deferred-fsync.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-deferred-fsync.md`
//!
//! Step 6.2: unit tests for the SessionDurability enum +
//! parse_synchronous_setting + SessionContext getter/setter.
//! Step 6.3: end-to-end tests through the SET dispatcher and the
//! autocommit wire-up in execute_with_ctx.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::session::{parse_synchronous_setting, SessionDurability};
use axiomdb_sql::SessionContext;
use common::*;

// ── Parser tests ──────────────────────────────────────────────────────────

#[test]
fn parse_synchronous_accepts_canonical_names() {
    assert_eq!(
        parse_synchronous_setting("STRICT").unwrap(),
        SessionDurability::Strict
    );
    assert_eq!(
        parse_synchronous_setting("NORMAL").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("OFF").unwrap(),
        SessionDurability::Off
    );
}

#[test]
fn parse_synchronous_case_insensitive() {
    assert_eq!(
        parse_synchronous_setting("normal").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("Normal").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("nOrMaL").unwrap(),
        SessionDurability::Normal
    );
}

#[test]
fn parse_synchronous_accepts_sqlite_aliases() {
    // SQLite has 5 levels (OFF/ON/NORMAL/FULL/EXTRA); our 3 collapse them.
    // FULL and EXTRA both map to Strict per the spec.
    assert_eq!(
        parse_synchronous_setting("FULL").unwrap(),
        SessionDurability::Strict
    );
    assert_eq!(
        parse_synchronous_setting("EXTRA").unwrap(),
        SessionDurability::Strict
    );
    // SQLite's legacy "ON" name historically meant the same as NORMAL.
    assert_eq!(
        parse_synchronous_setting("ON").unwrap(),
        SessionDurability::Normal
    );
}

#[test]
fn parse_synchronous_accepts_numeric_forms() {
    // SQLite getSafetyLevel: 0=OFF, 1=ON(=NORMAL legacy), 2=NORMAL,
    // 3=FULL, 4=EXTRA. Per the spec we map 0→Off, 1/2→Normal, 3/4→Strict.
    assert_eq!(
        parse_synchronous_setting("0").unwrap(),
        SessionDurability::Off
    );
    assert_eq!(
        parse_synchronous_setting("1").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("2").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("3").unwrap(),
        SessionDurability::Strict
    );
    assert_eq!(
        parse_synchronous_setting("4").unwrap(),
        SessionDurability::Strict
    );
}

#[test]
fn parse_synchronous_strips_quotes_and_whitespace() {
    assert_eq!(
        parse_synchronous_setting("'NORMAL'").unwrap(),
        SessionDurability::Normal
    );
    assert_eq!(
        parse_synchronous_setting("\"STRICT\"").unwrap(),
        SessionDurability::Strict
    );
    assert_eq!(
        parse_synchronous_setting("  off  ").unwrap(),
        SessionDurability::Off
    );
}

#[test]
fn parse_synchronous_rejects_garbage() {
    let err = parse_synchronous_setting("banana").unwrap_err();
    assert!(
        matches!(err, axiomdb_core::error::DbError::InvalidValue { .. }),
        "expected InvalidValue, got {err:?}"
    );
}

// ── SessionContext getter/setter tests ────────────────────────────────────

#[test]
fn session_context_default_is_strict() {
    let ctx = SessionContext::default();
    assert_eq!(
        ctx.synchronous(),
        SessionDurability::Strict,
        "default must be Strict — no durability regression for users \
         who don't opt in"
    );
}

#[test]
fn session_context_set_synchronous_updates_value() {
    let mut ctx = SessionContext::default();
    ctx.set_synchronous(SessionDurability::Normal);
    assert_eq!(ctx.synchronous(), SessionDurability::Normal);
    ctx.set_synchronous(SessionDurability::Off);
    assert_eq!(ctx.synchronous(), SessionDurability::Off);
    ctx.set_synchronous(SessionDurability::Strict);
    assert_eq!(ctx.synchronous(), SessionDurability::Strict);
}

#[test]
fn session_durability_maps_to_wal_policy() {
    // Compile-level guarantee that the mapping covers every variant.
    use axiomdb_storage::WalDurabilityPolicy;
    assert_eq!(
        SessionDurability::Strict.to_wal_policy(),
        WalDurabilityPolicy::Strict
    );
    assert_eq!(
        SessionDurability::Normal.to_wal_policy(),
        WalDurabilityPolicy::Normal
    );
    assert_eq!(
        SessionDurability::Off.to_wal_policy(),
        WalDurabilityPolicy::Off
    );
}

// ── End-to-end SET dispatcher tests (Step 6.3) ────────────────────────────

#[test]
fn set_synchronous_through_dispatcher_updates_session() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    for (sql, expected) in [
        ("SET synchronous = 'NORMAL'", SessionDurability::Normal),
        ("SET synchronous = 'OFF'", SessionDurability::Off),
        ("SET synchronous = 'STRICT'", SessionDurability::Strict),
        ("SET synchronous = 'FULL'", SessionDurability::Strict),
        ("SET synchronous = 'EXTRA'", SessionDurability::Strict),
        ("SET synchronous = 'ON'", SessionDurability::Normal),
        ("SET synchronous = DEFAULT", SessionDurability::Strict),
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        assert_eq!(ctx.synchronous(), expected, "after: {sql}");
    }
}

#[test]
fn set_synchronous_numeric_form_through_dispatcher() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    for (sql, expected) in [
        ("SET synchronous = 0", SessionDurability::Off),
        ("SET synchronous = 1", SessionDurability::Normal),
        ("SET synchronous = 2", SessionDurability::Normal),
        ("SET synchronous = 3", SessionDurability::Strict),
        ("SET synchronous = 4", SessionDurability::Strict),
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        assert_eq!(ctx.synchronous(), expected, "after: {sql}");
    }
}

#[test]
fn set_synchronous_invalid_value_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    let result = run_ctx(
        "SET synchronous = 'banana'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(result, Err(DbError::InvalidValue { .. })),
        "expected InvalidValue, got {result:?}"
    );
    // Mode unchanged on error.
    assert_eq!(ctx.synchronous(), SessionDurability::Strict);
}

#[test]
fn set_synchronous_inside_explicit_txn_is_rejected() {
    // SQLite analog: research/sqlite/src/pragma.c:1136-1138 — `PRAGMA
    // synchronous` is rejected inside a transaction. The override is
    // captured by `BEGIN` and frozen on the ConnectionTxn until the next
    // BEGIN, so a mid-txn change would silently no-op.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let result = run_ctx(
        "SET synchronous = 'NORMAL'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(&result, Err(DbError::InvalidValue { reason }) if reason.contains("transaction")),
        "expected InvalidValue mentioning 'transaction', got {result:?}"
    );
    // Mode unchanged.
    assert_eq!(ctx.synchronous(), SessionDurability::Strict);
    // Cleanup: end the transaction so the txn manager isn't left dangling.
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    // After ROLLBACK, SET succeeds again.
    run_ctx(
        "SET synchronous = 'NORMAL'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.synchronous(), SessionDurability::Normal);
}

#[test]
fn autocommit_insert_under_synchronous_normal_still_works() {
    // Functional smoke: changing the durability mode must not break the
    // autocommit INSERT path. The override is injected at txn.begin() via
    // begin_session_txn; commit() in the WAL layer then routes through
    // flush_no_sync() instead of commit_data_sync().
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SET synchronous = 'NORMAL'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.synchronous(), SessionDurability::Normal);
    // A plain autocommit INSERT must succeed.
    run_ctx(
        "INSERT INTO t (id, v) VALUES (1, 'one'), (2, 'two')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // And the data must be visible to a subsequent SELECT in the same session.
    let res = run_ctx(
        "SELECT id, v FROM t ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let axiomdb_sql::QueryResult::Rows { rows, .. } = res else {
        panic!("expected Rows, got {res:?}");
    };
    assert_eq!(rows.len(), 2, "INSERT under NORMAL should still persist");
}
