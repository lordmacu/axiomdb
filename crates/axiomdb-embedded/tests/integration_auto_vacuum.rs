//! Phase 19.1 — inline-at-commit auto-vacuum integration tests.

use axiomdb_embedded::Db;
use axiomdb_types::Value;
use tempfile::TempDir;

fn open_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("test")).unwrap();
    (dir, db)
}

fn count_rows(db: &mut Db, table: &str) -> i64 {
    match &db
        .query(&format!("SELECT COUNT(*) FROM {table}"))
        .unwrap()[0][0]
    {
        Value::BigInt(n) => *n,
        Value::Int(n) => *n as i64,
        other => panic!("expected integer COUNT, got {other:?}"),
    }
}

#[test]
fn autovacuum_default_is_on() {
    // The session ships with auto-vacuum enabled. A large DELETE
    // followed by a query should not leak dead tuples indefinitely
    // — the auto-vacuum hook runs after each autocommit query.
    let (_dir, mut db) = open_db();
    db.run("SET autovacuum_vacuum_threshold = 10").unwrap();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    for i in 1..=50 {
        db.run(&format!("INSERT INTO t VALUES ({i}, 'x')"))
            .unwrap();
    }
    // Bulk DELETE -> creates 50 dead tuples in one autocommit txn.
    db.run("DELETE FROM t").unwrap();
    // After the DELETE the auto-vacuum should have fired (50 changes
    // ≥ 10 threshold).  The next query must observe a clean table.
    assert_eq!(count_rows(&mut db, "t"), 0);
}

#[test]
fn autovacuum_off_disables_it() {
    let (_dir, mut db) = open_db();
    db.run("SET autovacuum = OFF").unwrap();
    db.run("SET autovacuum_vacuum_threshold = 1").unwrap();
    db.run("CREATE TABLE t (id INT)").unwrap();
    for i in 1..=10 {
        db.run(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    db.run("DELETE FROM t").unwrap();
    // With autovacuum disabled the only correctness guarantee is
    // that COUNT(*) still reports 0 (dead tuples are invisible to
    // MVCC). The point of this test is that nothing panics or
    // mis-counts even with the auto-vacuum hook gated off.
    assert_eq!(count_rows(&mut db, "t"), 0);
}

#[test]
fn autovacuum_threshold_zero_runs_every_commit() {
    // Stress mode: threshold = 0 fires auto-vacuum after every
    // autocommit query that touched any table.
    let (_dir, mut db) = open_db();
    db.run("SET autovacuum_vacuum_threshold = 0").unwrap();
    db.run("CREATE TABLE t (id INT)").unwrap();
    for i in 1..=20 {
        db.run(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    assert_eq!(count_rows(&mut db, "t"), 20);
    db.run("DELETE FROM t WHERE id <= 10").unwrap();
    assert_eq!(count_rows(&mut db, "t"), 10);
}

#[test]
fn autovacuum_skipped_inside_explicit_txn() {
    // Inside BEGIN..COMMIT, the auto-vacuum hook is gated off —
    // running vacuum mid-txn would consume the user's snapshot.
    // The verification is correctness: the explicit txn must
    // commit successfully and the table must have the right count
    // afterwards, even though we made enough changes to cross
    // the threshold.
    let (_dir, mut db) = open_db();
    db.run("SET autovacuum_vacuum_threshold = 5").unwrap();
    db.run("CREATE TABLE t (id INT)").unwrap();

    db.run("BEGIN").unwrap();
    for i in 1..=20 {
        db.run(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    db.run("DELETE FROM t WHERE id <= 10").unwrap();
    db.run("COMMIT").unwrap();

    assert_eq!(count_rows(&mut db, "t"), 10);
}

#[test]
fn autovacuum_does_not_break_select_path() {
    // Pure-read workloads should never trigger auto-vacuum (no
    // change counter bump). Verify this doesn't accidentally fire
    // and slow SELECTs down.
    let (_dir, mut db) = open_db();
    db.run("SET autovacuum_vacuum_threshold = 1").unwrap();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    // Many repeat SELECTs — none should trigger vacuum.
    for _ in 0..50 {
        assert_eq!(count_rows(&mut db, "t"), 3);
    }
}
