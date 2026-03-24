//! MySQL connection handler — handshake → auth → command loop.
//!
//! Each accepted TCP connection runs this handler in its own Tokio task.
//! The handler implements the MySQL connection lifecycle:
//!
//! ```text
//! Server → HandshakeV10
//! Client → HandshakeResponse41
//! Server → OK (auth success) or ERR (auth failure)
//! LOOP:
//!   Client → COM_QUERY | COM_PING | COM_QUIT | COM_INIT_DB
//!   Server → result set | OK | ERR
//! ```

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use axiomdb_core::error::DbError;
use axiomdb_sql::{ast::Stmt, result::ColumnMeta, SchemaCache, SessionContext};
use axiomdb_types::DataType;

use super::database::CommitRx;

use super::result::serialize_query_result_multi;
use super::{
    auth::{gen_challenge, is_allowed_user, verify_native_password, verify_sha256_password},
    codec::MySqlCodec,
    database::Database,
    error::dberror_to_mysql,
    packets::{
        build_auth_more_data, build_err_packet, build_ok_packet, build_server_greeting,
        parse_handshake_response,
    },
    prepared::{
        build_prepare_response, parse_execute_packet, substitute_params, substitute_params_in_ast,
    },
    result::serialize_query_result,
    session::ConnectionState,
};

/// Handles one MySQL connection from handshake to disconnection.
pub async fn handle_connection(stream: TcpStream, db: Arc<Mutex<Database>>, conn_id: u32) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    info!(conn_id, %peer, "connection accepted");

    let (reader, writer) = stream.into_split();
    let mut reader = FramedRead::new(reader, MySqlCodec);
    let mut writer = FramedWrite::new(writer, MySqlCodec);

    // ── Phase 1: Send Server Greeting ─────────────────────────────────────────
    // Advertise caching_sha2_password for MySQL 8.0+ client compatibility.
    // mysql_native_password clients also accepted (plugin negotiated per-connection).
    let challenge = gen_challenge();
    let greeting = build_server_greeting(conn_id, &challenge, "caching_sha2_password");
    if writer.send((0u8, greeting.as_slice())).await.is_err() {
        return;
    }

    // ── Phase 2: Receive HandshakeResponse41 ──────────────────────────────────
    let (_, payload) = match reader.next().await {
        Some(Ok(p)) => p,
        _ => {
            warn!(conn_id, "client disconnected during handshake");
            return;
        }
    };

    let response = match parse_handshake_response(&payload) {
        Some(r) => r,
        None => {
            warn!(conn_id, "malformed HandshakeResponse41");
            let err = build_err_packet(1045, b"28000", "Malformed handshake packet");
            let _ = writer.send((2u8, err.as_slice())).await;
            return;
        }
    };

    let plugin = response
        .auth_plugin_name
        .as_deref()
        .unwrap_or("caching_sha2_password");
    debug!(conn_id, username = %response.username, %plugin, "auth attempt");

    // ── Phase 3: Authenticate ─────────────────────────────────────────────────
    if !is_allowed_user(&response.username) {
        warn!(conn_id, username = %response.username, "user not allowed");
        let err = build_err_packet(
            1045,
            b"28000",
            &format!("Access denied for user '{}'", response.username),
        );
        let _ = writer.send((2u8, err.as_slice())).await;
        return;
    }

    // Phase 5 permissive: accept all allowed users regardless of password.
    // Real auth in Phase 13.
    if plugin.contains("caching_sha2") {
        // caching_sha2_password fast-auth sequence (4 packets total):
        //   seq=0: Server → HandshakeV10
        //   seq=1: Client → HandshakeResponse41
        //   seq=2: Server → AuthMoreData(0x03)  ← fast_auth_success
        //   seq=3: Client → empty ack (pymysql sends b"" to confirm)
        //   seq=4: Server → OK_Packet
        let _ = verify_sha256_password(&challenge, &response.auth_response);

        // Send AuthMoreData(0x03) = fast_auth_success.
        // Then send OK immediately at seq=3 — pymysql reads it directly
        // without sending an ack first. The previous ack-read caused a
        // deadlock: server waited for ack, pymysql waited for OK.
        let more_data = build_auth_more_data(0x03);
        if writer.send((2u8, more_data.as_slice())).await.is_err() {
            return;
        }

        let ok = build_ok_packet(0, 0, 0);
        if writer.send((3u8, ok.as_slice())).await.is_err() {
            return;
        }
    } else {
        // mysql_native_password (or unknown plugin): send OK directly (seq=2).
        let _ = verify_native_password("", &challenge, &response.auth_response);
        let ok = build_ok_packet(0, 0, 0);
        if writer.send((2u8, ok.as_slice())).await.is_err() {
            return;
        }
    }

    info!(conn_id, username = %response.username, %plugin, "authenticated");

    // ── Phase 4: Command loop ─────────────────────────────────────────────────
    let mut session = SessionContext::new();
    // Per-connection schema cache — avoids repeated catalog heap scans for the
    // same table across queries. Warm on second query to the same table.
    // Automatically invalidated by analyze_cached() on DDL statements.
    let mut schema_cache = SchemaCache::new();

    // Clone Arc<AtomicU64> once per connection — no lock needed to read it.
    // Used in COM_STMT_EXECUTE to detect stale prepared statement plans (5.13).
    let schema_version: Arc<AtomicU64> = {
        let guard = db.lock().await;
        Arc::clone(&guard.schema_version)
    };

    let mut conn_state = ConnectionState::new();

    // Populate initial current_database from handshake (if client sent one).
    if let Some(ref db) = response.database {
        conn_state.current_database = db.clone();
    }

    loop {
        let (_, payload) = match reader.next().await {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                debug!(conn_id, err = %e, "read error");
                break;
            }
            None => {
                debug!(conn_id, "client disconnected");
                break;
            }
        };

        if payload.is_empty() {
            break;
        }

        let cmd = payload[0];
        let body = &payload[1..];

        match cmd {
            // COM_QUIT
            0x01 => {
                debug!(conn_id, "COM_QUIT");
                break;
            }

            // COM_INIT_DB (USE database)
            0x02 => {
                let db_name = String::from_utf8_lossy(body).trim().to_string();
                debug!(conn_id, db = %db_name, "COM_INIT_DB");
                conn_state.current_database = db_name;
                let ok = build_ok_packet(0, 0, 0);
                if writer.send((1u8, ok.as_slice())).await.is_err() {
                    break;
                }
            }

            // COM_QUERY
            0x03 => {
                let sql = match std::str::from_utf8(body) {
                    Ok(s) => s.trim(),
                    Err(_) => {
                        let err = build_err_packet(1064, b"42000", "Query is not valid UTF-8");
                        let _ = writer.send((1u8, err.as_slice())).await;
                        continue;
                    }
                };
                debug!(conn_id, %sql, "COM_QUERY");

                // Intercept queries that ORMs/clients send automatically on connect.
                if let Some(packets) = intercept_special_query(sql, &mut conn_state) {
                    if send_packets(&mut writer, &packets).await.is_err() {
                        break;
                    }
                    continue;
                }

                // Split on ';' to support multi-statement COM_QUERY (Phase 5.12).
                // Each non-empty statement is executed and its result set sent
                // with SERVER_MORE_RESULTS_EXISTS in the final EOF/OK, except the
                // last statement which uses normal status flags.
                let stmts: Vec<&str> = split_sql_statements(sql);
                let stmt_count = stmts.len();
                let mut seq: u8 = 1;
                let mut connection_broken = false;

                'stmts: for (idx, stmt_sql) in stmts.into_iter().enumerate() {
                    let is_last = idx == stmt_count - 1;

                    if let Some(packets) = intercept_special_query(stmt_sql, &mut conn_state) {
                        if send_packets(&mut writer, &packets).await.is_err() {
                            connection_broken = true;
                            break 'stmts;
                        }
                        if !packets.is_empty() {
                            seq = packets
                                .last()
                                .map(|(s, _)| s.wrapping_add(1))
                                .unwrap_or(seq);
                        }
                        continue 'stmts;
                    }

                    let exec_result = {
                        let mut guard = db.lock().await;
                        guard.execute_query(stmt_sql, &mut session, &mut schema_cache)
                    };

                    match exec_result {
                        Ok((qr, commit_rx)) => {
                            if let Err(e) = await_commit_rx(commit_rx).await {
                                let me = dberror_to_mysql(&e);
                                debug!(conn_id, code = me.code, msg = %me.message, "commit error");
                                let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                                if writer.send((seq, pkt.as_slice())).await.is_err() {
                                    connection_broken = true;
                                }
                                break 'stmts;
                            }
                            let packets = serialize_query_result_multi(qr, seq, !is_last);
                            seq = packets
                                .last()
                                .map(|(s, _)| s.wrapping_add(1))
                                .unwrap_or(seq);
                            if send_packets(&mut writer, &packets).await.is_err() {
                                connection_broken = true;
                                break 'stmts;
                            }
                        }
                        Err(e) => {
                            let me = dberror_to_mysql(&e);
                            debug!(conn_id, code = me.code, msg = %me.message, "query error");
                            let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                            if writer.send((seq, pkt.as_slice())).await.is_err() {
                                connection_broken = true;
                            }
                            break 'stmts;
                        }
                    }
                }

                if connection_broken {
                    break;
                }
            }

            // COM_PING
            0x0e => {
                let ok = build_ok_packet(0, 0, 0);
                if writer.send((1u8, ok.as_slice())).await.is_err() {
                    break;
                }
            }

            // COM_RESET_CONNECTION
            0x1f => {
                session = SessionContext::new();
                conn_state = ConnectionState::new();
                let ok = build_ok_packet(0, 0, 0);
                if writer.send((1u8, ok.as_slice())).await.is_err() {
                    break;
                }
            }

            // COM_STMT_PREPARE — parse+analyze once and cache the result.
            0x16 => {
                let sql = match std::str::from_utf8(body) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => {
                        let e = build_err_packet(1064, b"42000", "Invalid UTF-8 in prepare");
                        let _ = writer.send((1u8, e.as_slice())).await;
                        continue;
                    }
                };
                debug!(conn_id, sql = %sql, "COM_STMT_PREPARE");

                // Parse+analyze once. The analyzed Stmt (with Expr::Param nodes)
                // is cached in PreparedStatement.analyzed_stmt for reuse on every
                // COM_STMT_EXECUTE without re-parsing or re-analyzing.
                let (analyzed_stmt, result_cols) = {
                    let guard = db.lock().await;
                    let snap = guard
                        .txn
                        .active_snapshot()
                        .unwrap_or_else(|_| guard.txn.snapshot());
                    match axiomdb_sql::parse(&sql, None)
                        .and_then(|s| axiomdb_sql::analyze(s, &guard.storage, snap))
                    {
                        Ok(analyzed) => {
                            let cols = extract_result_columns(&analyzed);
                            (Some(analyzed), cols)
                        }
                        Err(_) => (None, vec![]),
                    }
                };

                let current_version = schema_version.load(Ordering::Acquire);
                let (stmt_id, param_count) = conn_state.prepare_statement(sql, current_version);
                // Store the cached analyzed statement and its schema version.
                if let Some(ps) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    ps.analyzed_stmt = analyzed_stmt;
                    ps.compiled_at_version = current_version;
                }
                let packets = build_prepare_response(stmt_id, param_count, &result_cols, 1);
                if send_packets(&mut writer, &packets).await.is_err() {
                    break;
                }
            }

            // COM_STMT_EXECUTE — use cached plan, skip parse+analyze.
            0x17 => {
                if body.len() < 4 {
                    let e = build_err_packet(1105, b"HY000", "Malformed COM_STMT_EXECUTE");
                    let _ = writer.send((1u8, e.as_slice())).await;
                    continue;
                }
                let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);

                // Pre-compute next LRU seq before taking a mutable ref to the
                // statement (borrow checker: &mut conn_state ends here).
                let next_seq = conn_state.next_execute_seq();

                let result = if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    match parse_execute_packet(body, stmt) {
                        Ok(exec) => {
                            // ── Plan cache version check (Phase 5.13) ─────────────
                            // If the schema changed since this plan was compiled, re-analyze
                            // before using the cached plan. Lock is held only for analysis.
                            let current_version = schema_version.load(Ordering::Acquire);
                            if stmt.compiled_at_version != current_version
                                || stmt.analyzed_stmt.is_none()
                            {
                                debug!(
                                    conn_id,
                                    stmt_id,
                                    old_ver = stmt.compiled_at_version,
                                    new_ver = current_version,
                                    "plan stale: re-analyzing"
                                );
                                let (new_plan, _) = {
                                    let guard = db.lock().await;
                                    let snap = guard
                                        .txn
                                        .active_snapshot()
                                        .unwrap_or_else(|_| guard.txn.snapshot());
                                    match axiomdb_sql::parse(&stmt.sql_template, None)
                                        .and_then(|s| axiomdb_sql::analyze(s, &guard.storage, snap))
                                    {
                                        Ok(analyzed) => {
                                            let cols = extract_result_columns(&analyzed);
                                            (Some(analyzed), cols)
                                        }
                                        Err(_) => (None, vec![]),
                                    }
                                };
                                stmt.analyzed_stmt = new_plan;
                                // Update version even on failure — prevents infinite re-analysis.
                                stmt.compiled_at_version = current_version;
                            }

                            // Update LRU sequence (pre-computed above the borrow).
                            stmt.last_used_seq = next_seq;

                            if let Some(cached) = stmt.analyzed_stmt.clone() {
                                // ── FAST PATH: use cached plan (skip parse+analyze) ──
                                // Substitute Expr::Param nodes with actual values (~1µs)
                                // then execute directly (~50µs). Eliminates ~5ms overhead.
                                debug!(conn_id, stmt_id, "COM_STMT_EXECUTE (plan cache hit)");
                                match substitute_params_in_ast(cached, &exec.params) {
                                    Ok(ready_stmt) => {
                                        let mut guard = db.lock().await;
                                        guard.execute_stmt(ready_stmt, &mut session)
                                        // lock released here, before await below
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                // ── FALLBACK: no cached plan, use string substitution ──
                                let sql_template = stmt.sql_template.clone();
                                match substitute_params(&sql_template, &exec.params) {
                                    Ok(final_sql) => {
                                        debug!(conn_id, sql = %final_sql, "COM_STMT_EXECUTE (no cache)");
                                        let mut guard = db.lock().await;
                                        guard.execute_query(
                                            &final_sql,
                                            &mut session,
                                            &mut schema_cache,
                                        )
                                        // lock released here, before await below
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(axiomdb_core::error::DbError::Internal {
                        message: format!("Unknown prepared statement handler: stmt_id={stmt_id}"),
                    })
                };

                match result {
                    Ok((qr, commit_rx)) => {
                        // Await fsync confirmation outside the lock (group commit).
                        if let Err(e) = await_commit_rx(commit_rx).await {
                            let me = dberror_to_mysql(&e);
                            let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                            let _ = writer.send((1u8, pkt.as_slice())).await;
                            continue;
                        }
                        let packets = serialize_query_result(qr, 1);
                        if send_packets(&mut writer, &packets).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        // Map unknown stmt to error 1243
                        let me = if e.to_string().contains("Unknown prepared statement") {
                            super::error::MysqlError {
                                code: 1243,
                                sql_state: *b"HY000",
                                message: e.to_string(),
                            }
                        } else {
                            super::error::dberror_to_mysql(&e)
                        };
                        let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                        let _ = writer.send((1u8, pkt.as_slice())).await;
                    }
                }
            }

            // COM_STMT_CLOSE — no response
            0x19 => {
                if body.len() >= 4 {
                    let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    conn_state.prepared_statements.remove(&stmt_id);
                    debug!(conn_id, stmt_id, "COM_STMT_CLOSE");
                }
                // No response for COM_STMT_CLOSE
            }

            // COM_STMT_RESET
            0x1a => {
                let ok = build_ok_packet(0, 0, 0);
                let _ = writer.send((1u8, ok.as_slice())).await;
            }

            other => {
                warn!(conn_id, cmd = other, "unknown command");
                let err = build_err_packet(1047, b"HY000", "Unknown command");
                if writer.send((1u8, err.as_slice())).await.is_err() {
                    break;
                }
            }
        }
    }

    info!(conn_id, "connection closed");
}

// ── Multi-statement SQL splitter ─────────────────────────────────────────────

/// Splits a SQL string on `;` delimiters, returning non-empty trimmed statements.
///
/// Respects single-quoted string literals: a `;` inside `'...'` is not treated
/// as a statement separator. Backslash-escaped quotes `\'` inside strings are
/// handled correctly.
///
/// Strips a trailing `;` on the last statement (common in SQL scripts).
/// Returns `[sql]` unchanged if there is only one statement.
fn split_sql_statements(sql: &str) -> Vec<&str> {
    let mut stmts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut in_string = false;
    let bytes = sql.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_string => {
                in_string = true;
                i += 1;
            }
            b'\'' if in_string => {
                // Handle escaped quote `''` (SQL standard) or `\'` (MySQL extension)
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2; // skip both quotes
                } else {
                    in_string = false;
                    i += 1;
                }
            }
            b'\\' if in_string => {
                i += 2; // skip escaped character
            }
            b';' if !in_string => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    stmts.push(stmt);
                }
                start = i + 1;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Remaining text after the last `;`
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        stmts.push(tail);
    }

    if stmts.is_empty() {
        stmts.push(sql.trim());
    }

    stmts
}

// ── Group commit helper ───────────────────────────────────────────────────────

/// Awaits fsync confirmation from the `CommitCoordinator`.
///
/// - `None` → group commit is disabled or the transaction was read-only;
///   returns `Ok(())` immediately (no-op).
/// - `Some(rx)` → waits for the background task to fsync and confirm;
///   returns `Ok(())` on success or `Err(WalGroupCommitFailed)` on failure.
///
/// Must be called **after** the `Database` lock has been released so that
/// other connections can proceed while this one awaits the fsync.
async fn await_commit_rx(rx: Option<CommitRx>) -> Result<(), DbError> {
    match rx {
        None => Ok(()),
        Some(rx) => rx.await.unwrap_or_else(|_| {
            Err(DbError::WalGroupCommitFailed {
                message: "commit coordinator dropped before fsync".into(),
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
fn intercept_special_query(
    sql: &str,
    conn_state: &mut ConnectionState,
) -> Option<Vec<(u8, Vec<u8>)>> {
    use super::packets::build_ok_packet;
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let lower = sql.trim().to_ascii_lowercase();

    // ── SET statements ────────────────────────────────────────────────────────
    if lower.starts_with("set ") {
        conn_state.apply_set(sql);
        return Some(vec![(1u8, build_ok_packet(0, 0, 0))]);
    }

    // ── SELECT @@variable (single-variable form) ──────────────────────────────
    // Handles: SELECT @@x, SELECT @@session.x, SELECT @@x AS alias
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
        if let Some(val) = conn_state.get_variable(varname) {
            return Some(single_text_row(varname, &val));
        }
        // Unknown @@variable → return NULL (not an error)
        return Some(single_null_row(varname));
    }

    // ── SELECT version() / VERSION() ─────────────────────────────────────────
    if lower == "select version()" || lower.starts_with("select version()") {
        return Some(single_text_row("version()", "8.0.36-AxiomDB-0.1.0"));
    }

    // ── SELECT @@version mixed with other vars ────────────────────────────────
    if lower.contains("@@version") && !lower.contains("from ") {
        return Some(single_text_row("@@version", "8.0.36-AxiomDB-0.1.0"));
    }

    // ── SELECT DATABASE() / current_database() ────────────────────────────────
    if lower.contains("database()") || lower.contains("current_database()") {
        if conn_state.current_database.is_empty() {
            return Some(single_null_row("DATABASE()"));
        }
        return Some(single_text_row(
            "DATABASE()",
            &conn_state.current_database.clone(),
        ));
    }

    // ── SHOW WARNINGS ─────────────────────────────────────────────────────────
    if lower.starts_with("show warnings") || lower.starts_with("show errors") {
        let cols = vec![
            ColumnMeta::computed("Level".to_string(), DataType::Text),
            ColumnMeta::computed("Code".to_string(), DataType::BigInt),
            ColumnMeta::computed("Message".to_string(), DataType::Text),
        ];
        let qr = QueryResult::Rows {
            columns: cols,
            rows: vec![],
        };
        return Some(serialize_query_result(qr, 1));
    }

    // ── SHOW DATABASES ────────────────────────────────────────────────────────
    if lower.starts_with("show databases") {
        let cols = vec![ColumnMeta::computed("Database".to_string(), DataType::Text)];
        let rows = vec![vec![Value::Text("axiomdb".into())]];
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        return Some(serialize_query_result(qr, 1));
    }

    // ── SHOW VARIABLES ────────────────────────────────────────────────────────
    if lower.starts_with("show") && lower.contains("variables") {
        return Some(show_variables_result(&lower, conn_state));
    }

    // ── SHOW SESSION STATUS (e.g. LIKE 'Ssl%') ────────────────────────────────
    if lower.starts_with("show") && lower.contains("status") {
        let cols = vec![
            ColumnMeta::computed("Variable_name".to_string(), DataType::Text),
            ColumnMeta::computed("Value".to_string(), DataType::Text),
        ];
        let qr = QueryResult::Rows {
            columns: cols,
            rows: vec![],
        };
        return Some(serialize_query_result(qr, 1));
    }

    // ── SHOW FULL PROCESSLIST ─────────────────────────────────────────────────
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
        let db_val = if conn_state.current_database.is_empty() {
            Value::Null
        } else {
            Value::Text(conn_state.current_database.clone())
        };
        let rows = vec![vec![
            Value::BigInt(1),
            Value::Text("root".into()),
            Value::Text("localhost".into()),
            db_val,
            Value::Text("Query".into()),
            Value::BigInt(0),
            Value::Null,
            Value::Null,
        ]];
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        return Some(serialize_query_result(qr, 1));
    }

    None
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

    let charset = conn_state.character_set_client.clone();
    let all_vars: Vec<(&str, String)> = vec![
        ("character_set_client", charset.clone()),
        ("character_set_connection", charset.clone()),
        ("character_set_database", "utf8mb4".into()),
        ("character_set_results", charset.clone()),
        ("character_set_server", "utf8mb4".into()),
        ("character_set_system", "utf8mb3".into()),
        ("collation_connection", "utf8mb4_0900_ai_ci".into()),
        ("collation_database", "utf8mb4_0900_ai_ci".into()),
        ("collation_server", "utf8mb4_0900_ai_ci".into()),
    ];

    // Extract LIKE pattern if present
    let like_pattern = if lower.contains("like") {
        lower.split("like").nth(1).map(|s| {
            s.trim()
                .trim_matches('\'')
                .trim_matches('"')
                .replace('%', "")
        })
    } else {
        None
    };

    let rows: Vec<Vec<Value>> = all_vars
        .into_iter()
        .filter(|(name, _)| {
            if let Some(ref pat) = like_pattern {
                name.contains(pat.as_str())
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
    serialize_query_result(qr, 1)
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
    serialize_query_result(qr, 1)
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
    serialize_query_result(qr, 1)
}

// ── Prepared statement helpers ────────────────────────────────────────────────

/// Extracts the result column metadata from an analyzed SELECT statement.
/// Returns an empty vec for non-SELECT statements (INSERT/UPDATE/DELETE/DDL).
fn extract_result_columns(stmt: &Stmt) -> Vec<ColumnMeta> {
    use axiomdb_sql::ast::SelectItem;
    match stmt {
        Stmt::Select(s) => s
            .columns
            .iter()
            .map(|item| match item {
                SelectItem::Expr { alias, expr } => {
                    let name = alias.clone().unwrap_or_else(|| format!("{expr:?}"));
                    ColumnMeta::computed(name, DataType::Text) // type unknown without full inference
                }
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    ColumnMeta::computed("*".to_string(), DataType::Text)
                }
            })
            .collect(),
        _ => vec![],
    }
}

// ── Batched packet sending ────────────────────────────────────────────────────

// ── Batched packet sending ────────────────────────────────────────────────────

/// Sends multiple MySQL packets in a single TCP write.
///
/// Encodes all packets into one `Vec<u8>` buffer and calls `write_all` once.
/// This is critical for multi-packet responses (result sets): sending 5
/// packets individually causes 5 TCP writes with round-trip overhead each,
/// turning a ~0.04ms response into ~17ms. One write → all packets arrive
/// together → single syscall → one kernel context switch.
async fn send_packets(
    writer: &mut tokio_util::codec::FramedWrite<
        tokio::net::tcp::OwnedWriteHalf,
        super::codec::MySqlCodec,
    >,
    packets: &[(u8, Vec<u8>)],
) -> std::io::Result<()> {
    use futures::SinkExt;
    // Use feed() for all but the last packet (no flush), send() for the last
    // (which flushes once). This sends all packets in one TCP write.
    let n = packets.len();
    for (i, (seq, pkt)) in packets.iter().enumerate() {
        if i + 1 < n {
            writer.feed((*seq, pkt.as_slice())).await?;
        } else {
            writer.send((*seq, pkt.as_slice())).await?;
        }
    }
    Ok(())
}
