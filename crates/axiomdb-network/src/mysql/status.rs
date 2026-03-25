//! SHOW STATUS subsystem — subfase 5.9c.
//!
//! Provides server-wide and per-connection status counters queryable through:
//!
//! - `SHOW STATUS` (session scope)
//! - `SHOW GLOBAL STATUS`
//! - `SHOW SESSION STATUS`
//! - `SHOW LOCAL STATUS`
//! - any of the above with `LIKE 'pattern'`
//!
//! ## Architecture
//!
//! `Database` owns `Arc<StatusRegistry>`. `handle_connection` clones that
//! `Arc` once per connection (same pattern as `schema_version`) so that
//! `SHOW STATUS` never needs to take the `Database` mutex.
//!
//! `ConnectionState` owns `SessionStatus` with per-connection cumulative
//! counters. `COM_RESET_CONNECTION` resets it automatically because it
//! recreates `ConnectionState::new()`.
//!
//! LIKE filtering reuses the already-tested `like_match` from
//! `axiomdb-sql` — proper `%` / `_` wildcard semantics, not substring.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axiomdb_sql::like_match;
use axiomdb_sql::result::{ColumnMeta, QueryResult};
use axiomdb_types::{DataType, Value};

// ── StatusRegistry — global server-wide counters ──────────────────────────────

/// Server-wide status counters.
///
/// All fields use `AtomicU64` because they are updated by multiple connection
/// tasks concurrently. Relaxed ordering is sufficient — these are telemetry
/// values, not correctness guards.
pub struct StatusRegistry {
    started_at: Instant,
    pub threads_connected: AtomicU64,
    pub threads_running: AtomicU64,
    pub questions: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub com_select: AtomicU64,
    pub com_insert: AtomicU64,
    /// Best-effort compatibility counter for logical page-read requests.
    /// AxiomDB uses `MmapStorage`, not an InnoDB buffer pool — this counter
    /// approximates storage access frequency until a dedicated storage metrics
    /// hook is added.
    pub innodb_buffer_pool_read_requests: AtomicU64,
    /// Best-effort compatibility counter for physical page reads.
    pub innodb_buffer_pool_reads: AtomicU64,
}

impl StatusRegistry {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            threads_connected: AtomicU64::new(0),
            threads_running: AtomicU64::new(0),
            questions: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            com_select: AtomicU64::new(0),
            com_insert: AtomicU64::new(0),
            innodb_buffer_pool_read_requests: AtomicU64::new(0),
            innodb_buffer_pool_reads: AtomicU64::new(0),
        }
    }

    /// Whole seconds since the status subsystem was initialized.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

impl Default for StatusRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── SessionStatus — per-connection cumulative counters ────────────────────────

/// Per-connection cumulative status counters.
///
/// Owned by `ConnectionState`. Plain integers — only one task owns them.
/// Reset automatically when `COM_RESET_CONNECTION` recreates `ConnectionState`.
#[derive(Debug, Default, Clone)]
pub struct SessionStatus {
    pub questions: u64,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub com_select: u64,
    pub com_insert: u64,
}

// ── RAII guards ───────────────────────────────────────────────────────────────

/// Increments `threads_connected` on creation, decrements on drop.
///
/// Create this once after authentication succeeds. Rust's drop guarantees
/// the decrement runs even on network errors or early returns.
pub struct ConnectedGuard(Arc<StatusRegistry>);

impl ConnectedGuard {
    pub fn new(registry: Arc<StatusRegistry>) -> Self {
        registry.threads_connected.fetch_add(1, Ordering::Relaxed);
        ConnectedGuard(registry)
    }
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        // Saturating to guard against any unexpected double-drop scenario.
        let prev = self.0.threads_connected.fetch_sub(1, Ordering::Relaxed);
        if prev == 0 {
            self.0.threads_connected.store(0, Ordering::Relaxed);
        }
    }
}

/// Increments `threads_running` on creation, decrements on drop.
///
/// Create this at the start of each command execution block (COM_QUERY,
/// COM_STMT_EXECUTE, COM_STMT_PREPARE). Dropped at the end of the block,
/// including on error or early return — no drift on panics or broken sockets.
pub struct RunningGuard(Arc<StatusRegistry>);

impl RunningGuard {
    pub fn new(registry: &Arc<StatusRegistry>) -> Self {
        registry.threads_running.fetch_add(1, Ordering::Relaxed);
        RunningGuard(Arc::clone(registry))
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let prev = self.0.threads_running.fetch_sub(1, Ordering::Relaxed);
        if prev == 0 {
            self.0.threads_running.store(0, Ordering::Relaxed);
        }
    }
}

// ── Statement classification ──────────────────────────────────────────────────

/// Classifies a SQL statement for `Com_select` / `Com_insert` counters.
///
/// Classification is intentionally shallow: only the leading keyword matters
/// because only `SELECT` and `INSERT` are tracked in this subphase.
pub enum SqlCommandClass {
    Select,
    Insert,
    Other,
}

impl SqlCommandClass {
    pub fn from_sql(sql: &str) -> Self {
        let s = sql.trim();
        if s.len() >= 6 {
            let prefix = s[..6].to_ascii_lowercase();
            if prefix == "select" {
                return SqlCommandClass::Select;
            }
            if prefix == "insert" {
                return SqlCommandClass::Insert;
            }
        }
        SqlCommandClass::Other
    }
}

// ── SHOW STATUS query parsing ─────────────────────────────────────────────────

/// Scope for a `SHOW STATUS` query.
pub enum StatusScope {
    Global,
    Session,
}

/// Parsed representation of a `SHOW [GLOBAL|SESSION|LOCAL] STATUS [LIKE 'pat']`.
pub struct ShowStatusQuery {
    pub scope: StatusScope,
    pub like_pattern: Option<String>,
}

/// Parses a lowercased SQL string as `SHOW [scope] STATUS [LIKE 'pattern']`.
///
/// Returns `None` if the string does not match the expected form.
pub fn parse_show_status(lower: &str) -> Option<ShowStatusQuery> {
    let s = lower.trim();
    let rest = s.strip_prefix("show")?.trim();

    // Optional scope keyword.
    let (scope, rest) = if let Some(r) = rest.strip_prefix("global") {
        (StatusScope::Global, r.trim())
    } else if let Some(r) = rest.strip_prefix("session") {
        (StatusScope::Session, r.trim())
    } else if let Some(r) = rest.strip_prefix("local") {
        (StatusScope::Session, r.trim())
    } else {
        (StatusScope::Session, rest)
    };

    // Must have "status".
    let rest = rest.strip_prefix("status")?.trim();

    // Optional trailing semicolon.
    let rest = rest.trim_end_matches(';').trim();

    // Optional LIKE 'pattern'.
    let like_pattern = if let Some(r) = rest.strip_prefix("like") {
        let pat = r.trim().trim_matches('\'').trim_matches('"').to_string();
        Some(pat)
    } else {
        None
    };

    Some(ShowStatusQuery {
        scope,
        like_pattern,
    })
}

// ── Canonical variable list and row builder ────────────────────────────────────

/// Canonical status variable names in ascending order.
///
/// The order is deterministic and matches MySQL's alphabetical ordering.
const STATUS_VARS: &[&str] = &[
    "Bytes_received",
    "Bytes_sent",
    "Com_insert",
    "Com_select",
    "Innodb_buffer_pool_read_requests",
    "Innodb_buffer_pool_reads",
    "Questions",
    "Threads_connected",
    "Threads_running",
    "Uptime",
];

/// Builds a `QueryResult::Rows` for `SHOW [scope] STATUS [LIKE 'pattern']`.
///
/// - Session scope returns per-connection values for `Questions`, `Bytes_*`,
///   and `Com_*`; `Threads_running` is always `1` (current connection is
///   actively processing); all other variables come from the global registry.
/// - Global scope returns server-wide registry values for all variables.
///
/// LIKE uses proper SQL wildcard semantics (`%` / `_`) via `like_match` from
/// `axiomdb-sql`. Case-insensitive against variable names.
pub fn build_status_rows(
    query: &ShowStatusQuery,
    registry: &StatusRegistry,
    sess: &SessionStatus,
) -> QueryResult {
    let cols = vec![
        ColumnMeta::computed("Variable_name".to_string(), DataType::Text),
        ColumnMeta::computed("Value".to_string(), DataType::Text),
    ];

    let rows: Vec<Vec<Value>> = STATUS_VARS
        .iter()
        .filter_map(|&name| {
            if let Some(ref pat) = query.like_pattern {
                let name_lc = name.to_ascii_lowercase();
                let pat_lc = pat.to_ascii_lowercase();
                if !like_match(&name_lc, &pat_lc) {
                    return None;
                }
            }
            let value = variable_value(name, query, registry, sess);
            Some(vec![Value::Text(name.into()), Value::Text(value)])
        })
        .collect();

    QueryResult::Rows {
        columns: cols,
        rows,
    }
}

fn variable_value(
    name: &str,
    query: &ShowStatusQuery,
    registry: &StatusRegistry,
    sess: &SessionStatus,
) -> String {
    let is_session = matches!(query.scope, StatusScope::Session);
    match name {
        "Bytes_received" => {
            if is_session {
                sess.bytes_received.to_string()
            } else {
                registry.bytes_received.load(Ordering::Relaxed).to_string()
            }
        }
        "Bytes_sent" => {
            if is_session {
                sess.bytes_sent.to_string()
            } else {
                registry.bytes_sent.load(Ordering::Relaxed).to_string()
            }
        }
        "Com_insert" => {
            if is_session {
                sess.com_insert.to_string()
            } else {
                registry.com_insert.load(Ordering::Relaxed).to_string()
            }
        }
        "Com_select" => {
            if is_session {
                sess.com_select.to_string()
            } else {
                registry.com_select.load(Ordering::Relaxed).to_string()
            }
        }
        "Innodb_buffer_pool_read_requests" => registry
            .innodb_buffer_pool_read_requests
            .load(Ordering::Relaxed)
            .to_string(),
        "Innodb_buffer_pool_reads" => registry
            .innodb_buffer_pool_reads
            .load(Ordering::Relaxed)
            .to_string(),
        "Questions" => {
            if is_session {
                sess.questions.to_string()
            } else {
                registry.questions.load(Ordering::Relaxed).to_string()
            }
        }
        "Threads_connected" => registry
            .threads_connected
            .load(Ordering::Relaxed)
            .to_string(),
        "Threads_running" => {
            if is_session {
                // The current connection is actively processing the statement.
                "1".to_string()
            } else {
                registry.threads_running.load(Ordering::Relaxed).to_string()
            }
        }
        "Uptime" => registry.uptime_secs().to_string(),
        _ => "0".to_string(),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> Arc<StatusRegistry> {
        Arc::new(StatusRegistry::new())
    }

    fn make_sess() -> SessionStatus {
        SessionStatus::default()
    }

    // ── parse_show_status ─────────────────────────────────────────────────────

    #[test]
    fn show_status_defaults_to_session() {
        let q = parse_show_status("show status").unwrap();
        assert!(matches!(q.scope, StatusScope::Session));
        assert!(q.like_pattern.is_none());
    }

    #[test]
    fn show_global_status_parses_as_global() {
        let q = parse_show_status("show global status").unwrap();
        assert!(matches!(q.scope, StatusScope::Global));
    }

    #[test]
    fn show_session_status_parses_as_session() {
        let q = parse_show_status("show session status").unwrap();
        assert!(matches!(q.scope, StatusScope::Session));
    }

    #[test]
    fn show_local_status_parses_as_session() {
        let q = parse_show_status("show local status").unwrap();
        assert!(matches!(q.scope, StatusScope::Session));
    }

    #[test]
    fn show_status_with_like_extracts_pattern() {
        let q = parse_show_status("show status like 'Com_%'").unwrap();
        assert_eq!(q.like_pattern.as_deref(), Some("Com_%"));
    }

    #[test]
    fn show_status_non_matching_returns_none() {
        assert!(parse_show_status("show variables").is_none());
        assert!(parse_show_status("select 1").is_none());
    }

    // ── build_status_rows ─────────────────────────────────────────────────────

    #[test]
    fn rows_have_correct_column_names() {
        let q = parse_show_status("show status").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { columns, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(columns[0].name, "Variable_name");
        assert_eq!(columns[1].name, "Value");
    }

    #[test]
    fn rows_are_in_deterministic_canonical_order() {
        let q = parse_show_status("show status").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        let names: Vec<&str> = rows
            .iter()
            .map(|row| {
                if let Value::Text(s) = &row[0] {
                    s.as_str()
                } else {
                    ""
                }
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "rows must be in ascending order");
    }

    #[test]
    fn like_unknown_pattern_returns_zero_rows() {
        let q = parse_show_status("show status like 'x'").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert!(rows.is_empty());
    }

    #[test]
    fn like_com_percent_returns_com_insert_and_com_select() {
        let q = parse_show_status("show status like 'com_%'").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        let names: Vec<&str> = rows
            .iter()
            .map(|row| {
                if let Value::Text(s) = &row[0] {
                    s.as_str()
                } else {
                    ""
                }
            })
            .collect();
        assert!(names.contains(&"Com_insert"), "Com_insert must be present");
        assert!(names.contains(&"Com_select"), "Com_select must be present");
        assert_eq!(names.len(), 2, "only Com_insert and Com_select");
    }

    #[test]
    fn like_com_inser_underscore_returns_only_com_insert() {
        let q = parse_show_status("show status like 'com_inser_'").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Text("Com_insert".into()));
    }

    #[test]
    fn like_threads_percent_is_case_insensitive() {
        let q = parse_show_status("show status like 'threads%'").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        let names: Vec<&str> = rows
            .iter()
            .map(|row| {
                if let Value::Text(s) = &row[0] {
                    s.as_str()
                } else {
                    ""
                }
            })
            .collect();
        assert!(names.contains(&"Threads_connected"));
        assert!(names.contains(&"Threads_running"));
    }

    #[test]
    fn session_snapshot_uses_session_counters_for_questions() {
        let registry = make_registry();
        registry.questions.store(100, Ordering::Relaxed);
        let mut sess = make_sess();
        sess.questions = 3;
        let q = parse_show_status("show session status like 'questions'").unwrap();
        let r = build_status_rows(&q, &registry, &sess);
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("3".into()));
    }

    #[test]
    fn global_snapshot_uses_registry_for_questions() {
        let registry = make_registry();
        registry.questions.store(100, Ordering::Relaxed);
        let mut sess = make_sess();
        sess.questions = 3;
        let q = parse_show_status("show global status like 'questions'").unwrap();
        let r = build_status_rows(&q, &registry, &sess);
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(rows[0][1], Value::Text("100".into()));
    }

    #[test]
    fn session_threads_running_is_always_one() {
        let q = parse_show_status("show session status like 'threads_running'").unwrap();
        let r = build_status_rows(&q, &make_registry(), &make_sess());
        let QueryResult::Rows { rows, .. } = r else {
            panic!("expected Rows")
        };
        assert_eq!(rows[0][1], Value::Text("1".into()));
    }

    #[test]
    fn fresh_session_status_starts_at_zero() {
        let sess = make_sess();
        assert_eq!(sess.questions, 0);
        assert_eq!(sess.bytes_received, 0);
        assert_eq!(sess.bytes_sent, 0);
        assert_eq!(sess.com_select, 0);
        assert_eq!(sess.com_insert, 0);
    }

    // ── RAII guards ───────────────────────────────────────────────────────────

    #[test]
    fn running_guard_increments_and_decrements() {
        let r = make_registry();
        assert_eq!(r.threads_running.load(Ordering::Relaxed), 0);
        {
            let _g = RunningGuard::new(&r);
            assert_eq!(r.threads_running.load(Ordering::Relaxed), 1);
        }
        assert_eq!(r.threads_running.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn connected_guard_increments_and_decrements() {
        let r = make_registry();
        assert_eq!(r.threads_connected.load(Ordering::Relaxed), 0);
        {
            let _g = ConnectedGuard::new(Arc::clone(&r));
            assert_eq!(r.threads_connected.load(Ordering::Relaxed), 1);
        }
        assert_eq!(r.threads_connected.load(Ordering::Relaxed), 0);
    }

    // ── SqlCommandClass ───────────────────────────────────────────────────────

    #[test]
    fn classify_select() {
        assert!(matches!(
            SqlCommandClass::from_sql("SELECT * FROM t"),
            SqlCommandClass::Select
        ));
        assert!(matches!(
            SqlCommandClass::from_sql("  select 1"),
            SqlCommandClass::Select
        ));
    }

    #[test]
    fn classify_insert() {
        assert!(matches!(
            SqlCommandClass::from_sql("INSERT INTO t VALUES (1)"),
            SqlCommandClass::Insert
        ));
    }

    #[test]
    fn classify_other() {
        assert!(matches!(
            SqlCommandClass::from_sql("UPDATE t SET x=1"),
            SqlCommandClass::Other
        ));
        assert!(matches!(
            SqlCommandClass::from_sql("SHOW STATUS"),
            SqlCommandClass::Other
        ));
    }
}
