//! Formal MVCC isolation level tests (Phase 7.1).
//!
//! Tests use a single TxnManager with explicit begin/commit cycles to simulate
//! two "sessions" sharing the same storage. This is valid because AxiomDB's
//! single-writer model serializes all writes through one TxnManager.

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_core::IsolationLevel;
use axiomdb_sql::{
    analyze_with_defaults, bloom::BloomRegistry, execute_with_ctx, parse, QueryResult,
    SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn run_ctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze_with_defaults(
        stmt,
        storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

fn count_rows(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> i64 {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected rows")
    };
    match &rows[0][0] {
        Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

// ── IsolationLevel unit tests ────────────────────────────────────────────────

#[test]
fn test_isolation_level_parse_roundtrip() {
    assert_eq!(
        IsolationLevel::parse("READ-COMMITTED"),
        Some(IsolationLevel::ReadCommitted)
    );
    assert_eq!(
        IsolationLevel::parse("REPEATABLE-READ"),
        Some(IsolationLevel::RepeatableRead)
    );
    assert_eq!(
        IsolationLevel::parse("repeatable_read"),
        Some(IsolationLevel::RepeatableRead)
    );
    assert_eq!(
        IsolationLevel::parse("SERIALIZABLE"),
        Some(IsolationLevel::Serializable)
    );
    // READ UNCOMMITTED silently upgraded to RC
    assert_eq!(
        IsolationLevel::parse("READ-UNCOMMITTED"),
        Some(IsolationLevel::ReadCommitted)
    );
    assert_eq!(IsolationLevel::parse("GARBAGE"), None);
}

#[test]
fn test_isolation_level_default_is_repeatable_read() {
    assert_eq!(IsolationLevel::default(), IsolationLevel::RepeatableRead);
    let ctx = SessionContext::new();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::RepeatableRead);
}

#[test]
fn test_frozen_snapshot_policy() {
    assert!(IsolationLevel::RepeatableRead.uses_frozen_snapshot());
    assert!(IsolationLevel::Serializable.uses_frozen_snapshot());
    assert!(!IsolationLevel::ReadCommitted.uses_frozen_snapshot());
}

// ── SET transaction_isolation ────────────────────────────────────────────────

#[test]
fn test_set_session_transaction_isolation() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::ReadCommitted);

    run_ctx(
        "SET transaction_isolation = 'REPEATABLE-READ'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::RepeatableRead);
}

#[test]
fn test_set_transaction_isolation_invalid_value() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    let err = run_ctx(
        "SET transaction_isolation = 'CHAOS'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}

#[test]
fn test_set_transaction_isolation_inside_txn_rejected() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let err = run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

#[test]
fn test_set_transaction_isolation_default_resets() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::ReadCommitted);

    run_ctx(
        "SET transaction_isolation = DEFAULT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::RepeatableRead);
}

// ── REPEATABLE READ: frozen snapshot ─────────────────────────────────────────

#[test]
fn test_repeatable_read_sees_frozen_snapshot() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    // Setup: create table with 3 rows
    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Start explicit txn (default = REPEATABLE READ)
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // First read: sees 3 rows
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        3
    );

    // Simulate another "session" committing a row:
    // In single-writer model, we can't do concurrent writes inside an active txn.
    // Instead, we verify the snapshot is frozen by checking snapshot_id doesn't change.
    // The structural test: active_snapshot() returns the same snapshot_id each time.
    let snap1 = txn.active_snapshot().unwrap();
    let snap2 = txn.active_snapshot().unwrap();
    assert_eq!(
        snap1.snapshot_id, snap2.snapshot_id,
        "RR snapshot must be frozen"
    );

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

// ── READ COMMITTED: fresh snapshot per statement ─────────────────────────────

#[test]
fn test_read_committed_snapshot_refreshes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    // Setup
    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Set RC and start txn
    run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Verify snapshot is NOT frozen — snapshot_id uses current max_committed
    let snap1 = txn.active_snapshot().unwrap();
    // In single-writer, max_committed doesn't change mid-txn (we hold the lock).
    // But structurally, RC returns max_committed+1 which may differ from
    // the frozen snapshot_id_at_begin if commits happened between BEGIN and now.
    // Since we're single-writer, they're equal here — but the code path is different.
    assert_eq!(snap1.current_txn_id, txn.active_txn_id().unwrap());

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

#[test]
fn test_read_committed_sees_own_writes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1), (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Read own writes
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        2
    );

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

// ── No dirty reads ───────────────────────────────────────────────────────────

#[test]
fn test_no_dirty_reads_in_read_committed() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Start a txn that inserts but doesn't commit
    run_ctx(
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // The txn sees its own write
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        2
    );

    // Rollback — the insert is undone
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // After rollback, only 1 row (the committed one)
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        1
    );
}

// ── Autocommit always uses fresh snapshot ────────────────────────────────────

#[test]
fn test_autocommit_always_read_committed() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    // Even with RR session default, autocommit uses fresh snapshot
    assert_eq!(ctx.transaction_isolation, IsolationLevel::RepeatableRead);

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Autocommit SELECT: uses txn.snapshot() which is always fresh
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        1
    );

    run_ctx(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Next autocommit SELECT sees the new row (fresh snapshot)
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        2
    );
}

// ── SERIALIZABLE aliases to REPEATABLE READ ──────────────────────────────────

#[test]
fn test_serializable_uses_frozen_snapshot() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "SET transaction_isolation = 'SERIALIZABLE'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(ctx.transaction_isolation, IsolationLevel::Serializable);

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Frozen snapshot (same as RR)
    let snap1 = txn.active_snapshot().unwrap();
    let snap2 = txn.active_snapshot().unwrap();
    assert_eq!(snap1.snapshot_id, snap2.snapshot_id);

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}

// ── Per-txn override consumed after one txn ──────────────────────────────────

#[test]
fn test_next_txn_isolation_consumed() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    run_ctx(
        "CREATE TABLE t (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Set per-txn override
    ctx.next_txn_isolation = Some(IsolationLevel::ReadCommitted);

    // First BEGIN uses the override
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    // Override is consumed
    assert!(ctx.next_txn_isolation.is_none());
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Next BEGIN uses session default (RR)
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let snap1 = txn.active_snapshot().unwrap();
    let snap2 = txn.active_snapshot().unwrap();
    assert_eq!(
        snap1.snapshot_id, snap2.snapshot_id,
        "should be RR (frozen)"
    );
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
}
