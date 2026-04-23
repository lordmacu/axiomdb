use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

struct Db {
    storage: MemoryStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    ctx: SessionContext,
    _dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wal_path = dir.path().join("test.wal");
        let storage = MemoryStorage::new();
        axiomdb_catalog::bootstrap::CatalogBootstrap::init(&storage).unwrap();
        let txn = TxnManager::create(&wal_path).unwrap();
        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            ctx: SessionContext::new(),
            _dir: dir,
        }
    }

    fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = if let Some(ref ct) = self.ctx.conn_txn {
            self.txn.active_snapshot(ct)
        } else {
            self.txn.snapshot()
        };
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(
            analyzed,
            &self.storage,
            &self.txn,
            &self.bloom,
            &mut self.ctx,
        )
    }

    fn ok(&mut self, sql: &str) -> QueryResult {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"))
    }

    fn err(&mut self, sql: &str) -> DbError {
        self.run(sql)
            .expect_err(&format!("expected error for: {sql}"))
    }
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn deferred_fk_allows_child_before_parent_until_commit() {
    let mut db = Db::new();
    db.ok("CREATE TABLE parents (id INT PRIMARY KEY)");
    db.ok("CREATE TABLE children (
            id INT PRIMARY KEY,
            parent_id INT,
            CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parents(id)
                DEFERRABLE INITIALLY DEFERRED
        )");

    db.ok("BEGIN");
    db.ok("INSERT INTO children VALUES (1, 10)");
    db.ok("INSERT INTO parents VALUES (10)");
    db.ok("COMMIT");

    let out = rows(db.ok("SELECT parent_id FROM children"));
    assert_eq!(out, vec![vec![Value::Int(10)]]);
}

#[test]
fn deferred_fk_violation_surfaces_on_commit_and_rolls_back() {
    let mut db = Db::new();
    db.ok("CREATE TABLE parents (id INT PRIMARY KEY)");
    db.ok("CREATE TABLE children (
            id INT PRIMARY KEY,
            parent_id INT,
            CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parents(id)
                DEFERRABLE INITIALLY DEFERRED
        )");

    db.ok("BEGIN");
    db.ok("INSERT INTO children VALUES (1, 99)");
    let err = db.err("COMMIT");
    assert!(matches!(err, DbError::ForeignKeyViolation { .. }));
    assert!(
        db.ctx.conn_txn.is_none(),
        "transaction must be closed after failed COMMIT"
    );
    let out = rows(db.ok("SELECT COUNT(*) FROM children"));
    assert_eq!(out, vec![vec![Value::BigInt(0)]]);
}

#[test]
fn deferred_fk_savepoint_rollback_discards_pending_validation() {
    let mut db = Db::new();
    db.ok("CREATE TABLE parents (id INT PRIMARY KEY)");
    db.ok("CREATE TABLE children (
            id INT PRIMARY KEY,
            parent_id INT,
            CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parents(id)
                DEFERRABLE INITIALLY DEFERRED
        )");

    db.ok("BEGIN");
    db.ok("SAVEPOINT s1");
    db.ok("INSERT INTO children VALUES (1, 77)");
    db.ok("ROLLBACK TO SAVEPOINT s1");
    db.ok("COMMIT");

    let out = rows(db.ok("SELECT COUNT(*) FROM children"));
    assert_eq!(out, vec![vec![Value::BigInt(0)]]);
}
