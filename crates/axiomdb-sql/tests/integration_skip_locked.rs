/// Phase 13.8b — Integration tests for SELECT … FOR UPDATE / FOR SHARE SKIP LOCKED.
///
/// All lock-conflict tests use a second SessionContext (different txn_id) with
/// NOWAIT to pre-lock rows, then verify SKIP LOCKED omits them.
mod common;

use std::sync::Arc;

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

fn int(v: i32) -> Value {
    Value::Int(v)
}

/// Create a heap table with 3 rows: (1,'a'), (2,'b'), (3,'c')
fn setup_table(
    storage: &MemoryStorage,
    txn: &TxnManager,
    lm: &LockManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) {
    ok_locked(
        "CREATE TABLE skip_test (id INT, val TEXT)",
        storage,
        txn,
        lm,
        bloom,
        ctx,
    );
    ok_locked(
        "INSERT INTO skip_test VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        storage,
        txn,
        lm,
        bloom,
        ctx,
    );
}

/// Start an explicit transaction on the given context.
fn begin(
    sql: &str,
    storage: &MemoryStorage,
    txn: &TxnManager,
    lm: &LockManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) {
    ok_locked(sql, storage, txn, lm, bloom, ctx);
}

// ── Test 1: basic — returns all unlocked rows ─────────────────────────────────

#[test]
fn test_skip_locked_returns_all_rows_when_none_locked() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    // No competing locks — SKIP LOCKED should return all 3 rows.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);

    let r = rows(result);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], int(1));
    assert_eq!(r[2][0], int(3));
}

// ── Test 2: all rows locked → 0 rows, no error ───────────────────────────────

#[test]
fn test_skip_locked_all_locked_returns_empty() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx1 = SessionContext::new();
    let mut ctx2 = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx1);

    // ctx1 locks all rows with FOR UPDATE.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx1);
    ok_locked(
        "SELECT id FROM skip_test FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx1,
    );

    // ctx2 tries SKIP LOCKED — all rows are locked, should get 0 rows, no error.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx2);
    let result = ok_locked(
        "SELECT id FROM skip_test FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx2,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx1);

    assert_eq!(rows(result).len(), 0, "all locked → 0 rows");
}

// ── Test 3: LIMIT 1 SKIP LOCKED with first row locked → returns second row ───

#[test]
fn test_skip_locked_limit_skips_first_locked_row() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx1 = SessionContext::new();
    let mut ctx2 = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx1);

    // ctx1 locks id=1 only.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx1);
    ok_locked(
        "SELECT id FROM skip_test WHERE id = 1 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx1,
    );

    // ctx2 wants LIMIT 1 — id=1 is locked, so it should return id=2.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx2);
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx2,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx1);

    let r = rows(result);
    assert_eq!(r.len(), 1, "LIMIT 1 SKIP LOCKED must return exactly 1 row");
    assert_eq!(
        r[0][0],
        int(2),
        "first unlocked row (id=1 was locked) is id=2"
    );
}

// ── Test 4: empty table → 0 rows, no error ───────────────────────────────────

#[test]
fn test_skip_locked_empty_table() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE empty_skip (id INT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let result = ok_locked(
        "SELECT id FROM empty_skip FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);

    assert_eq!(rows(result).len(), 0);
}

// ── Test 5: same txn holds the lock → row is included (idempotent) ───────────

#[test]
fn test_skip_locked_same_txn_holds_lock_includes_row() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    // Lock id=1 explicitly.
    ok_locked(
        "SELECT id FROM skip_test WHERE id = 1 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    // Same txn does SKIP LOCKED — id=1 should still be included (same txn).
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx);

    let r = rows(result);
    assert_eq!(r.len(), 3, "same txn holds the lock — all 3 rows included");
    assert_eq!(r[0][0], int(1));
}

// ── Test 6: FOR SHARE SKIP LOCKED — two sessions compatible ──────────────────

#[test]
fn test_skip_locked_for_share_compatible() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx1 = SessionContext::new();
    let mut ctx2 = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx1);

    // ctx1 acquires FOR SHARE on all rows.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx1);
    ok_locked(
        "SELECT id FROM skip_test FOR SHARE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx1,
    );

    // ctx2 also FOR SHARE SKIP LOCKED — shared locks are compatible, all rows visible.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx2);
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id FOR SHARE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx2,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx1);

    assert_eq!(
        rows(result).len(),
        3,
        "FOR SHARE is compatible — all rows should be visible"
    );
}

// ── Test 7: autocommit mode → all rows returned, no error ────────────────────

#[test]
fn test_skip_locked_autocommit_returns_all_rows() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx);

    // No explicit transaction — autocommit path skips locking entirely.
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    assert_eq!(rows(result).len(), 3, "autocommit: all rows returned");
}

// ── Test 8: OFFSET + SKIP LOCKED — offset applied after filtering ─────────────

#[test]
fn test_skip_locked_offset_applied_after_filtering() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx1 = SessionContext::new();
    let mut ctx2 = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx1);

    // ctx1 locks id=1.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx1);
    ok_locked(
        "SELECT id FROM skip_test WHERE id = 1 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx1,
    );

    // ctx2: ORDER BY id, SKIP LOCKED filters out id=1, leaving (2, 3).
    // OFFSET 1 on that filtered set → returns id=3 only.
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx2);
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id LIMIT 10 OFFSET 1 FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx2,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx1);

    let r = rows(result);
    assert_eq!(
        r.len(),
        1,
        "offset 1 after filtering (2 unlocked rows) = 1 row"
    );
    assert_eq!(r[0][0], int(3), "should be id=3");
}

// ── Test 9: ORDER BY + SKIP LOCKED — deterministic row selection ───────────────

#[test]
fn test_skip_locked_order_by_deterministic() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx1 = SessionContext::new();
    let mut ctx2 = SessionContext::new();

    setup_table(&storage, &txn, &lm, &bloom, &mut ctx1);

    // ctx1 locks id=2 (middle row).
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx1);
    ok_locked(
        "SELECT id FROM skip_test WHERE id = 2 FOR UPDATE",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx1,
    );

    // ctx2: ORDER BY id DESC — order is (3, 2, 1); skip id=2 → (3, 1).
    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx2);
    let result = ok_locked(
        "SELECT id FROM skip_test ORDER BY id DESC FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx2,
    );
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx2);
    ok_locked("COMMIT", &storage, &txn, &lm, &bloom, &mut ctx1);

    let r = rows(result);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], int(3), "first in DESC order after skip");
    assert_eq!(r[1][0], int(1), "second in DESC order after skip");
}

// ── Test 10: SKIP LOCKED on clustered table → NotImplemented ──────────────────

#[test]
fn test_skip_locked_clustered_table_not_implemented() {
    let (storage, txn, lm) = setup_with_lock();
    let bloom = BloomRegistry::new();
    let mut ctx = SessionContext::new();

    ok_locked(
        "CREATE TABLE clust_skip (id INT PRIMARY KEY, val TEXT)",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );
    ok_locked(
        "INSERT INTO clust_skip VALUES (1, 'x')",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    );

    begin("BEGIN", &storage, &txn, &lm, &bloom, &mut ctx);
    let err = run_locked(
        "SELECT id FROM clust_skip FOR UPDATE SKIP LOCKED",
        &storage,
        &txn,
        &lm,
        &bloom,
        &mut ctx,
    )
    .unwrap_err();
    ok_locked("ROLLBACK", &storage, &txn, &lm, &bloom, &mut ctx);

    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "expected NotImplemented, got {err:?}"
    );
}

// ── Test 11: SKIP LOCKED parser — all strength variants ───────────────────────

#[test]
fn test_skip_locked_parses_all_strength_variants() {
    use axiomdb_sql::ast::{LockStrength, LockWaitPolicy, Stmt};

    let cases = [
        ("SELECT 1 FOR UPDATE SKIP LOCKED", LockStrength::ForUpdate),
        (
            "SELECT 1 FOR NO KEY UPDATE SKIP LOCKED",
            LockStrength::ForNoKeyUpdate,
        ),
        ("SELECT 1 FOR SHARE SKIP LOCKED", LockStrength::ForShare),
        (
            "SELECT 1 FOR KEY SHARE SKIP LOCKED",
            LockStrength::ForKeyShare,
        ),
    ];

    for (sql, expected_strength) in cases {
        let stmt = axiomdb_sql::parse(sql, None).unwrap();
        let Stmt::Select(sel) = stmt else {
            panic!("expected Select");
        };
        let lc = sel.lock_clause.expect("expected lock_clause");
        assert_eq!(
            lc.strength, expected_strength,
            "strength mismatch for: {sql}"
        );
        assert_eq!(
            lc.wait_policy,
            LockWaitPolicy::SkipLocked,
            "wait_policy mismatch for: {sql}"
        );
    }
}
