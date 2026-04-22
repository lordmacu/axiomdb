// ── Multi-statement SQL splitter ─────────────────────────────────────────────

/// Splits a SQL string on `;` delimiters, returning non-empty trimmed statements.
///
/// Respects single-quoted string literals: a `;` inside `'...'` is not treated
/// as a statement separator. Backslash-escaped quotes `\'` inside strings are
/// handled correctly.
///
/// Strips a trailing `;` on the last statement (common in SQL scripts).
/// Returns `[sql]` unchanged if there is only one statement.
fn split_sql_statements(sql: &str, ansi_quotes: bool) -> Vec<&str> {
    super::sql_scan::split_sql_statements(sql, ansi_quotes)
}

// ── Group commit helper ───────────────────────────────────────────────────────

/// Awaits fsync confirmation from the WAL fsync pipeline.
///
/// - `None` → the transaction was read-only or the current connection was the
///   leader / expired follower; returns `Ok(())` immediately.
/// - `Some(rx)` → waits for the leader to fsync and confirm;
///   returns `Ok(())` on success or `Err(WalGroupCommitFailed)` on failure.
///
/// Must be called **after** the `Database` lock has been released so that
/// other connections can proceed while this one awaits the fsync.
async fn await_commit_rx(rx: Option<CommitRx>) -> Result<(), DbError> {
    match rx {
        None => Ok(()),
        Some(rx) => rx.await.unwrap_or_else(|_| {
            Err(DbError::WalGroupCommitFailed {
                message: "fsync pipeline leader dropped before fsync".into(),
            })
        }),
    }
}

// ── ORM / driver query interception ──────────────────────────────────────────

/// Returns pre-computed responses for queries that MySQL drivers and ORMs send
/// automatically on connect — before any user SQL is executed.
///
/// Without these stubs, most clients (PyMySQL, SQLAlchemy, ActiveRecord, etc.)
/// fail to connect because they receive ERR packets for these mandatory queries.
///
/// `status` is used by `SHOW STATUS` to build the live counter rowset (5.9c).
fn intercept_special_query(
    sql: &str,
    conn_state: &mut ConnectionState,
    status: &Arc<StatusRegistry>,
    registry: &super::processlist::Registry,
) -> InterceptResult {
    use super::packets::build_ok_packet;
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let lower = sql.trim().to_ascii_lowercase();

    // ── SET statements ────────────────────────────────────────────────────────
    if lower.starts_with("set ") {
        conn_state.apply_set(sql)?;
        return Ok(Some(vec![(1u8, build_ok_packet(0, 0, 0))]));
    }

    // ── SELECT @@variable (single-variable form) ──────────────────────────────
    // Handles: SELECT @@x, SELECT @@session.x, SELECT @@x AS alias
    // @@in_transaction is NOT handled here — it requires live txn state and is
    // intercepted in database.execute_query() instead.
    if lower.starts_with("select @@") || lower.starts_with("select @@session.") {
        // Extract the variable name (stop at whitespace, comma, or 'as')
        let rest = lower
            .trim_start_matches("select ")
            .trim_start_matches("@@session.")
            .trim_start_matches("@@");
        let varname = rest
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .next()
            .unwrap_or("");
        // Let @@in_transaction fall through to database.execute_query().
        if varname == "in_transaction" {
            return Ok(None);
        }
        if let Some(val) = conn_state.get_variable(varname) {
            return Ok(Some(single_text_row(varname, &val)));
        }
        // Unknown @@variable → return NULL (not an error)
        return Ok(Some(single_null_row(varname)));
    }

    // ── SELECT version() / VERSION() ─────────────────────────────────────────
    if lower == "select version()" || lower.starts_with("select version()") {
        return Ok(Some(single_text_row("version()", "8.0.36-AxiomDB-0.1.0")));
    }

    // ── SELECT @@version mixed with other vars ────────────────────────────────
    if lower.contains("@@version") && !lower.contains("from ") {
        return Ok(Some(single_text_row("@@version", "8.0.36-AxiomDB-0.1.0")));
    }

    // ── SELECT DATABASE() / current_database() ────────────────────────────────
    if lower.contains("database()") || lower.contains("current_database()") {
        if conn_state.current_database.is_empty() {
            return Ok(Some(single_null_row("DATABASE()")));
        }
        return Ok(Some(single_text_row(
            "DATABASE()",
            &conn_state.current_database.clone(),
        )));
    }

    // SHOW WARNINGS / SHOW ERRORS are handled in database.execute_query()
    // where session.warnings is accessible. Do NOT intercept here.

    // ── SHOW VARIABLES ────────────────────────────────────────────────────────
    if lower.starts_with("show") && lower.contains("variables") {
        return Ok(Some(show_variables_result(&lower, conn_state)));
    }

    // ── SHOW [GLOBAL|SESSION|LOCAL] STATUS [LIKE '...'] (5.9c) ───────────────
    if lower.starts_with("show") {
        use super::status::{build_status_rows, parse_show_status};
        if let Some(query) = parse_show_status(&lower) {
            let qr = build_status_rows(&query, status, &conn_state.session_status);
            return Ok(Some(
                serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
                    .expect("utf8mb4 encoding always valid for ASCII data"),
            ));
        }
    }

    // ── SHOW [FULL] PROCESSLIST (GAP-B.7) ────────────────────────────────────
    // Reads the shared connection registry maintained by every handler's
    // ProcesslistGuard. `FULL` extends the Info column from MySQL's usual
    // 100-char truncation to unlimited — we always emit the full text.
    if lower.starts_with("show") && lower.contains("processlist") {
        let cols = vec![
            ColumnMeta::computed("Id".to_string(), DataType::BigInt),
            ColumnMeta::computed("User".to_string(), DataType::Text),
            ColumnMeta::computed("Host".to_string(), DataType::Text),
            ColumnMeta::computed("db".to_string(), DataType::Text),
            ColumnMeta::computed("Command".to_string(), DataType::Text),
            ColumnMeta::computed("Time".to_string(), DataType::BigInt),
            ColumnMeta::computed("State".to_string(), DataType::Text),
            ColumnMeta::computed("Info".to_string(), DataType::Text),
        ];
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let snapshot = super::processlist::snapshot(registry);
        let rows: Vec<Vec<Value>> = snapshot
            .into_iter()
            .map(|info| {
                vec![
                    Value::BigInt(info.id as i64),
                    Value::Text(info.user),
                    Value::Text(info.host),
                    info.db.map(Value::Text).unwrap_or(Value::Null),
                    Value::Text(info.command),
                    Value::BigInt((now - info.command_started_at).max(0)),
                    info.state.map(Value::Text).unwrap_or(Value::Null),
                    info.info.map(Value::Text).unwrap_or(Value::Null),
                ]
            })
            .collect();
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        return Ok(Some(
            serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
                .expect("utf8mb4 encoding always valid for ASCII data"),
        ));
    }

    Ok(None)
}

/// Builds a SHOW VARIABLES result filtered by the LIKE pattern in `lower`.
fn show_variables_result(lower: &str, conn_state: &ConnectionState) -> Vec<(u8, Vec<u8>)> {
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let cols = vec![
        ColumnMeta::computed("Variable_name".to_string(), DataType::Text),
        ColumnMeta::computed("Value".to_string(), DataType::Text),
    ];

    let sql_mode_val = conn_state
        .get_variable("sql_mode")
        .unwrap_or_else(|| "STRICT_TRANS_TABLES".into());
    let strict_mode_val = conn_state
        .get_variable("strict_mode")
        .unwrap_or_else(|| "ON".into());
    let on_error_val = conn_state
        .get_variable("on_error")
        .unwrap_or_else(|| "rollback_statement".into());

    let all_vars: Vec<(&str, String)> = vec![
        // Alphabetical order — matches MySQL SHOW VARIABLES output order.
        ("axiom_compat", conn_state.compat_mode().to_string()),
        (
            "character_set_client",
            conn_state.character_set_client_name().into(),
        ),
        (
            "character_set_connection",
            conn_state.character_set_connection_name().into(),
        ),
        ("character_set_database", "utf8mb4".into()),
        (
            "character_set_results",
            conn_state.character_set_results_name().into(),
        ),
        ("character_set_server", "utf8mb4".into()),
        ("character_set_system", "utf8mb3".into()),
        (
            "collation",
            conn_state.effective_collation_name().to_string(),
        ),
        (
            "collation_connection",
            conn_state.collation_connection_name().into(),
        ),
        ("collation_database", "utf8mb4_0900_ai_ci".into()),
        ("collation_server", "utf8mb4_0900_ai_ci".into()),
        ("on_error", on_error_val),
        ("sql_mode", sql_mode_val),
        ("strict_mode", strict_mode_val),
    ];

    // Extract LIKE pattern if present.
    // Use real SQL wildcard semantics (% and _) instead of substring matching.
    let like_pattern: Option<String> = if lower.contains("like") {
        lower.split("like").nth(1).map(|s| {
            s.trim()
                .trim_matches('\'')
                .trim_matches('"')
                .to_ascii_lowercase()
        })
    } else {
        None
    };

    let rows: Vec<Vec<Value>> = all_vars
        .into_iter()
        .filter(|(name, _)| {
            if let Some(ref pat) = like_pattern {
                // Real SQL LIKE wildcards (% = any sequence, _ = any char).
                axiomdb_sql::like_match(name, pat)
            } else {
                true
            }
        })
        .map(|(name, val)| vec![Value::Text(name.into()), Value::Text(val)])
        .collect();

    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
        .expect("utf8mb4 encoding always valid for ASCII data")
}

/// Builds a single-column, single-row text result set.
fn single_text_row(col_name: &str, value: &str) -> Vec<(u8, Vec<u8>)> {
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let cols = vec![ColumnMeta::computed(col_name.to_string(), DataType::Text)];
    let rows = vec![vec![Value::Text(value.into())]];
    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
        .expect("utf8mb4 encoding always valid for ASCII data")
}

/// Builds a single-column, single-row result set with a NULL value.
/// Used for unknown @@variables that should return NULL instead of an error.
fn single_null_row(col_name: &str) -> Vec<(u8, Vec<u8>)> {
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let cols = vec![ColumnMeta::computed(col_name.to_string(), DataType::Text)];
    let rows = vec![vec![Value::Null]];
    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
        .expect("utf8mb4 encoding always valid for ASCII data")
}

#[cfg(test)]
mod intercept_tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use super::intercept_special_query;
    use crate::mysql::processlist::Registry;
    use crate::mysql::session::ConnectionState;
    use crate::mysql::status::StatusRegistry;

    #[test]
    fn show_table_status_is_not_captured_by_show_status_intercept() {
        let mut conn = ConnectionState::default();
        let status = Arc::new(StatusRegistry::default());
        let registry: Registry = Arc::new(RwLock::new(HashMap::new()));

        let intercepted = intercept_special_query(
            "SHOW TABLE STATUS LIKE 'users'",
            &mut conn,
            &status,
            &registry,
        )
        .expect("intercept should not error");

        assert!(
            intercepted.is_none(),
            "SHOW TABLE STATUS must fall through to the SQL executor"
        );
    }
}
