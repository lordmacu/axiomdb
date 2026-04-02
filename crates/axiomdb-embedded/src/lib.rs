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
pub use db::Db;

#[cfg(feature = "sync-api")]
pub use db::Row;

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
        execute_with_ctx,
        expr::Expr,
        parse,
        result::QueryResult,
        verify_and_repair_indexes_on_open, SchemaCache, SessionContext,
    };
    use axiomdb_storage::MmapStorage;
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
        storage: MmapStorage,
        txn: TxnManager,
        bloom: BloomRegistry,
        schema_cache: SchemaCache,
        session: SessionContext,
        /// Set to `true` after the first `DiskFull` error. When degraded,
        /// all mutating operations are rejected immediately without touching
        /// WAL or storage again.
        degraded: bool,
        /// Last error message. Cleared on success, set on any error.
        /// Exposed via `last_error()` (Rust) and `axiomdb_last_error()` (C FFI).
        pub(crate) error_msg: Option<CString>,
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
                let mut storage = MmapStorage::open(&db_path)?;
                let (mut txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
                verify_and_repair_indexes_on_open(&mut storage, &mut txn)?;
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
                error_msg: None,
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
            let stmt = parse(sql, None)?;
            let snap = self
                .txn
                .active_snapshot()
                .unwrap_or_else(|_| self.txn.snapshot());
            let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
            execute_with_ctx(
                analyzed,
                &mut self.storage,
                &mut self.txn,
                &mut self.bloom,
                &mut self.session,
            )
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
        /// Prepares a SQL statement for repeated execution.
        ///
        /// The SQL may contain `?` parameter placeholders. The returned
        /// [`PreparedStatement`] can be executed multiple times with different
        /// parameter values, skipping parse + analyze on each call.
        pub fn prepare(&mut self, sql: &str) -> Result<PreparedStatement, DbError> {
            // Parse with parameter support.
            let stmt = parse(sql, None)?;

            // Count Param nodes in the AST.
            let param_count = count_params(&stmt);

            // Analyze — resolves column indices, type checks.
            let snap = self
                .txn
                .active_snapshot()
                .unwrap_or_else(|_| self.txn.snapshot());
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
            let stmt = substitute_params(self.analyzed.clone(), params)?;

            // Execute directly — no parse, no analyze.
            execute_with_ctx(
                stmt,
                &mut db.storage,
                &mut db.txn,
                &mut db.bloom,
                &mut db.session,
            )
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
            Value::Text(s) => {
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
