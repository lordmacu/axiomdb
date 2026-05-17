//! Tests for Attack 6 — SET synchronous = STRICT|NORMAL|OFF|DEFAULT.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-deferred-fsync.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-deferred-fsync.md`
//!
//! Step 6.2 (this file at first): unit tests for the SessionDurability
//! enum + parse_synchronous_setting + SessionContext getter/setter.
//! Subsequent steps add end-to-end tests through the SET dispatcher
//! and the autocommit wire-up.

use axiomdb_sql::session::{parse_synchronous_setting, SessionDurability};
use axiomdb_sql::SessionContext;

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
