use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    analyze_with_defaults, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::{Checkpointer, TxnManager};

struct Db {
    storage: MemoryStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    _dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("t.wal");
        let storage = MemoryStorage::new();
        CatalogBootstrap::init(&storage).unwrap();
        let txn = TxnManager::create(&wal).unwrap();
        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            _dir: dir,
        }
    }

    fn run_in_ctx(&self, ctx: &mut SessionContext, sql: &str) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = if let Some(ref ct) = ctx.conn_txn {
            self.txn.active_snapshot(ct)
        } else {
            self.txn.snapshot()
        };
        let analyzed = analyze_with_defaults(
            stmt,
            &self.storage,
            snap,
            ctx.effective_database(),
            ctx.current_schema(),
        )?;
        execute_with_ctx(analyzed, &self.storage, &self.txn, &self.bloom, ctx)
    }

    fn ok_in_ctx(&self, ctx: &mut SessionContext, sql: &str) -> QueryResult {
        self.run_in_ctx(ctx, sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
    }

    fn err_in_ctx(&self, ctx: &mut SessionContext, sql: &str) -> DbError {
        self.run_in_ctx(ctx, sql)
            .expect_err(&format!("expected error for: {sql}"))
    }
}

#[test]
fn checkpoint_advances_last_checkpoint_lsn_without_leaving_a_transaction_open() {
    let db = Db::new();
    let mut ctx = SessionContext::new();

    assert_eq!(Checkpointer::last_checkpoint_lsn(&db.storage).unwrap(), 0);
    assert!(!db.txn.has_active_txn());

    match db.ok_in_ctx(&mut ctx, "CHECKPOINT") {
        QueryResult::Empty => {}
        other => panic!("expected Empty result, got {other:?}"),
    }

    let first = Checkpointer::last_checkpoint_lsn(&db.storage).unwrap();
    assert!(first > 0, "checkpoint must persist a positive LSN");
    assert!(
        ctx.conn_txn.is_none(),
        "checkpoint must not leave an implicit txn"
    );
    assert!(!db.txn.has_active_txn());

    db.ok_in_ctx(&mut ctx, "CHECKPOINT");
    let second = Checkpointer::last_checkpoint_lsn(&db.storage).unwrap();
    assert!(second > first, "checkpoint LSN must advance monotonically");
    assert!(
        ctx.conn_txn.is_none(),
        "second checkpoint must still leave no txn"
    );
    assert!(!db.txn.has_active_txn());
}

#[test]
fn checkpoint_works_with_autocommit_disabled_when_no_transaction_is_active() {
    let db = Db::new();
    let mut ctx = SessionContext::new();
    ctx.autocommit = false;

    db.ok_in_ctx(&mut ctx, "CHECKPOINT");

    assert!(Checkpointer::last_checkpoint_lsn(&db.storage).unwrap() > 0);
    assert!(
        ctx.conn_txn.is_none(),
        "checkpoint must bypass implicit txn start"
    );
    assert!(!db.txn.has_active_txn());
}

#[test]
fn checkpoint_is_rejected_inside_an_explicit_transaction() {
    let db = Db::new();
    let mut ctx = SessionContext::new();

    db.ok_in_ctx(&mut ctx, "BEGIN");
    let err = db.err_in_ctx(&mut ctx, "CHECKPOINT");

    match err {
        DbError::TransactionAlreadyActive { txn_id } => {
            assert!(txn_id > 0, "must report the active transaction id");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    db.ok_in_ctx(&mut ctx, "ROLLBACK");
}

#[test]
fn checkpoint_is_rejected_when_another_session_has_an_active_transaction() {
    let db = Db::new();
    let mut writer_ctx = SessionContext::new();
    let mut admin_ctx = SessionContext::new();

    db.ok_in_ctx(&mut writer_ctx, "BEGIN");
    let writer_txn_id = writer_ctx.conn_txn.as_ref().unwrap().txn_id;

    let err = db.err_in_ctx(&mut admin_ctx, "CHECKPOINT");
    match err {
        DbError::TransactionAlreadyActive { txn_id } => {
            assert_eq!(txn_id, writer_txn_id);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    db.ok_in_ctx(&mut writer_ctx, "ROLLBACK");
}
