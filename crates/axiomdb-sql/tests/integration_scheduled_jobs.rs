//! Integration tests for cron job runtime functions (Phase 22b.1).
//!
//! Covers: cron_schedule(), cron_unschedule(), cron_enable(), cron_disable(),
//! and SELECT from information_schema.scheduled_jobs.

mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

fn text_col(row: &[Value], i: usize) -> String {
    match &row[i] {
        Value::Text(s) => s.clone(),
        other => panic!("expected text at col {i}, got {other:?}"),
    }
}

fn bool_col(row: &[Value], i: usize) -> bool {
    match &row[i] {
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::BigInt(n) => *n != 0,
        // IS columns return YES/NO text
        Value::Text(s) => s.eq_ignore_ascii_case("yes"),
        other => panic!("expected bool at col {i}, got {other:?}"),
    }
}

#[test]
fn test_cron_schedule_persists_job() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let result = run_ctx(
        "SELECT cron_schedule('cleanup', '@daily', 'DELETE FROM logs WHERE ts < NOW()')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(text_col(&rows[0], 0), "cleanup");
}

#[test]
fn test_information_schema_scheduled_jobs_shows_registered_job() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('daily_cleanup', '@daily', 'DELETE FROM tmp WHERE 1=1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT JOB_NAME, SCHEDULE, COMMAND, DATABASE_NAME, ENABLED FROM information_schema.scheduled_jobs",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 1, "expected 1 job, got {}", rows.len());
    assert_eq!(text_col(&rows[0], 0), "daily_cleanup");
    assert_eq!(text_col(&rows[0], 1), "@daily");
    assert_eq!(text_col(&rows[0], 2), "DELETE FROM tmp WHERE 1=1");
    assert!(
        bool_col(&rows[0], 4),
        "job should be enabled by default"
    );
}

#[test]
fn test_cron_schedule_multiple_jobs_visible() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('job_a', '0 * * * *', 'SELECT 1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SELECT cron_schedule('job_b', '30 2 * * *', 'SELECT 2')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT JOB_NAME FROM information_schema.scheduled_jobs ORDER BY JOB_NAME",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(text_col(&rows[0], 0), "job_a");
    assert_eq!(text_col(&rows[1], 0), "job_b");
}

#[test]
fn test_cron_unschedule_removes_job() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('to_delete', '@hourly', 'SELECT 1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let del = run_ctx(
        "SELECT cron_unschedule('to_delete')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = del else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::BigInt(n) => assert_eq!(*n, 1, "expected 1 deleted row"),
        other => panic!("expected bigint, got {other:?}"),
    }

    let result = run_ctx(
        "SELECT JOB_NAME FROM information_schema.scheduled_jobs",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 0, "job should be removed");
}

#[test]
fn test_cron_unschedule_nonexistent_returns_zero() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let result = run_ctx(
        "SELECT cron_unschedule('ghost_job')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    match &rows[0][0] {
        Value::BigInt(n) => assert_eq!(*n, 0),
        other => panic!("expected bigint, got {other:?}"),
    }
}

#[test]
fn test_cron_disable_sets_enabled_false() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('pauseable', '@weekly', 'SELECT 1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "SELECT cron_disable('pauseable')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT ENABLED FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'pauseable'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert!(!bool_col(&rows[0], 0), "job should be disabled");
}

#[test]
fn test_cron_enable_restores_enabled_true() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('toggled', '@monthly', 'VACUUM')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SELECT cron_disable('toggled')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "SELECT cron_enable('toggled')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT ENABLED FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'toggled'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert!(bool_col(&rows[0], 0), "job should be re-enabled");
}

#[test]
fn test_cron_schedule_rejects_invalid_expression() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    let err = run_ctx(
        "SELECT cron_schedule('bad', 'not a cron', 'SELECT 1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();

    let msg = format!("{err:?}");
    assert!(
        msg.contains("cron schedule") || msg.contains("fields"),
        "unexpected error: {msg}"
    );
}

#[test]
fn test_cron_schedule_accepts_5field_expressions() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for (name, schedule) in [
        ("every_min", "* * * * *"),
        ("hourly_exact", "0 * * * *"),
        ("daily_2am", "0 2 * * *"),
        ("weekly_sun", "0 0 * * 0"),
        ("monthly_1st", "0 0 1 * *"),
        ("step_5min", "*/5 * * * *"),
        ("range_hours", "0 9-17 * * *"),
        ("list_days", "0 0 1,15 * *"),
    ] {
        run_ctx(
            &format!("SELECT cron_schedule('{name}', '{schedule}', 'SELECT 1')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap_or_else(|e| panic!("schedule '{schedule}' rejected: {e:?}"));
    }

    let result = run_ctx(
        "SELECT JOB_NAME FROM information_schema.scheduled_jobs ORDER BY JOB_NAME",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 8, "expected 8 jobs registered");
}

#[test]
fn test_cron_schedule_accepts_aliases() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for alias in [
        "@hourly",
        "@daily",
        "@weekly",
        "@monthly",
        "@yearly",
        "@annually",
        "@midnight",
    ] {
        let name = alias.trim_start_matches('@');
        run_ctx(
            &format!("SELECT cron_schedule('{name}', '{alias}', 'SELECT 1')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap_or_else(|e| panic!("alias '{alias}' rejected: {e:?}"));
    }
}

#[test]
fn test_cron_schedule_upserts_existing_job() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    run_ctx(
        "SELECT cron_schedule('my_job', '@daily', 'SELECT 1')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Re-schedule with different params — should upsert, not error
    run_ctx(
        "SELECT cron_schedule('my_job', '@hourly', 'SELECT 2')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SELECT SCHEDULE, COMMAND FROM information_schema.scheduled_jobs WHERE JOB_NAME = 'my_job'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1, "upsert should keep exactly 1 row");
    assert_eq!(text_col(&rows[0], 0), "@hourly");
    assert_eq!(text_col(&rows[0], 1), "SELECT 2");
}
