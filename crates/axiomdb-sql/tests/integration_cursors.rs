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
fn declare_cursor_requires_explicit_transaction() {
    let mut db = Db::new();
    let err = db.err("DECLARE c CURSOR FOR SELECT 1");
    match err {
        DbError::InvalidValue { reason } => {
            assert!(reason.contains("explicit transaction"), "got {reason}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn declare_fetch_next_forward_all_and_close() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t VALUES (2, 'b')",
        "INSERT INTO t VALUES (3, 'c')",
        "BEGIN",
        "DECLARE c CURSOR FOR SELECT id, name FROM t ORDER BY id"
    );

    let rows = db.rows("FETCH NEXT FROM c");
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("a".into())]]);

    let rows = db.rows("FETCH 0 FROM c");
    assert!(rows.is_empty(), "FETCH 0 should return no rows");

    let rows = db.rows("FETCH FORWARD 1 FROM c");
    assert_eq!(rows, vec![vec![Value::Int(2), Value::Text("b".into())]]);

    let rows = db.rows("FETCH ALL FROM c");
    assert_eq!(rows, vec![vec![Value::Int(3), Value::Text("c".into())]]);

    match db.ok("FETCH NEXT FROM c") {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 2, "EOF fetch must preserve metadata");
            assert!(rows.is_empty(), "EOF fetch must return zero rows");
        }
        other => panic!("expected rows result, got {other:?}"),
    }

    db.ok("CLOSE c");
    let err = db.err("FETCH NEXT FROM c");
    match err {
        DbError::InvalidValue { reason } => assert!(reason.contains("not found"), "got {reason}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn duplicate_cursor_names_are_rejected_case_insensitively() {
    let mut db = Db::new();
    setup!(db, "BEGIN", "DECLARE SalesCur CURSOR FOR SELECT 1");
    let err = db.err("DECLARE salescur CURSOR FOR SELECT 2");
    match err {
        DbError::InvalidValue { reason } => {
            assert!(reason.contains("already exists"), "got {reason}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn close_all_is_noop_when_empty_and_removes_open_cursors() {
    let mut db = Db::new();
    setup!(db, "BEGIN");
    db.ok("CLOSE ALL");
    setup!(
        db,
        "DECLARE c1 CURSOR FOR SELECT 1",
        "DECLARE c2 CURSOR FOR SELECT 2",
        "CLOSE ALL"
    );
    let err = db.err("FETCH NEXT FROM c1");
    match err {
        DbError::InvalidValue { reason } => assert!(reason.contains("not found"), "got {reason}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn commit_closes_all_cursors() {
    let mut db = Db::new();
    setup!(
        db,
        "BEGIN",
        "DECLARE c CURSOR FOR SELECT 1",
        "COMMIT",
        "BEGIN"
    );
    let err = db.err("FETCH NEXT FROM c");
    match err {
        DbError::InvalidValue { reason } => assert!(reason.contains("not found"), "got {reason}"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn declare_cursor_accepts_cte_and_setop_queries() {
    let mut db = Db::new();
    setup!(
        db,
        "BEGIN",
        "DECLARE c CURSOR FOR WITH x AS (SELECT 1 AS id) SELECT id FROM x UNION ALL SELECT 2"
    );
    let rows = db.rows("FETCH ALL FROM c");
    assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}
