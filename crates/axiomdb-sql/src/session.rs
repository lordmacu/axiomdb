//! Session context — per-connection state including the schema cache and warnings.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use axiomdb_catalog::{
    schema::{ColumnDef, TableDef, DEFAULT_DATABASE_NAME},
    IndexDef, ResolvedTable,
};
use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use axiomdb_wal::ConnectionTxn;

use crate::clustered_secondary::ClusteredSecondaryLayout;
use crate::expr::Expr;
use crate::result::{ColumnMeta, Row};

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
    let s = raw.trim().trim_matches('\'').trim_matches('"');
    if s.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    match normalize_collation_name(s)?.as_str() {
        "binary" => Ok(Some(SessionCollation::Binary)),
        "es" => Ok(Some(SessionCollation::Es)),
        "default" => Ok(None),
        _ => Err(DbError::InvalidValue {
            reason: format!("invalid collation value '{raw}'; expected binary | es | DEFAULT"),
        }),
    }
}

/// Returns the canonical persisted name for a supported collation.
///
/// Supported canonical names:
/// - `binary`
/// - `es`
///
/// Supported aliases:
/// - `C`, `utf8mb4_bin` -> `binary`
/// - `utf8mb4_0900_ai_ci`, `utf8mb4_general_ci`, `utf8mb4_unicode_ci` -> `es`
pub fn normalize_collation_name(raw: &str) -> Result<String, DbError> {
    let s = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_lowercase();
    match s.as_str() {
        "binary" | "c" | "utf8mb4_bin" => Ok("binary".into()),
        "es" | "utf8mb4_0900_ai_ci" | "utf8mb4_general_ci" | "utf8mb4_unicode_ci" => {
            Ok("es".into())
        }
        _ => Err(DbError::InvalidValue {
            reason: format!(
                "invalid collation value '{raw}'; expected binary | es or a supported alias"
            ),
        }),
    }
}

pub fn session_collation_from_name(raw: &str) -> Result<SessionCollation, DbError> {
    match normalize_collation_name(raw)?.as_str() {
        "binary" => Ok(SessionCollation::Binary),
        "es" => Ok(SessionCollation::Es),
        _ => Err(DbError::InvalidValue {
            reason: format!("invalid collation value '{raw}'"),
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

static TEMP_SCHEMA_SEQ: AtomicU64 = AtomicU64::new(1);
static NOTIFICATION_SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

// ── LISTEN / NOTIFY runtime ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNotification {
    pub channel: String,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    pub channel: String,
    pub payload: String,
}

#[derive(Default)]
struct NotificationBroker {
    subscriptions: HashMap<String, HashSet<u64>>,
    queues: HashMap<u64, VecDeque<SessionNotification>>,
}

impl NotificationBroker {
    fn listen(&mut self, session_id: u64, channel: &str) {
        self.subscriptions
            .entry(channel.to_string())
            .or_default()
            .insert(session_id);
        self.queues.entry(session_id).or_default();
    }

    fn unlisten(&mut self, session_id: u64, channel: &str) {
        if let Some(sessions) = self.subscriptions.get_mut(channel) {
            sessions.remove(&session_id);
            if sessions.is_empty() {
                self.subscriptions.remove(channel);
            }
        }
    }

    fn unlisten_all(&mut self, session_id: u64) {
        self.subscriptions.retain(|_, sessions| {
            sessions.remove(&session_id);
            !sessions.is_empty()
        });
    }

    fn publish(&mut self, emitter_session_id: u64, channel: &str, payload: &str) {
        let Some(listeners) = self.subscriptions.get(channel).cloned() else {
            return;
        };
        for session_id in listeners {
            if session_id == emitter_session_id {
                continue;
            }
            self.queues
                .entry(session_id)
                .or_default()
                .push_back(SessionNotification {
                    channel: channel.to_string(),
                    payload: payload.to_string(),
                });
        }
    }

    fn drain(&mut self, session_id: u64) -> Vec<SessionNotification> {
        self.queues
            .entry(session_id)
            .or_default()
            .drain(..)
            .collect()
    }

    fn unregister(&mut self, session_id: u64) {
        self.unlisten_all(session_id);
        self.queues.remove(&session_id);
    }
}

fn notification_broker() -> &'static Mutex<NotificationBroker> {
    static BROKER: OnceLock<Mutex<NotificationBroker>> = OnceLock::new();
    BROKER.get_or_init(|| Mutex::new(NotificationBroker::default()))
}

pub fn normalize_notification_channel(name: &str) -> Result<String, DbError> {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "notification channel name cannot be empty".into(),
        });
    }
    if normalized.len() > 64 {
        return Err(DbError::InvalidValue {
            reason: format!("notification channel '{name}' exceeds 64 characters"),
        });
    }
    Ok(normalized)
}

// ── SQL cursors ───────────────────────────────────────────────────────────────

/// Materialized transaction-scoped SQL cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCursor {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    pub pos: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionSavepoint {
    pub wal: axiomdb_wal::Savepoint,
    pub deferred_fk_len: usize,
    pub pending_notify_len: usize,
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
        | DbError::IndexNotFound { .. }
        | DbError::TriggerNotFound { .. }
        | DbError::ImmutableTable { .. }
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
        | DbError::ExclusionViolation { .. }
        | DbError::TriggerValidationFailed { .. }
        | DbError::ColumnCountMismatch { .. }
        | DbError::TypeMismatch { .. }
        | DbError::InvalidValue { .. }
        | DbError::InvalidCoercion { .. }
        | DbError::DivisionByZero
        | DbError::Overflow
        | DbError::ValueTooLarge { .. }
        | DbError::DataTooLong { .. }
        | DbError::NoActiveTransaction
        | DbError::TransactionAlreadyActive { .. }
        | DbError::CardinalityViolation { .. }
        | DbError::ColumnAlreadyExists { .. }
        | DbError::TableAlreadyExists { .. }
        | DbError::DatabaseAlreadyExists { .. }
        | DbError::SchemaAlreadyExists { .. }
        | DbError::SchemaNotFound { .. }
        | DbError::SchemaNotEmpty { .. }
        | DbError::IndexAlreadyExists { .. }
        | DbError::TriggerAlreadyExists { .. }
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
        | DbError::Other(_)
        | DbError::BackupError { .. } => false,
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

/// Minimal parser-affecting SQL mode flags tracked by the engine.
///
/// Phase 4.2f only needs `ANSI_QUOTES`. Other parser-affecting MySQL modes
/// (`NO_BACKSLASH_ESCAPES`, `PIPES_AS_CONCAT`, `IGNORE_SPACE`, ...) are deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SqlModeFlags {
    pub ansi_quotes: bool,
}

impl SqlModeFlags {
    /// Builds flags from a normalized `sql_mode` string.
    pub fn from_normalized_sql_mode(normalized: &str) -> Self {
        Self {
            ansi_quotes: sql_mode_has_ansi_quotes(normalized),
        }
    }

    /// Builds flags from a raw `sql_mode` string, applying normalization first.
    pub fn from_sql_mode(raw: &str) -> Self {
        Self::from_normalized_sql_mode(&normalize_sql_mode(raw))
    }
}

/// Returns `true` when `normalized` contains `ANSI_QUOTES`.
pub fn sql_mode_has_ansi_quotes(normalized: &str) -> bool {
    normalized.split(',').any(|t| t.trim() == "ANSI_QUOTES")
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
    /// Pre-compiled index expressions, parallel to `indexes` (Phase 21.8).
    pub compiled_index_exprs: Vec<Vec<Option<Expr>>>,
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

/// Pre-encoded row staged inside a `ClusteredInsertBatch`.
///
/// Mirrors `PreparedClusteredInsertRow` (defined in `clustered_table.rs`) but
/// lives here to avoid a circular dependency: `clustered_table.rs` imports
/// `SessionContext` from this module, so `session.rs` cannot import from
/// `clustered_table.rs`.
#[derive(Debug, Clone)]
pub struct StagedClusteredRow {
    pub values: Vec<Value>,
    pub encoded_row: Vec<u8>,
    pub primary_key_values: Vec<Value>,
    pub primary_key_bytes: Vec<u8>,
}

/// Transaction-local staging buffer for consecutive `INSERT ... VALUES` into a clustered table.
///
/// Mirrors `PendingInsertBatch` for heap tables, but stores pre-encoded
/// `StagedClusteredRow`s. Flushed (via `flush_clustered_insert_batch`) before
/// any barrier statement: SELECT, UPDATE, DELETE, DDL, COMMIT, SAVEPOINT, or
/// INSERT into a different table. On ROLLBACK, discarded without storage writes.
///
/// Only active inside an explicit user transaction (`in_explicit_txn = true`).
#[derive(Debug)]
pub struct ClusteredInsertBatch {
    pub table_id: u32,
    pub table_def: TableDef,
    pub columns: Vec<ColumnDef>,
    /// Wrapped in `Arc` so per-statement reuse is an atomic increment
    /// instead of a full `IndexDef` clone (which allocates its inner
    /// `Vec<IndexColumnDef>` and predicate string). See Attack 3.B Step 2.
    pub primary_idx: std::sync::Arc<IndexDef>,
    pub secondary_indexes: Vec<IndexDef>,
    pub secondary_layouts: Vec<ClusteredSecondaryLayout>,
    pub compiled_preds: Vec<Option<Expr>>,
    pub rows: Vec<StagedClusteredRow>,
    /// O(1) intra-batch PK duplicate detection (encoded key bytes).
    pub staged_pks: HashSet<Vec<u8>>,
}

// ── SessionContext ────────────────────────────────────────────────────────────

/// Per-connection state: schema cache + session variables visible to the executor.
#[derive(Debug)]
pub struct SessionContext {
    /// Stable per-session id used by the in-process LISTEN / NOTIFY broker.
    pub session_id: u64,
    /// Cached table schemas keyed by `"database.schema.table"`.
    cache: HashMap<String, ResolvedTable>,
    /// Per-table heap-tail hint cache (Phase 5.18).
    ///
    /// Key: `table_id`. Value: `(root_page_id, tail_page_id)`.
    /// Cleared whenever the schema cache is invalidated or a root rotation is detected.
    heap_tail: HashMap<u32, (u64, u64)>,
    /// Attack 3.B Step 3: cached `build_insert_column_positions` results
    /// keyed by `(table_id, columns_signature)`. Skips ~1-3 µs per INSERT
    /// call by avoiding the per-column name lookup + Vec allocation.
    ///
    /// The value is `(schema_version, positions)`. On lookup, callers pass
    /// the current schema_version; a mismatch causes lazy eviction +
    /// recompute. This keeps the cache correct across DDL without coupling
    /// it to specific invalidation paths (which are scattered across
    /// invalidate_all, invalidate_table, root rotations, etc.).
    ///
    /// `columns_signature = 0` is the "all columns in declaration order"
    /// shape (when the INSERT has no column list). Other shapes hash the
    /// column-name list with `DefaultHasher`.
    insert_col_positions: HashMap<(u32, u64), (u64, Vec<usize>)>,
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
    /// Whether double quotes are parsed as quoted identifiers (`true`) or as
    /// string literals (`false`, MySQL default).
    ///
    /// Derived from `SET sql_mode = ...`.
    pub ansi_quotes: bool,
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
    /// Staging buffer for consecutive `INSERT ... VALUES` into a clustered table
    /// inside an explicit transaction. Mirrors `pending_inserts` for clustered storage.
    ///
    /// `None` when no rows are pending. Flushed on any barrier statement or COMMIT.
    /// Discarded (without storage writes) on ROLLBACK.
    pub clustered_insert_batch: Option<ClusteredInsertBatch>,
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
    /// Hidden schema name used for session-local TEMP tables.
    ///
    /// When present, it is always kept at the front of `search_path` so
    /// unqualified lookups resolve temporary tables before permanent ones.
    pub temp_schema: Option<String>,
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
    pub savepoints: Vec<(String, SessionSavepoint)>,
    /// Active per-connection transaction state (Phase 40.4b).
    ///
    /// `Some` when a transaction is open (explicit or autocommit-implicit).
    /// `None` between transactions.
    pub conn_txn: Option<ConnectionTxn>,
    /// Pending deferred commit txn_id from the last `commit()` call in
    /// pipeline mode (Phase 40.10).
    ///
    /// Set by `execute_with_ctx` after each `txn.commit()` returns
    /// `Some(txn_id)`. Consumed by the network layer to drive the fsync
    /// pipeline. `None` for read-only or immediate commits.
    pub pending_deferred_txn_id: Option<axiomdb_core::TxnId>,
    /// Deferred foreign keys touched in the current transaction.
    pub deferred_fk_constraint_ids: Vec<u32>,
    /// Notifications emitted inside the current transaction but not yet committed.
    pub pending_notifications: Vec<PendingNotification>,
    /// Session-local SQL cursors declared via `DECLARE ... CURSOR`.
    ///
    /// Keyed by normalized lowercase cursor name. Cleared on transaction end
    /// and connection/session reset.
    pub cursors: HashMap<String, SessionCursor>,
    /// Session-local `CURRVAL` state keyed by lowercase `schema.sequence`.
    ///
    /// PostgreSQL defines `CURRVAL` only after this session has successfully
    /// called `NEXTVAL` for the same sequence.
    pub sequence_currvals: HashMap<String, i64>,
    /// Per-session holiday set cache keyed by upper-cased country code.
    ///
    /// Loaded lazily on first call to `IS_BUSINESS_DAY` / `NEXT_BUSINESS_DAY` /
    /// `BUSINESS_DAYS_BETWEEN` for a given country code. Cleared whenever the
    /// schema cache is invalidated (i.e., after any DDL statement).
    pub holiday_cache: HashMap<String, Arc<HashSet<i32>>>,
    /// Per-session exchange rate cache keyed by (from_currency, to_currency).
    ///
    /// Loaded lazily on first call to `CONVERT(money, 'CUR')` for a given pair.
    /// Cleared whenever the schema cache is invalidated (after any DDL statement).
    /// Value: `(mantissa, scale)` — same fixed-point encoding as `ExchangeRateDef`.
    pub exchange_rate_cache: HashMap<(String, String), (i128, u8)>,
}

impl Default for SessionContext {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SessionContext {
    fn drop(&mut self) {
        self.cleanup_notification_runtime();
    }
}

impl SessionContext {
    /// Creates an empty session context with autocommit enabled (MySQL default).
    pub fn new() -> Self {
        Self {
            session_id: NOTIFICATION_SESSION_SEQ.fetch_add(1, Ordering::Relaxed),
            cache: HashMap::new(),
            heap_tail: HashMap::new(),
            insert_col_positions: HashMap::new(),
            autocommit: true,
            strict_mode: true,
            ansi_quotes: false,
            on_error: OnErrorMode::RollbackStatement,
            compat_mode: CompatMode::Standard,
            explicit_collation: None,
            warnings: Vec::new(),
            stats: StaleStatsTracker::default(),
            pending_inserts: None,
            clustered_insert_batch: None,
            in_explicit_txn: false,
            current_database: String::new(),
            search_path: vec!["public".to_string()],
            temp_schema: None,
            transaction_isolation: axiomdb_core::IsolationLevel::default(),
            next_txn_isolation: None,
            lock_timeout_secs: 30,
            savepoints: Vec::new(),
            conn_txn: None,
            pending_deferred_txn_id: None,
            deferred_fk_constraint_ids: Vec::new(),
            pending_notifications: Vec::new(),
            cursors: HashMap::new(),
            sequence_currvals: HashMap::new(),
            holiday_cache: HashMap::new(),
            exchange_rate_cache: HashMap::new(),
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

    /// Discards any staged clustered INSERT rows without writing to storage or WAL.
    ///
    /// Called on ROLLBACK and on error paths that abort the current transaction.
    /// Does NOT clear `in_explicit_txn` — the caller is responsible for that.
    pub fn discard_clustered_insert_batch(&mut self) {
        self.clustered_insert_batch = None;
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

    fn normalize_cursor_name(name: &str) -> String {
        name.to_ascii_lowercase()
    }

    pub fn cursor(&self, name: &str) -> Option<&SessionCursor> {
        self.cursors.get(&Self::normalize_cursor_name(name))
    }

    pub fn cursor_mut(&mut self, name: &str) -> Option<&mut SessionCursor> {
        self.cursors.get_mut(&Self::normalize_cursor_name(name))
    }

    pub fn declare_cursor(&mut self, name: &str, cursor: SessionCursor) -> Result<(), DbError> {
        let key = Self::normalize_cursor_name(name);
        if self.cursors.contains_key(&key) {
            return Err(DbError::InvalidValue {
                reason: format!("cursor '{name}' already exists"),
            });
        }
        self.cursors.insert(key, cursor);
        Ok(())
    }

    pub fn close_cursor(&mut self, name: &str) -> Result<(), DbError> {
        let key = Self::normalize_cursor_name(name);
        if self.cursors.remove(&key).is_none() {
            return Err(DbError::InvalidValue {
                reason: format!("cursor '{name}' was not found"),
            });
        }
        Ok(())
    }

    pub fn close_all_cursors(&mut self) {
        self.cursors.clear();
    }

    pub fn mark_deferred_fk_constraints<I>(&mut self, fk_ids: I)
    where
        I: IntoIterator<Item = u32>,
    {
        self.deferred_fk_constraint_ids.extend(fk_ids);
    }

    pub fn truncate_deferred_fk_constraints(&mut self, len: usize) {
        self.deferred_fk_constraint_ids.truncate(len);
    }

    pub fn clear_deferred_fk_constraints(&mut self) {
        self.deferred_fk_constraint_ids.clear();
    }

    pub fn pending_notification_len(&self) -> usize {
        self.pending_notifications.len()
    }

    pub fn truncate_pending_notifications(&mut self, len: usize) {
        self.pending_notifications.truncate(len);
    }

    pub fn clear_pending_notifications(&mut self) {
        self.pending_notifications.clear();
    }

    pub fn listen_channel(&mut self, channel: &str) -> Result<(), DbError> {
        let channel = normalize_notification_channel(channel)?;
        notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listen(self.session_id, &channel);
        Ok(())
    }

    pub fn unlisten_channel(&mut self, channel: &str) -> Result<(), DbError> {
        let channel = normalize_notification_channel(channel)?;
        notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unlisten(self.session_id, &channel);
        Ok(())
    }

    pub fn unlisten_all_channels(&mut self) {
        notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unlisten_all(self.session_id);
    }

    pub fn enqueue_notification(
        &mut self,
        channel: &str,
        payload: impl Into<String>,
    ) -> Result<(), DbError> {
        let channel = normalize_notification_channel(channel)?;
        self.pending_notifications.push(PendingNotification {
            channel,
            payload: payload.into(),
        });
        Ok(())
    }

    pub fn flush_pending_notifications(&mut self) {
        if self.pending_notifications.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_notifications);
        let mut broker = notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for notif in pending {
            broker.publish(self.session_id, &notif.channel, &notif.payload);
        }
    }

    pub fn drain_notifications(&mut self) -> Vec<SessionNotification> {
        notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(self.session_id)
    }

    pub fn cleanup_notification_runtime(&mut self) {
        self.pending_notifications.clear();
        notification_broker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unregister(self.session_id);
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

    /// Returns the parser-affecting SQL mode flags for this session.
    pub fn sql_mode_flags(&self) -> SqlModeFlags {
        SqlModeFlags {
            ansi_quotes: self.ansi_quotes,
        }
    }

    // ── Schema cache ──────────────────────────────────────────────────────────

    fn key(database: &str, schema: &str, table: &str) -> String {
        format!("{database}.{schema}.{table}")
    }

    pub fn get_table(&self, database: &str, schema: &str, table: &str) -> Option<&ResolvedTable> {
        self.cache.get(&Self::key(database, schema, table))
    }

    /// Returns the cached `ResolvedTable` for `(database, schema, table)` only
    /// if its `def.schema_version` equals `expected_version`.
    ///
    /// Returns `None` on cache miss OR on version mismatch. Does NOT auto-evict
    /// on mismatch — the caller is expected to re-resolve and overwrite via
    /// [`Self::cache_table`].
    ///
    /// This is the cache-hit fast path for `resolve_table_cached` inside
    /// explicit transactions, mirroring SQLite's schema-cookie check
    /// (`research/sqlite/src/prepare.c:518-526`).
    pub fn get_table_if_version(
        &self,
        database: &str,
        schema: &str,
        table: &str,
        expected_version: u64,
    ) -> Option<&ResolvedTable> {
        let cached = self.cache.get(&Self::key(database, schema, table))?;
        if cached.def.schema_version == expected_version {
            Some(cached)
        } else {
            None
        }
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
        //
        // `insert_col_positions` is NOT cleared here — its entries carry a
        // schema_version stamp and get lazy-evicted on the next lookup that
        // sees a mismatch (see `get_insert_col_positions`).
        if let Some(resolved) = self.cache.get(&Self::key(database, schema, table)) {
            self.heap_tail.remove(&resolved.def.id);
        }
        self.cache.remove(&Self::key(database, schema, table));
    }

    pub fn invalidate_all(&mut self) {
        self.cache.clear();
        self.heap_tail.clear();
        // `insert_col_positions` is intentionally NOT cleared. Its entries
        // carry a schema_version stamp and get lazy-evicted on the next
        // lookup with a mismatching version (see get_insert_col_positions).
        // Avoids clearing on every commit (staging.rs flush) which would
        // defeat the cache's purpose.
        self.holiday_cache.clear();
        self.exchange_rate_cache.clear();
    }

    // ── Insert col_positions cache (Attack 3.B Step 3) ────────────────────────

    /// Returns the cached column-position vector for `table_id` and the
    /// statement's `columns_signature`, but only if its stored
    /// `schema_version` matches `expected_schema_version`.
    ///
    /// On stale entries the cache lazily evicts and returns `None`, so the
    /// caller recomputes and re-caches under the new version. This keeps
    /// the cache correct across DDL without coupling it to specific
    /// invalidation paths.
    pub fn get_insert_col_positions(
        &mut self,
        table_id: u32,
        expected_schema_version: u64,
        columns_signature: u64,
    ) -> Option<&Vec<usize>> {
        let key = (table_id, columns_signature);
        let stale = self
            .insert_col_positions
            .get(&key)
            .map(|(ver, _)| *ver != expected_schema_version)
            .unwrap_or(false);
        if stale {
            self.insert_col_positions.remove(&key);
            return None;
        }
        self.insert_col_positions.get(&key).map(|(_, pos)| pos)
    }

    /// Caches a `build_insert_column_positions` result tagged with the
    /// current schema_version (so future lookups can lazy-evict on stale).
    pub fn cache_insert_col_positions(
        &mut self,
        table_id: u32,
        schema_version: u64,
        columns_signature: u64,
        col_positions: Vec<usize>,
    ) {
        self.insert_col_positions
            .insert((table_id, columns_signature), (schema_version, col_positions));
    }

    /// Diagnostic / test accessor for the cache size.
    pub fn insert_col_positions_count(&self) -> usize {
        self.insert_col_positions.len()
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
        self.set_search_path(vec!["public".to_string()]);
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

    /// Returns the default schema for ordinary CREATE/DDL targets.
    ///
    /// Temporary schemas are skipped so permanent objects continue to land in
    /// the first non-temp schema even when TEMP shadowing is active.
    pub fn default_create_schema(&self) -> &str {
        self.search_path
            .iter()
            .find(|schema| self.temp_schema.as_deref() != Some(schema.as_str()))
            .map(|s| s.as_str())
            .unwrap_or("public")
    }

    /// Replaces the user-visible search path while preserving any active TEMP
    /// schema as an implicit first entry.
    pub fn set_search_path(&mut self, mut schemas: Vec<String>) {
        if schemas.is_empty() {
            schemas.push("public".to_string());
        }
        if let Some(temp_schema) = self.temp_schema.clone() {
            schemas.retain(|schema| schema != &temp_schema);
            schemas.insert(0, temp_schema);
        }
        self.search_path = schemas;
    }

    /// Allocates the hidden TEMP schema name for this session if needed and
    /// keeps it at the front of `search_path`.
    pub fn ensure_temp_schema(&mut self) -> String {
        if let Some(existing) = &self.temp_schema {
            return existing.clone();
        }
        let temp_schema = format!(
            "__axiom_temp_session_{}",
            TEMP_SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        self.temp_schema = Some(temp_schema.clone());
        self.set_search_path(self.search_path.clone());
        self.invalidate_all();
        temp_schema
    }

    /// Returns the active hidden TEMP schema name, if any.
    pub fn temp_schema_name(&self) -> Option<&str> {
        self.temp_schema.as_deref()
    }

    /// Clears the session TEMP schema and removes it from `search_path`.
    pub fn clear_temp_schema(&mut self) {
        let Some(temp_schema) = self.temp_schema.take() else {
            return;
        };
        self.search_path.retain(|schema| schema != &temp_schema);
        if self.search_path.is_empty() {
            self.search_path.push("public".to_string());
        }
        self.invalidate_all();
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

    #[test]
    fn test_session_context_ansi_quotes_default_false() {
        let ctx = SessionContext::new();
        assert!(
            !ctx.ansi_quotes,
            "ansi_quotes must default to false (MySQL default)"
        );
    }

    #[test]
    fn test_temp_schema_prefixes_search_path_but_not_default_create_schema() {
        let mut ctx = SessionContext::new();
        ctx.set_search_path(vec!["custom".into(), "public".into()]);
        let temp_schema = ctx.ensure_temp_schema();
        assert_eq!(ctx.current_schema(), temp_schema);
        assert_eq!(ctx.default_create_schema(), "custom");
        assert_eq!(ctx.search_path[0], temp_schema);
    }

    #[test]
    fn test_set_current_database_preserves_temp_schema_prefix() {
        let mut ctx = SessionContext::new();
        let temp_schema = ctx.ensure_temp_schema();
        ctx.set_current_database("analytics");
        assert_eq!(ctx.current_schema(), temp_schema);
        assert_eq!(ctx.default_create_schema(), "public");
        assert_eq!(
            ctx.search_path,
            vec![temp_schema.to_string(), "public".to_string()]
        );
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
    fn test_sql_mode_has_ansi_quotes() {
        assert!(sql_mode_has_ansi_quotes("ANSI_QUOTES"));
        assert!(sql_mode_has_ansi_quotes("STRICT_TRANS_TABLES,ANSI_QUOTES"));
        assert!(!sql_mode_has_ansi_quotes("STRICT_TRANS_TABLES"));
        assert!(!sql_mode_has_ansi_quotes(""));
    }

    #[test]
    fn test_sql_mode_flags_from_normalized_sql_mode() {
        let flags = SqlModeFlags::from_normalized_sql_mode("STRICT_TRANS_TABLES,ANSI_QUOTES");
        assert!(flags.ansi_quotes);
        let flags = SqlModeFlags::from_normalized_sql_mode("STRICT_TRANS_TABLES");
        assert!(!flags.ansi_quotes);
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

    // ── GAP-C.9 regression: MySQL compat mode implies CI collation ────────
    #[test]
    fn gap_c9_mysql_compat_defaults_to_case_insensitive_collation() {
        let mut ctx = SessionContext::new();
        // Default is Standard → Binary (case-sensitive).
        assert_eq!(ctx.effective_collation(), SessionCollation::Binary);

        // Switching to MySQL compat flips the effective collation to Es (CI).
        ctx.compat_mode = CompatMode::MySql;
        assert_eq!(ctx.effective_collation(), SessionCollation::Es);
        assert_eq!(ctx.effective_collation_name(), "es");

        // Explicit override still wins over the compat-derived default.
        ctx.explicit_collation = Some(SessionCollation::Binary);
        assert_eq!(ctx.effective_collation(), SessionCollation::Binary);
    }

    #[test]
    fn test_cursor_names_are_case_insensitive() {
        let mut ctx = SessionContext::new();
        ctx.declare_cursor(
            "SalesCur",
            SessionCursor {
                columns: vec![],
                rows: vec![],
                pos: 0,
            },
        )
        .unwrap();
        assert!(ctx.cursor("salescur").is_some());
        assert!(ctx.cursor("SALESCUR").is_some());
    }

    #[test]
    fn test_close_all_cursors_clears_state() {
        let mut ctx = SessionContext::new();
        ctx.declare_cursor(
            "c1",
            SessionCursor {
                columns: vec![],
                rows: vec![],
                pos: 0,
            },
        )
        .unwrap();
        ctx.declare_cursor(
            "c2",
            SessionCursor {
                columns: vec![],
                rows: vec![],
                pos: 0,
            },
        )
        .unwrap();
        ctx.close_all_cursors();
        assert!(ctx.cursors.is_empty());
    }
}
