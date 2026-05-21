//! Concurrent embedded database handle — [`SharedDb`] + [`Connection`].
//!
//! [`SharedDb`] wraps the engine state in an `Arc` so multiple [`Connection`]s
//! can share the same database without any outer lock. Each [`Connection`] owns
//! its per-connection state (`SessionContext`, `SchemaCache`) independently.
//!
//! ## Concurrency model
//!
//! | Operation | Lock held |
//! |---|---|
//! | `Connection::query()` (autocommit SELECT) | none — pure MVCC snapshot |
//! | `Connection::execute()` DML | none — per-page locks in `MmapStorage` |
//! | `Connection::execute()` DDL | `catalog_lock` write guard (brief) |
//!
//! This is strictly better than [`crate::Db`] + `Mutex` (which serializes
//! everything) and matches SQLite WAL mode for reads. For DML it exceeds
//! SQLite WAL mode because two writers on *different* pages proceed in
//! parallel (SQLite allows only one writer at a time regardless of pages).
//!
//! ## Multi-process access
//!
//! Not supported. For multi-process access use `axiomdb-server` (wire protocol).
//! See `docs/progreso.md` for the gap entry.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::{error::DbError, parse_dsn, ParsedDsn};
use axiomdb_sql::{
    analyze_cached,
    ast::Stmt,
    bloom::BloomRegistry,
    execute_with_ctx,
    parse_with_sql_mode,
    result::QueryResult,
    verify_and_repair_indexes_on_open, SchemaCache, SessionContext,
};
use axiomdb_storage::MmapStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

/// A row returned by a query — ordered list of column values.
pub type Row = Vec<Value>;

// ── SharedDbInner ─────────────────────────────────────────────────────────────

struct SharedDbInner {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    /// Incremented after every successful DDL. Connections compare their local
    /// `schema_version_seen` and call `SchemaCache::invalidate()` when stale.
    schema_version: AtomicU64,
    /// Serializes DDL against concurrent operations. DDL takes the write guard;
    /// DML and SELECTs need no guard — storage page locks handle concurrency.
    catalog_lock: RwLock<()>,
    /// Keeps the temp directory alive for `:memory:` databases.
    _tmpdir: Option<tempfile::TempDir>,
}

// SAFETY: MmapStorage, TxnManager, BloomRegistry are all Send+Sync via their
// internal RwLock/Mutex/AtomicU64 fields. RwLock<()> and AtomicU64 are
// Send+Sync. TempDir is Send+Sync.
unsafe impl Send for SharedDbInner {}
unsafe impl Sync for SharedDbInner {}

// ── SharedDb ──────────────────────────────────────────────────────────────────

/// Shared database engine handle.
///
/// Wraps `MmapStorage`, `TxnManager`, and `BloomRegistry` in an `Arc` so
/// multiple [`Connection`]s can run queries concurrently without any outer lock.
/// `Clone` is cheap — it is a single `Arc` refcount bump.
///
/// # Example
///
/// ```rust,no_run
/// use axiomdb_embedded::SharedDb;
/// use std::sync::Arc;
/// use std::thread;
///
/// let db = Arc::new(SharedDb::open("./myapp.db").unwrap());
///
/// let handles: Vec<_> = (0..4).map(|_| {
///     let db = Arc::clone(&db);
///     thread::spawn(move || {
///         let mut conn = db.connect();
///         conn.query("SELECT COUNT(*) FROM users").unwrap()
///     })
/// }).collect();
///
/// for h in handles { h.join().unwrap(); }
/// ```
#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<SharedDbInner>,
}

impl SharedDb {
    /// Opens or creates a database at `path`.
    ///
    /// Creates the file and initializes the catalog if it does not exist.
    /// Pass `":memory:"` for an ephemeral in-memory database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        if path.as_os_str() == ":memory:" {
            return Self::open_memory();
        }

        let db_path = path.with_extension("db");
        let wal_path = path.with_extension("wal");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(DbError::Io)?;
        }

        let (storage, txn) = if db_path.exists() {
            let mut storage = MmapStorage::open(&db_path)?;
            let (txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
            verify_and_repair_indexes_on_open(&storage, &txn)?;
            (storage, txn)
        } else {
            let storage = MmapStorage::create(&db_path)?;
            CatalogBootstrap::init(&storage)?;
            let txn = TxnManager::create(&wal_path)?;
            (storage, txn)
        };

        Ok(Self {
            inner: Arc::new(SharedDbInner {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_version: AtomicU64::new(0),
                catalog_lock: RwLock::new(()),
                _tmpdir: None,
            }),
        })
    }

    /// Opens an ephemeral in-memory database.
    ///
    /// Data lives in a temporary directory and is discarded when the last
    /// [`SharedDb`] clone (and all [`Connection`]s derived from it) are dropped.
    pub fn open_memory() -> Result<Self, DbError> {
        let tmpdir = tempfile::tempdir().map_err(DbError::Io)?;
        let db_path = tmpdir.path().join("mem.db");
        let wal_path = tmpdir.path().join("mem.wal");

        let storage = MmapStorage::create(&db_path)?;
        CatalogBootstrap::init(&storage)?;
        let txn = TxnManager::create(&wal_path)?;

        Ok(Self {
            inner: Arc::new(SharedDbInner {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_version: AtomicU64::new(0),
                catalog_lock: RwLock::new(()),
                _tmpdir: Some(tmpdir),
            }),
        })
    }

    /// Opens a database from a DSN string.
    ///
    /// Accepted forms: plain paths, `file:` URIs, `axiomdb:///local/path`,
    /// or `":memory:"`.
    pub fn open_dsn(dsn: impl Into<String>) -> Result<Self, DbError> {
        let dsn = dsn.into();
        if dsn == ":memory:" {
            return Self::open_memory();
        }
        let path = resolve_local_dsn_path(&dsn)?;
        Self::open(path)
    }

    /// Creates a new [`Connection`] to this database.
    ///
    /// Each connection has independent session state and schema cache.
    /// This call is cheap — no I/O, no locking, one `Arc` clone.
    pub fn connect(&self) -> Connection {
        Connection {
            inner: Arc::clone(&self.inner),
            session: SessionContext::default(),
            schema_cache: SchemaCache::default(),
            schema_version_seen: 0,
        }
    }
}

// ── Connection ────────────────────────────────────────────────────────────────

/// Per-connection execution context.
///
/// Holds independent session state (`SessionContext`, `SchemaCache`) and a
/// shared reference to the engine. `Connection` is `Send` — move it across
/// threads freely. It is not `Clone` — call [`SharedDb::connect`] for each
/// independent execution context.
///
/// Every `execute()`/`query()` call is autocommit unless an explicit
/// transaction is opened with [`begin()`](Connection::begin).
///
/// # Drop behaviour
///
/// If dropped while inside an explicit transaction, the transaction is
/// automatically rolled back.
pub struct Connection {
    inner: Arc<SharedDbInner>,
    session: SessionContext,
    schema_cache: SchemaCache,
    /// Last schema version this connection has seen. When `inner.schema_version`
    /// advances past this, the local `schema_cache` is invalidated.
    schema_version_seen: u64,
}

impl Connection {
    // ── Schema cache invalidation ─────────────────────────────────────────────

    fn maybe_invalidate_schema_cache(&mut self) {
        let current = self.inner.schema_version.load(Ordering::Acquire);
        if current != self.schema_version_seen {
            self.schema_cache.invalidate();
            self.schema_version_seen = current;
        }
    }

    // ── Core execution ────────────────────────────────────────────────────────

    fn run_inner(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        self.maybe_invalidate_schema_cache();

        // Fast path: SELECT through the per-session statement cache.
        // read_only=true when autocommit (no staged writes visible via snapshot).
        // read_only=false inside an explicit txn (must see the txn's own writes).
        if axiomdb_sql::sql_starts_with_select_keyword(sql) {
            let read_only = self.session.conn_txn.is_none();
            return axiomdb_sql::statement_cache::run_cached(
                sql,
                &self.inner.storage,
                &self.inner.txn,
                &self.inner.bloom,
                &mut self.schema_cache,
                &mut self.session,
                read_only,
            );
        }

        // Parse to distinguish DDL from DML.
        let stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;
        let is_ddl = is_schema_changing(&stmt);

        let snap = if let Some(ref ct) = self.session.conn_txn {
            self.inner.txn.active_snapshot(ct)
        } else {
            self.inner.txn.snapshot()
        };
        let analyzed =
            analyze_cached(stmt, &self.inner.storage, snap, &mut self.schema_cache)?;

        // DDL: hold catalog write lock for the duration of schema mutation so
        // concurrent connections do not see a half-applied schema.
        // DML: no outer lock — MmapStorage per-page locks handle concurrency.
        let _guard = if is_ddl {
            Some(
                self.inner
                    .catalog_lock
                    .write()
                    .map_err(|_| DbError::Other("catalog lock poisoned".into()))?,
            )
        } else {
            None
        };

        let result = execute_with_ctx(
            analyzed,
            &self.inner.storage,
            &self.inner.txn,
            &self.inner.bloom,
            &mut self.session,
        )?;

        if is_ddl {
            let new_ver = self.inner.schema_version.fetch_add(1, Ordering::Release) + 1;
            self.schema_version_seen = new_ver;
            self.schema_cache.invalidate();
        }

        Ok(result)
    }

    // ── Public query API ─────────────────────────────────────────────────────

    /// Executes a SQL statement (DDL or DML). Returns rows affected.
    pub fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result {
            QueryResult::Affected { count, .. } => count,
            QueryResult::Rows { rows, .. } => rows.len() as u64,
            QueryResult::Empty => 0,
        })
    }

    /// Executes a SQL SELECT. Returns rows.
    pub fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result {
            QueryResult::Rows { rows, .. } => rows,
            _ => vec![],
        })
    }

    /// Executes a SQL SELECT. Returns column names and rows.
    pub fn query_with_columns(
        &mut self,
        sql: &str,
    ) -> Result<(Vec<String>, Vec<Row>), DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result {
            QueryResult::Rows { columns, rows } => {
                (columns.into_iter().map(|c| c.name).collect(), rows)
            }
            _ => (vec![], vec![]),
        })
    }

    // ── Explicit transaction API ──────────────────────────────────────────────

    /// Begins an explicit transaction.
    ///
    /// Subsequent `execute()`/`query()` calls run inside this transaction until
    /// [`commit()`](Connection::commit) or [`rollback()`](Connection::rollback).
    ///
    /// # Errors
    /// Returns `DbError::Other` if a transaction is already active.
    pub fn begin(&mut self) -> Result<(), DbError> {
        if self.session.conn_txn.is_some() {
            return Err(DbError::Other("already in transaction".into()));
        }
        self.session.conn_txn = Some(self.inner.txn.begin()?);
        self.session.in_explicit_txn = true;
        Ok(())
    }

    /// Commits the active explicit transaction.
    ///
    /// # Errors
    /// Returns `DbError::Other` if no transaction is active.
    pub fn commit(&mut self) -> Result<(), DbError> {
        let conn = self
            .session
            .conn_txn
            .take()
            .ok_or_else(|| DbError::Other("no active transaction".into()))?;
        self.session.in_explicit_txn = false;
        self.inner.txn.commit(conn)?;
        Ok(())
    }

    /// Rolls back the active explicit transaction.
    ///
    /// # Errors
    /// Returns `DbError::Other` if no transaction is active.
    pub fn rollback(&mut self) -> Result<(), DbError> {
        let conn = self
            .session
            .conn_txn
            .take()
            .ok_or_else(|| DbError::Other("no active transaction".into()))?;
        self.session.in_explicit_txn = false;
        self.inner.txn.rollback(conn, &self.inner.storage)?;
        Ok(())
    }

    /// Returns `true` if inside an explicit transaction.
    pub fn in_transaction(&self) -> bool {
        self.session.conn_txn.is_some()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(conn) = self.session.conn_txn.take() {
            let _ = self.inner.txn.rollback(conn, &self.inner.storage);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns `true` if `stmt` modifies the schema (DDL).
fn is_schema_changing(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::CreateDatabase(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropTable(_)
            | Stmt::DropDatabase(_)
            | Stmt::DropIndex(_)
            | Stmt::AlterTable(_)
            | Stmt::TruncateTable(_)
    )
}

fn resolve_local_dsn_path(dsn: &str) -> Result<PathBuf, DbError> {
    let parsed = parse_dsn(dsn)?;
    match parsed {
        ParsedDsn::Local(local) => {
            if !local.query.is_empty() {
                let params = local.query.keys().cloned().collect::<Vec<_>>().join(", ");
                return Err(DbError::InvalidDsn {
                    reason: format!(
                        "embedded DSN does not support query parameters: {params}"
                    ),
                });
            }
            Ok(local.path)
        }
        ParsedDsn::Wire(_) => Err(DbError::InvalidDsn {
            reason: "embedded open_dsn only supports local-path DSNs".into(),
        }),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (tempfile::TempDir, SharedDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path().join("t.db")).unwrap();
        (dir, db)
    }

    // ── Basic smoke ──────────────────────────────────────────────────────────

    #[test]
    fn connection_basic_smoke() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();
        let rows = conn.query("SELECT id, v FROM t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][1], Value::Text("hello".into()));
    }

    #[test]
    fn query_with_columns_returns_names() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES (7, 'bob')").unwrap();
        let (cols, rows) = conn.query_with_columns("SELECT id, name FROM t").unwrap();
        assert_eq!(cols, vec!["id", "name"]);
        assert_eq!(rows[0][1], Value::Text("bob".into()));
    }

    #[test]
    fn open_memory_works() {
        let db = SharedDb::open_memory().unwrap();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (x INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (42)").unwrap();
        let rows = conn.query("SELECT x FROM t").unwrap();
        assert_eq!(rows[0][0], Value::Int(42));
    }

    #[test]
    fn open_colon_memory_shorthand() {
        let db = SharedDb::open(":memory:").unwrap();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (x INT)").unwrap();
        let rows = conn.query("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(rows[0][0], Value::BigInt(0));
    }

    // ── Multi-connection ─────────────────────────────────────────────────────

    #[test]
    fn two_connections_share_data() {
        let (_dir, db) = open_tmp();
        let mut conn1 = db.connect();
        let mut conn2 = db.connect();
        conn1.execute("CREATE TABLE t (id INT)").unwrap();
        conn1.execute("INSERT INTO t VALUES (42)").unwrap();
        let rows = conn2.query("SELECT id FROM t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(42));
    }

    #[test]
    fn clone_is_same_database() {
        let (_dir, db1) = open_tmp();
        let db2 = db1.clone();
        let mut c1 = db1.connect();
        let mut c2 = db2.connect();
        c1.execute("CREATE TABLE t (id INT)").unwrap();
        c1.execute("INSERT INTO t VALUES (99)").unwrap();
        let rows = c2.query("SELECT id FROM t").unwrap();
        assert_eq!(rows[0][0], Value::Int(99));
    }

    // ── DDL cross-connection schema invalidation ──────────────────────────────

    #[test]
    fn ddl_on_conn_a_visible_to_conn_b() {
        let (_dir, db) = open_tmp();
        let mut conn_a = db.connect();
        let mut conn_b = db.connect();
        conn_a.execute("CREATE TABLE t (id INT)").unwrap();
        conn_a.execute("INSERT INTO t VALUES (1)").unwrap();
        // conn_b schema_version_seen=0, conn_a incremented to 1 → cache invalidated
        let rows = conn_b.query("SELECT id FROM t").unwrap();
        assert_eq!(rows.len(), 1);
    }

    // ── Concurrent reads ─────────────────────────────────────────────────────

    #[test]
    fn concurrent_reads_no_deadlock() {
        use std::sync::Arc;
        use std::thread;

        let (_dir, db) = open_tmp();
        let mut setup = db.connect();
        setup.execute("CREATE TABLE t (id INT)").unwrap();
        for i in 0..100i32 {
            setup
                .execute(&format!("INSERT INTO t VALUES ({i})"))
                .unwrap();
        }
        drop(setup);

        let db = Arc::new(db);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let db = Arc::clone(&db);
                thread::spawn(move || {
                    let mut conn = db.connect();
                    for _ in 0..20 {
                        let rows = conn.query("SELECT COUNT(*) FROM t").unwrap();
                        assert_eq!(rows[0][0], Value::BigInt(100));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Explicit transactions ─────────────────────────────────────────────────

    #[test]
    fn explicit_txn_commit() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.begin().unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.commit().unwrap();
        let rows = conn.query("SELECT * FROM t").unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn explicit_txn_rollback() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.begin().unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.rollback().unwrap();
        let rows = conn.query("SELECT * FROM t").unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn drop_rolls_back_open_txn() {
        let (_dir, db) = open_tmp();
        {
            let mut conn = db.connect();
            conn.execute("CREATE TABLE t (id INT)").unwrap();
            conn.begin().unwrap();
            conn.execute("INSERT INTO t VALUES (1)").unwrap();
            // dropped without commit → rollback
        }
        let mut conn2 = db.connect();
        let rows = conn2.query("SELECT * FROM t").unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn begin_while_in_txn_is_error() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        conn.begin().unwrap();
        assert!(conn.begin().is_err());
    }

    #[test]
    fn commit_without_txn_is_error() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        assert!(conn.commit().is_err());
    }

    #[test]
    fn rollback_without_txn_is_error() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        assert!(conn.rollback().is_err());
    }

    #[test]
    fn in_transaction_reflects_state() {
        let (_dir, db) = open_tmp();
        let mut conn = db.connect();
        assert!(!conn.in_transaction());
        conn.begin().unwrap();
        assert!(conn.in_transaction());
        conn.rollback().unwrap();
        assert!(!conn.in_transaction());
    }
}
