mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_core::error::DbError;
use axiomdb_sql::{ast::Stmt, parse, QueryResult, SessionContext};
use axiomdb_types::Value;

static CHANNEL_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_channel() -> String {
    format!("jobs_{}", CHANNEL_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn parses_listen_unlisten_notify_and_show_notifications() {
    let stmt = parse("LISTEN jobs", None).unwrap();
    assert!(matches!(stmt, Stmt::Listen(_)));

    let stmt = parse("UNLISTEN jobs", None).unwrap();
    assert!(matches!(stmt, Stmt::Unlisten(_)));

    let stmt = parse("UNLISTEN *", None).unwrap();
    assert!(matches!(stmt, Stmt::Unlisten(_)));

    let stmt = parse("NOTIFY jobs, 'ready'", None).unwrap();
    assert!(matches!(stmt, Stmt::Notify(_)));

    let stmt = parse("SHOW NOTIFICATIONS", None).unwrap();
    assert!(matches!(stmt, Stmt::ShowNotifications));
}

#[test]
fn listen_notify_show_notifications_across_sessions() {
    let (mut storage, mut txn, mut bloom, mut listener) = common::setup_ctx();
    let mut emitter = SessionContext::new();
    let channel = unique_channel();

    common::run_ctx(
        &format!("LISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut listener,
    )
    .unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'ready'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Text(channel), Value::Text("ready".into())]]
    );

    let drained = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert!(drained.is_empty(), "queue must drain after read");
}

#[test]
fn notify_does_not_echo_back_to_emitter() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let channel = unique_channel();
    common::run_ctx(
        &format!("LISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'self'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let got = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(
        got.is_empty(),
        "emitter should not receive its own notification"
    );
}

#[test]
fn notify_inside_transaction_flushes_on_commit_and_discards_on_rollback() {
    let (mut storage, mut txn, mut bloom, mut listener) = common::setup_ctx();
    let mut emitter = SessionContext::new();
    let channel = unique_channel();

    common::run_ctx(
        &format!("LISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut listener,
    )
    .unwrap();

    common::run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'commit-me'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    let before_commit = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert!(
        before_commit.is_empty(),
        "notification must wait for commit"
    );
    common::run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();

    let after_commit = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert_eq!(
        after_commit,
        vec![vec![
            Value::Text(channel.clone()),
            Value::Text("commit-me".into())
        ]]
    );

    common::run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'rollback-me'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    common::run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();

    let after_rollback = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert!(
        after_rollback.is_empty(),
        "rolled-back notification must be discarded"
    );
}

#[test]
fn rollback_to_savepoint_discards_later_notifications_only() {
    let (mut storage, mut txn, mut bloom, mut listener) = common::setup_ctx();
    let mut emitter = SessionContext::new();
    let channel = unique_channel();

    common::run_ctx(
        &format!("LISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut listener,
    )
    .unwrap();
    common::run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'before'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    common::run_ctx(
        "SAVEPOINT sp1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'after'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    common::run_ctx(
        "ROLLBACK TO SAVEPOINT sp1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    common::run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut emitter).unwrap();

    let got = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Text(channel), Value::Text("before".into())]]
    );
}

#[test]
fn unlisten_and_invalid_payload_behave_correctly() {
    let (mut storage, mut txn, mut bloom, mut listener) = common::setup_ctx();
    let mut emitter = SessionContext::new();
    let channel = unique_channel();

    common::run_ctx(
        &format!("LISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut listener,
    )
    .unwrap();
    common::run_ctx(
        &format!("UNLISTEN {channel}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut listener,
    )
    .unwrap();
    common::run_ctx(
        &format!("NOTIFY {channel}, 'nobody'"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .unwrap();
    let got = rows(
        common::run_ctx(
            "SHOW NOTIFICATIONS",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut listener,
        )
        .unwrap(),
    );
    assert!(got.is_empty());

    let err = common::run_ctx(
        &format!("NOTIFY {channel}, 42"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut emitter,
    )
    .expect_err("non-string payload should be rejected");
    match err {
        DbError::InvalidValue { reason } => {
            assert!(reason.contains("string literal"), "got {reason}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
