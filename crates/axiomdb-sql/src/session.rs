//! Session context — per-connection state including the schema cache and warnings.

use std::collections::{HashMap, HashSet};

use axiomdb_catalog::ResolvedTable;

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

// ── SessionContext ────────────────────────────────────────────────────────────

/// Per-connection state: schema cache + session variables visible to the executor.
#[derive(Debug)]
pub struct SessionContext {
    /// Cached table schemas keyed by `"schema_name.table_name"`.
    cache: HashMap<String, ResolvedTable>,
    /// Staleness tracker for per-column statistics (Phase 6.11).
    pub stats: StaleStatsTracker,
    /// Whether the connection is in autocommit mode (MySQL default: `true`).
    ///
    /// When `false` (`SET autocommit=0`), the executor does not wrap DML statements
    /// in implicit `BEGIN / COMMIT`. Instead, the first DML starts an implicit
    /// transaction that remains open until the client sends an explicit `COMMIT`
    /// or `ROLLBACK`. DDL always triggers an implicit commit of any open transaction.
    pub autocommit: bool,
    /// Warnings accumulated during the last statement.
    ///
    /// Cleared automatically before each new statement execution (in
    /// `Database::execute_query`). The handler reads `warnings.len()` to set
    /// `warning_count` in the OK packet, and `SHOW WARNINGS` returns this list.
    pub warnings: Vec<SqlWarning>,
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
            autocommit: true,
            warnings: Vec::new(),
            stats: StaleStatsTracker::default(),
        }
    }

    /// Clears all accumulated warnings. Called before each statement.
    pub fn clear_warnings(&mut self) {
        self.warnings.clear();
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

    // ── Schema cache ──────────────────────────────────────────────────────────

    fn key(schema: &str, table: &str) -> String {
        format!("{schema}.{table}")
    }

    pub fn get_table(&self, schema: &str, table: &str) -> Option<&ResolvedTable> {
        self.cache.get(&Self::key(schema, table))
    }

    pub fn cache_table(&mut self, schema: &str, table: &str, resolved: ResolvedTable) {
        self.cache.insert(Self::key(schema, table), resolved);
    }

    pub fn invalidate_table(&mut self, schema: &str, table: &str) {
        self.cache.remove(&Self::key(schema, table));
    }

    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }
}
