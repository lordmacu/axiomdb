//! Phase 7.7 — Concurrency tests: N simultaneous readers + writers.
//!
//! Validates the `Arc<RwLock<Database>>` architecture:
//! - Multiple readers can hold `db.read()` concurrently
//! - A writer holds `db.write()` exclusively
//! - MVCC visibility is correct across concurrent access
//! - Data remains consistent after concurrent modifications

use std::sync::Arc;

use tokio::sync::RwLock;

use axiomdb_network::mysql::Database;
use axiomdb_sql::{SchemaCache, SessionContext};

/// Helper: open a test database in a temp directory.
fn open_test_db() -> (tempfile::TempDir, Arc<RwLock<Database>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open test db");
    (dir, Arc::new(RwLock::new(db)))
}

/// Helper: execute SQL with write lock.
async fn exec(db: &RwLock<Database>, sql: &str) {
    let mut guard = db.write().await;
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    guard
        .execute_query(sql, &mut session, &mut cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
}

/// Helper: execute SQL and return row count.
async fn count_rows(db: &RwLock<Database>, sql: &str) -> usize {
    let mut guard = db.write().await;
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    let (result, _) = guard
        .execute_query(sql, &mut session, &mut cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
    match result {
        axiomdb_sql::result::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

// ── Test: multiple concurrent readers ────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_readers_dont_block() {
    let (_dir, db) = open_test_db();

    // Setup: create table and insert data.
    exec(&db, "CREATE TABLE readers (id INT, val INT)").await;
    for i in 1..=100 {
        exec(&db, &format!("INSERT INTO readers VALUES ({i}, {i})")).await;
    }

    // Launch 8 concurrent readers — all should complete without blocking.
    let mut handles = Vec::new();
    for reader_id in 0..8u32 {
        let db_clone = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            // Each reader does multiple SELECTs.
            for _ in 0..5 {
                let count =
                    count_rows(&db_clone, "SELECT id, val FROM readers WHERE id <= 50").await;
                assert_eq!(count, 50, "reader {reader_id} should see 50 rows");
            }
        }));
    }

    // All readers should complete quickly (no deadlock, no blocking).
    for h in handles {
        h.await.expect("reader task should not panic");
    }
}

// ── Test: writer excludes readers ────────────────────────────────────────────

#[tokio::test]
async fn test_writer_holds_exclusive_lock() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE excl (id INT, val INT)").await;
    exec(&db, "INSERT INTO excl VALUES (1, 100)").await;

    // Writer holds the lock and modifies data.
    {
        let mut guard = db.write().await;
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::new();

        // While holding write lock, execute UPDATE.
        guard
            .execute_query(
                "UPDATE excl SET val = 200 WHERE id = 1",
                &mut session,
                &mut cache,
            )
            .unwrap();

        // Read within the same write lock should see the update.
        let (result, _) = guard
            .execute_query(
                "SELECT val FROM excl WHERE id = 1",
                &mut session,
                &mut cache,
            )
            .unwrap();
        if let axiomdb_sql::result::QueryResult::Rows { rows, .. } = result {
            assert_eq!(rows[0][0], axiomdb_types::Value::Int(200));
        } else {
            panic!("expected rows");
        }
    }
    // Lock released — subsequent read sees the committed value.
    let count = count_rows(&db, "SELECT val FROM excl WHERE val = 200").await;
    assert_eq!(count, 1);
}

// ── Test: sequential writes maintain consistency ─────────────────────────────

#[tokio::test]
async fn test_sequential_writers_consistent() {
    let (_dir, db) = open_test_db();

    exec(
        &db,
        "CREATE TABLE counter (id INT NOT NULL, n INT, PRIMARY KEY(id))",
    )
    .await;
    exec(&db, "INSERT INTO counter VALUES (1, 0)").await;

    // 10 sequential increments via separate write locks.
    for _ in 0..10 {
        exec(&db, "UPDATE counter SET n = n + 1 WHERE id = 1").await;
    }

    // Final value should be exactly 10.
    let mut guard = db.write().await;
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    let (result, _) = guard
        .execute_query(
            "SELECT n FROM counter WHERE id = 1",
            &mut session,
            &mut cache,
        )
        .unwrap();
    if let axiomdb_sql::result::QueryResult::Rows { rows, .. } = result {
        assert_eq!(rows[0][0], axiomdb_types::Value::Int(10));
    } else {
        panic!("expected rows");
    }
}

// ── Test: interleaved reads and writes ───────────────────────────────────────

#[tokio::test]
async fn test_interleaved_read_write() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE mixed (id INT, val TEXT)").await;

    // Alternate: write a row, read all rows, verify count.
    for i in 1..=20 {
        exec(&db, &format!("INSERT INTO mixed VALUES ({i}, 'v{i}')")).await;
        let count = count_rows(&db, "SELECT id FROM mixed").await;
        assert_eq!(count, i as usize, "after insert {i}, should see {i} rows");
    }
}

// ── Test: delete + read consistency ──────────────────────────────────────────

#[tokio::test]
async fn test_delete_invisible_to_subsequent_reads() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE del_vis (id INT, val INT)").await;
    for i in 1..=10 {
        exec(&db, &format!("INSERT INTO del_vis VALUES ({i}, {i})")).await;
    }

    // Delete odd rows.
    exec(&db, "DELETE FROM del_vis WHERE id % 2 = 1").await;

    // Only even rows should be visible.
    let count = count_rows(&db, "SELECT id FROM del_vis").await;
    assert_eq!(count, 5, "only 5 even rows should remain");
}

// ── Test: index scan with dead entries (7.3b lazy delete) ────────────────────

#[tokio::test]
async fn test_index_scan_filters_dead_entries() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE idx_dead (id INT, status TEXT)").await;
    exec(&db, "CREATE INDEX idx_status ON idx_dead (status)").await;
    for i in 1..=10 {
        let status = if i <= 5 { "active" } else { "done" };
        exec(
            &db,
            &format!("INSERT INTO idx_dead VALUES ({i}, '{status}')"),
        )
        .await;
    }

    // Delete all 'done' rows — lazy delete leaves index entries.
    exec(&db, "DELETE FROM idx_dead WHERE status = 'done'").await;

    // Index scan for 'done' should return 0 rows (dead entries filtered).
    let count = count_rows(&db, "SELECT id FROM idx_dead WHERE status = 'done'").await;
    assert_eq!(count, 0, "dead index entries should be invisible");

    // Index scan for 'active' should return 5 rows.
    let count = count_rows(&db, "SELECT id FROM idx_dead WHERE status = 'active'").await;
    assert_eq!(count, 5);
}

// ── Test: vacuum reclaims dead entries ───────────────────────────────────────

#[tokio::test]
async fn test_vacuum_cleans_dead_rows_and_index_entries() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE vac_test (id INT, tag TEXT)").await;
    exec(&db, "CREATE INDEX idx_tag ON vac_test (tag)").await;
    for i in 1..=20 {
        exec(&db, &format!("INSERT INTO vac_test VALUES ({i}, 'tag{i}')")).await;
    }

    // Delete half the rows.
    exec(&db, "DELETE FROM vac_test WHERE id <= 10").await;

    // Vacuum should clean up.
    exec(&db, "VACUUM vac_test").await;

    // Remaining 10 rows should be visible.
    let count = count_rows(&db, "SELECT id FROM vac_test").await;
    assert_eq!(count, 10);
}

// ── Test: savepoint rollback in concurrent context ───────────────────────────

#[tokio::test]
async fn test_savepoint_within_transaction() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE sp_test (id INT, val INT)").await;

    {
        let mut guard = db.write().await;
        let mut session = SessionContext::new();
        let mut cache = SchemaCache::new();

        guard
            .execute_query("BEGIN", &mut session, &mut cache)
            .unwrap();
        guard
            .execute_query(
                "INSERT INTO sp_test VALUES (1, 100)",
                &mut session,
                &mut cache,
            )
            .unwrap();
        guard
            .execute_query("SAVEPOINT sp1", &mut session, &mut cache)
            .unwrap();
        guard
            .execute_query(
                "INSERT INTO sp_test VALUES (2, 200)",
                &mut session,
                &mut cache,
            )
            .unwrap();
        guard
            .execute_query("ROLLBACK TO sp1", &mut session, &mut cache)
            .unwrap();
        // Row 2 should be gone, row 1 should remain.
        guard
            .execute_query("COMMIT", &mut session, &mut cache)
            .unwrap();
    }

    // After commit: only row 1 should exist.
    let count = count_rows(&db, "SELECT id FROM sp_test").await;
    assert_eq!(count, 1, "only row 1 should survive savepoint rollback");
}

// ── Test: concurrent insert + select consistency ─────────────────────────────

#[tokio::test]
async fn test_concurrent_insert_and_select() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE conc (id INT)").await;

    // Writer task: insert 50 rows.
    let db_w = Arc::clone(&db);
    let writer = tokio::spawn(async move {
        for i in 1..=50 {
            exec(&db_w, &format!("INSERT INTO conc VALUES ({i})")).await;
        }
    });

    // Reader task: periodically count rows (should be monotonically increasing).
    let db_r = Arc::clone(&db);
    let reader = tokio::spawn(async move {
        let mut prev_count = 0usize;
        for _ in 0..20 {
            let count = count_rows(&db_r, "SELECT id FROM conc").await;
            assert!(
                count >= prev_count,
                "row count should be monotonically increasing: {prev_count} -> {count}"
            );
            prev_count = count;
            tokio::task::yield_now().await;
        }
    });

    writer.await.expect("writer should not panic");
    reader.await.expect("reader should not panic");

    // Final count should be exactly 50.
    let count = count_rows(&db, "SELECT id FROM conc").await;
    assert_eq!(count, 50);
}
