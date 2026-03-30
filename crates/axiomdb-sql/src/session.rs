//! Session context — per-connection state including the schema cache and warnings.

use std::collections::{HashMap, HashSet};

use axiomdb_catalog::{
    schema::{ColumnDef, TableDef, DEFAULT_DATABASE_NAME},
    IndexDef, ResolvedTable,
};
use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

// ── CompatMode ────────────────────────────────────────────────────────────────

/// High-level compatibility mode for the session.
///
/// Controls the **default** session collation and other behavioral defaults.
/// Set via `SET AXIOM_COMPAT = 'standard' | 'mysql' | 'postgresql' | DEFAULT`.
/// Inspected via `SELECT @@axiom_compat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatMode {
    /// **Default.** Standard AxiomDB behavior — exact binary text semantics.
    #[default]
    Standard,
    /// MySQL-compatible behavior — default collation becomes `es` (CI+AI fold).
    MySql,
    /// PostgreSQL-compatible behavior — exact binary text semantics (same as standard).
    PostgreSql,
}

/// Parses a `SET AXIOM_COMPAT = ...` value.
pub fn parse_compat_mode_setting(raw: &str) -> Result<CompatMode, DbError> {
    let s = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_lowercase();
    match s.as_str() {
        "standard" | "default" => Ok(CompatMode::Standard),
        "mysql" => Ok(CompatMode::MySql),
        "postgresql" | "postgres" => Ok(CompatMode::PostgreSql),
        _ => Err(DbError::InvalidValue {
            reason: format!(
                "invalid axiom_compat value '{raw}'; expected standard | mysql | postgresql"
            ),
        }),
    }
}

/// Returns the canonical lowercase name of a [`CompatMode`] for `@@axiom_compat`.
pub fn compat_mode_name(mode: CompatMode) -> &'static str {
    match mode {
        CompatMode::Standard => "standard",
        CompatMode::MySql => "mysql",
        CompatMode::PostgreSql => "postgresql",
    }
}

impl std::fmt::Display for CompatMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(compat_mode_name(*self))
    }
}

// ── SessionCollation ──────────────────────────────────────────────────────────

/// Executor-visible text-comparison behavior for the session.
///
/// Set via `SET collation = 'binary' | 'es' | DEFAULT`.
/// Inspected via `SELECT @@collation`.
///
/// This is **distinct** from `@@collation_connection` (transport charset from 5.2a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionCollation {
    /// **Default.** Exact byte-order comparison — current AxiomDB behavior.
    #[default]
    Binary,
    /// CI+AI fold: NFC + lowercase + strip combining marks.
    /// `Jose`, `JOSE`, `José` compare equal.
    Es,
}

/// Parses a `SET collation = ...` value.
///
/// Returns `Ok(None)` for `DEFAULT` (resets to compat-derived default).
pub fn parse_session_collation_setting(raw: &str) -> Result<Option<SessionCollation>, DbError> {
    let s = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_lowercase();
    match s.as_str() {
        "binary" => Ok(Some(SessionCollation::Binary)),
        "es" => Ok(Some(SessionCollation::Es)),
        "default" => Ok(None),
        _ => Err(DbError::InvalidValue {
            reason: format!("invalid collation value '{raw}'; expected binary | es | DEFAULT"),
        }),
    }
}

/// Returns the canonical lowercase name of a [`SessionCollation`] for `@@collation`.
pub fn session_collation_name(c: SessionCollation) -> &'static str {
    match c {
        SessionCollation::Binary => "binary",
        SessionCollation::Es => "es",
    }
}

// ── OnErrorMode ───────────────────────────────────────────────────────────────

/// Per-session policy that controls how a statement error affects the current
/// transaction and whether certain SQL errors are converted to warnings.
///
/// Set via `SET on_error = 'rollback_statement' | 'rollback_transaction' |
/// 'savepoint' | 'ignore'`. Inspected via `SELECT @@on_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnErrorMode {
    /// **Default.** On statement error inside an active transaction, roll back
    /// only that statement's writes and keep the transaction open. In
    /// autocommit mode, the implicit single-statement transaction is rolled back.
    #[default]
    RollbackStatement,
    /// On statement error inside an active transaction, roll back the entire
    /// transaction eagerly. `@@in_transaction` becomes 0 after the error.
    RollbackTransaction,
    /// Like `RollbackStatement` when a transaction is already active.
    /// When `autocommit = 0`, also preserves the implicit transaction after a
    /// failing first DML — the key difference from `RollbackStatement`.
    Savepoint,
    /// Convert ignorable SQL/user errors into session warnings and return
    /// success (OK packet with `warning_count > 0`). Non-ignorable errors
    /// (I/O, WAL, corruption) still surface as ERR.
    Ignore,
}

/// Parses a `SET on_error = ...` value into an [`OnErrorMode`].
///
/// Accepts quoted strings and bare identifiers in any case.
/// `DEFAULT` resets to [`OnErrorMode::RollbackStatement`].
pub fn parse_on_error_setting(raw: &str) -> Result<OnErrorMode, DbError> {
    let s = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_lowercase();
    match s.as_str() {
        "rollback_statement" | "default" => Ok(OnErrorMode::RollbackStatement),
        "rollback_transaction" => Ok(OnErrorMode::RollbackTransaction),
        "savepoint" => Ok(OnErrorMode::Savepoint),
        "ignore" => Ok(OnErrorMode::Ignore),
        _ => Err(DbError::InvalidValue {
            reason: format!(
                "invalid on_error value '{raw}'; expected \
                 rollback_statement | rollback_transaction | savepoint | ignore"
            ),
        }),
    }
}

/// Returns the canonical lowercase name of an [`OnErrorMode`] for `@@on_error`.
pub fn on_error_mode_name(mode: OnErrorMode) -> &'static str {
    match mode {
        OnErrorMode::RollbackStatement => "rollback_statement",
        OnErrorMode::RollbackTransaction => "rollback_transaction",
        OnErrorMode::Savepoint => "savepoint",
        OnErrorMode::Ignore => "ignore",
    }
}

/// Returns `true` if `err` is a SQL/user-facing error that `on_error = 'ignore'`
/// is allowed to suppress as a warning.
///
/// Non-ignorable errors (I/O, WAL, storage corruption, internal errors) are
/// **always** returned as ERR even when `on_error = 'ignore'`.
///
/// This match is intentionally exhaustive so that new `DbError` variants force
/// a conscious decision about their ignorability.
pub fn is_ignorable_on_error(err: &DbError) -> bool {
    match err {
        // ── SQL / user-facing ─────────────────────────────────────────────────
        DbError::ParseError { .. }
        | DbError::TableNotFound { .. }
        | DbError::DatabaseNotFound { .. }
        | DbError::ColumnNotFound { .. }
        | DbError::AmbiguousColumn { .. }
        | DbError::UniqueViolation { .. }
        | DbError::DuplicateKey
        | DbError::ForeignKeyViolation { .. }
        | DbError::ForeignKeyParentViolation { .. }
        | DbError::ForeignKeyCascadeDepth { .. }
        | DbError::ForeignKeySetNullNotNullable { .. }
        | DbError::ForeignKeyNoParentIndex { .. }
        | DbError::NotNullViolation { .. }
        | DbError::CheckViolation { .. }
        | DbError::TypeMismatch { .. }
        | DbError::InvalidValue { .. }
        | DbError::InvalidCoercion { .. }
        | DbError::DivisionByZero
        | DbError::Overflow
        | DbError::ValueTooLarge { .. }
        | DbError::NoActiveTransaction
        | DbError::TransactionAlreadyActive { .. }
        | DbError::CardinalityViolation { .. }
        | DbError::ColumnAlreadyExists { .. }
        | DbError::TableAlreadyExists { .. }
        | DbError::DatabaseAlreadyExists { .. }
        | DbError::SchemaAlreadyExists { .. }
        | DbError::SchemaNotFound { .. }
        | DbError::IndexAlreadyExists { .. }
        | DbError::IndexKeyTooLong { .. }
        | DbError::ActiveDatabaseDrop { .. }
        | DbError::NotImplemented { .. } => true,

        // ── Infrastructure / runtime — never ignorable ────────────────────────
        DbError::PageNotFound { .. }
        | DbError::ChecksumMismatch { .. }
        | DbError::StorageFull
        | DbError::DiskFull { .. }
        | DbError::Io(_)
        | DbError::FileLocked { .. }
        | DbError::WalGroupCommitFailed { .. }
        | DbError::WalChecksumMismatch { .. }
        | DbError::WalEntryTruncated { .. }
        | DbError::WalUnknownEntryType { .. }
        | DbError::WalInvalidHeader { .. }
        | DbError::DeadlockDetected
        | DbError::TransactionExpired { .. }
        | DbError::PermissionDenied { .. }
        | DbError::HeapPageFull { .. }
        | DbError::InvalidSlot { .. }
        | DbError::AlreadyDeleted { .. }
        | DbError::KeyTooLong { .. }
        | DbError::BTreeCorrupted { .. }
        | DbError::CatalogNotInitialized
        | DbError::ColumnIndexOutOfBounds { .. }
        | DbError::CatalogTableNotFound { .. }
        | DbError::CatalogIndexNotFound { .. }
        | DbError::IndexIntegrityFailure { .. }
        | DbError::SequenceOverflow
        | DbError::InvalidDsn { .. }
        | DbError::LockTimeout
        | DbError::Internal { .. }
        | DbError::Other(_) => false,
    }
}

// ── SqlWarning ────────────────────────────────────────────────────────────────

/// A single SQL warning, surfaced via `SHOW WARNINGS`.
///
/// Warnings are accumulated during a statement and cleared before the next one.
/// The warning_count field in the OK packet tells the client how many to fetch.
#[derive(Debug, Clone)]
pub struct SqlWarning {
    /// Severity level shown in `SHOW WARNINGS` Level column.
    pub level: &'static str, // "Note" | "Warning" | "Error"
    /// MySQL warning code (e.g. 1592 for "no active transaction").
    pub code: u16,
    /// Human-readable message shown in `SHOW WARNINGS` Message column.
    pub message: String,
}

// ── StaleStatsTracker ─────────────────────────────────────────────────────────

/// Tracks per-table row changes since the last stats load or ANALYZE (Phase 6.11).
///
/// When accumulated changes exceed 20% of the baseline row count, the table's
/// stats are considered stale. The query planner falls back to
/// `DEFAULT_NUM_DISTINCT = 200` for stale tables so it doesn't make expensive
/// index scan decisions based on outdated selectivity estimates.
///
/// This is **in-memory only** — resets on server restart. Persistent stale
/// tracking (like PostgreSQL's `pg_stat_user_tables.n_mod_since_analyze`) is
/// deferred to Phase 6.15.
#[derive(Debug, Default)]
pub struct StaleStatsTracker {
    /// Accumulated row changes per table since the last `set_baseline` call.
    changes: HashMap<u32, u64>,
    /// Row count at the last stats load (from `StatsDef.row_count`).
    baseline: HashMap<u32, u64>,
    /// Tables currently considered stale (changes > 20% of baseline).
    stale: HashSet<u32>,
}

impl StaleStatsTracker {
    /// Records one row INSERT or DELETE for `table_id`.
    /// Marks the table stale if accumulated changes exceed 20% of baseline.
    pub fn on_row_changed(&mut self, table_id: u32) {
        *self.changes.entry(table_id).or_insert(0) += 1;
        self.check_stale(table_id);
    }

    /// Records multiple row changes at once (e.g. after batch DELETE).
    pub fn on_rows_changed(&mut self, table_id: u32, count: u64) {
        *self.changes.entry(table_id).or_insert(0) += count;
        self.check_stale(table_id);
    }

    /// Sets the baseline row count from loaded `StatsDef.row_count`.
    /// Called by the planner on first stats use for a table in this session.
    pub fn set_baseline(&mut self, table_id: u32, row_count: u64) {
        self.baseline.insert(table_id, row_count);
        self.check_stale(table_id);
    }

    /// Clears staleness for `table_id`. Called after a successful ANALYZE.
    pub fn mark_fresh(&mut self, table_id: u32) {
        self.stale.remove(&table_id);
        self.changes.remove(&table_id);
    }

    /// Returns `true` if the stats for `table_id` are considered stale.
    pub fn is_stale(&self, table_id: u32) -> bool {
        self.stale.contains(&table_id)
    }

    fn check_stale(&mut self, table_id: u32) {
        let changes = self.changes.get(&table_id).copied().unwrap_or(0);
        let baseline = self.baseline.get(&table_id).copied().unwrap_or(0);
        // Threshold: > 20% change = > baseline / 5
        if baseline > 0 && changes > baseline / 5 {
            self.stale.insert(table_id);
        }
    }
}

// ── Strict-mode helpers ───────────────────────────────────────────────────────

/// Parses a boolish setting value (`ON`/`OFF`/`1`/`0`/`TRUE`/`FALSE`).
///
/// Used by `SET strict_mode = ...` in both the executor and the wire layer so
/// both code paths accept the same set of literals.
pub fn parse_boolish_setting(raw: &str) -> Result<bool, DbError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Ok(true),
        "0" | "off" | "false" => Ok(false),
        other => Err(DbError::InvalidValue {
            reason: format!("expected ON/OFF/1/0/TRUE/FALSE, got '{other}'"),
        }),
    }
}

/// Normalises a raw `sql_mode` string.
///
/// - Trims outer quotes.
/// - Splits on `,`, trims and uppercases each token.
/// - Removes empty tokens and duplicates (first occurrence wins).
/// - Rejoins with `,`.
pub fn normalize_sql_mode(raw: &str) -> String {
    let stripped = raw.trim().trim_matches('\'').trim_matches('"');
    let mut seen = std::collections::HashSet::new();
    let mut tokens: Vec<String> = Vec::new();
    for part in stripped.split(',') {
        let token = part.trim().to_ascii_uppercase();
        if !token.is_empty() && seen.insert(token.clone()) {
            tokens.push(token);
        }
    }
    tokens.join(",")
}

/// Returns `true` when `normalized` contains `STRICT_TRANS_TABLES` or
/// `STRICT_ALL_TABLES` (i.e. strict DML assignment is enabled).
pub fn sql_mode_is_strict(normalized: &str) -> bool {
    normalized
        .split(',')
        .any(|t| t.trim() == "STRICT_TRANS_TABLES" || t.trim() == "STRICT_ALL_TABLES")
}

/// Returns a new `sql_mode` string with the strict tokens added or removed.
///
/// All non-strict tokens from `current` are preserved. When `enabled` is
/// `true`, `STRICT_TRANS_TABLES` is prepended. The result is always normalised.
pub fn apply_strict_to_sql_mode(current: &str, enabled: bool) -> String {
    let normalized = normalize_sql_mode(current);
    let others: Vec<&str> = normalized
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "STRICT_TRANS_TABLES" && *t != "STRICT_ALL_TABLES")
        .collect();
    if enabled {
        let mut parts = vec!["STRICT_TRANS_TABLES"];
        parts.extend_from_slice(&others);
        parts.join(",")
    } else {
        others.join(",")
    }
}

// ── PendingInsertBatch ────────────────────────────────────────────────────────

/// Transaction-local staging buffer for consecutive `INSERT ... VALUES` statements.
#[derive(Debug)]
///
/// Rows are enqueued here instead of being written to the heap immediately.
/// The buffer is flushed (heap + WAL + index write) before any barrier statement
/// (`SELECT`, `UPDATE`, `DELETE`, DDL, `COMMIT`, table switch, ineligible INSERT).
/// On `ROLLBACK`, the buffer is discarded without touching heap or WAL.
///
/// Only active inside an explicit user transaction (`in_explicit_txn = true`).
pub struct PendingInsertBatch {
    pub table_id: u32,
    pub table_def: TableDef,
    pub columns: Vec<ColumnDef>,
    /// Secondary indexes (non-primary, non-empty columns) for this table.
    pub indexes: Vec<IndexDef>,
    /// Pre-compiled partial-index predicates, parallel to `indexes`.
    pub compiled_preds: Vec<Option<Expr>>,
    /// Fully materialized rows ready to be written to the heap.
    pub rows: Vec<Vec<Value>>,
    /// For each unique (non-FK) index: set of encoded keys already staged.
    /// Used to detect cross-row UNIQUE violations before heap mutation.
    pub unique_seen: HashMap<u32, HashSet<Vec<u8>>>,
    /// Set of index_ids whose committed BTree root was empty when the batch
    /// was created. For these indexes, the enqueue-time UNIQUE precheck
    /// skips the BTree::lookup_in against committed data (guaranteed no
    /// committed keys exist) and only checks `unique_seen`.
    pub committed_empty: HashSet<u32>,
}

// ── SessionContext ────────────────────────────────────────────────────────────

/// Per-connection state: schema cache + session variables visible to the executor.
#[derive(Debug)]
pub struct SessionContext {
    /// Cached table schemas keyed by `"database.schema.table"`.
    cache: HashMap<String, ResolvedTable>,
    /// Per-table heap-tail hint cache (Phase 5.18).
    ///
    /// Key: `table_id`. Value: `(root_page_id, tail_page_id)`.
    /// Cleared whenever the schema cache is invalidated or a root rotation is detected.
    heap_tail: HashMap<u32, (u64, u64)>,
    /// Staleness tracker for per-column statistics (Phase 6.11).
    pub stats: StaleStatsTracker,
    /// Whether the connection is in autocommit mode (MySQL default: `true`).
    ///
    /// When `false` (`SET autocommit=0`), the executor does not wrap DML statements
    /// in implicit `BEGIN / COMMIT`. Instead, the first DML starts an implicit
    /// transaction that remains open until the client sends an explicit `COMMIT`
    /// or `ROLLBACK`. DDL always triggers an implicit commit of any open transaction.
    pub autocommit: bool,
    /// Whether DML column assignment coercion is in strict mode (default: `true`).
    ///
    /// When `true` (default): `INSERT`/`UPDATE` column values that cannot be
    /// coerced under `CoercionMode::Strict` return an error immediately.
    ///
    /// When `false` (`SET strict_mode = OFF` / `SET sql_mode = ''`): the engine
    /// first tries strict coercion; on failure it falls back to permissive
    /// coercion, stores the result, and appends a SQL warning 1265 to the
    /// session. If permissive coercion also fails the error is returned.
    pub strict_mode: bool,
    /// How statement errors affect the current transaction (default: `RollbackStatement`).
    ///
    /// Set via `SET on_error = 'rollback_statement' | 'rollback_transaction' |
    /// 'savepoint' | 'ignore'`. Applied by the executor and by the network
    /// pipeline (`database.rs`) to parse/analyze failures.
    pub on_error: OnErrorMode,
    /// High-level compatibility mode (default: `Standard`).
    ///
    /// Set via `SET AXIOM_COMPAT = 'standard' | 'mysql' | 'postgresql' | DEFAULT`.
    /// Controls the default session collation when no explicit override is active.
    pub compat_mode: CompatMode,
    /// Explicit session collation override. `None` means use the compat-derived default.
    ///
    /// Set via `SET collation = 'binary' | 'es' | DEFAULT`.
    /// `SET AXIOM_COMPAT = ...` does NOT clear an explicit override already set.
    pub explicit_collation: Option<SessionCollation>,
    /// Warnings accumulated during the last statement.
    ///
    /// Cleared automatically before each new statement execution (in
    /// `Database::execute_query`). The handler reads `warnings.len()` to set
    /// `warning_count` in the OK packet, and `SHOW WARNINGS` returns this list.
    pub warnings: Vec<SqlWarning>,
    /// Staging buffer for consecutive `INSERT ... VALUES` inside an explicit transaction.
    ///
    /// `None` when no rows are pending. Flushed on any barrier statement or COMMIT.
    /// Discarded (without heap/WAL writes) on ROLLBACK.
    pub pending_inserts: Option<PendingInsertBatch>,
    /// `true` while the connection is inside an explicit user transaction
    /// (after `BEGIN`, before `COMMIT` / `ROLLBACK`).
    ///
    /// Used by the INSERT path to decide whether rows are eligible for staging.
    /// Autocommit-wrapped single-statement transactions do NOT set this flag.
    pub in_explicit_txn: bool,
    /// Currently selected database for this session.
    ///
    /// Empty string means "no explicit USE yet". Resolution still falls back
    /// to [`DEFAULT_DATABASE_NAME`] so legacy single-database behavior remains
    /// intact, while `DATABASE()` can still return NULL on the wire.
    pub current_database: String,
    /// Schema search path for unqualified name resolution (PostgreSQL-style).
    /// Default: `["public"]`. Reset to `["public"]` on every `USE db`.
    pub search_path: Vec<String>,
    /// Session default isolation level for new explicit transactions.
    /// Default: `RepeatableRead` (MySQL default).
    pub transaction_isolation: axiomdb_core::IsolationLevel,
    /// Per-transaction isolation level override set by
    /// `SET TRANSACTION ISOLATION LEVEL`. Consumed by the next `BEGIN`.
    pub next_txn_isolation: Option<axiomdb_core::IsolationLevel>,
    /// Lock wait timeout in seconds (Phase 7.10).
    ///
    /// Maximum time a statement waits for a write lock before returning
    /// `LockTimeout`. Default: 30 seconds (matches MySQL `innodb_lock_wait_timeout`).
    /// Set via `SET lock_timeout = N`.
    pub lock_timeout_secs: u64,
    /// Named savepoint stack for `SAVEPOINT / ROLLBACK TO / RELEASE` (Phase 7.12).
    ///
    /// Pushed by `SAVEPOINT name`, searched by name on `ROLLBACK TO` / `RELEASE`.
    /// Stack is truncated on rollback/release (later savepoints destroyed).
    /// Cleared entirely on `COMMIT` / `ROLLBACK`.
    pub savepoints: Vec<(String, axiomdb_wal::Savepoint)>,
}

impl Default for SessionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionContext {
    /// Creates an empty session context with autocommit enabled (MySQL default).
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            heap_tail: HashMap::new(),
            autocommit: true,
            strict_mode: true,
            on_error: OnErrorMode::RollbackStatement,
            compat_mode: CompatMode::Standard,
            explicit_collation: None,
            warnings: Vec::new(),
            stats: StaleStatsTracker::default(),
            pending_inserts: None,
            in_explicit_txn: false,
            current_database: String::new(),
            search_path: vec!["public".to_string()],
            transaction_isolation: axiomdb_core::IsolationLevel::default(),
            next_txn_isolation: None,
            lock_timeout_secs: 30,
            savepoints: Vec::new(),
        }
    }

    /// Clears all accumulated warnings. Called before each statement.
    pub fn clear_warnings(&mut self) {
        self.warnings.clear();
    }

    /// Discards any staged INSERT rows without writing to heap or WAL.
    ///
    /// Called on `ROLLBACK` to cleanly drop buffered rows that were never
    /// physically inserted. Also clears the explicit-transaction flag.
    pub fn discard_pending_inserts(&mut self) {
        self.pending_inserts = None;
        self.in_explicit_txn = false;
    }

    /// Appends a warning. Called by the executor when a no-op or non-fatal
    /// condition is detected (e.g. COMMIT/ROLLBACK with no active transaction).
    pub fn warn(&mut self, code: u16, message: impl Into<String>) {
        self.warnings.push(SqlWarning {
            level: "Warning",
            code,
            message: message.into(),
        });
    }

    /// Returns the number of warnings from the last statement.
    pub fn warning_count(&self) -> u16 {
        self.warnings.len().min(u16::MAX as usize) as u16
    }

    // ── Collation / compat ────────────────────────────────────────────────────

    /// Returns the effective session collation for text comparisons.
    ///
    /// Priority: explicit override > compat-derived default > Binary.
    pub fn effective_collation(&self) -> SessionCollation {
        if let Some(c) = self.explicit_collation {
            return c;
        }
        match self.compat_mode {
            CompatMode::MySql => SessionCollation::Es,
            _ => SessionCollation::Binary,
        }
    }

    /// Returns the canonical name of the effective session collation (`"binary"` or `"es"`).
    pub fn effective_collation_name(&self) -> &'static str {
        session_collation_name(self.effective_collation())
    }

    // ── Schema cache ──────────────────────────────────────────────────────────

    fn key(database: &str, schema: &str, table: &str) -> String {
        format!("{database}.{schema}.{table}")
    }

    pub fn get_table(&self, database: &str, schema: &str, table: &str) -> Option<&ResolvedTable> {
        self.cache.get(&Self::key(database, schema, table))
    }

    pub fn cache_table(
        &mut self,
        database: &str,
        schema: &str,
        table: &str,
        resolved: ResolvedTable,
    ) {
        self.cache
            .insert(Self::key(database, schema, table), resolved);
    }

    pub fn invalidate_table(&mut self, database: &str, schema: &str, table: &str) {
        // Also clear any heap-tail hint for this table so a stale tail is not
        // reused after a DDL change or root rotation.
        if let Some(resolved) = self.cache.get(&Self::key(database, schema, table)) {
            self.heap_tail.remove(&resolved.def.id);
        }
        self.cache.remove(&Self::key(database, schema, table));
    }

    pub fn invalidate_all(&mut self) {
        self.cache.clear();
        self.heap_tail.clear();
    }

    // ── Heap tail hint cache (Phase 5.18) ─────────────────────────────────────

    /// Returns a [`axiomdb_storage::HeapAppendHint`] for `table_id` if one is
    /// cached and the stored `root_page_id` matches the caller's current root.
    ///
    /// Returns `None` on root mismatch (root-rotation detected) or cache miss.
    pub fn get_heap_tail_hint(
        &self,
        table_id: u32,
        root_page_id: u64,
    ) -> Option<axiomdb_storage::HeapAppendHint> {
        let (cached_root, tail) = self.heap_tail.get(&table_id)?;
        if *cached_root != root_page_id {
            return None; // root rotation — discard stale hint
        }
        Some(axiomdb_storage::HeapAppendHint {
            root_page_id: *cached_root,
            tail_page_id: *tail,
        })
    }

    /// Stores (or updates) the heap tail hint for `table_id`.
    pub fn set_heap_tail_hint(&mut self, table_id: u32, root_page_id: u64, tail_page_id: u64) {
        self.heap_tail
            .insert(table_id, (root_page_id, tail_page_id));
    }

    /// Clears the heap tail hint for a specific `table_id`.
    pub fn invalidate_heap_tail(&mut self, table_id: u32) {
        self.heap_tail.remove(&table_id);
    }

    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

    /// Returns the selected database if `USE db` ran in this session.
    pub fn selected_database(&self) -> Option<&str> {
        if self.current_database.is_empty() {
            None
        } else {
            Some(&self.current_database)
        }
    }

    /// Returns the database used for name resolution.
    pub fn effective_database(&self) -> &str {
        self.selected_database().unwrap_or(DEFAULT_DATABASE_NAME)
    }

    /// Updates the selected database for the session and invalidates cached table metadata.
    /// Also resets the search path to `["public"]` since schema names are per-database.
    pub fn set_current_database(&mut self, database: impl Into<String>) {
        self.current_database = database.into();
        self.search_path = vec!["public".to_string()];
        self.invalidate_all();
    }

    /// Returns the first schema in the search path (used for `current_schema()`
    /// and as the default creation schema).
    pub fn current_schema(&self) -> &str {
        self.search_path
            .first()
            .map(|s| s.as_str())
            .unwrap_or("public")
    }

    /// Returns the isolation level for the next `BEGIN`, consuming any per-txn
    /// override set by `SET TRANSACTION ISOLATION LEVEL`.
    pub fn effective_isolation(&mut self) -> axiomdb_core::IsolationLevel {
        self.next_txn_isolation
            .take()
            .unwrap_or(self.transaction_isolation)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_context_strict_mode_default_true() {
        let ctx = SessionContext::new();
        assert!(ctx.strict_mode, "strict_mode must default to true");
    }

    // ── on_error helpers ──────────────────────────────────────────────────────

    #[test]
    fn test_on_error_default() {
        let ctx = SessionContext::new();
        assert_eq!(ctx.on_error, OnErrorMode::RollbackStatement);
    }

    #[test]
    fn test_parse_on_error_setting_all_variants() {
        assert_eq!(
            parse_on_error_setting("rollback_statement").unwrap(),
            OnErrorMode::RollbackStatement
        );
        assert_eq!(
            parse_on_error_setting("ROLLBACK_STATEMENT").unwrap(),
            OnErrorMode::RollbackStatement
        );
        assert_eq!(
            parse_on_error_setting("rollback_transaction").unwrap(),
            OnErrorMode::RollbackTransaction
        );
        assert_eq!(
            parse_on_error_setting("ROLLBACK_TRANSACTION").unwrap(),
            OnErrorMode::RollbackTransaction
        );
        assert_eq!(
            parse_on_error_setting("savepoint").unwrap(),
            OnErrorMode::Savepoint
        );
        assert_eq!(
            parse_on_error_setting("SAVEPOINT").unwrap(),
            OnErrorMode::Savepoint
        );
        assert_eq!(
            parse_on_error_setting("ignore").unwrap(),
            OnErrorMode::Ignore
        );
        assert_eq!(
            parse_on_error_setting("IGNORE").unwrap(),
            OnErrorMode::Ignore
        );
    }

    #[test]
    fn test_parse_on_error_setting_default() {
        assert_eq!(
            parse_on_error_setting("DEFAULT").unwrap(),
            OnErrorMode::RollbackStatement
        );
        assert_eq!(
            parse_on_error_setting("default").unwrap(),
            OnErrorMode::RollbackStatement
        );
    }

    #[test]
    fn test_parse_on_error_setting_quoted() {
        assert_eq!(
            parse_on_error_setting("'rollback_statement'").unwrap(),
            OnErrorMode::RollbackStatement
        );
        assert_eq!(
            parse_on_error_setting("\"savepoint\"").unwrap(),
            OnErrorMode::Savepoint
        );
    }

    #[test]
    fn test_parse_on_error_setting_invalid() {
        assert!(parse_on_error_setting("banana").is_err());
        assert!(parse_on_error_setting("").is_err());
        assert!(parse_on_error_setting("ignore_all").is_err());
    }

    #[test]
    fn test_on_error_mode_name() {
        assert_eq!(
            on_error_mode_name(OnErrorMode::RollbackStatement),
            "rollback_statement"
        );
        assert_eq!(
            on_error_mode_name(OnErrorMode::RollbackTransaction),
            "rollback_transaction"
        );
        assert_eq!(on_error_mode_name(OnErrorMode::Savepoint), "savepoint");
        assert_eq!(on_error_mode_name(OnErrorMode::Ignore), "ignore");
    }

    #[test]
    fn test_is_ignorable_on_error_sql_errors() {
        use axiomdb_core::error::DbError;
        assert!(is_ignorable_on_error(&DbError::ParseError {
            message: "oops".into(),
            position: None
        }));
        assert!(is_ignorable_on_error(&DbError::TableNotFound {
            name: "t".into()
        }));
        assert!(is_ignorable_on_error(&DbError::UniqueViolation {
            index_name: "idx".into(),
            value: None
        }));
        assert!(is_ignorable_on_error(&DbError::DivisionByZero));
        assert!(is_ignorable_on_error(&DbError::NotImplemented {
            feature: "x".into()
        }));
    }

    #[test]
    fn test_is_ignorable_on_error_infrastructure_errors() {
        use axiomdb_core::error::DbError;
        assert!(!is_ignorable_on_error(&DbError::DiskFull {
            operation: "write"
        }));
        assert!(!is_ignorable_on_error(&DbError::StorageFull));
        assert!(!is_ignorable_on_error(&DbError::Internal {
            message: "bad".into()
        }));
        assert!(!is_ignorable_on_error(&DbError::WalGroupCommitFailed {
            message: "fsync failed".into()
        }));
    }

    #[test]
    fn test_parse_boolish_setting_on_off() {
        assert!(parse_boolish_setting("ON").unwrap());
        assert!(parse_boolish_setting("on").unwrap());
        assert!(parse_boolish_setting("1").unwrap());
        assert!(parse_boolish_setting("TRUE").unwrap());
        assert!(parse_boolish_setting("true").unwrap());
        assert!(!parse_boolish_setting("OFF").unwrap());
        assert!(!parse_boolish_setting("off").unwrap());
        assert!(!parse_boolish_setting("0").unwrap());
        assert!(!parse_boolish_setting("FALSE").unwrap());
        assert!(!parse_boolish_setting("false").unwrap());
        assert!(parse_boolish_setting("maybe").is_err());
    }

    #[test]
    fn test_normalize_sql_mode_deduplicates_and_uppercases() {
        let result = normalize_sql_mode("ansi_quotes,strict_trans_tables,ansi_quotes");
        assert_eq!(result, "ANSI_QUOTES,STRICT_TRANS_TABLES");
    }

    #[test]
    fn test_normalize_sql_mode_trims_quotes() {
        assert_eq!(
            normalize_sql_mode("'STRICT_TRANS_TABLES'"),
            "STRICT_TRANS_TABLES"
        );
        assert_eq!(
            normalize_sql_mode("\"STRICT_ALL_TABLES\""),
            "STRICT_ALL_TABLES"
        );
    }

    #[test]
    fn test_normalize_sql_mode_empty() {
        assert_eq!(normalize_sql_mode(""), "");
        assert_eq!(normalize_sql_mode("''"), "");
    }

    #[test]
    fn test_sql_mode_is_strict() {
        assert!(sql_mode_is_strict("STRICT_TRANS_TABLES"));
        assert!(sql_mode_is_strict("ANSI_QUOTES,STRICT_TRANS_TABLES"));
        assert!(sql_mode_is_strict("STRICT_ALL_TABLES"));
        assert!(!sql_mode_is_strict("ANSI_QUOTES"));
        assert!(!sql_mode_is_strict(""));
    }

    #[test]
    fn test_apply_strict_to_sql_mode_enable() {
        let result = apply_strict_to_sql_mode("ANSI_QUOTES", true);
        assert!(result.starts_with("STRICT_TRANS_TABLES"));
        assert!(result.contains("ANSI_QUOTES"));
    }

    #[test]
    fn test_apply_strict_to_sql_mode_disable() {
        let result = apply_strict_to_sql_mode("STRICT_TRANS_TABLES,ANSI_QUOTES", false);
        assert!(!result.contains("STRICT_TRANS_TABLES"));
        assert!(result.contains("ANSI_QUOTES"));
    }

    #[test]
    fn test_apply_strict_to_sql_mode_idempotent_enable() {
        // Enabling when already strict should not duplicate the token.
        let result = apply_strict_to_sql_mode("STRICT_TRANS_TABLES", true);
        assert_eq!(result, "STRICT_TRANS_TABLES");
    }
}
