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
//! db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
//!
//! let rows = db.query("SELECT * FROM users").unwrap();
//! for row in &rows {
//!     println!("{:?}", row);
//! }
//!
//! // With column names:
//! let (columns, rows) = db.query_with_columns("SELECT id, name FROM users").unwrap();
//! println!("columns: {:?}", columns); // ["id", "name"]
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
//! ## C API (for C, C++, Python ctypes, Swift, Kotlin JNI)
//!
//! ```c
//! #include "axiomdb.h"
//!
//! AxiomDb* db = axiomdb_open("./myapp.db");
//! axiomdb_execute(db, "INSERT INTO users VALUES (1, 'Alice')");
//!
//! AxiomRows* rows = axiomdb_query(db, "SELECT id, name FROM users");
//! if (rows) {
//!     int64_t n = axiomdb_rows_count(rows);
//!     int32_t ncols = axiomdb_rows_columns(rows);
//!     for (int64_t r = 0; r < n; r++) {
//!         for (int32_t c = 0; c < ncols; c++) {
//!             printf("%s = %s\n",
//!                 axiomdb_rows_column_name(rows, c),
//!                 axiomdb_rows_get_text(rows, r, c));
//!         }
//!     }
//!     axiomdb_rows_free(rows);
//! } else {
//!     printf("error: %s\n", axiomdb_last_error(db));
//! }
//! axiomdb_close(db);
//! ```

// ── Sync Rust API ─────────────────────────────────────────────────────────────

#[cfg(feature = "sync-api")]
pub use appender::Appender;

#[cfg(feature = "sync-api")]
pub use db::Db;

#[cfg(feature = "sync-api")]
pub use db::Row;

#[cfg(feature = "sync-api")]
pub use shared_db::{Connection, SharedDb};

#[cfg(feature = "sync-api")]
mod appender;

#[cfg(feature = "sync-api")]
mod shared_db;

#[cfg(feature = "sync-api")]
mod db {
    use std::ffi::CString;
    use std::path::{Path, PathBuf};

    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_core::{error::DbError, parse_dsn, ParsedDsn};
    use axiomdb_sql::{
        analyze_cached,
        ast::{InsertSource, SelectItem, Stmt},
        bloom::BloomRegistry,
        execute_read_only_with_ctx, execute_with_ctx,
        expr::Expr,
        parse_with_sql_mode,
        result::QueryResult,
        verify_and_repair_indexes_on_open, SchemaCache, SessionContext,
    };
    use axiomdb_storage::{DbConfig, MmapStorage, RedoMode};
    use axiomdb_types::Value;
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
        pub(super) storage: MmapStorage,
        pub(super) txn: TxnManager,
        pub(super) bloom: BloomRegistry,
        pub(super) schema_cache: SchemaCache,
        pub(super) session: SessionContext,
        /// Set to `true` after the first `DiskFull` error. When degraded,
        /// all mutating operations are rejected immediately without touching
        /// WAL or storage again.
        degraded: bool,
        /// Last error message. Cleared on success, set on any error.
        /// Exposed via `last_error()` (Rust) and `axiomdb_last_error()` (C FFI).
        pub(crate) error_msg: Option<CString>,
        /// Temp directory for `:memory:` mode (Phase 11.3).
        /// Kept alive as long as the `Db` exists; cleaned up on drop.
        _tmpdir: Option<tempfile::TempDir>,
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
            // Phase 11.3: `:memory:` shorthand → ephemeral tempdir-backed database.
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
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_cache: SchemaCache::new(),
                session: SessionContext::default(),
                degraded: false,
                error_msg: None,
                _tmpdir: None,
            })
        }

        /// Opens or creates a database at `path` with explicit configuration.
        ///
        /// Project B subphase 6c: `config.redo == Some(RedoMode::FrameOnly)` selects
        /// frame-only durability (a commit is durable via the frame fsync + REDO
        /// recovery; the per-commit main-file flush is dropped). Default config = today's
        /// dual-write + per-commit flush.
        ///
        /// Ordering is load-bearing: the catalog bootstrap lands in the main file
        /// (durable, read on every open) BEFORE redo is enabled, and redo is enabled
        /// BEFORE recovery so REDO can replay committed data frames into the main file.
        pub fn open_with_config(
            path: impl AsRef<Path>,
            config: &DbConfig,
        ) -> Result<Self, DbError> {
            let path = path.as_ref();
            if path.as_os_str() == ":memory:" {
                return Self::open_memory();
            }
            let db_path = path.with_extension("db");
            let wal_path = path.with_extension("wal");

            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).map_err(DbError::Io)?;
            }

            let frame_only = config.resolved_redo() == RedoMode::FrameOnly;
            let (storage, txn) = if db_path.exists() {
                let mut storage = MmapStorage::open(&db_path)?;
                if frame_only {
                    storage.enable_frame_only_redo(&db_path)?;
                }
                let (txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
                verify_and_repair_indexes_on_open(&storage, &txn)?;
                (storage, txn)
            } else {
                let mut storage = MmapStorage::create(&db_path)?;
                CatalogBootstrap::init(&storage)?;
                if frame_only {
                    storage.enable_frame_only_redo(&db_path)?;
                }
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
                error_msg: None,
                _tmpdir: None,
            })
        }

        /// Opens an ephemeral in-memory database (Phase 11.3).
        ///
        /// Data lives in a temporary directory and is discarded when the `Db`
        /// is dropped. Equivalent to SQLite's `":memory:"` mode.
        ///
        /// ```rust,no_run
        /// let mut db = axiomdb_embedded::Db::open_memory().unwrap();
        /// db.execute("CREATE TABLE t (id INT)").unwrap();
        /// // data disappears when `db` goes out of scope
        /// ```
        pub fn open_memory() -> Result<Self, DbError> {
            let tmpdir = tempfile::tempdir().map_err(DbError::Io)?;
            let db_path = tmpdir.path().join("mem.db");
            let wal_path = tmpdir.path().join("mem.wal");

            let storage = MmapStorage::create(&db_path)?;
            CatalogBootstrap::init(&storage)?;
            let txn = TxnManager::create(&wal_path)?;

            Ok(Self {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_cache: SchemaCache::new(),
                session: SessionContext::default(),
                degraded: false,
                error_msg: None,
                _tmpdir: Some(tmpdir),
            })
        }

        /// Opens or creates a database from a local DSN.
        ///
        /// Accepted forms in `5.15`:
        /// - plain paths
        /// - `file:` URIs
        /// - `axiomdb:///local/path`
        ///
        /// Remote wire DSNs parse successfully but are rejected for the
        /// embedded API in this subphase.
        pub fn open_dsn(dsn: impl AsRef<str>) -> Result<Self, DbError> {
            let path = resolve_local_dsn_path(dsn.as_ref())?;
            Self::open(path)
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

        /// Executes a SQL SELECT and returns both column names and rows.
        ///
        /// Use this when you need to know column names at runtime (e.g. to build
        /// a table display, serialize to JSON, or pass column headers to a UI).
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// let (columns, rows) = db.query_with_columns("SELECT id, name FROM users").unwrap();
        /// println!("columns: {:?}", columns); // ["id", "name"]
        /// for row in rows {
        ///     for (col, val) in columns.iter().zip(row.iter()) {
        ///         println!("{col} = {val}");
        ///     }
        /// }
        /// ```
        pub fn query_with_columns(
            &mut self,
            sql: &str,
        ) -> Result<(Vec<String>, Vec<Row>), DbError> {
            let result = self.run(sql)?;
            Ok(match result {
                QueryResult::Rows { columns, rows } => {
                    let names = columns.into_iter().map(|c| c.name).collect();
                    (names, rows)
                }
                _ => (vec![], vec![]),
            })
        }

        /// Executes a DDL/DML statement with bound `?` parameters.
        ///
        /// Real prepared-statement binding (parse + analyze + substitute), not
        /// string interpolation — safe against SQL injection. Returns rows
        /// affected.
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// # db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        /// use axiomdb_types::Value;
        /// db.execute_params(
        ///     "INSERT INTO t VALUES (?, ?)",
        ///     &[Value::Int(1), Value::Text("Alice".into())],
        /// ).unwrap();
        /// ```
        pub fn execute_params(&mut self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
            Ok(match self.run_params(sql, params)? {
                QueryResult::Affected { count, .. } => count,
                QueryResult::Rows { rows, .. } => rows.len() as u64,
                QueryResult::Empty => 0,
            })
        }

        /// Executes a SELECT with bound `?` parameters, returning column names
        /// and rows. Safe against SQL injection (see [`execute_params`]).
        ///
        /// [`execute_params`]: Db::execute_params
        pub fn query_params(
            &mut self,
            sql: &str,
            params: &[Value],
        ) -> Result<(Vec<String>, Vec<Row>), DbError> {
            Ok(match self.run_params(sql, params)? {
                QueryResult::Rows { columns, rows } => {
                    (columns.into_iter().map(|c| c.name).collect(), rows)
                }
                _ => (vec![], vec![]),
            })
        }

        /// Prepares + executes `sql` with `params`, capturing any error into
        /// `error_msg` (so `last_error()` works, mirroring `run`).
        fn run_params(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
            let prepared = match self.prepare(sql) {
                Ok(p) => p,
                Err(e) => {
                    self.error_msg = CString::new(e.to_string()).ok();
                    return Err(e);
                }
            };
            let result = prepared.execute(self, params);
            match &result {
                Ok(_) => self.error_msg = None,
                Err(e) => self.error_msg = CString::new(e.to_string()).ok(),
            }
            result
        }

        /// Executes a SQL statement and returns the full `QueryResult`.
        ///
        /// Useful when you need column metadata, last_insert_id, etc.
        pub fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            let result = self.run_inner(sql);
            match &result {
                Ok(_) => {
                    self.error_msg = None;
                }
                Err(e) => {
                    if matches!(e, DbError::DiskFull { .. }) {
                        self.degraded = true;
                    }
                    self.error_msg = CString::new(e.to_string()).ok();
                }
            }
            result
        }

        /// Inner implementation — all errors bubble up through `run()` which
        /// captures them into `error_msg`. Using `?` here is safe because
        /// `run()` wraps the whole call and always sets `error_msg` on error.
        fn run_inner(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            if self.degraded && sql_may_mutate(sql) {
                return Err(DbError::DiskFull {
                    operation: "database is in read-only degraded mode",
                });
            }
            // Attack 20: route SELECTs through the per-session statement
            // cache. INSERT/UPDATE/DELETE are gated off — Attack 22
            // tried to extend the cache to them and reproduced the
            // original Attack 2 regression (~17% on insert_autocommit).
            // Two repair paths investigated and rejected:
            //   (a) Drop PlanDeps.is_stale + clear the cache from
            //       invalidate_all → defeats the cache because every DML
            //       calls invalidate_all (35% slower than the baseline).
            //   (b) Keep PlanDeps.is_stale → 17% slower on INSERT due to
            //       the per-dep catalog probe (analyze for INSERT is
            //       already cheap, so the probe is net-negative).
            // Attack 23 implemented option (d) for the SELECT path:
            // run_cached now checks epoch_plan_fast_path first — if all
            // dep table epochs are current, skips CatalogReader creation
            // and PlanDeps::is_stale entirely (O(1) HashMap lookup).
            let result = if axiomdb_sql::sql_starts_with_select_keyword(sql) {
                // Read-only (snapshot, no per-statement write-txn begin/commit)
                // only in autocommit: with no open `conn_txn` there are no staged
                // writes the read-only executor could miss. Inside an explicit
                // transaction we keep the write-capable path so staged rows are
                // flushed/visible. Computed before the call (avoids borrowing
                // `self.session` both mutably and immutably).
                let read_only = self.session.conn_txn.is_none();
                axiomdb_sql::statement_cache::run_cached(
                    sql,
                    &self.storage,
                    &self.txn,
                    &self.bloom,
                    &mut self.schema_cache,
                    &mut self.session,
                    read_only,
                )
            } else {
                let stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;
                let snap = if let Some(ref ct) = self.session.conn_txn {
                    self.txn.active_snapshot(ct)
                } else {
                    self.txn.snapshot()
                };
                let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
                execute_with_ctx(
                    analyzed,
                    &self.storage,
                    &self.txn,
                    &self.bloom,
                    &mut self.session,
                )
            };

            // Phase 19.1: inline auto-vacuum after a successful autocommit
            // query. Skipped inside explicit txns and in degraded mode
            // (the helper guards both). Errors are logged inside the
            // helper, never propagated — the user's already-successful
            // query result must not be turned into a failure by background
            // maintenance.
            if result.is_ok() && !self.degraded {
                axiomdb_sql::vacuum::auto_vacuum_if_needed(
                    &self.storage,
                    &self.txn,
                    &self.bloom,
                    &mut self.session,
                );
            }

            result
        }

        /// Returns the last error message, or `None` if the last operation succeeded.
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// if db.query("SELECT * FROM missing").is_err() {
        ///     println!("error: {:?}", db.last_error());
        /// }
        /// ```
        pub fn last_error(&self) -> Option<&str> {
            self.error_msg.as_deref().and_then(|s| s.to_str().ok())
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

    // ── PreparedStatement (Phase 10.8) ────────────────────────────────────────
    //
    // SQLite: sqlite3_prepare_v2 → sqlite3_bind_* → sqlite3_step (reuse VDBE bytecode)
    // PostgreSQL: PREPARE → EXECUTE (reuse parsed + planned statement)
    // MySQL: COM_STMT_PREPARE → COM_STMT_EXECUTE (reuse parsed statement)
    //
    // Our approach: parse + analyze ONCE at prepare(), store the analyzed Stmt
    // with Param placeholders. Each execute() substitutes params and runs.

    /// A prepared statement — parsed and analyzed once, executed many times.
    ///
    /// Eliminates parse + analyze overhead on repeated executions.
    /// Parameters are bound as `?` placeholders in the SQL.
    ///
    /// ```rust,no_run
    /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
    /// # db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    /// let mut stmt = db.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    /// stmt.execute(&mut db, &[axiomdb_types::Value::Int(1), axiomdb_types::Value::Text("Alice".into())]).unwrap();
    /// stmt.execute(&mut db, &[axiomdb_types::Value::Int(2), axiomdb_types::Value::Text("Bob".into())]).unwrap();
    /// ```
    pub struct PreparedStatement {
        analyzed: Stmt,
        param_count: usize,
    }

    impl Db {
        /// Opens an [`Appender`](crate::Appender) for high-throughput INSERT
        /// into `table_name`.
        ///
        /// The Appender skips the SQL parser/analyzer/dispatcher and writes
        /// typed [`Value`]s directly to the heap + WAL. Analogous to DuckDB's
        /// Appender and SQLite's `sqlite3_bind_*` + `sqlite3_step` pattern.
        ///
        /// Holds an active transaction for its whole lifetime;
        /// `Appender::finish()` commits, `Drop` rolls back.
        ///
        /// # Errors
        /// - [`DbError::TableNotFound`] if `table_name` doesn't exist.
        /// - [`DbError::NotImplemented`] if the table is clustered or has
        ///   triggers (deferred to a future Attack).
        /// - [`DbError::TransactionAlreadyActive`] if a SQL `BEGIN` is open.
        /// - I/O errors from `txn.begin()`.
        ///
        /// ```rust,no_run
        /// # let mut db = axiomdb_embedded::Db::open("./test.db").unwrap();
        /// # db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
        /// use axiomdb_types::Value;
        /// let mut app = db.appender("t").unwrap();
        /// app.append_row(&[Value::Int(1), Value::Text("a".into())]).unwrap();
        /// app.finish().unwrap();
        /// ```
        pub fn appender(&mut self, table_name: &str) -> Result<crate::Appender<'_>, DbError> {
            crate::appender::Appender::open(self, table_name)
        }
    }

    impl Db {
        /// Prepares a SQL statement for repeated execution.
        ///
        /// The SQL may contain `?` parameter placeholders. The returned
        /// [`PreparedStatement`] can be executed multiple times with different
        /// parameter values, skipping parse + analyze on each call.
        pub fn prepare(&mut self, sql: &str) -> Result<PreparedStatement, DbError> {
            // Parse with parameter support.
            let stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;

            // Count Param nodes in the AST.
            let param_count = count_params(&stmt);

            // Analyze — resolves column indices, type checks.
            let snap = if let Some(ref ct) = self.session.conn_txn {
                self.txn.active_snapshot(ct)
            } else {
                self.txn.snapshot()
            };
            let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;

            Ok(PreparedStatement {
                analyzed,
                param_count,
            })
        }
    }

    impl PreparedStatement {
        /// Executes the prepared statement with the given parameter values.
        ///
        /// `params` must have exactly the number of `?` placeholders in the SQL.
        /// Skips parse + analyze — only substitutes params and executes.
        pub fn execute(&self, db: &mut Db, params: &[Value]) -> Result<QueryResult, DbError> {
            if params.len() != self.param_count {
                return Err(DbError::Other(format!(
                    "expected {} parameters, got {}",
                    self.param_count,
                    params.len()
                )));
            }

            // Clone the analyzed AST and substitute Param nodes with Literal values.
            let stmt = axiomdb_sql::time_select_phase!(
                clone_ns,
                substitute_params(self.analyzed.clone(), params)?
            );
            axiomdb_sql::bench_timings::bump_select_calls(1);

            // Read-only fast path: a SELECT in autocommit (no staged writes in an
            // open txn) is served from a snapshot — skips the per-statement
            // write-txn begin/commit that `execute_with_ctx` does for SELECT.
            axiomdb_sql::time_select_phase!(exec_ns, {
                if matches!(stmt, Stmt::Select(_)) && db.session.conn_txn.is_none() {
                    execute_read_only_with_ctx(
                        stmt,
                        &db.storage,
                        &db.txn,
                        &db.bloom,
                        &mut db.session,
                    )
                } else {
                    execute_with_ctx(stmt, &db.storage, &db.txn, &db.bloom, &mut db.session)
                }
            })
        }

        /// Returns the number of `?` parameters in this prepared statement.
        pub fn param_count(&self) -> usize {
            self.param_count
        }
    }

    /// Counts `Expr::Param` nodes in a statement (recursive walk).
    fn count_params(stmt: &Stmt) -> usize {
        // Simple heuristic: count Param nodes in the string repr.
        // A proper implementation would walk the Expr tree.
        // For now, count ? in the original SQL is sufficient since
        // parse() converts each ? to Expr::Param { idx }.
        let debug = format!("{stmt:?}");
        debug.matches("Param {").count()
    }

    /// Replaces `Expr::Param { idx }` with `Expr::Literal(params[idx])` in the AST.
    fn substitute_params(mut stmt: Stmt, params: &[Value]) -> Result<Stmt, DbError> {
        fn sub_expr(expr: &mut Expr, params: &[Value]) {
            match expr {
                Expr::Param { idx } => {
                    if let Some(v) = params.get(*idx) {
                        *expr = Expr::Literal(v.clone());
                    }
                }
                Expr::BinaryOp { left, right, .. } => {
                    sub_expr(left, params);
                    sub_expr(right, params);
                }
                Expr::UnaryOp { operand, .. } => sub_expr(operand, params),
                Expr::IsNull { expr: e, .. } => sub_expr(e, params),
                Expr::Between {
                    expr, low, high, ..
                } => {
                    sub_expr(expr, params);
                    sub_expr(low, params);
                    sub_expr(high, params);
                }
                Expr::In { expr, list, .. } => {
                    sub_expr(expr, params);
                    for item in list {
                        sub_expr(item, params);
                    }
                }
                Expr::Like { expr, pattern, .. } => {
                    sub_expr(expr, params);
                    sub_expr(pattern, params);
                }
                Expr::Function { args, .. } => {
                    for arg in args {
                        sub_expr(arg, params);
                    }
                }
                Expr::Cast { expr: e, .. } => sub_expr(e, params),
                _ => {}
            }
        }

        match &mut stmt {
            Stmt::Select(s) => {
                if let Some(ref mut wc) = s.where_clause {
                    sub_expr(wc, params);
                }
                for item in &mut s.columns {
                    if let SelectItem::Expr { expr, .. } = item {
                        sub_expr(expr, params);
                    }
                }
            }
            Stmt::Insert(s) => {
                if let InsertSource::Values(rows) = &mut s.source {
                    for row in rows {
                        for expr in row {
                            sub_expr(expr, params);
                        }
                    }
                }
            }
            Stmt::Update(s) => {
                for a in &mut s.assignments {
                    sub_expr(&mut a.value, params);
                }
                if let Some(ref mut wc) = s.where_clause {
                    sub_expr(wc, params);
                }
            }
            Stmt::Delete(s) => {
                if let Some(ref mut wc) = s.where_clause {
                    sub_expr(wc, params);
                }
            }
            _ => {}
        }

        Ok(stmt)
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

    // sql_starts_with_select_keyword is now axiomdb_sql::sql_starts_with_select_keyword

    fn resolve_local_dsn_path(dsn: &str) -> Result<PathBuf, DbError> {
        let parsed = parse_dsn(dsn)?;
        match parsed {
            ParsedDsn::Local(local) => {
                if !local.query.is_empty() {
                    let params = local.query.keys().cloned().collect::<Vec<_>>().join(", ");
                    return Err(DbError::InvalidDsn {
                        reason: format!(
                            "embedded DSN does not support query parameters in 5.15: {params}"
                        ),
                    });
                }
                Ok(local.path)
            }
            ParsedDsn::Wire(_) => Err(DbError::InvalidDsn {
                reason: "embedded open_dsn only supports local-path DSNs in 5.15".into(),
            }),
        }
    }
}

// ── C FFI ─────────────────────────────────────────────────────────────────────

// Re-export the FFI surface so integration tests can drive the
// `#[no_mangle] extern "C"` functions through Rust paths (without
// needing dynamic linking).
#[cfg(feature = "c-ffi")]
pub use ffi::{
    axiomdb_appender_append_bigint, axiomdb_appender_append_bool, axiomdb_appender_append_bytes,
    axiomdb_appender_append_int, axiomdb_appender_append_null, axiomdb_appender_append_real,
    axiomdb_appender_append_text, axiomdb_appender_end_row, axiomdb_appender_finish,
    axiomdb_appender_flush, axiomdb_appender_free, axiomdb_appender_open, axiomdb_close,
    axiomdb_execute, axiomdb_last_error, axiomdb_open, AxiomDbAppender,
};

#[cfg(feature = "c-ffi")]
mod ffi {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};

    use axiomdb_types::Value;

    use super::db::Db;

    // ── Type codes (match SQLite conventions for easy porting) ────────────────

    /// Cell type: SQL NULL.
    pub const AXIOMDB_TYPE_NULL: c_int = 0;
    /// Cell type: integer (Bool, Int, BigInt, Date days, Timestamp µs).
    pub const AXIOMDB_TYPE_INTEGER: c_int = 1;
    /// Cell type: floating-point (Real, Decimal).
    pub const AXIOMDB_TYPE_REAL: c_int = 2;
    /// Cell type: UTF-8 text (Text, UUID).
    pub const AXIOMDB_TYPE_TEXT: c_int = 3;
    /// Cell type: binary blob (Bytes).
    pub const AXIOMDB_TYPE_BLOB: c_int = 4;

    // ── Internal cell representation ──────────────────────────────────────────

    enum CellValue {
        Null,
        Integer(i64),
        Real(f64),
        Text(CString),
        Blob(Vec<u8>),
    }

    impl CellValue {
        fn type_code(&self) -> c_int {
            match self {
                Self::Null => AXIOMDB_TYPE_NULL,
                Self::Integer(_) => AXIOMDB_TYPE_INTEGER,
                Self::Real(_) => AXIOMDB_TYPE_REAL,
                Self::Text(_) => AXIOMDB_TYPE_TEXT,
                Self::Blob(_) => AXIOMDB_TYPE_BLOB,
            }
        }
    }

    fn value_to_cell(v: Value) -> CellValue {
        match v {
            Value::Null => CellValue::Null,
            Value::Bool(b) => CellValue::Integer(b as i64),
            Value::Int(i) => CellValue::Integer(i as i64),
            Value::BigInt(i) => CellValue::Integer(i),
            Value::Real(f) => CellValue::Real(f),
            Value::Decimal(m, s) => CellValue::Real(m as f64 / 10f64.powi(s as i32)),
            Value::Date(d) => CellValue::Integer(d as i64),
            Value::Timestamp(t) => CellValue::Integer(t),
            Value::TimestampTz(t) => CellValue::Integer(t),
            Value::Text(s) | Value::Json(s) => {
                CellValue::Text(CString::new(s).unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Jsonb(blob) => {
                let s = axiomdb_types::JsonbDecoder::to_string(blob.as_ref())
                    .unwrap_or_else(|_| "null".to_string());
                CellValue::Text(CString::new(s).unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Bytes(b) => CellValue::Blob(b),
            Value::Uuid(u) => {
                let s = format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
                    u16::from_be_bytes([u[4], u[5]]),
                    u16::from_be_bytes([u[6], u[7]]),
                    u16::from_be_bytes([u[8], u[9]]),
                    {
                        let mut buf = [0u8; 8];
                        buf[2..].copy_from_slice(&u[10..16]);
                        u64::from_be_bytes(buf)
                    }
                );
                CellValue::Text(CString::new(s).unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Array(_elems) => {
                // Array FFI deferred to Step 10.
                CellValue::Text(CString::new("{}").unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Range(rv) => {
                let s = rv.to_display_string();
                CellValue::Text(CString::new(s).unwrap_or_else(|_| CString::new("empty").unwrap()))
            }
            Value::Composite(fields) => {
                let disp = Value::Composite(fields).to_string();
                CellValue::Text(CString::new(disp).unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Money(m, s, c) => {
                let disp = Value::Money(m, s, c).to_string();
                CellValue::Text(CString::new(disp).unwrap_or_else(|_| CString::new("").unwrap()))
            }
            Value::Ltree(s) | Value::Xml(s) => CellValue::Text(
                CString::new(s.as_str()).unwrap_or_else(|_| CString::new("").unwrap()),
            ),
        }
    }

    // ── AxiomRows — C-safe result set ─────────────────────────────────────────

    /// A materialized query result set returned by `axiomdb_query`.
    ///
    /// All row data and column names are owned by this struct.
    /// Must be freed with `axiomdb_rows_free` when no longer needed.
    pub struct AxiomRows {
        col_names: Vec<CString>,
        cells: Vec<Vec<CellValue>>,
    }

    // ── Open / close ──────────────────────────────────────────────────────────

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

    /// Opens or creates a database from a local DSN.
    ///
    /// # Safety
    /// `dsn` must be a valid non-null pointer to a UTF-8 null-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_open_dsn(dsn: *const c_char) -> *mut Db {
        if dsn.is_null() {
            return std::ptr::null_mut();
        }
        let dsn = match CStr::from_ptr(dsn).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        match Db::open_dsn(dsn) {
            Ok(db) => Box::into_raw(Box::new(db)),
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// Executes a SQL statement (INSERT, UPDATE, DELETE, DDL — no result rows).
    ///
    /// Returns the number of rows affected, or -1 on error.
    /// On error, `axiomdb_last_error(db)` returns the error message.
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

    /// Executes a SQL SELECT and returns a result set.
    ///
    /// Returns an `AxiomRows*` that must be freed with `axiomdb_rows_free`,
    /// or NULL on error. On error, `axiomdb_last_error(db)` returns the message.
    ///
    /// # Safety
    /// `db` must be a valid pointer from `axiomdb_open`.
    /// `sql` must be a valid non-null null-terminated UTF-8 string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_query(db: *mut Db, sql: *const c_char) -> *mut AxiomRows {
        if db.is_null() || sql.is_null() {
            return std::ptr::null_mut();
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        match db.run(sql) {
            Ok(axiomdb_sql::result::QueryResult::Rows { columns, rows }) => {
                let col_names: Vec<CString> = columns
                    .into_iter()
                    .map(|c| CString::new(c.name).unwrap_or_else(|_| CString::new("").unwrap()))
                    .collect();
                let cells: Vec<Vec<CellValue>> = rows
                    .into_iter()
                    .map(|row| row.into_iter().map(value_to_cell).collect())
                    .collect();
                Box::into_raw(Box::new(AxiomRows { col_names, cells }))
            }
            Ok(_) => {
                // DDL / DML with no rows: return an empty result set
                Box::into_raw(Box::new(AxiomRows {
                    col_names: vec![],
                    cells: vec![],
                }))
            }
            Err(_) => std::ptr::null_mut(),
        }
    }

    // ── Packed result buffer (single-FFI-call materialization) ────────────────
    //
    // The per-cell accessors above cross the FFI boundary ~2× per cell, which
    // dominates Python-binding latency (~120K ctypes calls for a 10K×6 result).
    // `axiomdb_query_packed` serializes the whole result into one contiguous
    // buffer so the binding crosses the boundary exactly once. Format (LE):
    //
    //   u32 magic = PACKED_MAGIC ("AXM1")
    //   u32 n_cols
    //   u64 n_rows
    //   n_cols × { u32 name_len, name_bytes (UTF-8) }
    //   n_rows × n_cols × cell:
    //     u8 tag (0=NULL,1=INT i64,2=REAL f64,3=TEXT,4=BLOB)
    //     payload: INT→i64 | REAL→f64 | TEXT/BLOB→ u32 len + bytes | NULL→∅
    //
    // Type mapping reuses `value_to_cell`, so the packed path is byte-for-byte
    // consistent with the per-cell accessors.

    /// Magic header identifying a packed result buffer ("AXM1" little-endian).
    const PACKED_MAGIC: u32 = 0x41584D31;

    /// Serializes one value into `buf` using the packed cell encoding.
    fn pack_value(buf: &mut Vec<u8>, v: Value) {
        match value_to_cell(v) {
            CellValue::Null => buf.push(0),
            CellValue::Integer(n) => {
                buf.push(1);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            CellValue::Real(f) => {
                buf.push(2);
                buf.extend_from_slice(&f.to_le_bytes());
            }
            CellValue::Text(s) => {
                let b = s.as_bytes();
                buf.push(3);
                buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                buf.extend_from_slice(b);
            }
            CellValue::Blob(b) => {
                buf.push(4);
                buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                buf.extend_from_slice(&b);
            }
        }
    }

    /// Serializes a full result set (column names + cells) into a packed buffer.
    fn pack_rows(columns: &[axiomdb_sql::result::ColumnMeta], rows: Vec<Vec<Value>>) -> Vec<u8> {
        let n_cols = columns.len();
        let n_rows = rows.len();
        // Rough pre-size: header + names + ~12 bytes/cell average.
        let mut buf = Vec::with_capacity(16 + n_cols * 16 + n_rows * n_cols * 12);
        buf.extend_from_slice(&PACKED_MAGIC.to_le_bytes());
        buf.extend_from_slice(&(n_cols as u32).to_le_bytes());
        buf.extend_from_slice(&(n_rows as u64).to_le_bytes());
        for col in columns {
            let nb = col.name.as_bytes();
            buf.extend_from_slice(&(nb.len() as u32).to_le_bytes());
            buf.extend_from_slice(nb);
        }
        for row in rows {
            for v in row {
                pack_value(&mut buf, v);
            }
        }
        buf
    }

    // ── Columnar packed buffer (AXM2) ─────────────────────────────────────────
    //
    // Same header as AXM1, but values are grouped BY COLUMN, and each column
    // declares a kind so the consumer can bulk-decode homogeneous columns in one
    // call (e.g. Ruby `unpack('q<*')`, Python `struct.unpack`, JS typed arrays)
    // instead of an interpreted per-cell loop — measured ~4× faster parse in
    // Ruby. Columns with NULLs or mixed types fall back to per-cell ('M').
    //
    //   u32 magic = COLUMNAR_MAGIC ("AXM2")
    //   u32 n_cols ; u64 n_rows
    //   n_cols × { u32 name_len, name_bytes }
    //   per column: u8 kind, then:
    //     'I' (73): n_rows × i64        | 'F' (70): n_rows × f64
    //     'T' (84)/'B' (66): n_rows × u32 len, then concatenated bytes
    //     'M' (77): n_rows × (u8 tag + payload)   (AXM1 cell encoding; NULLs ok)

    /// Magic header for the columnar packed buffer ("AXM2" little-endian).
    const COLUMNAR_MAGIC: u32 = 0x41584D32;

    /// Classifies a column: `b'I'`/`b'F'`/`b'T'`/`b'B'` if every cell is that one
    /// fast type (no NULL), else `b'M'` (mixed → per-cell fallback).
    fn column_kind(rows: &[Vec<Value>], col: usize) -> u8 {
        let mut kind: Option<u8> = None;
        for row in rows {
            let k = match row.get(col) {
                Some(Value::Bool(_))
                | Some(Value::Int(_))
                | Some(Value::BigInt(_))
                | Some(Value::Date(_))
                | Some(Value::Timestamp(_))
                | Some(Value::TimestampTz(_)) => b'I',
                Some(Value::Real(_)) | Some(Value::Decimal(..)) => b'F',
                Some(Value::Text(_)) | Some(Value::Json(_)) => b'T',
                Some(Value::Bytes(_)) => b'B',
                _ => return b'M', // NULL / Uuid / Jsonb / Array / … → mixed
            };
            match kind {
                None => kind = Some(k),
                Some(prev) if prev == k => {}
                _ => return b'M', // mixed types within the column
            }
        }
        kind.unwrap_or(b'M')
    }

    fn col_as_i64(v: &Value) -> i64 {
        match v {
            Value::Bool(b) => *b as i64,
            Value::Int(i) => *i as i64,
            Value::BigInt(i) => *i,
            Value::Date(d) => *d as i64,
            Value::Timestamp(t) | Value::TimestampTz(t) => *t,
            _ => 0,
        }
    }

    fn col_as_f64(v: &Value) -> f64 {
        match v {
            Value::Real(f) => *f,
            Value::Decimal(m, s) => *m as f64 / 10f64.powi(*s as i32),
            _ => 0.0,
        }
    }

    fn col_bytes(v: &Value) -> &[u8] {
        match v {
            Value::Text(s) | Value::Json(s) => s.as_bytes(),
            Value::Bytes(b) => b.as_slice(),
            _ => &[],
        }
    }

    /// Serializes a result set in columnar (AXM2) layout.
    fn pack_columnar(columns: &[axiomdb_sql::result::ColumnMeta], rows: &[Vec<Value>]) -> Vec<u8> {
        let n_cols = columns.len();
        let n_rows = rows.len();
        let mut buf = Vec::with_capacity(16 + n_cols * 16 + n_rows * n_cols * 9);
        buf.extend_from_slice(&COLUMNAR_MAGIC.to_le_bytes());
        buf.extend_from_slice(&(n_cols as u32).to_le_bytes());
        buf.extend_from_slice(&(n_rows as u64).to_le_bytes());
        for col in columns {
            let nb = col.name.as_bytes();
            buf.extend_from_slice(&(nb.len() as u32).to_le_bytes());
            buf.extend_from_slice(nb);
        }
        for c in 0..n_cols {
            let kind = column_kind(rows, c);
            buf.push(kind);
            match kind {
                b'I' => {
                    for row in rows {
                        buf.extend_from_slice(&col_as_i64(&row[c]).to_le_bytes());
                    }
                }
                b'F' => {
                    for row in rows {
                        buf.extend_from_slice(&col_as_f64(&row[c]).to_le_bytes());
                    }
                }
                b'T' | b'B' => {
                    for row in rows {
                        buf.extend_from_slice(&(col_bytes(&row[c]).len() as u32).to_le_bytes());
                    }
                    for row in rows {
                        buf.extend_from_slice(col_bytes(&row[c]));
                    }
                }
                _ => {
                    // 'M': per-cell (AXM1) encoding; clone since pack_value owns.
                    for row in rows {
                        pack_value(&mut buf, row[c].clone());
                    }
                }
            }
        }
        buf
    }

    /// Like `axiomdb_query_packed` but uses the columnar (AXM2) layout, which
    /// lets the consumer bulk-decode homogeneous columns. Free with
    /// `axiomdb_packed_free(ptr, len)`.
    ///
    /// # Safety
    /// Same as `axiomdb_query_packed`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_query_packed_columnar(
        db: *mut Db,
        sql: *const c_char,
        out_len: *mut usize,
    ) -> *mut u8 {
        if db.is_null() || sql.is_null() || out_len.is_null() {
            return std::ptr::null_mut();
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let buf = match db.run(sql) {
            Ok(axiomdb_sql::result::QueryResult::Rows { columns, rows }) => {
                pack_columnar(&columns, &rows)
            }
            Ok(_) => pack_columnar(&[], &[]),
            Err(_) => return std::ptr::null_mut(),
        };
        let boxed = buf.into_boxed_slice();
        *out_len = boxed.len();
        Box::into_raw(boxed) as *mut u8
    }

    /// Executes a SELECT and serializes the entire result set into one heap
    /// buffer. Returns the buffer pointer and writes its byte length to
    /// `out_len`. Returns NULL on error (see `axiomdb_last_error`). The buffer
    /// must be freed with `axiomdb_packed_free(ptr, len)`.
    ///
    /// Non-row statements (DDL/DML) yield an empty (0-row, 0-col) buffer.
    ///
    /// # Safety
    /// `db` must be a valid pointer from `axiomdb_open`. `sql` must be a valid
    /// non-null null-terminated UTF-8 string. `out_len` must be a valid
    /// non-null pointer to a `usize`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_query_packed(
        db: *mut Db,
        sql: *const c_char,
        out_len: *mut usize,
    ) -> *mut u8 {
        if db.is_null() || sql.is_null() || out_len.is_null() {
            return std::ptr::null_mut();
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let buf = match db.run(sql) {
            Ok(axiomdb_sql::result::QueryResult::Rows { columns, rows }) => {
                pack_rows(&columns, rows)
            }
            Ok(_) => pack_rows(&[], Vec::new()),
            Err(_) => return std::ptr::null_mut(),
        };
        let boxed = buf.into_boxed_slice();
        *out_len = boxed.len();
        Box::into_raw(boxed) as *mut u8
    }

    /// Frees a buffer returned by `axiomdb_query_packed`.
    ///
    /// # Safety
    /// `ptr` and `len` must be exactly the values produced by a single
    /// `axiomdb_query_packed` call, and must not have been freed already.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_packed_free(ptr: *mut u8, len: usize) {
        if !ptr.is_null() {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }

    // ── Parameter binding ─────────────────────────────────────────────────────
    //
    // Real prepared-statement binding (not client-side string escaping): the
    // binding serializes the `?` parameter values into a small buffer, the FFI
    // deserializes them to `Value`s, then `Db::prepare` + `PreparedStatement::
    // execute` substitutes and runs. Buffer format (LE):
    //   u32 n_params
    //   n_params × { u8 tag, payload }  (tag: 0=NULL,1=INT i64,2=REAL f64,
    //                                    3=TEXT u32+bytes, 4=BLOB u32+bytes)

    /// Deserializes a parameter buffer into `Value`s. NULL/empty → no params.
    unsafe fn deserialize_params(ptr: *const u8, len: usize) -> Vec<Value> {
        if ptr.is_null() || len < 4 {
            return Vec::new();
        }
        let buf = std::slice::from_raw_parts(ptr, len);
        let rd_u32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let n = rd_u32(buf, 0) as usize;
        let mut off = 4;
        let mut params = Vec::with_capacity(n);
        for _ in 0..n {
            if off >= buf.len() {
                break;
            }
            let tag = buf[off];
            off += 1;
            match tag {
                1 => {
                    let v = i64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                    off += 8;
                    params.push(Value::BigInt(v));
                }
                2 => {
                    let v = f64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                    off += 8;
                    params.push(Value::Real(v));
                }
                3 => {
                    let l = rd_u32(buf, off) as usize;
                    off += 4;
                    params.push(Value::Text(
                        String::from_utf8_lossy(&buf[off..off + l]).into_owned(),
                    ));
                    off += l;
                }
                4 => {
                    let l = rd_u32(buf, off) as usize;
                    off += 4;
                    params.push(Value::Bytes(buf[off..off + l].to_vec()));
                    off += l;
                }
                _ => params.push(Value::Null),
            }
        }
        params
    }

    /// Executes a DDL/DML statement with bound `?` parameters. Returns rows
    /// affected, or -1 on error (see `axiomdb_last_error`).
    ///
    /// # Safety
    /// `db`/`sql` as in `axiomdb_execute`; `params_ptr`/`params_len` describe a
    /// parameter buffer (may be NULL/0 for no params).
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_execute_params(
        db: *mut Db,
        sql: *const c_char,
        params_ptr: *const u8,
        params_len: usize,
    ) -> i64 {
        if db.is_null() || sql.is_null() {
            return -1;
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        };
        let params = deserialize_params(params_ptr, params_len);
        let prepared = match db.prepare(sql) {
            Ok(p) => p,
            Err(e) => {
                db.error_msg = std::ffi::CString::new(e.to_string()).ok();
                return -1;
            }
        };
        match prepared.execute(db, &params) {
            Ok(axiomdb_sql::result::QueryResult::Affected { count, .. }) => count as i64,
            Ok(axiomdb_sql::result::QueryResult::Rows { rows, .. }) => rows.len() as i64,
            Ok(_) => 0,
            Err(e) => {
                db.error_msg = std::ffi::CString::new(e.to_string()).ok();
                -1
            }
        }
    }

    /// Executes a SELECT with bound `?` parameters and serializes the result
    /// into a packed (AXM1) buffer. Returns NULL on error. Free with
    /// `axiomdb_packed_free`.
    ///
    /// # Safety
    /// As in `axiomdb_query_packed`, plus `params_ptr`/`params_len`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_query_packed_params(
        db: *mut Db,
        sql: *const c_char,
        params_ptr: *const u8,
        params_len: usize,
        out_len: *mut usize,
    ) -> *mut u8 {
        if db.is_null() || sql.is_null() || out_len.is_null() {
            return std::ptr::null_mut();
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let params = deserialize_params(params_ptr, params_len);
        let prepared = match db.prepare(sql) {
            Ok(p) => p,
            Err(e) => {
                db.error_msg = std::ffi::CString::new(e.to_string()).ok();
                return std::ptr::null_mut();
            }
        };
        let buf = match prepared.execute(db, &params) {
            Ok(axiomdb_sql::result::QueryResult::Rows { columns, rows }) => {
                pack_rows(&columns, rows)
            }
            Ok(_) => pack_rows(&[], Vec::new()),
            Err(e) => {
                db.error_msg = std::ffi::CString::new(e.to_string()).ok();
                return std::ptr::null_mut();
            }
        };
        let boxed = buf.into_boxed_slice();
        *out_len = boxed.len();
        Box::into_raw(boxed) as *mut u8
    }

    // ── Cursor API (zero-copy over the materialized result, Tier 1) ───────────
    //
    // Keeps the engine's `Vec<Vec<Value>>` AS-IS — no second pass into
    // `CellValue`/`CString` like `axiomdb_query` does. Text/blob accessors return
    // pointers directly into the live `Value`, so reading text costs no
    // allocation. Memory is still O(n) (full streaming is the deeper Approach B).
    // Mirrors SQLite's `sqlite3_step` + `sqlite3_column_*` model.

    /// A forward-only cursor over a materialized result set.
    pub struct AxiomCursor {
        col_names: Vec<CString>,
        rows: Vec<Vec<Value>>,
        /// Current row index; `usize::MAX` before the first `step`.
        pos: usize,
        /// Scratch buffer for single-cell text accessors on values that need
        /// formatting (Uuid/Jsonb/Array/Range/Composite). Valid until the next
        /// such access.
        scratch: Vec<u8>,
        /// Per-cell scratch for the bulk `axiomdb_cursor_row` accessor: one
        /// owned buffer per formatted cell of the current row. Inner `Vec<u8>`
        /// allocations stay valid even when the outer `Vec` reallocs, so the
        /// pointers handed out remain valid until the next `cursor_row`/step/close.
        row_scratch: Vec<Vec<u8>>,
    }

    /// One result cell, flattened for the bulk row accessor.
    ///
    /// `type_code`: 0=NULL, 1=INT, 2=REAL, 3=TEXT, 4=BLOB. For INT read
    /// `int_val`; for REAL read `real_val`; for TEXT/BLOB read `ptr`/`len`
    /// (zero-copy into the live row, valid until the next `cursor_row`/step/close).
    #[repr(C)]
    pub struct AxiomCell {
        pub type_code: c_int,
        pub int_val: i64,
        pub real_val: f64,
        pub ptr: *const u8,
        pub len: usize,
    }

    /// Type code for a [`Value`] — mirrors `CellValue::type_code`.
    fn value_type_code(v: &Value) -> c_int {
        match v {
            Value::Null => AXIOMDB_TYPE_NULL,
            Value::Bool(_)
            | Value::Int(_)
            | Value::BigInt(_)
            | Value::Date(_)
            | Value::Timestamp(_)
            | Value::TimestampTz(_) => AXIOMDB_TYPE_INTEGER,
            Value::Real(_) | Value::Decimal(..) => AXIOMDB_TYPE_REAL,
            Value::Bytes(_) => AXIOMDB_TYPE_BLOB,
            _ => AXIOMDB_TYPE_TEXT,
        }
    }

    fn format_uuid(u: &[u8; 16]) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
            u16::from_be_bytes([u[4], u[5]]),
            u16::from_be_bytes([u[6], u[7]]),
            u16::from_be_bytes([u[8], u[9]]),
            {
                let mut buf = [0u8; 8];
                buf[2..].copy_from_slice(&u[10..16]);
                u64::from_be_bytes(buf)
            }
        )
    }

    /// Opens a forward-only cursor over the result of `sql`.
    ///
    /// Returns NULL on error (see `axiomdb_last_error`). Non-row statements
    /// yield an empty cursor. Free with `axiomdb_cursor_close`.
    ///
    /// # Safety
    /// `db` from `axiomdb_open`; `sql` a valid non-null UTF-8 C string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_open(
        db: *mut Db,
        sql: *const c_char,
    ) -> *mut AxiomCursor {
        if db.is_null() || sql.is_null() {
            return std::ptr::null_mut();
        }
        let db = &mut *db;
        let sql = match CStr::from_ptr(sql).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        match db.run(sql) {
            Ok(axiomdb_sql::result::QueryResult::Rows { columns, rows }) => {
                let col_names = columns
                    .into_iter()
                    .map(|c| CString::new(c.name).unwrap_or_else(|_| CString::new("").unwrap()))
                    .collect();
                Box::into_raw(Box::new(AxiomCursor {
                    col_names,
                    rows,
                    pos: usize::MAX,
                    scratch: Vec::new(),
                    row_scratch: Vec::new(),
                }))
            }
            Ok(_) => Box::into_raw(Box::new(AxiomCursor {
                col_names: Vec::new(),
                rows: Vec::new(),
                pos: usize::MAX,
                scratch: Vec::new(),
                row_scratch: Vec::new(),
            })),
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// Advances to the next row. Returns 1 if a row is available, 0 at end.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_step(cur: *mut AxiomCursor) -> c_int {
        if cur.is_null() {
            return 0;
        }
        let cur = &mut *cur;
        cur.pos = cur.pos.wrapping_add(1);
        c_int::from(cur.pos < cur.rows.len())
    }

    /// Returns the number of columns.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_columns(cur: *const AxiomCursor) -> c_int {
        if cur.is_null() {
            return 0;
        }
        let cur = &*cur;
        cur.col_names.len() as c_int
    }

    /// Returns the null-terminated name of column `col`, or NULL if out of range.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_column_name(
        cur: *const AxiomCursor,
        col: c_int,
    ) -> *const c_char {
        if cur.is_null() || col < 0 {
            return std::ptr::null();
        }
        let cur = &*cur;
        match cur.col_names.get(col as usize) {
            Some(n) => n.as_ptr(),
            None => std::ptr::null(),
        }
    }

    /// Returns the type code of the current row's cell `col`.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_type(cur: *const AxiomCursor, col: c_int) -> c_int {
        if cur.is_null() || col < 0 {
            return AXIOMDB_TYPE_NULL;
        }
        let cur = &*cur;
        cur.rows
            .get(cur.pos)
            .and_then(|r| r.get(col as usize))
            .map(value_type_code)
            .unwrap_or(AXIOMDB_TYPE_NULL)
    }

    /// Returns the integer value of the current row's cell `col` (0 otherwise).
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_int(cur: *const AxiomCursor, col: c_int) -> i64 {
        if cur.is_null() || col < 0 {
            return 0;
        }
        let cur = &*cur;
        match cur.rows.get(cur.pos).and_then(|r| r.get(col as usize)) {
            Some(Value::Bool(b)) => *b as i64,
            Some(Value::Int(i)) => *i as i64,
            Some(Value::BigInt(i)) => *i,
            Some(Value::Date(d)) => *d as i64,
            Some(Value::Timestamp(t)) | Some(Value::TimestampTz(t)) => *t,
            _ => 0,
        }
    }

    /// Returns the floating-point value of the current row's cell `col`.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_double(cur: *const AxiomCursor, col: c_int) -> f64 {
        if cur.is_null() || col < 0 {
            return 0.0;
        }
        let cur = &*cur;
        match cur.rows.get(cur.pos).and_then(|r| r.get(col as usize)) {
            Some(Value::Real(f)) => *f,
            Some(Value::Decimal(m, s)) => *m as f64 / 10f64.powi(*s as i32),
            _ => 0.0,
        }
    }

    /// Returns a pointer to the current row's text cell `col` and writes its
    /// byte length to `*len`. The pointer is **not** null-terminated and is
    /// valid until the next `axiomdb_cursor_step` or `axiomdb_cursor_close`.
    /// Returns NULL for non-text cells.
    ///
    /// # Safety
    /// `cur` from `axiomdb_cursor_open`; `len` a valid non-null pointer.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_text(
        cur: *mut AxiomCursor,
        col: c_int,
        len: *mut usize,
    ) -> *const c_char {
        let set_len = |n: usize| {
            if !len.is_null() {
                *len = n;
            }
        };
        if cur.is_null() || col < 0 {
            set_len(0);
            return std::ptr::null();
        }
        let cur = &mut *cur;
        // Zero-copy for plain text; format the rest into the scratch buffer.
        let formatted: Option<String> =
            match cur.rows.get(cur.pos).and_then(|r| r.get(col as usize)) {
                Some(Value::Text(s)) | Some(Value::Json(s)) => {
                    set_len(s.len());
                    // SAFETY: the pointer aliases the live `String` in `rows`,
                    // which is not mutated until step/close (documented contract).
                    return s.as_ptr() as *const c_char;
                }
                Some(Value::Uuid(u)) => Some(format_uuid(u)),
                Some(Value::Jsonb(b)) => Some(
                    axiomdb_types::JsonbDecoder::to_string(b.as_ref())
                        .unwrap_or_else(|_| "null".to_string()),
                ),
                Some(v @ (Value::Array(_) | Value::Range(_) | Value::Composite(_))) => {
                    Some(v.to_string())
                }
                _ => None,
            };
        match formatted {
            Some(s) => {
                cur.scratch.clear();
                cur.scratch.extend_from_slice(s.as_bytes());
                set_len(cur.scratch.len());
                cur.scratch.as_ptr() as *const c_char
            }
            None => {
                set_len(0);
                std::ptr::null()
            }
        }
    }

    /// Returns a pointer to the current row's blob cell `col` and writes its
    /// length to `*len`. Zero-copy; valid until the next step or close.
    ///
    /// # Safety
    /// `cur` from `axiomdb_cursor_open`; `len` a valid non-null pointer.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_blob(
        cur: *const AxiomCursor,
        col: c_int,
        len: *mut usize,
    ) -> *const u8 {
        let set_len = |n: usize| {
            if !len.is_null() {
                *len = n;
            }
        };
        if cur.is_null() || col < 0 {
            set_len(0);
            return std::ptr::null();
        }
        let cur = &*cur;
        match cur.rows.get(cur.pos).and_then(|r| r.get(col as usize)) {
            Some(Value::Bytes(b)) => {
                set_len(b.len());
                b.as_ptr()
            }
            _ => {
                set_len(0);
                std::ptr::null()
            }
        }
    }

    /// Fills `out[0..columns]` with every cell of the current row in a single
    /// call, eliminating the ~2 FFI crossings per cell of the scalar accessors.
    /// Returns the number of columns written, or 0 if there is no current row.
    ///
    /// TEXT/BLOB cells expose zero-copy pointers into the live row; values that
    /// need formatting (Uuid/Jsonb/Array/Range/Composite) point into per-cursor
    /// scratch. All pointers are valid until the next `axiomdb_cursor_row`,
    /// `axiomdb_cursor_step`, or `axiomdb_cursor_close`.
    ///
    /// # Safety
    /// `cur` from `axiomdb_cursor_open`; `out` must point to at least
    /// `axiomdb_cursor_columns(cur)` writable [`AxiomCell`] slots.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_row(
        cur: *mut AxiomCursor,
        out: *mut AxiomCell,
    ) -> c_int {
        if cur.is_null() || out.is_null() {
            return 0;
        }
        // Disjoint field borrows: read `rows`, write `row_scratch`.
        let AxiomCursor {
            rows,
            pos,
            row_scratch,
            ..
        } = &mut *cur;
        let row = match rows.get(*pos) {
            Some(r) => r,
            None => return 0,
        };
        row_scratch.clear();
        for (i, v) in row.iter().enumerate() {
            let cell = &mut *out.add(i);
            cell.int_val = 0;
            cell.real_val = 0.0;
            cell.ptr = std::ptr::null();
            cell.len = 0;
            match v {
                Value::Null => cell.type_code = AXIOMDB_TYPE_NULL,
                Value::Bool(b) => {
                    cell.type_code = AXIOMDB_TYPE_INTEGER;
                    cell.int_val = *b as i64;
                }
                Value::Int(x) => {
                    cell.type_code = AXIOMDB_TYPE_INTEGER;
                    cell.int_val = *x as i64;
                }
                Value::BigInt(x) => {
                    cell.type_code = AXIOMDB_TYPE_INTEGER;
                    cell.int_val = *x;
                }
                Value::Date(d) => {
                    cell.type_code = AXIOMDB_TYPE_INTEGER;
                    cell.int_val = *d as i64;
                }
                Value::Timestamp(t) | Value::TimestampTz(t) => {
                    cell.type_code = AXIOMDB_TYPE_INTEGER;
                    cell.int_val = *t;
                }
                Value::Real(f) => {
                    cell.type_code = AXIOMDB_TYPE_REAL;
                    cell.real_val = *f;
                }
                Value::Decimal(m, s) => {
                    cell.type_code = AXIOMDB_TYPE_REAL;
                    cell.real_val = *m as f64 / 10f64.powi(*s as i32);
                }
                Value::Text(s) | Value::Json(s) => {
                    cell.type_code = AXIOMDB_TYPE_TEXT;
                    cell.ptr = s.as_ptr();
                    cell.len = s.len();
                }
                Value::Bytes(b) => {
                    cell.type_code = AXIOMDB_TYPE_BLOB;
                    cell.ptr = b.as_ptr();
                    cell.len = b.len();
                }
                other => {
                    // Formatted text into per-cell scratch (stable pointer).
                    let formatted = match other {
                        Value::Uuid(u) => format_uuid(u),
                        Value::Jsonb(b) => axiomdb_types::JsonbDecoder::to_string(b.as_ref())
                            .unwrap_or_else(|_| "null".to_string()),
                        _ => other.to_string(),
                    };
                    row_scratch.push(formatted.into_bytes());
                    let buf = row_scratch.last().unwrap();
                    cell.type_code = AXIOMDB_TYPE_TEXT;
                    cell.ptr = buf.as_ptr();
                    cell.len = buf.len();
                }
            }
        }
        row.len() as c_int
    }

    /// Closes the cursor and frees its result set.
    ///
    /// # Safety
    /// `cur` must be a valid pointer from `axiomdb_cursor_open` (or NULL), and
    /// must not be used after this call.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_cursor_close(cur: *mut AxiomCursor) {
        if !cur.is_null() {
            drop(Box::from_raw(cur));
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

    // ── Row result accessors ──────────────────────────────────────────────────

    /// Returns the number of rows in the result set.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_count(rows: *const AxiomRows) -> i64 {
        if rows.is_null() {
            return 0;
        }
        (*rows).cells.len() as i64
    }

    /// Returns the number of columns in the result set.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_columns(rows: *const AxiomRows) -> i32 {
        if rows.is_null() {
            return 0;
        }
        (*rows).col_names.len() as i32
    }

    /// Returns the name of column `col` as a null-terminated UTF-8 string.
    ///
    /// Returns NULL if `col` is out of bounds.
    /// The returned pointer is valid until `axiomdb_rows_free` is called.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_column_name(
        rows: *const AxiomRows,
        col: i32,
    ) -> *const c_char {
        if rows.is_null() || col < 0 {
            return std::ptr::null();
        }
        let r = &*rows;
        match r.col_names.get(col as usize) {
            Some(name) => name.as_ptr(),
            None => std::ptr::null(),
        }
    }

    /// Returns the type code of cell `(row, col)`.
    ///
    /// Type codes: `AXIOMDB_TYPE_NULL=0`, `AXIOMDB_TYPE_INTEGER=1`,
    /// `AXIOMDB_TYPE_REAL=2`, `AXIOMDB_TYPE_TEXT=3`, `AXIOMDB_TYPE_BLOB=4`.
    ///
    /// Returns `AXIOMDB_TYPE_NULL` if the indices are out of bounds.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_type(
        rows: *const AxiomRows,
        row: i64,
        col: i32,
    ) -> c_int {
        cell(rows, row, col)
            .map(|c| c.type_code())
            .unwrap_or(AXIOMDB_TYPE_NULL)
    }

    /// Returns the integer value of cell `(row, col)`.
    ///
    /// Covers: `Bool` (0/1), `Int`, `BigInt`, `Date` (days since epoch),
    /// `Timestamp` (microseconds since epoch).
    ///
    /// Returns 0 for NULL or non-integer cells.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_get_int(
        rows: *const AxiomRows,
        row: i64,
        col: i32,
    ) -> i64 {
        match cell(rows, row, col) {
            Some(CellValue::Integer(v)) => *v,
            _ => 0,
        }
    }

    /// Returns the floating-point value of cell `(row, col)`.
    ///
    /// Covers: `Real`, `Decimal`.
    ///
    /// Returns `0.0` for NULL or non-real cells.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_get_double(
        rows: *const AxiomRows,
        row: i64,
        col: i32,
    ) -> f64 {
        match cell(rows, row, col) {
            Some(CellValue::Real(v)) => *v,
            _ => 0.0,
        }
    }

    /// Returns the text value of cell `(row, col)` as a null-terminated UTF-8
    /// string.
    ///
    /// Covers: `Text`, `UUID` (formatted as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
    ///
    /// Returns NULL for NULL cells, non-text cells, or out-of-bounds indices.
    /// The returned pointer is valid until `axiomdb_rows_free` is called.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_get_text(
        rows: *const AxiomRows,
        row: i64,
        col: i32,
    ) -> *const c_char {
        match cell(rows, row, col) {
            Some(CellValue::Text(s)) => s.as_ptr(),
            _ => std::ptr::null(),
        }
    }

    /// Returns the blob value of cell `(row, col)`.
    ///
    /// Sets `*len` to the number of bytes. Returns NULL for NULL cells,
    /// non-blob cells, or out-of-bounds indices.
    /// The returned pointer is valid until `axiomdb_rows_free` is called.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`.
    /// `len` must be a valid non-null pointer to a `size_t`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_get_blob(
        rows: *const AxiomRows,
        row: i64,
        col: i32,
        len: *mut usize,
    ) -> *const u8 {
        match cell(rows, row, col) {
            Some(CellValue::Blob(b)) => {
                if !len.is_null() {
                    *len = b.len();
                }
                b.as_ptr()
            }
            _ => {
                if !len.is_null() {
                    *len = 0;
                }
                std::ptr::null()
            }
        }
    }

    /// Frees a result set returned by `axiomdb_query`.
    ///
    /// After this call, all pointers returned by `axiomdb_rows_*` accessors
    /// for this result set are invalid and must not be dereferenced.
    ///
    /// # Safety
    /// `rows` must be a valid pointer from `axiomdb_query`, or NULL (no-op).
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_rows_free(rows: *mut AxiomRows) {
        if !rows.is_null() {
            drop(Box::from_raw(rows));
        }
    }

    // ── Error reporting ───────────────────────────────────────────────────────

    /// Returns the last error message for `db` as a null-terminated UTF-8 string.
    ///
    /// Returns NULL if the last operation succeeded.
    /// The returned pointer is valid until the next call to any `axiomdb_*`
    /// function on this handle.
    ///
    /// # Safety
    /// `db` must be a valid pointer from `axiomdb_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_last_error(db: *const Db) -> *const c_char {
        if db.is_null() {
            return std::ptr::null();
        }
        match &(*db).error_msg {
            Some(s) => s.as_ptr(),
            None => std::ptr::null(),
        }
    }

    // ── Appender C FFI (Attack 8) ────────────────────────────────────────────
    //
    // Opaque heap-allocated wrapper around the Rust `Appender<'_>`.
    // The Appender's lifetime is widened to `'static` via transmute —
    // SAFETY: the caller is responsible for keeping the `Db` pointer
    // alive (no `axiomdb_close` calls) until the appender is finished
    // or freed.
    //
    // Routing:
    //   axiomdb_appender_open                → Db::appender(name)
    //   axiomdb_appender_append_<type>       → Appender::append_<type>
    //   axiomdb_appender_end_row             → Appender::end_row
    //   axiomdb_appender_flush               → Appender::flush
    //   axiomdb_appender_finish              → Appender::finish (consumes)
    //   axiomdb_appender_free                → Drop (rollback)
    //
    // Errors set Db.error_msg so `axiomdb_last_error(db)` retrieves them.

    /// Opaque appender handle. Internally owns an `Appender<'static>`
    /// plus a back-pointer to the `Db` for error message routing.
    pub struct AxiomDbAppender {
        // Box<Appender<'static>> — but stored as a raw pointer so we
        // can move out of it in `axiomdb_appender_finish`.
        inner: Option<crate::appender::Appender<'static>>,
        db: *mut Db,
    }

    /// Sets `Db.error_msg` from a `DbError`. Called by every FFI fn
    /// before returning an error code.
    unsafe fn set_db_error(db: *mut Db, e: &axiomdb_core::error::DbError) {
        if db.is_null() {
            return;
        }
        (*db).error_msg = CString::new(e.to_string()).ok();
    }

    /// Opens an Appender for `table_name`. Returns NULL on error;
    /// the error message is in `axiomdb_last_error(db)`.
    ///
    /// The returned pointer must be freed with either
    /// [`axiomdb_appender_finish`] (commits + frees) or
    /// [`axiomdb_appender_free`] (rolls back + frees).
    ///
    /// # Safety
    /// - `db` must be a valid pointer from `axiomdb_open` and remain
    ///   alive until the appender is consumed.
    /// - `table_name` must be a valid non-NULL UTF-8 NUL-terminated
    ///   string.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_open(
        db: *mut Db,
        table_name: *const c_char,
    ) -> *mut AxiomDbAppender {
        if db.is_null() || table_name.is_null() {
            return std::ptr::null_mut();
        }
        let name = match CStr::from_ptr(table_name).to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let db_ref: &mut Db = &mut *db;
        match db_ref.appender(name) {
            Ok(app) => {
                // SAFETY: widen the lifetime to 'static. The caller
                // guarantees `db` outlives the appender.
                let app_static: crate::appender::Appender<'static> = std::mem::transmute(app);
                Box::into_raw(Box::new(AxiomDbAppender {
                    inner: Some(app_static),
                    db,
                }))
            }
            Err(e) => {
                set_db_error(db, &e);
                std::ptr::null_mut()
            }
        }
    }

    /// Macro: defines a typed appender FFI fn that delegates to a
    /// method on the inner Rust `Appender`. Returns 0 on success,
    /// -1 on error (with `Db.error_msg` set).
    ///
    /// # Safety
    /// All generated functions share the same safety contract: `app`
    /// must be a valid pointer from `axiomdb_appender_open` or NULL.
    /// The pointer must not be used after `axiomdb_appender_finish`
    /// or `axiomdb_appender_free`.
    macro_rules! ffi_append_typed {
        ($fn_name:ident, $rust_name:ident, $($arg:ident: $ty:ty),*) => {
            /// FFI typed appender setter — see the `ffi_append_typed!`
            /// macro for shared safety contract.
            ///
            /// # Safety
            /// `app` must be a valid pointer from `axiomdb_appender_open`,
            /// or NULL (returns -1). Must not be used after
            /// `axiomdb_appender_finish` / `axiomdb_appender_free`.
            #[no_mangle]
            pub unsafe extern "C" fn $fn_name(
                app: *mut AxiomDbAppender,
                $($arg: $ty,)*
            ) -> c_int {
                if app.is_null() {
                    return -1;
                }
                let wrapper = &mut *app;
                let Some(ref mut inner) = wrapper.inner else {
                    return -1;
                };
                match inner.$rust_name($($arg),*) {
                    Ok(()) => 0,
                    Err(e) => {
                        set_db_error(wrapper.db, &e);
                        -1
                    }
                }
            }
        };
    }

    ffi_append_typed!(axiomdb_appender_append_int, append_int, v: i32);
    ffi_append_typed!(axiomdb_appender_append_bigint, append_bigint, v: i64);
    ffi_append_typed!(axiomdb_appender_append_real, append_real, v: f64);

    /// Appends a BOOL value. `v == 0` is false; any non-zero is true.
    ///
    /// # Safety
    /// `app` must be a valid pointer from `axiomdb_appender_open`,
    /// or NULL (returns -1).
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_append_bool(
        app: *mut AxiomDbAppender,
        v: c_int,
    ) -> c_int {
        if app.is_null() {
            return -1;
        }
        let wrapper = &mut *app;
        let Some(ref mut inner) = wrapper.inner else {
            return -1;
        };
        match inner.append_bool(v != 0) {
            Ok(()) => 0,
            Err(e) => {
                set_db_error(wrapper.db, &e);
                -1
            }
        }
    }

    /// Appends a TEXT value from a NUL-terminated UTF-8 cstring.
    ///
    /// # Safety
    /// `app` must be a valid pointer from `axiomdb_appender_open`,
    /// or NULL (returns -1). `v` must be a NUL-terminated UTF-8
    /// string, or NULL (returns -1).
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_append_text(
        app: *mut AxiomDbAppender,
        v: *const c_char,
    ) -> c_int {
        if app.is_null() || v.is_null() {
            return -1;
        }
        let wrapper = &mut *app;
        let s = match CStr::from_ptr(v).to_str() {
            Ok(s) => s,
            Err(_) => {
                let e = axiomdb_core::error::DbError::InvalidValue {
                    reason: "axiomdb_appender_append_text: invalid UTF-8".into(),
                };
                set_db_error(wrapper.db, &e);
                return -1;
            }
        };
        let Some(ref mut inner) = wrapper.inner else {
            return -1;
        };
        match inner.append_text(s) {
            Ok(()) => 0,
            Err(e) => {
                set_db_error(wrapper.db, &e);
                -1
            }
        }
    }

    /// Appends a BYTES value from a (data, len) pair. `data` may be
    /// NULL only when `len == 0`.
    ///
    /// # Safety
    /// `app` must be a valid pointer from `axiomdb_appender_open`,
    /// or NULL (returns -1). `data` must point to at least `len`
    /// readable bytes, or be NULL when `len == 0`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_append_bytes(
        app: *mut AxiomDbAppender,
        data: *const u8,
        len: usize,
    ) -> c_int {
        if app.is_null() {
            return -1;
        }
        let wrapper = &mut *app;
        if data.is_null() && len > 0 {
            let e = axiomdb_core::error::DbError::InvalidValue {
                reason: "axiomdb_appender_append_bytes: NULL data with len > 0".into(),
            };
            set_db_error(wrapper.db, &e);
            return -1;
        }
        let slice = if len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data, len)
        };
        let Some(ref mut inner) = wrapper.inner else {
            return -1;
        };
        match inner.append_bytes(slice) {
            Ok(()) => 0,
            Err(e) => {
                set_db_error(wrapper.db, &e);
                -1
            }
        }
    }

    ffi_append_typed!(axiomdb_appender_append_null, append_null,);
    ffi_append_typed!(axiomdb_appender_end_row, end_row,);
    ffi_append_typed!(axiomdb_appender_flush, flush,);

    /// Flushes remaining rows, commits the transaction, and frees the
    /// appender. Returns the total rows-inserted count, or -1 on
    /// error. The appender pointer is invalid after this call.
    ///
    /// # Safety
    /// `app` must be a valid pointer from `axiomdb_appender_open`.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_finish(app: *mut AxiomDbAppender) -> i64 {
        if app.is_null() {
            return -1;
        }
        let mut wrapper = Box::from_raw(app);
        let inner = match wrapper.inner.take() {
            Some(a) => a,
            None => return -1,
        };
        match inner.finish() {
            Ok(n) => n as i64,
            Err(e) => {
                set_db_error(wrapper.db, &e);
                -1
            }
        }
    }

    /// Rolls back the appender's transaction and frees the appender.
    /// The pointer is invalid after this call. NULL is a no-op.
    ///
    /// # Safety
    /// `app` must be a valid pointer from `axiomdb_appender_open`, or NULL.
    #[no_mangle]
    pub unsafe extern "C" fn axiomdb_appender_free(app: *mut AxiomDbAppender) {
        if !app.is_null() {
            // Drop the Box → Drops the Appender → Drop impl rolls back.
            drop(Box::from_raw(app));
        }
    }

    // ── Internal helper ───────────────────────────────────────────────────────

    /// Returns a reference to cell `(row, col)`, or `None` if out of bounds.
    ///
    /// # Safety
    /// `rows` must be a valid pointer or null.
    unsafe fn cell(rows: *const AxiomRows, row: i64, col: i32) -> Option<&'static CellValue> {
        if rows.is_null() || row < 0 || col < 0 {
            return None;
        }
        let r = &*rows;
        r.cells
            .get(row as usize)
            .and_then(|row| row.get(col as usize))
    }

    // ── Packed buffer tests ───────────────────────────────────────────────────

    #[cfg(test)]
    mod packed_tests {
        use super::*;
        use axiomdb_sql::result::ColumnMeta;
        use axiomdb_types::DataType;

        fn col(name: &str) -> ColumnMeta {
            ColumnMeta {
                name: name.to_string(),
                data_type: DataType::Int,
                nullable: true,
                table_name: None,
            }
        }

        /// Minimal reader mirroring the Python parser — validates the layout.
        fn read_u32(buf: &[u8], off: &mut usize) -> u32 {
            let v = u32::from_le_bytes(buf[*off..*off + 4].try_into().unwrap());
            *off += 4;
            v
        }
        fn read_u64(buf: &[u8], off: &mut usize) -> u64 {
            let v = u64::from_le_bytes(buf[*off..*off + 8].try_into().unwrap());
            *off += 8;
            v
        }

        #[test]
        fn pack_header_and_names() {
            let cols = vec![col("id"), col("name")];
            let buf = pack_rows(&cols, Vec::new());
            let mut off = 0;
            assert_eq!(read_u32(&buf, &mut off), PACKED_MAGIC);
            assert_eq!(read_u32(&buf, &mut off), 2); // n_cols
            assert_eq!(read_u64(&buf, &mut off), 0); // n_rows
                                                     // col 0 name
            let l0 = read_u32(&buf, &mut off) as usize;
            assert_eq!(&buf[off..off + l0], b"id");
            off += l0;
            let l1 = read_u32(&buf, &mut off) as usize;
            assert_eq!(&buf[off..off + l1], b"name");
        }

        #[test]
        fn pack_all_cell_types_roundtrip() {
            let cols = vec![col("a"), col("b"), col("c"), col("d"), col("e")];
            let rows = vec![vec![
                Value::Int(42),
                Value::Real(3.5),
                Value::Text("héllo".to_string()),
                Value::Bytes(vec![1, 2, 3]),
                Value::Null,
            ]];
            let buf = pack_rows(&cols, rows);
            let mut off = 4 + 4 + 8; // skip magic + n_cols + n_rows
                                     // skip 5 column names (each: u32 len + bytes, all 1 char)
            for _ in 0..5 {
                let l = read_u32(&buf, &mut off) as usize;
                off += l;
            }
            // cell a: INT 42
            assert_eq!(buf[off], 1);
            off += 1;
            assert_eq!(read_u64(&buf, &mut off) as i64, 42);
            // cell b: REAL 3.5
            assert_eq!(buf[off], 2);
            off += 1;
            let f = f64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            off += 8;
            assert_eq!(f, 3.5);
            // cell c: TEXT "héllo"
            assert_eq!(buf[off], 3);
            off += 1;
            let tl = read_u32(&buf, &mut off) as usize;
            assert_eq!(&buf[off..off + tl], "héllo".as_bytes());
            off += tl;
            // cell d: BLOB [1,2,3]
            assert_eq!(buf[off], 4);
            off += 1;
            let bl = read_u32(&buf, &mut off) as usize;
            assert_eq!(&buf[off..off + bl], &[1, 2, 3]);
            off += bl;
            // cell e: NULL
            assert_eq!(buf[off], 0);
        }

        #[test]
        fn pack_empty_string_and_blob() {
            let cols = vec![col("a"), col("b")];
            let rows = vec![vec![Value::Text(String::new()), Value::Bytes(Vec::new())]];
            let buf = pack_rows(&cols, rows);
            let mut off = 16;
            for _ in 0..2 {
                let l = read_u32(&buf, &mut off) as usize;
                off += l;
            }
            assert_eq!(buf[off], 3); // TEXT
            off += 1;
            assert_eq!(read_u32(&buf, &mut off), 0); // empty len
            assert_eq!(buf[off], 4); // BLOB
            off += 1;
            assert_eq!(read_u32(&buf, &mut off), 0); // empty len
        }

        // ── Columnar (AXM2) ───────────────────────────────────────────────────

        #[test]
        fn column_kind_classification() {
            let rows = vec![
                vec![Value::Int(1), Value::Text("a".into()), Value::Null],
                vec![Value::Int(2), Value::Text("b".into()), Value::Int(9)],
            ];
            assert_eq!(column_kind(&rows, 0), b'I'); // all int
            assert_eq!(column_kind(&rows, 1), b'T'); // all text
            assert_eq!(column_kind(&rows, 2), b'M'); // NULL + int → mixed
        }

        #[test]
        fn pack_columnar_layout() {
            let cols = vec![col("id"), col("name")];
            let rows = vec![
                vec![Value::Int(10), Value::Text("x".into())],
                vec![Value::Int(20), Value::Text("yz".into())],
            ];
            let buf = pack_columnar(&cols, &rows);
            let mut off = 0;
            assert_eq!(read_u32(&buf, &mut off), COLUMNAR_MAGIC);
            assert_eq!(read_u32(&buf, &mut off), 2); // n_cols
            assert_eq!(read_u64(&buf, &mut off), 2); // n_rows
                                                     // names
            for name in ["id", "name"] {
                let l = read_u32(&buf, &mut off) as usize;
                assert_eq!(&buf[off..off + l], name.as_bytes());
                off += l;
            }
            // column 0: 'I' then 2 × i64
            assert_eq!(buf[off], b'I');
            off += 1;
            assert_eq!(read_u64(&buf, &mut off) as i64, 10);
            assert_eq!(read_u64(&buf, &mut off) as i64, 20);
            // column 1: 'T' then 2 lengths then bytes "x","yz"
            assert_eq!(buf[off], b'T');
            off += 1;
            assert_eq!(read_u32(&buf, &mut off), 1); // len "x"
            assert_eq!(read_u32(&buf, &mut off), 2); // len "yz"
            assert_eq!(&buf[off..off + 1], b"x");
            off += 1;
            assert_eq!(&buf[off..off + 2], b"yz");
        }

        #[test]
        fn pack_columnar_null_column_falls_back_to_mixed() {
            let cols = vec![col("m")];
            let rows = vec![vec![Value::Int(1)], vec![Value::Null]];
            let buf = pack_columnar(&cols, &rows);
            let mut off = 16; // skip header
            let l = read_u32(&buf, &mut off) as usize; // name
            off += l;
            assert_eq!(buf[off], b'M'); // NULL present → mixed per-cell
        }
    }

    // ── Cursor API tests ──────────────────────────────────────────────────────

    #[cfg(test)]
    mod cursor_tests {
        use super::*;

        unsafe fn open_mem_db() -> *mut Db {
            Box::into_raw(Box::new(Db::open_memory().unwrap()))
        }
        unsafe fn exec(db: *mut Db, sql: &str) {
            let c = CString::new(sql).unwrap();
            assert!(axiomdb_execute(db, c.as_ptr()) >= 0, "exec failed: {sql}");
        }

        #[test]
        fn cursor_basic_roundtrip() {
            unsafe {
                let db = open_mem_db();
                exec(db, "CREATE TABLE t (id INT, name TEXT, score REAL)");
                exec(db, "INSERT INTO t VALUES (1, 'alice', 3.5)");
                exec(db, "INSERT INTO t VALUES (2, 'béta', 2.0)");
                let sql = CString::new("SELECT id, name, score FROM t ORDER BY id").unwrap();
                let cur = axiomdb_cursor_open(db, sql.as_ptr());
                assert!(!cur.is_null());
                assert_eq!(axiomdb_cursor_columns(cur), 3);

                // row 1
                assert_eq!(axiomdb_cursor_step(cur), 1);
                assert_eq!(axiomdb_cursor_int(cur, 0), 1);
                let mut len = 0usize;
                let p = axiomdb_cursor_text(cur, 1, &mut len);
                assert_eq!(std::slice::from_raw_parts(p as *const u8, len), b"alice");
                assert_eq!(axiomdb_cursor_double(cur, 2), 3.5);

                // row 2 — non-ASCII text zero-copy
                assert_eq!(axiomdb_cursor_step(cur), 1);
                assert_eq!(axiomdb_cursor_int(cur, 0), 2);
                let p2 = axiomdb_cursor_text(cur, 1, &mut len);
                let s2 = std::slice::from_raw_parts(p2 as *const u8, len);
                assert_eq!(std::str::from_utf8(s2).unwrap(), "béta");

                // end is idempotent
                assert_eq!(axiomdb_cursor_step(cur), 0);
                assert_eq!(axiomdb_cursor_step(cur), 0);

                axiomdb_cursor_close(cur);
                axiomdb_close(db);
            }
        }

        #[test]
        fn cursor_null_blob_and_empty() {
            unsafe {
                let db = open_mem_db();
                exec(db, "CREATE TABLE t (id INT, maybe INT, b BLOB)");
                exec(db, "INSERT INTO t VALUES (1, NULL, NULL)");
                let sql = CString::new("SELECT id, maybe FROM t").unwrap();
                let cur = axiomdb_cursor_open(db, sql.as_ptr());
                assert_eq!(axiomdb_cursor_step(cur), 1);
                assert_eq!(axiomdb_cursor_type(cur, 1), AXIOMDB_TYPE_NULL);
                assert_eq!(axiomdb_cursor_int(cur, 1), 0);
                axiomdb_cursor_close(cur);

                // empty result → first step is 0
                let sql2 = CString::new("SELECT id FROM t WHERE id = 999").unwrap();
                let cur2 = axiomdb_cursor_open(db, sql2.as_ptr());
                assert_eq!(axiomdb_cursor_step(cur2), 0);
                axiomdb_cursor_close(cur2);
                axiomdb_close(db);
            }
        }

        #[test]
        fn cursor_matches_percell_values() {
            // The cursor must return the same data as the legacy per-cell path.
            unsafe {
                let db = open_mem_db();
                exec(db, "CREATE TABLE t (id INT, name TEXT)");
                for i in 0..50 {
                    exec(db, &format!("INSERT INTO t VALUES ({i}, 'u{i}')"));
                }
                let q = CString::new("SELECT id, name FROM t ORDER BY id").unwrap();

                let cur = axiomdb_cursor_open(db, q.as_ptr());
                let mut got = Vec::new();
                while axiomdb_cursor_step(cur) == 1 {
                    let id = axiomdb_cursor_int(cur, 0);
                    let mut len = 0usize;
                    let p = axiomdb_cursor_text(cur, 1, &mut len);
                    let name = std::str::from_utf8(std::slice::from_raw_parts(p as *const u8, len))
                        .unwrap()
                        .to_string();
                    got.push((id, name));
                }
                axiomdb_cursor_close(cur);

                assert_eq!(got.len(), 50);
                for (i, (id, name)) in got.iter().enumerate() {
                    assert_eq!(*id, i as i64);
                    assert_eq!(name, &format!("u{i}"));
                }
                axiomdb_close(db);
            }
        }

        #[test]
        fn cursor_row_bulk_matches_scalar() {
            unsafe {
                let db = open_mem_db();
                exec(db, "CREATE TABLE t (id INT, name TEXT, score REAL)");
                exec(db, "INSERT INTO t VALUES (1, 'alice', 3.5)");
                exec(db, "INSERT INTO t VALUES (2, 'béta', 2.0)");
                let sql = CString::new("SELECT id, name, score FROM t ORDER BY id").unwrap();
                let cur = axiomdb_cursor_open(db, sql.as_ptr());
                let ncols = axiomdb_cursor_columns(cur) as usize;
                let mut cells: Vec<AxiomCell> = (0..ncols)
                    .map(|_| AxiomCell {
                        type_code: 0,
                        int_val: 0,
                        real_val: 0.0,
                        ptr: std::ptr::null(),
                        len: 0,
                    })
                    .collect();

                // row 1 — bulk fills every cell in one call
                assert_eq!(axiomdb_cursor_step(cur), 1);
                assert_eq!(axiomdb_cursor_row(cur, cells.as_mut_ptr()) as usize, 3);
                assert_eq!(cells[0].type_code, AXIOMDB_TYPE_INTEGER);
                assert_eq!(cells[0].int_val, 1);
                assert_eq!(cells[1].type_code, AXIOMDB_TYPE_TEXT);
                assert_eq!(
                    std::slice::from_raw_parts(cells[1].ptr, cells[1].len),
                    b"alice"
                );
                assert_eq!(cells[2].type_code, AXIOMDB_TYPE_REAL);
                assert_eq!(cells[2].real_val, 3.5);

                // row 2 — non-ASCII zero-copy text
                assert_eq!(axiomdb_cursor_step(cur), 1);
                axiomdb_cursor_row(cur, cells.as_mut_ptr());
                assert_eq!(cells[0].int_val, 2);
                let n2 = std::slice::from_raw_parts(cells[1].ptr, cells[1].len);
                assert_eq!(std::str::from_utf8(n2).unwrap(), "béta");

                // past end → 0
                assert_eq!(axiomdb_cursor_step(cur), 0);
                assert_eq!(axiomdb_cursor_row(cur, cells.as_mut_ptr()), 0);

                axiomdb_cursor_close(cur);
                axiomdb_close(db);
            }
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
    //!     db.execute("CREATE TABLE t (id INT NOT NULL)").await.unwrap();
    //!
    //!     let (columns, rows) = db.query_with_columns("SELECT * FROM t").await.unwrap();
    //!     println!("columns: {:?}", columns);
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
                .map_err(|e| DbError::Io(std::io::Error::other(e.to_string())))??;
            Ok(Self {
                inner: Arc::new(Mutex::new(db)),
            })
        }

        /// Opens or creates a database from a local DSN.
        pub async fn open_dsn(dsn: impl Into<String>) -> Result<Self, DbError> {
            let dsn = dsn.into();
            let db = tokio::task::spawn_blocking(move || Db::open_dsn(&dsn))
                .await
                .map_err(|e| DbError::Io(std::io::Error::other(e.to_string())))??;
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
                .map_err(|e| DbError::Io(std::io::Error::other(e.to_string())))?
        }

        /// Executes a SQL SELECT. Returns rows.
        pub async fn query(&self, sql: impl Into<String>) -> Result<Vec<Row>, DbError> {
            let sql = sql.into();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || inner.lock().unwrap().query(&sql))
                .await
                .map_err(|e| DbError::Io(std::io::Error::other(e.to_string())))?
        }

        /// Executes a SQL SELECT. Returns column names and rows.
        pub async fn query_with_columns(
            &self,
            sql: impl Into<String>,
        ) -> Result<(Vec<String>, Vec<Row>), DbError> {
            let sql = sql.into();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || inner.lock().unwrap().query_with_columns(&sql))
                .await
                .map_err(|e| DbError::Io(std::io::Error::other(e.to_string())))?
        }
    }
}
