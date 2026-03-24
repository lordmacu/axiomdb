//! Session context — per-connection state including the schema cache and warnings.

use std::collections::HashMap;

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

// ── SessionContext ────────────────────────────────────────────────────────────

/// Per-connection state: schema cache + session variables visible to the executor.
#[derive(Debug)]
pub struct SessionContext {
    /// Cached table schemas keyed by `"schema_name.table_name"`.
    cache: HashMap<String, ResolvedTable>,
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
