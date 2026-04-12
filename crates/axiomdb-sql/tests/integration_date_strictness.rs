//! Regression tests for Phase 12.9 date strictness.
//!
//! Verifies that obviously-invalid dates — zero-date, month 13, Feb 30 —
//! are rejected with `InvalidCoercion` rather than silently accepted.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

use common::*;

fn setup_with_date_table() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE events (id INT, when_ DATE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn must_reject(sql: &str) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_with_date_table();
    let err = run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx)
        .expect_err("invalid date must be rejected");
    assert!(
        matches!(
            err,
            DbError::InvalidCoercion { .. } | DbError::InvalidValue { .. }
        ),
        "unexpected error type for {sql:?}: {err:?}",
    );
}

#[test]
fn zero_date_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '0000-00-00')");
}

#[test]
fn month_13_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2024-13-01')");
}

#[test]
fn month_0_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2024-00-15')");
}

#[test]
fn day_0_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2024-01-00')");
}

#[test]
fn feb_30_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2024-02-30')");
}

#[test]
fn feb_29_non_leap_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2023-02-29')");
}

#[test]
fn feb_29_leap_year_is_accepted() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_with_date_table();
    run_ctx(
        "INSERT INTO events VALUES (1, '2024-02-29')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn day_32_is_rejected() {
    must_reject("INSERT INTO events VALUES (1, '2024-01-32')");
}

#[test]
fn april_31_is_rejected() {
    // April has 30 days.
    must_reject("INSERT INTO events VALUES (1, '2024-04-31')");
}
