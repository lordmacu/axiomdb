/// Phase 13.7 — Integration tests for SELECT … FOR UPDATE / FOR SHARE [NOWAIT].
///
/// All tests that verify lock conflict use NOWAIT (so they can run single-threaded).
/// Blocking-wait and deadlock behaviors are covered by axiomdb-lock unit tests.
mod common;

use std::sync::Arc;
use std::time::Duration;

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_lock::LockManager;
use axiomdb_sql::{
    analyze_with_defaults, bloom::BloomRegistry, execute_with_ctx_locked, parse, QueryResult,
    SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Shared helpers ────────────────────────────────────────────────────────────

fn setup_with_lock() -> (MemoryStorage, TxnManager, Arc<LockManager>) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let storage = MemoryStorage::new();
    CatalogBootstrap::init(&storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let lm = Arc::new(LockManager::new());
    (storage, txn, lm)
}

fn run_locked(
    sql: &str,
    storage: &MemoryStorage,
    txn: &TxnManager,
    lm: &LockManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = if let Some(ref ct) = ctx.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };
    let analyzed = analyze_with_defaults(
        stmt,
        storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )?;
    execute_with_ctx_locked(analyzed, storage, txn, bloom, Some(lm), ctx)
}

fn ok_locked(
    sql: &str,
    storage: &MemoryStorage,
    txn: &TxnManager,
    lm: &LockManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_locked(sql, storage, txn, lm, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn setup_table(
    storage: &MemoryStorage,
    txn: &TxnManager,
    lm: &LockManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) {
    ok_locked(
        "CREATE TABLE lock_test (id INT, val TEXT)",
        storage,
        txn,
        lm,
        bloom,
        ctx,
    );
    ok_locked(
        "INSERT INTO lock_test VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        storage,
        txn,
        lm,
        bloom,
        ctx,
    );
}

// ── Test 1: Basic FOR UPDATE returns rows ─────────────────────────────────────

#[test]
fn test_for_update_returns_rows() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    ok_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM lock_test FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 3);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);
}

// ── Test 2: FOR UPDATE on empty table ─────────────────────────────────────────

#[test]
fn test_for_update_empty_table() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE empty_lock (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    ok_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM empty_lock FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 0);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);
}

// ── Test 3: Autocommit — no error, rows returned ─────────────────────────────

#[test]
fn test_for_update_autocommit_no_error() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    // No explicit BEGIN — autocommit, no active txn → locking skipped silently.
    let result = ok_locked(
        "SELECT id FROM lock_test FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 3);
}

// ── Test 4: FOR KEY SHARE maps to Shared lock ─────────────────────────────────

#[test]
fn test_for_key_share_shared_lock() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE ks_test (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked(
        "INSERT INTO ks_test VALUES (1), (2)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    ok_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM ks_test FOR KEY SHARE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);
}

// ── Test 5: FOR NO KEY UPDATE maps to Exclusive lock ─────────────────────────

#[test]
fn test_for_no_key_update_exclusive_lock() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE nku_test (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked(
        "INSERT INTO nku_test VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    ok_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM nku_test FOR NO KEY UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 1);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);
}

// ── Test 6: LOCK IN SHARE MODE maps to ForShare + Block ──────────────────────

#[test]
fn test_lock_in_share_mode() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    ok_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM lock_test LOCK IN SHARE MODE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert_eq!(rows(result).len(), 3);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);
}

// ── Test 7: NOWAIT fails immediately when row is locked by another txn ────────

#[test]
fn test_for_update_nowait_fails_immediately() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE nowait_test (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO nowait_test VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A acquires exclusive lock.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked(
        "SELECT id FROM nowait_test FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();

    // Txn B requests exclusive + NOWAIT → must fail immediately (not wait 50s).
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let start = std::time::Instant::now();
    let result = run_locked(
        "SELECT id FROM nowait_test FOR UPDATE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    let elapsed = start.elapsed();

    assert!(result.is_err(), "expected NOWAIT conflict error");
    assert!(
        elapsed < Duration::from_secs(2),
        "NOWAIT must not block; elapsed={elapsed:?}"
    );
    match result.unwrap_err() {
        DbError::LockTimeout => {}
        other => panic!("expected LockTimeout, got {other:?}"),
    }

    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
}

// ── Test 8: FOR SHARE compatible with FOR SHARE ───────────────────────────────

#[test]
fn test_for_share_compatible_with_for_share() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE share_compat (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO share_compat VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A: FOR SHARE.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked(
        "SELECT id FROM share_compat FOR SHARE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();

    // Txn B: FOR SHARE — S+S is compatible, must succeed.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let result = run_locked(
        "SELECT id FROM share_compat FOR SHARE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    assert!(result.is_ok(), "FOR SHARE + FOR SHARE must be compatible");

    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
}

// ── Test 9: FOR SHARE blocked by FOR UPDATE (via NOWAIT) ──────────────────────

#[test]
fn test_for_share_blocked_by_for_update_nowait() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE xs_test (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO xs_test VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A holds exclusive lock.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked(
        "SELECT id FROM xs_test FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();

    // Txn B requests shared lock + NOWAIT — X and S conflict → fail.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let result = run_locked(
        "SELECT id FROM xs_test FOR SHARE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    assert!(result.is_err(), "FOR SHARE NOWAIT should fail when X held");
    match result.unwrap_err() {
        DbError::LockTimeout => {}
        other => panic!("expected LockTimeout, got {other:?}"),
    }

    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
}

// ── Test 10: ROLLBACK releases locks ──────────────────────────────────────────

#[test]
fn test_rollback_releases_locks() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE rollback_lock (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO rollback_lock VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A acquires X lock.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked(
        "SELECT id FROM rollback_lock FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();

    // While A holds the lock, txn B with NOWAIT should fail.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let fail = run_locked(
        "SELECT id FROM rollback_lock FOR UPDATE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    assert!(fail.is_err(), "should fail while A holds the lock");
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();

    // Txn A rolls back — locks released.
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();

    // Txn C (new session) can now acquire immediately with NOWAIT.
    let mut ctx_c = SessionContext::new();
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_c).unwrap();
    let ok = run_locked(
        "SELECT id FROM rollback_lock FOR UPDATE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_c,
    );
    assert!(
        ok.is_ok(),
        "after ROLLBACK, lock should be released: {ok:?}"
    );
    run_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx_c).unwrap();
}

// ── Test 11: FOR UPDATE + LIMIT locks only returned rows ──────────────────────

#[test]
fn test_for_update_with_limit() {
    let (storage, txn, _) = setup_with_lock();
    let lm = Arc::new(LockManager::with_timeout(Duration::from_millis(200)));
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE limit_lock (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO limit_lock VALUES (1), (2), (3)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A locks only the first row (ORDER BY id LIMIT 1).
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    let result = run_locked(
        "SELECT id FROM limit_lock ORDER BY id LIMIT 1 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));

    // Txn B can lock row 2 (not locked by A) immediately with NOWAIT.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let ok2 = run_locked(
        "SELECT id FROM limit_lock WHERE id = 2 FOR UPDATE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    assert!(
        ok2.is_ok(),
        "row 2 not locked by A (only row 1 was): {ok2:?}"
    );

    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
}

// ── Test 12: Lock upgrade S → X on same txn ───────────────────────────────────

#[test]
fn test_lock_upgrade_s_to_x() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE upgrade_lock (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked(
        "INSERT INTO upgrade_lock VALUES (1)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    // Same txn: acquire S then X — upgrade in place.
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx).unwrap();
    run_locked(
        "SELECT id FROM upgrade_lock FOR SHARE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    )
    .unwrap();
    let result = run_locked(
        "SELECT id FROM upgrade_lock FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    assert!(
        result.is_ok(),
        "S→X upgrade on same txn should succeed: {result:?}"
    );
    run_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx).unwrap();
}

// ── Test 13: WHERE filter — only matching rows locked ─────────────────────────

#[test]
fn test_for_update_where_locks_only_matching_rows() {
    let (storage, txn, _) = setup_with_lock();
    let lm = Arc::new(LockManager::with_timeout(Duration::from_millis(200)));
    let bloom = BloomRegistry::new();
    let mut ctx_a = SessionContext::new();
    let mut ctx_b = SessionContext::new();

    ok_locked(
        "CREATE TABLE where_lock (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );
    ok_locked(
        "INSERT INTO where_lock VALUES (1), (2), (3)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    );

    // Txn A locks only row 1 (WHERE id = 1).
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked(
        "SELECT id FROM where_lock WHERE id = 1 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_a,
    )
    .unwrap();

    // Txn B can lock row 2 immediately (not held by A).
    run_locked("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
    let ok2 = run_locked(
        "SELECT id FROM where_lock WHERE id = 2 FOR UPDATE NOWAIT",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx_b,
    );
    assert!(ok2.is_ok(), "row 2 not locked by A: {ok2:?}");

    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_a).unwrap();
    run_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx_b).unwrap();
}

// ── Test 14: Deadlock detected (via LockManager sync path) ───────────────────

#[test]
fn test_deadlock_detected_via_lock_manager() {
    // This test exercises the deadlock path at the LockManager level,
    // using threads (as deadlock requires concurrent blocking waits).
    use std::sync::Arc;
    use std::sync::Barrier;

    // Short timeout so thread A doesn't block 50s after B is chosen as victim.
    let lm = Arc::new(LockManager::with_timeout(Duration::from_millis(500)));

    // Txn A holds X on page 100, slot 0.
    lm.acquire_record_lock_sync(
        1,
        100,
        0,
        axiomdb_lock::LockMode::Exclusive,
        axiomdb_lock::LockFlags::NONE,
    )
    .unwrap();
    // Txn B holds X on page 101, slot 0 (different page to avoid bitmap fast-path).
    lm.acquire_record_lock_sync(
        2,
        101,
        0,
        axiomdb_lock::LockMode::Exclusive,
        axiomdb_lock::LockFlags::NONE,
    )
    .unwrap();

    let barrier = Arc::new(Barrier::new(2));

    // Thread A (txn 1): try page 101 (held by txn 2) → blocks.
    let lm_a = Arc::clone(&lm);
    let barrier_a = Arc::clone(&barrier);
    let handle_a = std::thread::spawn(move || {
        barrier_a.wait();
        lm_a.acquire_record_lock_sync(
            1,
            101,
            0,
            axiomdb_lock::LockMode::Exclusive,
            axiomdb_lock::LockFlags::NONE,
        )
    });

    // Thread B (txn 2): try page 100 (held by txn 1) → blocks → deadlock cycle.
    let lm_b = Arc::clone(&lm);
    let barrier_b = Arc::clone(&barrier);
    let handle_b = std::thread::spawn(move || {
        barrier_b.wait();
        lm_b.acquire_record_lock_sync(
            2,
            100,
            0,
            axiomdb_lock::LockMode::Exclusive,
            axiomdb_lock::LockFlags::NONE,
        )
    });

    // B detects the cycle and aborts; A then times out (500ms) or also gets Deadlock.
    let result_b = handle_b.join().unwrap();
    let result_a = handle_a.join().unwrap();

    let got_deadlock = matches!(
        result_a,
        Err(DbError::DeadlockDetected) | Err(DbError::LockTimeout)
    ) || matches!(
        result_b,
        Err(DbError::DeadlockDetected) | Err(DbError::LockTimeout)
    );
    assert!(
        got_deadlock,
        "expected DeadlockDetected or LockTimeout on at least one side; a={result_a:?}, b={result_b:?}"
    );
}
