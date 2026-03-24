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
