//! Integration tests for 3.5a (SET autocommit=0), 3.5b (implicit transaction start),
//! and 3.5c (statement-level rollback on error inside explicit transaction).

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze, bloom::BloomRegistry, execute_with_ctx, parse, result::QueryResult, SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.into_path().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn run_ctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute_with_ctx(analyzed, storage, txn, bloom, ctx)
}

fn ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("Expected Ok for: {sql}\nError: {e:?}"))
}

fn err(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> DbError {
    run_ctx(sql, storage, txn, bloom, ctx).expect_err(&format!("Expected Err for: {sql}"))
}

fn count_rows(result: QueryResult) -> usize {
    match result {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("Expected Rows result"),
    }
}

fn select_count(
    table: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> usize {
    let r = ok(&format!("SELECT * FROM {table}"), storage, txn, bloom, ctx);
    count_rows(r)
}

// ── 3.5a: SET autocommit=0 is respected ──────────────────────────────────────

/// Default: autocommit=true — INSERT is committed immediately.
#[test]
fn test_autocommit_true_default() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    assert!(ctx.autocommit, "default must be autocommit=true");

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // No explicit COMMIT needed — row is visible immediately.
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        1
    );
}

/// SET autocommit=0: INSERT without COMMIT must NOT persist.
/// After ROLLBACK, the row is gone.
#[test]
fn test_autocommit_false_rollback_discards_row() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false; // simulate SET autocommit=0

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // INSERT starts implicit txn.
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        txn.active_txn_id().is_some(),
        "implicit txn must be active after DML"
    );

    // ROLLBACK — row must disappear.
    ok("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);
    assert!(
        txn.active_txn_id().is_none(),
        "txn must be closed after ROLLBACK"
    );
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        0
    );
}

/// SET autocommit=0: INSERT + explicit COMMIT persists the row.
#[test]
fn test_autocommit_false_commit_persists_row() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false;

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        1
    );
}

// ── 3.5b: Implicit transaction start ─────────────────────────────────────────

/// SELECT does NOT start a transaction in autocommit=false mode.
#[test]
fn test_autocommit_false_select_no_txn() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false;

    // Setup table (DDL uses its own autocommit txn)
    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(txn.active_txn_id().is_none(), "DDL must leave no open txn");

    // SELECT — no txn should start
    ok(
        "SELECT * FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        txn.active_txn_id().is_none(),
        "SELECT must not start implicit txn"
    );
}

/// DML starts an implicit transaction in autocommit=false mode.
#[test]
fn test_autocommit_false_dml_starts_implicit_txn() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false;

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    assert!(txn.active_txn_id().is_none());
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        txn.active_txn_id().is_some(),
        "INSERT must start implicit txn"
    );
}

/// DDL causes implicit COMMIT of any open transaction (MySQL semantics).
#[test]
fn test_autocommit_false_ddl_commits_open_txn() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false;

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(txn.active_txn_id().is_some());

    // CREATE TABLE triggers implicit COMMIT of the open txn.
    ok(
        "CREATE TABLE t2 (id INT PRIMARY KEY)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(txn.active_txn_id().is_none(), "DDL must close open txn");

    // The INSERT into t must be committed (visible after DDL).
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        1
    );
}

/// Multiple DML statements in autocommit=false share the same implicit transaction.
#[test]
fn test_autocommit_false_multiple_dml_same_txn() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ctx.autocommit = false;

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let txn_id_1 = txn.active_txn_id();

    ok(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let txn_id_2 = txn.active_txn_id();

    assert_eq!(
        txn_id_1, txn_id_2,
        "both INSERTs must share the same implicit txn"
    );

    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        2
    );
}

// ── 3.5c: Statement-level rollback on error inside explicit transaction ────────

/// Error in explicit txn: only the failed statement rolls back; txn stays active.
#[test]
fn test_explicit_txn_error_keeps_txn_active() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Commit row 1 first (autocommit=true) so it's visible to uniqueness checks.
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Now open an explicit transaction.
    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Duplicate of committed row 1 — must error but NOT abort the transaction.
    let e = err(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::DuplicateKey | DbError::UniqueViolation { .. }),
        "expected duplicate key error, got: {e:?}"
    );
    assert!(
        txn.active_txn_id().is_some(),
        "transaction must remain active after statement error"
    );

    // Subsequent statement must work — transaction is still alive.
    ok(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Rows 1 and 2 committed; the failed duplicate attempt left no trace.
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        2
    );
}

/// Savepoint: failed INSERT's partial writes are undone; prior INSERTs survive.
#[test]
fn test_statement_rollback_undoes_partial_writes() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Commit row 10 first so it's visible to the uniqueness check below.
    ok(
        "INSERT INTO t VALUES (10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO t VALUES (20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Error: duplicate of committed row 10 — statement rolls back, txn stays.
    let _ = err(
        "INSERT INTO t VALUES (10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        txn.active_txn_id().is_some(),
        "txn must stay active after error"
    );

    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Rows 10 and 20 committed; the failed duplicate attempt is gone.
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        2
    );
}

/// TxnManager savepoint unit test: rollback_to_savepoint undoes only post-savepoint ops.
#[test]
fn test_savepoint_rollback_leaves_pre_savepoint_writes() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Capture savepoint after first INSERT.
    let sp = txn.savepoint();

    ok(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Rollback to savepoint — row 2 disappears, row 1 survives, txn still active.
    txn.rollback_to_savepoint(sp, &mut storage).unwrap();
    assert!(txn.active_txn_id().is_some(), "txn must remain active");

    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Only row 1 committed.
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        1
    );
}

/// Savepoint is a no-op when no writes happened after it.
#[test]
fn test_savepoint_noop_when_no_writes_after() {
    let (mut storage, mut txn) = setup();
    let mut bloom = BloomRegistry::default();
    let mut ctx = SessionContext::new();

    ok(
        "CREATE TABLE t (id INT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO t VALUES (99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let sp = txn.savepoint();
    // No writes after the savepoint.
    txn.rollback_to_savepoint(sp, &mut storage).unwrap();

    // Row 99 still there (was before the savepoint).
    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
    assert_eq!(
        select_count("t", &mut storage, &mut txn, &mut bloom, &mut ctx),
        1
    );
}
