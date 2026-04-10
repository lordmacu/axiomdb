//! Phase 7.7 / 40.10 — concurrency tests on the shared database handle.
//!
//! Validates the `Arc<SharedDatabase>` architecture:
//! - Multiple sessions can execute without an outer `RwLock<Database>`
//! - Concurrent readers observe stable results
//! - Concurrent writers preserve consistency for disjoint keys
//! - Data remains consistent after concurrent modifications

use std::sync::Arc;

use axiomdb_network::mysql::SharedDatabase;
use axiomdb_sql::{SchemaCache, SessionContext};

/// Helper: open a test database in a temp directory.
fn open_test_db() -> (tempfile::TempDir, Arc<SharedDatabase>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = SharedDatabase::open(dir.path()).expect("open test db");
    (dir, Arc::new(db))
}

/// Helper: execute SQL with a fresh autocommit session.
fn exec(db: &SharedDatabase, sql: &str) {
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    db.execute_query(sql, &mut session, &mut cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
}

/// Helper: execute SQL and return row count.
fn count_rows(db: &SharedDatabase, sql: &str) -> usize {
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    let (result, _) = db
        .execute_query(sql, &mut session, &mut cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
    match result {
        axiomdb_sql::result::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

/// Helper: execute SQL while preserving the provided session state.
fn exec_in_session(
    db: &SharedDatabase,
    sql: &str,
    session: &mut SessionContext,
    cache: &mut SchemaCache,
) {
    db.execute_query(sql, session, cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
}

// ── Test: multiple concurrent readers ────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_readers_dont_block() {
    let (_dir, db) = open_test_db();

    // Setup: create table and insert data.
    exec(&db, "CREATE TABLE readers (id INT, val INT)");
    for i in 1..=100 {
        exec(&db, &format!("INSERT INTO readers VALUES ({i}, {i})"));
    }

    // Launch 8 concurrent readers — all should complete without a shared
    // outer database lock serializing the sessions.
    let mut handles = Vec::new();
    for reader_id in 0..8u32 {
        let db_clone = Arc::clone(&db);
        handles.push(tokio::task::spawn_blocking(move || {
            for _ in 0..5 {
                let count = count_rows(&db_clone, "SELECT id, val FROM readers WHERE id <= 50");
                assert_eq!(count, 50, "reader {reader_id} should see 50 rows");
            }
        }));
    }

    // All readers should complete quickly (no deadlock, no blocking).
    for h in handles {
        h.await.expect("reader task should not panic");
    }
}

// ── Test: concurrent writers preserve consistency ────────────────────────────

#[tokio::test]
async fn test_concurrent_writers_preserve_consistency() {
    let (_dir, db) = open_test_db();

    exec(
        &db,
        "CREATE TABLE writers (id INT NOT NULL, val INT, PRIMARY KEY(id))",
    );

    let mut handles = Vec::new();
    for worker_id in 0..4 {
        let db_clone = Arc::clone(&db);
        handles.push(tokio::task::spawn_blocking(move || {
            for offset in 0..25 {
                let id = worker_id * 25 + offset + 1;
                exec(
                    &db_clone,
                    &format!("INSERT INTO writers VALUES ({id}, {id})"),
                );
            }
        }));
    }

    for handle in handles {
        handle.await.expect("writer task should not panic");
    }

    let count = count_rows(&db, "SELECT id FROM writers");
    assert_eq!(count, 100, "all concurrent inserts should be committed");
}

// ── Test: sequential writes maintain consistency ─────────────────────────────

#[tokio::test]
async fn test_sequential_writers_consistent() {
    let (_dir, db) = open_test_db();

    exec(
        &db,
        "CREATE TABLE counter (id INT NOT NULL, n INT, PRIMARY KEY(id))",
    );
    exec(&db, "INSERT INTO counter VALUES (1, 0)");

    // 10 sequential increments via separate autocommit sessions.
    for _ in 0..10 {
        exec(&db, "UPDATE counter SET n = n + 1 WHERE id = 1");
    }

    // Final value should be exactly 10.
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    let (result, _) = db
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

    exec(&db, "CREATE TABLE mixed (id INT, val TEXT)");

    // Alternate: write a row, read all rows, verify count.
    for i in 1..=20 {
        exec(&db, &format!("INSERT INTO mixed VALUES ({i}, 'v{i}')"));
        let count = count_rows(&db, "SELECT id FROM mixed");
        assert_eq!(count, i as usize, "after insert {i}, should see {i} rows");
    }
}

// ── Test: delete + read consistency ──────────────────────────────────────────

#[tokio::test]
async fn test_delete_invisible_to_subsequent_reads() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE del_vis (id INT, val INT)");
    for i in 1..=10 {
        exec(&db, &format!("INSERT INTO del_vis VALUES ({i}, {i})"));
    }

    // Delete odd rows.
    exec(&db, "DELETE FROM del_vis WHERE id % 2 = 1");

    // Only even rows should be visible.
    let count = count_rows(&db, "SELECT id FROM del_vis");
    assert_eq!(count, 5, "only 5 even rows should remain");
}

// ── Test: index scan with dead entries (7.3b lazy delete) ────────────────────

#[tokio::test]
async fn test_index_scan_filters_dead_entries() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE idx_dead (id INT, status TEXT)");
    exec(&db, "CREATE INDEX idx_status ON idx_dead (status)");
    for i in 1..=10 {
        let status = if i <= 5 { "active" } else { "done" };
        exec(
            &db,
            &format!("INSERT INTO idx_dead VALUES ({i}, '{status}')"),
        );
    }

    // Delete all 'done' rows — lazy delete leaves index entries.
    exec(&db, "DELETE FROM idx_dead WHERE status = 'done'");

    // Index scan for 'done' should return 0 rows (dead entries filtered).
    let count = count_rows(&db, "SELECT id FROM idx_dead WHERE status = 'done'");
    assert_eq!(count, 0, "dead index entries should be invisible");

    // Index scan for 'active' should return 5 rows.
    let count = count_rows(&db, "SELECT id FROM idx_dead WHERE status = 'active'");
    assert_eq!(count, 5);
}

// ── Test: vacuum reclaims dead entries ───────────────────────────────────────

#[tokio::test]
async fn test_vacuum_cleans_dead_rows_and_index_entries() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE vac_test (id INT, tag TEXT)");
    exec(&db, "CREATE INDEX idx_tag ON vac_test (tag)");
    for i in 1..=20 {
        exec(&db, &format!("INSERT INTO vac_test VALUES ({i}, 'tag{i}')"));
    }

    // Delete half the rows.
    exec(&db, "DELETE FROM vac_test WHERE id <= 10");

    // Vacuum should clean up.
    exec(&db, "VACUUM vac_test");

    // Remaining 10 rows should be visible.
    let count = count_rows(&db, "SELECT id FROM vac_test");
    assert_eq!(count, 10);
}

// ── Test: savepoint rollback in concurrent context ───────────────────────────

#[tokio::test]
async fn test_savepoint_within_transaction() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE sp_test (id INT, val INT)");

    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();
    exec_in_session(&db, "BEGIN", &mut session, &mut cache);
    exec_in_session(
        &db,
        "INSERT INTO sp_test VALUES (1, 100)",
        &mut session,
        &mut cache,
    );
    exec_in_session(&db, "SAVEPOINT sp1", &mut session, &mut cache);
    exec_in_session(
        &db,
        "INSERT INTO sp_test VALUES (2, 200)",
        &mut session,
        &mut cache,
    );
    exec_in_session(&db, "ROLLBACK TO sp1", &mut session, &mut cache);
    exec_in_session(&db, "COMMIT", &mut session, &mut cache);

    // After commit: only row 1 should exist.
    let count = count_rows(&db, "SELECT id FROM sp_test");
    assert_eq!(count, 1, "only row 1 should survive savepoint rollback");
}

// ── Test: concurrent insert + select consistency ─────────────────────────────

#[tokio::test]
async fn test_concurrent_insert_and_select() {
    let (_dir, db) = open_test_db();

    exec(&db, "CREATE TABLE conc (id INT)");

    // Writer task: insert 50 rows.
    let db_w = Arc::clone(&db);
    let writer = tokio::task::spawn_blocking(move || {
        for i in 1..=50 {
            exec(&db_w, &format!("INSERT INTO conc VALUES ({i})"));
        }
    });

    // Reader task: periodically count rows (should be monotonically increasing).
    let db_r = Arc::clone(&db);
    let reader = tokio::task::spawn_blocking(move || {
        let mut prev_count = 0usize;
        for _ in 0..20 {
            let count = count_rows(&db_r, "SELECT id FROM conc");
            assert!(
                count >= prev_count,
                "row count should be monotonically increasing: {prev_count} -> {count}"
            );
            prev_count = count;
            std::thread::yield_now();
        }
    });

    writer.await.expect("writer should not panic");
    reader.await.expect("reader should not panic");

    // Final count should be exactly 50.
    let count = count_rows(&db, "SELECT id FROM conc");
    assert_eq!(count, 50);
}
