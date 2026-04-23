use axiomdb_network::mysql::SharedDatabase;
use axiomdb_sql::{SchemaCache, SessionContext};
use axiomdb_types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static CHANNEL_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_channel() -> String {
    format!("jobs_{}", CHANNEL_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn rows(result: axiomdb_sql::QueryResult) -> Vec<Vec<Value>> {
    match result {
        axiomdb_sql::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn shared_database_routes_notifications_between_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDatabase::open(dir.path()).unwrap();
    let channel = unique_channel();

    let mut listener = SessionContext::new();
    let mut listener_cache = SchemaCache::new();
    let mut emitter = SessionContext::new();
    let mut emitter_cache = SchemaCache::new();

    db.execute_query(
        &format!("LISTEN {channel}"),
        &mut listener,
        &mut listener_cache,
    )
    .unwrap();
    db.execute_query(
        &format!("NOTIFY {channel}, 'ready'"),
        &mut emitter,
        &mut emitter_cache,
    )
    .unwrap();

    let got = rows(
        db.execute_query("SHOW NOTIFICATIONS", &mut listener, &mut listener_cache)
            .unwrap()
            .0,
    );
    assert_eq!(
        got,
        vec![vec![
            Value::Text(channel.into()),
            Value::Text("ready".into())
        ]]
    );
}

#[test]
fn shared_database_respects_commit_and_reset_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDatabase::open(dir.path()).unwrap();
    let channel = unique_channel();

    let mut listener = SessionContext::new();
    let mut listener_cache = SchemaCache::new();
    let mut emitter = SessionContext::new();
    let mut emitter_cache = SchemaCache::new();

    db.execute_query(
        &format!("LISTEN {channel}"),
        &mut listener,
        &mut listener_cache,
    )
    .unwrap();
    db.execute_query("BEGIN", &mut emitter, &mut emitter_cache)
        .unwrap();
    db.execute_query(
        &format!("NOTIFY {channel}, 'tx'"),
        &mut emitter,
        &mut emitter_cache,
    )
    .unwrap();

    let before_commit = rows(
        db.execute_query("SHOW NOTIFICATIONS", &mut listener, &mut listener_cache)
            .unwrap()
            .0,
    );
    assert!(before_commit.is_empty());

    db.execute_query("COMMIT", &mut emitter, &mut emitter_cache)
        .unwrap();
    let after_commit = rows(
        db.execute_query("SHOW NOTIFICATIONS", &mut listener, &mut listener_cache)
            .unwrap()
            .0,
    );
    assert_eq!(
        after_commit,
        vec![vec![Value::Text(channel.into()), Value::Text("tx".into())]]
    );

    listener.cleanup_notification_runtime();
    let after_cleanup = rows(
        db.execute_query("SHOW NOTIFICATIONS", &mut listener, &mut listener_cache)
            .unwrap()
            .0,
    );
    assert!(after_cleanup.is_empty());
}
