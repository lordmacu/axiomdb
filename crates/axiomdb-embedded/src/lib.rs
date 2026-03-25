//! # axiomdb-embedded
//!
//! In-process AxiomDB engine. No server, no TCP — the database runs inside
//! your application process.
//!
//! ## Quick start (Rust)
//!
//! ```rust,no_run
//! use axiomdb_embedded::Db;
//!
//! let mut db = Db::open("./myapp.db").unwrap();
//! db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, PRIMARY KEY(id))").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
//! let rows = db.query("SELECT * FROM users").unwrap();
//! ```
//!
//! ## Build profiles
//!
//! | Profile | Features | Use case |
//! |---|---|---|
//! | `desktop` (default) | sync API + C FFI | Desktop apps, mobile (Swift/Kotlin via .so) |
//! | `async-api` | + tokio | Async Rust services |
//! | `wasm` | sync, no mmap | Browser (future) |
//!
//! ## C API (for non-Rust languages)
//!
//! ```c
//! AxiomDb* db = axiomdb_open("./myapp.db");
//! axiomdb_execute(db, "INSERT INTO users VALUES (1, 'Alice')");
//! AxiomRows* rows = axiomdb_query(db, "SELECT * FROM users");
//! axiomdb_rows_free(rows);
//! axiomdb_close(db);
//! ```

// ── Sync Rust API ─────────────────────────────────────────────────────────────

#[cfg(feature = "sync-api")]
pub use db::Db;

#[cfg(feature = "sync-api")]
pub use db::Row;

#[cfg(feature = "sync-api")]
mod db {
    use std::path::Path;

    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_core::error::DbError;
    use axiomdb_sql::{
        analyze_cached, bloom::BloomRegistry, execute_with_ctx, parse, result::QueryResult,
        SchemaCache, SessionContext,
    };
    use axiomdb_storage::MmapStorage;
    use axiomdb_wal::TxnManager;

    /// A single result row — a `Vec` of `Value`s in column order.
    pub type Row = Vec<axiomdb_types::Value>;

    /// An in-process AxiomDB database.
    ///
    /// All operations are synchronous and single-writer. Concurrent reads from
    /// multiple threads are supported via MVCC snapshots (future: Phase 7).
    ///
    /// ## Autocommit
    ///
    /// Every `execute()` and `query()` call is wrapped in an implicit BEGIN/COMMIT
    /// unless an explicit `begin()` is active.
    pub struct Db {
        storage: MmapStorage,
        txn: TxnManager,
        bloom: BloomRegistry,
        schema_cache: SchemaCache,
        session: SessionContext,
        /// Set to `true` after the first `DiskFull` error. When degraded,
        /// all mutating operations are rejected immediately without touching
        /// WAL or storage again.
        degraded: bool,
    }

    impl Db {
        /// Opens or creates a database at `path`.
        ///
        /// Creates the file and initializes the catalog if it does not exist.
        ///
        /// ```rust,no_run
        /// let mut db = axiomdb_embedded::Db::open("./myapp.db").unwrap();
        /// ```
        pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
            let path = path.as_ref();
            let db_path = path.with_extension("db");
            let wal_path = path.with_extension("wal");

            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).map_err(DbError::Io)?;
            }

            let (storage, txn) = if db_path.exists() {
                let storage = MmapStorage::open(&db_path)?;
                let txn = TxnManager::open(&wal_path)?;
                (storage, txn)
            } else {
                let mut storage = MmapStorage::create(&db_path)?;
                CatalogBootstrap::init(&mut storage)?;
                let txn = TxnManager::create(&wal_path)?;
                (storage, txn)
            };

            Ok(Self {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_cache: SchemaCache::new(),
                session: SessionContext::default(),
                degraded: false,
            })
        }

        /// Executes a SQL statement that does not return rows
        /// (INSERT, UPDATE, DELETE, CREATE TABLE, etc.).
        ///
        /// Returns the number of rows affected.
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        /// ```
        pub fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
            let result = self.run(sql)?;
            Ok(match result {
                QueryResult::Affected { count, .. } => count,
                QueryResult::Rows { rows, .. } => rows.len() as u64,
                QueryResult::Empty => 0,
            })
        }

        /// Executes a SQL SELECT and returns the rows.
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// let rows = db.query("SELECT * FROM users WHERE id = 1").unwrap();
        /// for row in rows {
        ///     println!("{:?}", row);
        /// }
        /// ```
        pub fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
            let result = self.run(sql)?;
            Ok(match result {
                QueryResult::Rows { rows, .. } => rows,
                _ => vec![],
            })
        }

        /// Executes a SQL statement and returns the full `QueryResult`.
        ///
        /// Useful when you need column metadata, last_insert_id, etc.
        pub fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            if self.degraded && sql_may_mutate(sql) {
                return Err(DbError::DiskFull {
                    operation: "database is in read-only degraded mode",
                });
            }
            let stmt = parse(sql, None)?;
            let snap = self
                .txn
                .active_snapshot()
                .unwrap_or_else(|_| self.txn.snapshot());
            let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
            let result = execute_with_ctx(
                analyzed,
                &mut self.storage,
                &mut self.txn,
                &mut self.bloom,
                &mut self.session,
            );
            if let Err(ref e) = result {
                if matches!(e, DbError::DiskFull { .. }) {
                    self.degraded = true;
                }
            }
            result
        }

        /// Opens an explicit transaction. All subsequent `execute()`/`query()`
        /// calls run inside this transaction until `commit()` or `rollback()`.
        pub fn begin(&mut self) -> Result<(), DbError> {
            self.run("BEGIN")?;
            Ok(())
        }

        /// Commits the current explicit transaction.
        pub fn commit(&mut self) -> Result<(), DbError> {
            self.run("COMMIT")?;
            Ok(())
        }

        /// Rolls back the current explicit transaction.
        pub fn rollback(&mut self) -> Result<(), DbError> {
            self.run("ROLLBACK")?;
            Ok(())
        }
    }

    /// Returns `true` if the SQL string looks like it may mutate durable state.
    ///
    /// Conservative keyword check — used to gate statements in degraded mode
    /// before they reach WAL/storage. False positives are acceptable (blocking
    /// a read); false negatives are not (allowing a write).
    fn sql_may_mutate(sql: &str) -> bool {
        let lower = sql.trim_start().to_ascii_lowercase();
        lower.starts_with("insert")
            || lower.starts_with("update")
            || lower.starts_with("delete")
            || lower.starts_with("truncate")
            || lower.starts_with("create")
            || lower.starts_with("drop")
            || lower.starts_with("alter")
            || lower.starts_with("begin")
            || lower.starts_with("start transaction")
            || lower.starts_with("commit")
            || lower.starts_with("rollback")
            || lower.starts_with("savepoint")
            || lower.starts_with("release")
    }
}

// ── C FFI ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "c-ffi")]
mod ffi {
    use std::ffi::CStr;
    use std::os::raw::c_char;

    use super::db::Db;

    /// Opens or creates a database at `path`.
    ///
    /// Returns a heap-allocated `AxiomDb*` handle, or NULL on error.
    /// The caller must free it with `axiomdb_close()`.
    ///
    /// # Safety
    /// `path` must be a valid non-null pointer to a UTF-8 null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_open(path: *const c_char) -> *mut Db {
        if path.is_null() {
            return std::ptr::null_mut();
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        match Db::open(path) {
            Ok(db) => Box::into_raw(Box::new(db)),
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// Executes a SQL statement (no result rows expected).
    ///
    /// Returns the number of rows affected, or -1 on error.
    ///
    /// # Safety
    /// `db` must be a valid pointer from `axiomdb_open`.
    /// `sql` must be a valid non-null null-terminated UTF-8 string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_execute(db: *mut Db, sql: *const c_char) -> i64 {
        if db.is_null() || sql.is_null() {
            return -1;
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        };
        match db.execute(sql) {
            Ok(n) => n as i64,
            Err(_) => -1,
        }
    }

    /// Closes the database and frees all resources.
    ///
    /// # Safety
    /// `db` must be a valid pointer from `axiomdb_open`. After this call,
    /// `db` is invalid and must not be used.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_close(db: *mut Db) {
        if !db.is_null() {
            drop(Box::from_raw(db));
        }
    }
}

// ── Async API ─────────────────────────────────────────────────────────────────

#[cfg(feature = "async-api")]
pub mod async_db {
    //! Tokio-based async wrapper around [`Db`].
    //!
    //! Uses `tokio::task::spawn_blocking` to run the synchronous engine
    //! on a dedicated thread, keeping the async executor unblocked.
    //!
    //! ```rust,no_run
    //! use axiomdb_embedded::async_db::AsyncDb;
    //!
    //! #[tokio::main]
    //! async fn main() {
    //!     let db = AsyncDb::open("./myapp.db").await.unwrap();
    //!     db.execute("CREATE TABLE t (id INT NOT NULL, PRIMARY KEY(id))").await.unwrap();
    //! }
    //! ```
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use axiomdb_core::error::DbError;

    use super::db::{Db, Row};

    /// Async wrapper — the synchronous engine runs in a blocking thread pool.
    #[derive(Clone)]
    pub struct AsyncDb {
        inner: Arc<Mutex<Db>>,
    }

    impl AsyncDb {
        /// Opens or creates a database at `path`.
        pub async fn open(path: impl Into<PathBuf>) -> Result<Self, DbError> {
            let path = path.into();
            let db = tokio::task::spawn_blocking(move || Db::open(&path))
                .await
                .map_err(|e| {
                    DbError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })??;
            Ok(Self {
                inner: Arc::new(Mutex::new(db)),
            })
        }

        /// Executes a SQL DML/DDL statement. Returns rows affected.
        pub async fn execute(&self, sql: impl Into<String>) -> Result<u64, DbError> {
            let sql = sql.into();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || inner.lock().unwrap().execute(&sql))
                .await
                .map_err(|e| {
                    DbError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })?
        }

        /// Executes a SQL SELECT. Returns rows.
        pub async fn query(&self, sql: impl Into<String>) -> Result<Vec<Row>, DbError> {
            let sql = sql.into();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || inner.lock().unwrap().query(&sql))
                .await
                .map_err(|e| {
                    DbError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    ))
                })?
        }
    }
}
