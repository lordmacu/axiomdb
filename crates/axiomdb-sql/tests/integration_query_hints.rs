//! Integration tests for Phase 21.11 query hints.

use axiomdb_catalog::bootstrap::CatalogBootstrap;
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
        let wal = dir.path().join("t.wal");
        let storage = MemoryStorage::new();
        CatalogBootstrap::init(&storage).unwrap();
        let txn = TxnManager::create(&wal).unwrap();
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

    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.ok(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+ $(,)?) => { $( $db.ok($sql); )+ };
}

#[test]
fn explain_uses_hinted_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, status TEXT)",
        "CREATE INDEX idx_users_email ON users(email)",
        "INSERT INTO users VALUES (1, 'alice@example.com', 'active')",
        "INSERT INTO users VALUES (2, 'bob@example.com', 'inactive')"
    );

    let rows = db.rows(
        "EXPLAIN SELECT /*+ INDEX(users idx_users_email) */ id \
         FROM users WHERE email = 'alice@example.com'",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][5], Value::Text("idx_users_email".into()));
}

#[test]
fn hinted_unknown_index_errors() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_users_email ON users(email)"
    );

    let err = db.err(
        "SELECT /*+ INDEX(users idx_users_missing) */ id \
         FROM users WHERE email = 'alice@example.com'",
    );
    assert!(matches!(err, DbError::IndexNotFound { .. }));
}

#[test]
fn hash_join_hint_executes_small_equijoin() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY)",
        "CREATE TABLE u (t_id INT PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (3)",
        "INSERT INTO u VALUES (2)",
        "INSERT INTO u VALUES (3)"
    );

    let rows = db.rows(
        "SELECT /*+ HASH_JOIN */ t.id \
         FROM t JOIN u ON t.id = u.t_id ORDER BY t.id",
    );
    assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
}

#[test]
fn explain_reports_hash_join_hint() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY)",
        "CREATE TABLE u (t_id INT PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO u VALUES (1)"
    );

    let rows = db.rows("EXPLAIN SELECT /*+ HASH_JOIN */ * FROM t JOIN u ON t.id = u.t_id");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][9], Value::Text("Using hash join (hint)".into()));
}

#[test]
fn parallel_hint_executes_and_is_visible_in_explain() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_t_email ON t(email)",
        "INSERT INTO t VALUES (1, 'alice@example.com')"
    );

    let rows = db.rows("SELECT /*+ PARALLEL(2) */ id FROM t WHERE email = 'alice@example.com'");
    assert_eq!(rows, vec![vec![Value::Int(1)]]);

    let explain =
        db.rows("EXPLAIN SELECT /*+ PARALLEL(2) */ id FROM t WHERE email = 'alice@example.com'");
    assert_eq!(
        explain[0][9],
        Value::Text("Using where; PARALLEL(2) hint".into())
    );
}
