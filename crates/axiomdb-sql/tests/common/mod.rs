#![allow(dead_code)]

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze, analyze_with_defaults, bloom::BloomRegistry, execute, execute_with_ctx, parse,
    QueryResult, SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

pub(crate) fn run(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> QueryResult {
    run_result(sql, storage, txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

pub(crate) fn run_result(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

pub(crate) fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

pub(crate) fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

pub(crate) fn affected_count(result: QueryResult) -> u64 {
    match result {
        QueryResult::Affected { count, .. } => count,
        other => panic!("expected Affected, got {other:?}"),
    }
}

pub(crate) fn run_ctx(
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

pub(crate) fn setup_ctx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

pub(crate) fn run_ctx_5_21(
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

pub(crate) fn ok_ctx_5_21(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx_5_21(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}
