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

// ── PreparedStatement ─────────────────────────────────────────────────────────

/// A compiled prepared statement stored per-connection.
///
/// Created by `COM_STMT_PREPARE` and used by subsequent `COM_STMT_EXECUTE` calls
/// with the same `stmt_id`. Freed on `COM_STMT_CLOSE`.
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    pub stmt_id: u32,
    /// Original SQL with `?` placeholders, as sent by the client.
    pub sql_template: String,
    /// Number of `?` placeholders detected at prepare time.
    pub param_count: u16,
    /// MySQL type codes for each parameter (populated from first COM_STMT_EXECUTE).
    /// Empty until the first execution with `new_params_bound_flag = 1`.
    pub param_types: Vec<u16>,
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
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionState {
    /// Creates a connection state with MySQL-compatible defaults.
    pub fn new() -> Self {
        let mut variables = HashMap::new();
        variables.insert("time_zone".into(), "SYSTEM".into());
        variables.insert("sql_mode".into(), String::new());
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
        }
    }

    /// Applies a SET statement, updating the relevant session variable.
    ///
    /// Returns `true` if the statement was recognized (caller should send OK).
    /// Returns `false` if it should be executed by the engine instead.
    pub fn apply_set(&mut self, sql: &str) -> bool {
        let trimmed = sql.trim();
        // Only handle SET statements.
        if !trimmed.to_ascii_lowercase().starts_with("set ") {
            return false;
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
            return true;
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
                other => {
                    self.variables.insert(other.to_string(), value);
                }
            }
            return true;
        }

        // SET without '=' (e.g., SET TRANSACTION ...) — just accept
        true
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
    pub fn prepare_statement(&mut self, sql: String) -> (u32, u16) {
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
        assert!(s.apply_set("SET autocommit=0"));
        assert!(!s.autocommit);
        assert_eq!(s.get_variable("@@autocommit"), Some("0".into()));
    }

    #[test]
    fn test_set_names() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET NAMES latin1"));
        assert_eq!(s.character_set_client, "latin1");
        assert_eq!(
            s.get_variable("@@character_set_client"),
            Some("latin1".into())
        );
    }

    #[test]
    fn test_set_session_variable() {
        let mut s = ConnectionState::new();
        assert!(s.apply_set("SET @@session.time_zone = 'UTC'"));
        assert_eq!(s.get_variable("time_zone"), Some("UTC".into()));
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
}
