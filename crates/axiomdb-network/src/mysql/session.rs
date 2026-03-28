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
    apply_strict_to_sql_mode, compat_mode_name, normalize_sql_mode, on_error_mode_name,
    parse_boolish_setting, parse_compat_mode_setting, parse_on_error_setting,
    parse_session_collation_setting, session_collation_name, sql_mode_is_strict, CompatMode,
    OnErrorMode, SessionCollation,
};

use super::charset::{self, CharsetDef, CollationDef, DEFAULT_SERVER_COLLATION};
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
    /// Selected database at the last successful parse+analyze.
    pub compiled_database: String,
    /// Logical clock for LRU eviction. Updated to `ConnectionState::execute_seq`
    /// on every `COM_STMT_EXECUTE`. The statement with the lowest value is
    /// evicted when the per-connection cache reaches its limit.
    pub last_used_seq: u64,
    /// Pending long-data buffers for `COM_STMT_SEND_LONG_DATA` (Phase 5.11b).
    ///
    /// `None` = no long data provided for this parameter.
    /// `Some(vec![])` = explicit empty long data (distinct from "no long data").
    ///
    /// Indexed by parameter position. Cleared after every `COM_STMT_EXECUTE`
    /// attempt (success or failure). Bounds: one entry per `param_count`.
    pub pending_long_data: Vec<Option<Vec<u8>>>,
    /// Deferred error recorded by `COM_STMT_SEND_LONG_DATA` when a chunk
    /// overflows `max_allowed_packet` or targets an out-of-range parameter.
    /// Returned as ERR on the next `COM_STMT_EXECUTE` then cleared.
    pub pending_long_data_error: Option<String>,
}

impl PreparedStatement {
    /// Appends `chunk` to the pending long-data buffer for `param_idx`.
    ///
    /// If the accumulated size would exceed `max_len`, stores a deferred error
    /// instead of appending (the command still sends no response — the error is
    /// surfaced on the next `COM_STMT_EXECUTE`).
    ///
    /// Out-of-range `param_idx` also stores a deferred error.
    pub fn append_long_data(&mut self, param_idx: usize, chunk: &[u8], max_len: usize) {
        if param_idx >= self.pending_long_data.len() {
            self.pending_long_data_error = Some(format!(
                "COM_STMT_SEND_LONG_DATA: parameter index {param_idx} out of range \
                 (statement has {} parameters)",
                self.param_count
            ));
            return;
        }
        let entry = &mut self.pending_long_data[param_idx];
        let current_len = entry.as_deref().map_or(0, |v| v.len());
        if current_len + chunk.len() > max_len {
            self.pending_long_data_error = Some(format!(
                "COM_STMT_SEND_LONG_DATA: accumulated value for parameter {param_idx} \
                 exceeds max_allowed_packet ({max_len} bytes)"
            ));
            return;
        }
        match entry {
            Some(buf) => buf.extend_from_slice(chunk),
            None => *entry = Some(chunk.to_vec()),
        }
    }

    /// Clears all pending long-data buffers and the deferred error.
    ///
    /// Called immediately after every `COM_STMT_EXECUTE` attempt, regardless
    /// of whether parsing or execution succeeded.
    pub fn clear_long_data_state(&mut self) {
        self.pending_long_data.fill(None);
        self.pending_long_data_error = None;
    }
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
    /// Decode charset for inbound text from the client.
    /// Set at handshake time; changed by `SET character_set_client`.
    client_charset: &'static CharsetDef,
    /// Collation for the logical connection (affects sort, compare).
    /// Set at handshake time; changed by `SET NAMES` / `SET collation_connection`.
    connection_collation: &'static CollationDef,
    /// Collation used when encoding outbound result rows and metadata.
    /// Set at handshake time; changed by `SET NAMES` / `SET character_set_results`.
    results_collation: &'static CollationDef,
    /// ON_ERROR mode for this connection (default: `RollbackStatement`).
    on_error: OnErrorMode,
    /// Compatibility mode for this connection (default: `Standard`).
    compat_mode: CompatMode,
    /// Explicit session collation override (`None` = compat-derived default).
    explicit_collation: Option<SessionCollation>,
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
        variables.insert("on_error".into(), "rollback_statement".into());
        variables.insert("axiom_compat".into(), "standard".into());
        variables.insert("collation".into(), "binary".into());
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
            on_error: OnErrorMode::RollbackStatement,
            compat_mode: CompatMode::Standard,
            explicit_collation: None,
            client_charset: DEFAULT_SERVER_COLLATION.charset,
            connection_collation: DEFAULT_SERVER_COLLATION,
            results_collation: DEFAULT_SERVER_COLLATION,
            variables,
            prepared_statements: HashMap::new(),
            next_stmt_id: 1,
            max_prepared_stmts: 1024,
            execute_seq: 0,
            session_status: SessionStatus::default(),
        }
    }

    /// Initializes connection charset state from the client's handshake `character_set` byte.
    ///
    /// If the collation id is in the supported table, all three session charset fields
    /// (`client_charset`, `connection_collation`, `results_collation`) are initialized
    /// from the corresponding `CollationDef`.
    ///
    /// Returns `DbError::InvalidValue` for unsupported collation ids.  The handler
    /// sends `ER_UNKNOWN_CHARACTER_SET` (1115 / SQLSTATE 42000) and closes the connection.
    pub fn from_handshake_collation_id(id: u8) -> Result<Self, DbError> {
        let collation = charset::lookup_collation_by_id(u16::from(id)).ok_or_else(|| {
            DbError::InvalidValue {
                reason: format!("Unsupported handshake character_set id: {id}"),
            }
        })?;
        let mut s = Self::new();
        s.client_charset = collation.charset;
        s.connection_collation = collation;
        s.results_collation = collation;
        Ok(s)
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

    fn positive_timeout_secs(&self, name: &str) -> Result<u64, DbError> {
        let raw = self
            .variables
            .get(name)
            .map(|s| s.as_str())
            .unwrap_or(match name {
                "net_write_timeout" | "net_read_timeout" => "60",
                "wait_timeout" | "interactive_timeout" => "28800",
                _ => "",
            });
        let stripped = raw.trim().trim_matches('\'').trim_matches('"');
        stripped
            .parse::<u64>()
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!("{name} must be a positive integer, got '{raw}'"),
            })
    }

    pub fn net_read_timeout_secs(&self) -> Result<u64, DbError> {
        self.positive_timeout_secs("net_read_timeout")
    }

    pub fn net_write_timeout_secs(&self) -> Result<u64, DbError> {
        self.positive_timeout_secs("net_write_timeout")
    }

    pub fn wait_timeout_secs(&self) -> Result<u64, DbError> {
        self.positive_timeout_secs("wait_timeout")
    }

    pub fn interactive_timeout_secs(&self) -> Result<u64, DbError> {
        self.positive_timeout_secs("interactive_timeout")
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
            let after_names = rest[6..].trim();
            let tokens: Vec<&str> = after_names.split_whitespace().collect();
            let charset_str = tokens
                .first()
                .copied()
                .unwrap_or("utf8mb4")
                .trim_matches('\'')
                .trim_matches('"');
            let cs = charset::lookup_charset(charset_str).ok_or_else(|| DbError::InvalidValue {
                reason: format!("Unknown character set: '{charset_str}'"),
            })?;
            // Optional: COLLATE collation_name
            let collation: &'static CollationDef = if tokens.len() >= 3
                && tokens[1].eq_ignore_ascii_case("collate")
            {
                let col_name = tokens[2].trim_matches('\'').trim_matches('"');
                let col =
                    charset::lookup_collation(col_name).ok_or_else(|| DbError::InvalidValue {
                        reason: format!("Unknown collation: '{col_name}'"),
                    })?;
                if col.charset.canonical_name != cs.canonical_name {
                    return Err(DbError::InvalidValue {
                        reason: format!(
                            "Collation '{}' is not valid for character set '{}'",
                            col.name, cs.canonical_name
                        ),
                    });
                }
                col
            } else {
                cs.default_collation
            };
            self.client_charset = cs;
            self.connection_collation = collation;
            self.results_collation = collation;
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
                "character_set_client" => {
                    let cs =
                        charset::lookup_charset(&value).ok_or_else(|| DbError::InvalidValue {
                            reason: format!("Unknown character set: '{value}'"),
                        })?;
                    self.client_charset = cs;
                }
                "character_set_connection" => {
                    let cs =
                        charset::lookup_charset(&value).ok_or_else(|| DbError::InvalidValue {
                            reason: format!("Unknown character set: '{value}'"),
                        })?;
                    self.connection_collation = cs.default_collation;
                }
                "character_set_results" => {
                    let cs =
                        charset::lookup_charset(&value).ok_or_else(|| DbError::InvalidValue {
                            reason: format!("Unknown character set: '{value}'"),
                        })?;
                    self.results_collation = cs.default_collation;
                }
                "collation_connection" => {
                    let col =
                        charset::lookup_collation(&value).ok_or_else(|| DbError::InvalidValue {
                            reason: format!("Unknown collation: '{value}'"),
                        })?;
                    self.connection_collation = col;
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
                "net_read_timeout"
                | "net_write_timeout"
                | "wait_timeout"
                | "interactive_timeout" => {
                    let candidate = raw_val.trim().trim_matches('\'').trim_matches('"');
                    match candidate.parse::<u64>() {
                        Ok(n) if n > 0 => {
                            self.variables.insert(name.clone(), n.to_string());
                        }
                        _ => {
                            return Err(DbError::InvalidValue {
                                reason: format!(
                                    "{name} must be a positive integer, got '{raw_val}'"
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
                "on_error" => {
                    let mode = parse_on_error_setting(raw_val)?;
                    self.on_error = mode;
                    self.variables
                        .insert("on_error".to_string(), on_error_mode_name(mode).to_string());
                }
                "axiom_compat" => {
                    let mode = parse_compat_mode_setting(raw_val)?;
                    self.compat_mode = mode;
                    self.variables.insert(
                        "axiom_compat".to_string(),
                        compat_mode_name(mode).to_string(),
                    );
                    // Also sync the derived collation into variables (not the typed field).
                    if self.explicit_collation.is_none() {
                        let derived = if mode == CompatMode::MySql {
                            "es"
                        } else {
                            "binary"
                        };
                        self.variables
                            .insert("collation".to_string(), derived.to_string());
                    }
                }
                "collation" => {
                    let coll = parse_session_collation_setting(raw_val)?;
                    self.explicit_collation = coll;
                    let name = match coll {
                        Some(c) => session_collation_name(c),
                        None => {
                            // DEFAULT: restore compat-derived collation name
                            if self.compat_mode == CompatMode::MySql {
                                "es"
                            } else {
                                "binary"
                            }
                        }
                    };
                    self.variables
                        .insert("collation".to_string(), name.to_string());
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

    /// Returns the current `on_error` mode for this connection.
    pub fn on_error(&self) -> OnErrorMode {
        self.on_error
    }

    /// Returns the current compatibility mode for this connection.
    pub fn compat_mode(&self) -> CompatMode {
        self.compat_mode
    }

    /// Returns the explicit session collation override, if set.
    pub fn explicit_collation(&self) -> Option<SessionCollation> {
        self.explicit_collation
    }

    /// Returns the effective session collation (explicit override > compat-derived > Binary).
    pub fn effective_collation(&self) -> SessionCollation {
        if let Some(c) = self.explicit_collation {
            return c;
        }
        match self.compat_mode {
            CompatMode::MySql => SessionCollation::Es,
            _ => SessionCollation::Binary,
        }
    }

    /// Returns the canonical name of the effective collation (`"binary"` or `"es"`).
    pub fn effective_collation_name(&self) -> &'static str {
        session_collation_name(self.effective_collation())
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
            "character_set_client" => Some(self.client_charset.canonical_name.into()),
            "character_set_connection" => {
                Some(self.connection_collation.charset.canonical_name.into())
            }
            "character_set_results" => Some(self.results_collation.charset.canonical_name.into()),
            "character_set_server" => Some("utf8mb4".into()),
            "character_set_database" => Some("utf8mb4".into()),
            "collation_connection" => Some(self.connection_collation.name.into()),
            "collation_server" => Some(DEFAULT_SERVER_COLLATION.name.into()),
            "collation_database" => Some(DEFAULT_SERVER_COLLATION.name.into()),
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

    // ── Charset accessors ──────────────────────────────────────────────────────

    /// Returns the client-side decode charset (set at handshake or by `SET NAMES`).
    pub fn client_charset(&self) -> &'static CharsetDef {
        self.client_charset
    }

    /// Returns the result collation used for outbound row and metadata encoding.
    pub fn results_collation(&self) -> &'static CollationDef {
        self.results_collation
    }

    /// Canonical name for `@@character_set_client`.
    pub fn character_set_client_name(&self) -> &'static str {
        self.client_charset.canonical_name
    }

    /// Canonical name for `@@character_set_connection`.
    pub fn character_set_connection_name(&self) -> &'static str {
        self.connection_collation.charset.canonical_name
    }

    /// Canonical name for `@@character_set_results`.
    pub fn character_set_results_name(&self) -> &'static str {
        self.results_collation.charset.canonical_name
    }

    /// Name for `@@collation_connection`.
    pub fn collation_connection_name(&self) -> &'static str {
        self.connection_collation.name
    }

    /// Decodes inbound query bytes using the negotiated client charset.
    ///
    /// Used for `COM_QUERY` and `COM_STMT_PREPARE` payloads.
    pub fn decode_client_text(&self, bytes: &[u8]) -> Result<String, DbError> {
        charset::decode_text(self.client_charset, bytes).map(|cow| cow.into_owned())
    }

    /// Decodes identifier bytes (database name, username) using the negotiated client charset.
    ///
    /// Used for handshake `username`/`database` and `COM_INIT_DB` payloads.
    pub fn decode_identifier_text(&self, bytes: &[u8]) -> Result<String, DbError> {
        charset::decode_text(self.client_charset, bytes).map(|cow| cow.into_owned())
    }

    /// Encodes a result string using the negotiated result charset.
    ///
    /// Returns `DbError::InvalidValue` if the string cannot be represented in
    /// the selected charset (e.g., emoji in `latin1` or `utf8mb3`).
    pub fn encode_result_text(&self, text: &str) -> Result<Vec<u8>, DbError> {
        charset::encode_text(self.results_collation.charset, text).map(|cow| cow.into_owned())
    }

    /// Registers a new prepared statement and returns `(stmt_id, param_count)`.
    ///
    /// `schema_version` is the current `Database::schema_version` snapshot,
    /// stored as `compiled_at_version` so that `COM_STMT_EXECUTE` can detect
    /// stale plans after DDL (Phase 5.13).
    ///
    /// If the cache is at `max_prepared_stmts` capacity, the least-recently-used
    /// statement (lowest `last_used_seq`) is evicted before inserting the new one.
    pub fn prepare_statement(
        &mut self,
        sql: String,
        schema_version: u64,
        current_database: &str,
    ) -> (u32, u16) {
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
                compiled_database: current_database.to_string(),
                last_used_seq: 0,
                pending_long_data: vec![None; param_count as usize],
                pending_long_data_error: None,
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
        assert_eq!(s.character_set_client_name(), "latin1");
        assert_eq!(
            s.get_variable("@@character_set_client"),
            Some("latin1".into())
        );
        // SET NAMES sets all three charset variables together
        assert_eq!(
            s.get_variable("@@character_set_connection"),
            Some("latin1".into())
        );
        assert_eq!(
            s.get_variable("@@character_set_results"),
            Some("latin1".into())
        );
        assert_eq!(
            s.get_variable("collation_connection"),
            Some("latin1_swedish_ci".into())
        );
    }

    #[test]
    fn test_set_names_with_collate() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET NAMES latin1 COLLATE latin1_bin").unwrap());
        assert_eq!(s.character_set_client_name(), "latin1");
        assert_eq!(
            s.get_variable("collation_connection"),
            Some("latin1_bin".into())
        );
    }

    #[test]
    fn test_set_names_invalid_charset_errors() {
        let mut s = ConnectionState::new();
        let err = s.apply_set("SET NAMES cp1251").unwrap_err();
        assert!(
            err.to_string().contains("Unknown character set"),
            "error: {err}"
        );
    }

    #[test]
    fn test_set_names_incompatible_collation_errors() {
        let mut s = ConnectionState::new();
        let err = s
            .apply_set("SET NAMES latin1 COLLATE utf8mb3_bin")
            .unwrap_err();
        assert!(
            err.to_string().contains("not valid"),
            "error must mention incompatibility: {err}"
        );
    }

    #[test]
    fn test_set_character_set_client_only() {
        let mut s = ConnectionState::new();
        // Start with utf8mb4 results
        assert_eq!(s.character_set_results_name(), "utf8mb4");
        assert!(s.apply_set("SET character_set_client = latin1").unwrap());
        assert_eq!(s.character_set_client_name(), "latin1");
        // results_collation must NOT change
        assert_eq!(s.character_set_results_name(), "utf8mb4");
    }

    #[test]
    fn test_set_character_set_results_only() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET character_set_results = latin1").unwrap());
        assert_eq!(s.character_set_results_name(), "latin1");
        // client_charset must NOT change
        assert_eq!(s.character_set_client_name(), "utf8mb4");
    }

    #[test]
    fn test_set_collation_connection() {
        let mut s = ConnectionState::new();
        assert!(s
            .apply_set("SET collation_connection = latin1_bin")
            .unwrap());
        assert_eq!(
            s.get_variable("collation_connection"),
            Some("latin1_bin".into())
        );
        // character_set_client must NOT change
        assert_eq!(s.character_set_client_name(), "utf8mb4");
    }

    #[test]
    fn test_from_handshake_collation_id_255() {
        let s = ConnectionState::from_handshake_collation_id(255).unwrap();
        assert_eq!(s.character_set_client_name(), "utf8mb4");
        assert_eq!(
            s.get_variable("collation_connection"),
            Some("utf8mb4_0900_ai_ci".into())
        );
    }

    #[test]
    fn test_from_handshake_collation_id_8_latin1() {
        let s = ConnectionState::from_handshake_collation_id(8).unwrap();
        assert_eq!(s.character_set_client_name(), "latin1");
        assert_eq!(s.character_set_results_name(), "latin1");
        assert_eq!(
            s.get_variable("collation_connection"),
            Some("latin1_swedish_ci".into())
        );
    }

    #[test]
    fn test_from_handshake_collation_id_unknown_errors() {
        let err = ConnectionState::from_handshake_collation_id(99).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported handshake"),
            "error: {err}"
        );
    }

    #[test]
    fn test_decode_client_text_utf8mb4() {
        let s = ConnectionState::new();
        let decoded = s.decode_client_text("hello".as_bytes()).unwrap();
        assert_eq!(decoded, "hello");
    }

    #[test]
    fn test_decode_identifier_latin1_euro() {
        let s = ConnectionState::from_handshake_collation_id(8).unwrap();
        // 0x80 in cp1252 = '€'
        let decoded = s.decode_identifier_text(&[0x80u8]).unwrap();
        assert_eq!(decoded, "€");
    }

    #[test]
    fn test_encode_result_text_latin1_cafe() {
        let s = ConnectionState::from_handshake_collation_id(8).unwrap();
        let encoded = s.encode_result_text("café").unwrap();
        // 'é' encodes as 0xE9 in cp1252
        assert_eq!(encoded, b"caf\xE9".to_vec());
    }

    #[test]
    fn test_encode_result_text_emoji_latin1_errors() {
        let s = ConnectionState::from_handshake_collation_id(8).unwrap();
        let err = s.encode_result_text("hello 🎉").unwrap_err();
        assert!(
            err.to_string().contains("cannot be encoded"),
            "error: {err}"
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
        let (id, _) = s.prepare_statement("SELECT 1".into(), 42, "axiomdb");
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

        let (id1, _) = s.prepare_statement("SELECT 1".into(), 0, "axiomdb");
        let (id2, _) = s.prepare_statement("SELECT 2".into(), 0, "axiomdb");
        let (id3, _) = s.prepare_statement("SELECT 3".into(), 0, "axiomdb");

        // Mark id2 as recently used (higher seq)
        s.prepared_statements.get_mut(&id1).unwrap().last_used_seq = 1;
        s.prepared_statements.get_mut(&id2).unwrap().last_used_seq = 3;
        s.prepared_statements.get_mut(&id3).unwrap().last_used_seq = 2;

        // Prepare a 4th statement — should evict id1 (seq=1, the lowest)
        let (id4, _) = s.prepare_statement("SELECT 4".into(), 0, "axiomdb");

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
            s.prepare_statement(format!("SELECT {i}"), 0, "axiomdb");
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
        let (id, _) = s.prepare_statement("SELECT ?".into(), 7, "axiomdb");
        let ps = &s.prepared_statements[&id];
        assert_eq!(ps.compiled_at_version, 7);
        assert_eq!(ps.last_used_seq, 0);
        assert!(ps.analyzed_stmt.is_none());
    }
}
