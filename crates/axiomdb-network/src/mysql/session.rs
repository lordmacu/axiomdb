//! Per-connection session state.
//!
//! `ConnectionState` stores everything that changes per connection:
//! current database, autocommit flag, character set, and generic session
//! variables set via `SET`.
//!
//! It is created at handshake time and lives for the duration of the
//! connection. All `SET` and `@@variable` queries route through here
//! so the handler can respond correctly without touching the engine.

use std::collections::HashMap;

use axiomdb_core::error::DbError;
use axiomdb_sql::session::{
    apply_strict_to_sql_mode, normalize_sql_mode, parse_boolish_setting, sql_mode_is_strict,
};

use super::status::SessionStatus;

// ── PreparedStatement ─────────────────────────────────────────────────────────

/// A compiled prepared statement stored per-connection.
///
/// Created by `COM_STMT_PREPARE` and used by subsequent `COM_STMT_EXECUTE` calls
/// with the same `stmt_id`. Freed on `COM_STMT_CLOSE`.
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    pub stmt_id: u32,
    /// Original SQL with `?` placeholders (kept for fallback/re-analysis).
    pub sql_template: String,
    /// Number of `?` placeholders detected at prepare time.
    pub param_count: u16,
    /// MySQL type codes for each parameter (populated from first COM_STMT_EXECUTE).
    pub param_types: Vec<u16>,
    /// Analyzed statement with `Expr::Param` nodes.
    ///
    /// Cached at `COM_STMT_PREPARE` time. Used by `COM_STMT_EXECUTE` to skip
    /// `parse()` + `analyze()` (~5ms overhead) — only `substitute_params_in_ast`
    /// (~1µs tree walk) + `execute_with_ctx()` are needed.
    ///
    /// Set to `None` when re-analysis fails after a schema change.
    pub analyzed_stmt: Option<axiomdb_sql::ast::Stmt>,
    /// `Database::schema_version` snapshot at the last successful parse+analyze.
    ///
    /// If `compiled_at_version != current_schema_version`, the plan is stale
    /// and must be re-analyzed before the next `COM_STMT_EXECUTE` (Phase 5.13).
    pub compiled_at_version: u64,
    /// Logical clock for LRU eviction. Updated to `ConnectionState::execute_seq`
    /// on every `COM_STMT_EXECUTE`. The statement with the lowest value is
    /// evicted when the per-connection cache reaches its limit.
    pub last_used_seq: u64,
}

/// Counts unquoted `?` placeholders in a SQL string.
///
/// `?` characters inside single-quoted string literals are NOT counted.
pub fn count_params(sql: &str) -> u16 {
    let mut count = 0u16;
    let mut in_string = false;
    let mut prev = '\0';
    for ch in sql.chars() {
        match ch {
            '\'' if !in_string => in_string = true,
            '\'' if in_string && prev != '\\' => in_string = false,
            '?' if !in_string => count += 1,
            _ => {}
        }
        prev = ch;
    }
    count
}

// ── ConnectionState ───────────────────────────────────────────────────────────

/// Per-connection session state.
#[derive(Debug)]
pub struct ConnectionState {
    /// Current schema, set by `USE db` / COM_INIT_DB.
    pub current_database: String,
    /// Autocommit mode. MySQL default = true.
    pub autocommit: bool,
    /// Client character set (from handshake or `SET NAMES`).
    pub character_set_client: String,
    /// Generic key=value session variables.
    pub variables: HashMap<String, String>,
    /// Prepared statements cached for this connection.
    pub prepared_statements: HashMap<u32, PreparedStatement>,
    /// Monotonically increasing statement ID (never 0).
    pub next_stmt_id: u32,
    /// Maximum number of prepared statements to cache.
    /// When full, the LRU statement is evicted on the next PREPARE.
    pub max_prepared_stmts: usize,
    /// Monotonically increasing counter incremented on every COM_STMT_EXECUTE.
    /// Used as the `last_used_seq` for LRU eviction ordering.
    execute_seq: u64,
    /// Per-connection cumulative status counters (Phase 5.9c).
    /// Reset to zero by `COM_RESET_CONNECTION` (which recreates `ConnectionState`).
    pub session_status: SessionStatus,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionState {
    /// Default inbound payload limit: 64 MiB.
    ///
    /// Must match `MySqlCodec::default()` in `codec.rs`.  Both values are
    /// kept in sync by `handle_connection` after auth and after each
    /// `SET max_allowed_packet`.
    pub const DEFAULT_MAX_ALLOWED_PACKET: usize = 67_108_864;

    /// Creates a connection state with MySQL-compatible defaults and the
    /// given prepared-statement cache limit.
    pub fn new_with_limit(max_prepared_stmts: usize) -> Self {
        let mut s = Self::new();
        s.max_prepared_stmts = max_prepared_stmts;
        s
    }

    /// Creates a connection state with MySQL-compatible defaults.
    pub fn new() -> Self {
        let mut variables = HashMap::new();
        variables.insert("time_zone".into(), "SYSTEM".into());
        variables.insert("sql_mode".into(), "STRICT_TRANS_TABLES".into());
        variables.insert("strict_mode".into(), "ON".into());
        variables.insert("transaction_isolation".into(), "REPEATABLE-READ".into());
        variables.insert("tx_isolation".into(), "REPEATABLE-READ".into());
        variables.insert("max_allowed_packet".into(), "67108864".into());
        variables.insert("net_write_timeout".into(), "60".into());
        variables.insert("net_read_timeout".into(), "60".into());
        variables.insert("wait_timeout".into(), "28800".into());
        variables.insert("interactive_timeout".into(), "28800".into());
        Self {
            current_database: String::new(),
            autocommit: true,
            character_set_client: "utf8mb4".into(),
            variables,
            prepared_statements: HashMap::new(),
            next_stmt_id: 1,
            max_prepared_stmts: 1024,
            execute_seq: 0,
            session_status: SessionStatus::default(),
        }
    }

    /// Increments and returns the execute sequence counter.
    ///
    /// Called on every `COM_STMT_EXECUTE` to update `PreparedStatement::last_used_seq`
    /// for LRU eviction ordering.
    pub fn next_execute_seq(&mut self) -> u64 {
        self.execute_seq += 1;
        self.execute_seq
    }

    /// Parses the current `max_allowed_packet` session value into a byte limit.
    ///
    /// Accepts a plain decimal integer or a quoted decimal integer (e.g., `'2048'`).
    /// Returns `Err(DbError::InvalidValue)` for non-numeric or zero values.
    pub fn max_allowed_packet_bytes(&self) -> Result<usize, DbError> {
        let raw = self
            .variables
            .get("max_allowed_packet")
            .map(|s| s.as_str())
            .unwrap_or("67108864");
        let stripped = raw.trim().trim_matches('\'').trim_matches('"');
        stripped
            .parse::<usize>()
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!("max_allowed_packet must be a positive integer, got '{raw}'"),
            })
    }

    /// Applies a SET statement, updating the relevant session variable.
    ///
    /// Returns `Ok(true)` if the statement was recognized (caller should send OK).
    /// Returns `Ok(false)` if it should be executed by the engine instead.
    /// Returns `Err(DbError::InvalidValue)` if the value for a validated variable
    /// (currently only `max_allowed_packet`) is invalid — caller should send ERR.
    pub fn apply_set(&mut self, sql: &str) -> Result<bool, DbError> {
        let trimmed = sql.trim();
        // Only handle SET statements.
        if !trimmed.to_ascii_lowercase().starts_with("set ") {
            return Ok(false);
        }
        let rest = trimmed[4..].trim();

        // SET NAMES charset [COLLATE collation]
        let rest_lower = rest.to_ascii_lowercase();
        if rest_lower.starts_with("names ") {
            let charset = rest[6..]
                .split_whitespace()
                .next()
                .unwrap_or("utf8mb4")
                .trim_matches('\'')
                .trim_matches('"')
                .to_string();
            self.character_set_client = charset;
            return Ok(true);
        }

        // Parse: [@@session. | @@][varname] = value
        let rest = rest
            .strip_prefix("@@session.")
            .or_else(|| rest.strip_prefix("@@"))
            .unwrap_or(rest);

        if let Some(eq) = rest.find('=') {
            let name = rest[..eq].trim().to_ascii_lowercase();
            let raw_val = rest[eq + 1..].trim();
            let value = raw_val.trim_matches('\'').trim_matches('"').to_string();

            match name.as_str() {
                "autocommit" => {
                    self.autocommit = matches!(value.as_str(), "1" | "true" | "on");
                }
                "character_set_client" | "character_set_connection" | "character_set_results" => {
                    self.character_set_client = value;
                }
                "max_allowed_packet" => {
                    // Validate before storing: must be a positive decimal integer.
                    let candidate = raw_val.trim().trim_matches('\'').trim_matches('"');
                    match candidate.parse::<usize>() {
                        Ok(n) if n > 0 => {
                            self.variables
                                .insert("max_allowed_packet".to_string(), n.to_string());
                        }
                        _ => {
                            return Err(DbError::InvalidValue {
                                reason: format!(
                                    "max_allowed_packet must be a positive integer, got '{raw_val}'"
                                ),
                            });
                        }
                    }
                }
                "strict_mode" => {
                    let enabled = if value.eq_ignore_ascii_case("DEFAULT") {
                        true
                    } else {
                        parse_boolish_setting(&value)?
                    };
                    let strict_str = if enabled { "ON" } else { "OFF" };
                    self.variables
                        .insert("strict_mode".to_string(), strict_str.to_string());
                    let current_sql_mode =
                        self.variables.get("sql_mode").cloned().unwrap_or_default();
                    let new_sql_mode = apply_strict_to_sql_mode(&current_sql_mode, enabled);
                    self.variables.insert("sql_mode".to_string(), new_sql_mode);
                }
                "sql_mode" => {
                    let normalized = if value.eq_ignore_ascii_case("DEFAULT") {
                        "STRICT_TRANS_TABLES".to_string()
                    } else {
                        normalize_sql_mode(&value)
                    };
                    self.variables
                        .insert("sql_mode".to_string(), normalized.clone());
                    let strict_str = if sql_mode_is_strict(&normalized) {
                        "ON"
                    } else {
                        "OFF"
                    };
                    self.variables
                        .insert("strict_mode".to_string(), strict_str.to_string());
                }
                other => {
                    self.variables.insert(other.to_string(), value);
                }
            }
            return Ok(true);
        }

        // SET without '=' (e.g., SET TRANSACTION ...) — just accept
        Ok(true)
    }

    /// Returns the value of a session variable by name.
    ///
    /// Handles both `varname` and `@@session.varname` and `@@varname` forms.
    /// Returns `None` if the variable is unknown.
    pub fn get_variable(&self, raw_name: &str) -> Option<String> {
        let name = raw_name
            .trim()
            .trim_start_matches("@@session.")
            .trim_start_matches("@@")
            .to_ascii_lowercase();

        match name.as_str() {
            "autocommit" => Some(if self.autocommit {
                "1".into()
            } else {
                "0".into()
            }),
            "character_set_client" => Some(self.character_set_client.clone()),
            "character_set_connection" => Some(self.character_set_client.clone()),
            "character_set_results" => Some(self.character_set_client.clone()),
            "character_set_server" => Some("utf8mb4".into()),
            "character_set_database" => Some("utf8mb4".into()),
            "collation_connection" => Some("utf8mb4_0900_ai_ci".into()),
            "collation_server" => Some("utf8mb4_0900_ai_ci".into()),
            "collation_database" => Some("utf8mb4_0900_ai_ci".into()),
            "transaction_isolation" | "tx_isolation" => Some("REPEATABLE-READ".into()),
            "lower_case_table_names" => Some("0".into()),
            "version_comment" => Some("AxiomDB".into()),
            "version" => Some("8.0.36-AxiomDB-0.1.0".into()),
            "global.time_zone" | "time_zone" => Some(
                self.variables
                    .get("time_zone")
                    .cloned()
                    .unwrap_or("SYSTEM".into()),
            ),
            other => self.variables.get(other).cloned(),
        }
    }

    /// Registers a new prepared statement and returns `(stmt_id, param_count)`.
    ///
    /// `schema_version` is the current `Database::schema_version` snapshot,
    /// stored as `compiled_at_version` so that `COM_STMT_EXECUTE` can detect
    /// stale plans after DDL (Phase 5.13).
    ///
    /// If the cache is at `max_prepared_stmts` capacity, the least-recently-used
    /// statement (lowest `last_used_seq`) is evicted before inserting the new one.
    pub fn prepare_statement(&mut self, sql: String, schema_version: u64) -> (u32, u16) {
        // LRU eviction when at capacity.
        if self.prepared_statements.len() >= self.max_prepared_stmts {
            if let Some(&lru_id) = self
                .prepared_statements
                .iter()
                .min_by_key(|(_, ps)| ps.last_used_seq)
                .map(|(id, _)| id)
            {
                self.prepared_statements.remove(&lru_id);
            }
        }

        let param_count = count_params(&sql);
        let stmt_id = self.next_stmt_id;
        // Advance, wrapping to 1 (never 0)
        self.next_stmt_id = self.next_stmt_id.wrapping_add(1).max(1);
        if self.next_stmt_id == 0 {
            self.next_stmt_id = 1;
        }
        self.prepared_statements.insert(
            stmt_id,
            PreparedStatement {
                stmt_id,
                sql_template: sql,
                param_count,
                param_types: vec![],
                analyzed_stmt: None, // populated by handler after parse+analyze
                compiled_at_version: schema_version,
                last_used_seq: 0,
            },
        );
        (stmt_id, param_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_autocommit_is_true() {
        let s = ConnectionState::new();
        assert!(s.autocommit);
        assert_eq!(s.get_variable("autocommit"), Some("1".into()));
    }

    #[test]
    fn test_set_autocommit_false() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET autocommit=0").unwrap());
        assert!(!s.autocommit);
        assert_eq!(s.get_variable("@@autocommit"), Some("0".into()));
    }

    #[test]
    fn test_set_names() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET NAMES latin1").unwrap());
        assert_eq!(s.character_set_client, "latin1");
        assert_eq!(
            s.get_variable("@@character_set_client"),
            Some("latin1".into())
        );
    }

    #[test]
    fn test_set_session_variable() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET @@session.time_zone = 'UTC'").unwrap());
        assert_eq!(s.get_variable("time_zone"), Some("UTC".into()));
    }

    // ── Phase 5.4a: max_allowed_packet validation ─────────────────────────────

    #[test]
    fn test_set_max_allowed_packet_valid() {
        let mut s = ConnectionState::new();
        s.apply_set("SET max_allowed_packet = 2048").unwrap();
        assert_eq!(s.max_allowed_packet_bytes().unwrap(), 2048);
    }

    #[test]
    fn test_set_max_allowed_packet_quoted() {
        let mut s = ConnectionState::new();
        s.apply_set("SET max_allowed_packet = '4096'").unwrap();
        assert_eq!(s.max_allowed_packet_bytes().unwrap(), 4096);
    }

    #[test]
    fn test_set_max_allowed_packet_invalid_leaves_previous_limit() {
        let mut s = ConnectionState::new();
        s.apply_set("SET max_allowed_packet = 1024").unwrap();
        let err = s.apply_set("SET max_allowed_packet = 'abc'").unwrap_err();
        assert!(err.to_string().contains("max_allowed_packet"));
        // Previous valid value must survive
        assert_eq!(s.max_allowed_packet_bytes().unwrap(), 1024);
    }

    #[test]
    fn test_set_max_allowed_packet_zero_is_invalid() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET max_allowed_packet = 0").is_err());
    }

    #[test]
    fn test_max_allowed_packet_bytes_default() {
        let s = ConnectionState::new();
        assert_eq!(
            s.max_allowed_packet_bytes().unwrap(),
            ConnectionState::DEFAULT_MAX_ALLOWED_PACKET
        );
    }

    #[test]
    fn test_get_unknown_variable() {
        let s = ConnectionState::new();
        assert_eq!(s.get_variable("nonexistent_var"), None);
    }

    #[test]
    fn test_current_database_starts_empty() {
        let s = ConnectionState::new();
        assert!(s.current_database.is_empty());
    }

    // ── Phase 5.13: plan cache version + LRU tests ───────────────────────────

    #[test]
    fn test_prepare_statement_sets_compiled_at_version() {
        let mut s = ConnectionState::new();
        let (id, _) = s.prepare_statement("SELECT 1".into(), 42);
        assert_eq!(s.prepared_statements[&id].compiled_at_version, 42);
        assert_eq!(s.prepared_statements[&id].last_used_seq, 0);
    }

    #[test]
    fn test_next_execute_seq_is_monotonic() {
        let mut s = ConnectionState::new();
        assert_eq!(s.next_execute_seq(), 1);
        assert_eq!(s.next_execute_seq(), 2);
        assert_eq!(s.next_execute_seq(), 3);
    }

    #[test]
    fn test_lru_eviction_at_limit() {
        let mut s = ConnectionState::new_with_limit(3);

        let (id1, _) = s.prepare_statement("SELECT 1".into(), 0);
        let (id2, _) = s.prepare_statement("SELECT 2".into(), 0);
        let (id3, _) = s.prepare_statement("SELECT 3".into(), 0);

        // Mark id2 as recently used (higher seq)
        s.prepared_statements.get_mut(&id1).unwrap().last_used_seq = 1;
        s.prepared_statements.get_mut(&id2).unwrap().last_used_seq = 3;
        s.prepared_statements.get_mut(&id3).unwrap().last_used_seq = 2;

        // Prepare a 4th statement — should evict id1 (seq=1, the lowest)
        let (id4, _) = s.prepare_statement("SELECT 4".into(), 0);

        assert!(
            !s.prepared_statements.contains_key(&id1),
            "id1 (LRU) should be evicted"
        );
        assert!(
            s.prepared_statements.contains_key(&id2),
            "id2 (MRU) should survive"
        );
        assert!(s.prepared_statements.contains_key(&id3));
        assert!(s.prepared_statements.contains_key(&id4));
        assert_eq!(s.prepared_statements.len(), 3);
    }

    #[test]
    fn test_lru_no_eviction_below_limit() {
        let mut s = ConnectionState::new_with_limit(5);
        for i in 0..4 {
            s.prepare_statement(format!("SELECT {i}"), 0);
        }
        assert_eq!(s.prepared_statements.len(), 4, "no eviction below limit");
    }

    #[test]
    fn test_new_with_limit_sets_max() {
        let s = ConnectionState::new_with_limit(32);
        assert_eq!(s.max_prepared_stmts, 32);
    }

    // ── Phase 4.25c: strict_mode / sql_mode wire tests ────────────────────────

    #[test]
    fn test_default_sql_mode_is_strict_trans_tables() {
        let s = ConnectionState::new();
        assert_eq!(
            s.get_variable("sql_mode"),
            Some("STRICT_TRANS_TABLES".into())
        );
    }

    #[test]
    fn test_default_strict_mode_is_on() {
        let s = ConnectionState::new();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
    }

    #[test]
    fn test_set_strict_mode_off_updates_sql_mode() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET strict_mode = OFF").unwrap());
        assert_eq!(s.get_variable("strict_mode"), Some("OFF".into()));
        // sql_mode must no longer contain STRICT_TRANS_TABLES
        let sql_mode = s.get_variable("sql_mode").unwrap();
        assert!(
            !sql_mode.contains("STRICT_TRANS_TABLES"),
            "sql_mode should not contain STRICT_TRANS_TABLES after OFF: {sql_mode}"
        );
    }

    #[test]
    fn test_set_strict_mode_on_updates_sql_mode() {
        let mut s = ConnectionState::new();
        s.apply_set("SET strict_mode = OFF").unwrap();
        s.apply_set("SET strict_mode = ON").unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
        let sql_mode = s.get_variable("sql_mode").unwrap();
        assert!(
            sql_mode.contains("STRICT_TRANS_TABLES"),
            "sql_mode should contain STRICT_TRANS_TABLES after ON: {sql_mode}"
        );
    }

    #[test]
    fn test_set_strict_mode_default_restores_strict() {
        let mut s = ConnectionState::new();
        s.apply_set("SET strict_mode = OFF").unwrap();
        s.apply_set("SET strict_mode = DEFAULT").unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
    }

    #[test]
    fn test_set_sql_mode_empty_disables_strict() {
        let mut s = ConnectionState::new();
        s.apply_set("SET sql_mode = ''").unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("OFF".into()));
        assert_eq!(s.get_variable("sql_mode"), Some("".into()));
    }

    #[test]
    fn test_set_sql_mode_strict_trans_tables_enables_strict() {
        let mut s = ConnectionState::new();
        s.apply_set("SET sql_mode = ''").unwrap();
        s.apply_set("SET sql_mode = 'STRICT_TRANS_TABLES'").unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
        assert_eq!(
            s.get_variable("sql_mode"),
            Some("STRICT_TRANS_TABLES".into())
        );
    }

    #[test]
    fn test_set_sql_mode_ansi_quotes_with_strict_trans_tables() {
        let mut s = ConnectionState::new();
        s.apply_set("SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'")
            .unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
        let sql_mode = s.get_variable("sql_mode").unwrap();
        assert!(sql_mode.contains("STRICT_TRANS_TABLES"), "{sql_mode}");
        assert!(sql_mode.contains("ANSI_QUOTES"), "{sql_mode}");
    }

    #[test]
    fn test_set_sql_mode_default_restores_strict() {
        let mut s = ConnectionState::new();
        s.apply_set("SET sql_mode = ''").unwrap();
        s.apply_set("SET sql_mode = DEFAULT").unwrap();
        assert_eq!(s.get_variable("strict_mode"), Some("ON".into()));
        assert_eq!(
            s.get_variable("sql_mode"),
            Some("STRICT_TRANS_TABLES".into())
        );
    }

    #[test]
    fn test_set_strict_mode_invalid_value_returns_error() {
        let mut s = ConnectionState::new();
        let err = s.apply_set("SET strict_mode = maybe").unwrap_err();
        assert!(
            err.to_string().contains("ON/OFF"),
            "error must mention ON/OFF: {err}"
        );
    }

    #[test]
    fn test_prepare_statement_new_fields_have_defaults() {
        let mut s = ConnectionState::new();
        let (id, _) = s.prepare_statement("SELECT ?".into(), 7);
        let ps = &s.prepared_statements[&id];
        assert_eq!(ps.compiled_at_version, 7);
        assert_eq!(ps.last_used_seq, 0);
        assert!(ps.analyzed_stmt.is_none());
    }
}
