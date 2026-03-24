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

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use axiomdb_sql::SessionContext;

use super::{
    auth::{gen_challenge, is_allowed_user, verify_native_password},
    codec::MySqlCodec,
    database::Database,
    error::dberror_to_mysql,
    packets::{build_err_packet, build_ok_packet, build_server_greeting, parse_handshake_response},
    result::serialize_query_result,
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
    let challenge = gen_challenge();
    let greeting = build_server_greeting(conn_id, &challenge);
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

    debug!(conn_id, username = %response.username, "auth attempt");

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

    // Phase 5: permissive auth — accept any password for allowed users.
    // verify_native_password is called for correctness logging; result ignored.
    // Real auth in Phase 13.
    let _ = verify_native_password("", &challenge, &response.auth_response);

    let ok = build_ok_packet(0, 0, 0);
    if writer.send((2u8, ok.as_slice())).await.is_err() {
        return;
    }

    info!(conn_id, username = %response.username, "authenticated");

    // ── Phase 4: Command loop ─────────────────────────────────────────────────
    let mut session = SessionContext::new();

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
                debug!(conn_id, db = ?String::from_utf8_lossy(body), "COM_INIT_DB");
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
                if let Some(packets) = intercept_special_query(sql) {
                    for (seq, pkt) in packets {
                        if writer.send((seq, pkt.as_slice())).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }

                // Execute via the engine.
                let result = {
                    let mut guard = db.lock().await;
                    guard.execute_query(sql, &mut session)
                };

                match result {
                    Ok(qr) => {
                        for (seq, pkt) in serialize_query_result(qr, 1) {
                            if writer.send((seq, pkt.as_slice())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let me = dberror_to_mysql(&e);
                        debug!(conn_id, code = me.code, msg = %me.message, "query error");
                        let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                        if writer.send((1u8, pkt.as_slice())).await.is_err() {
                            break;
                        }
                    }
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
                let ok = build_ok_packet(0, 0, 0);
                if writer.send((1u8, ok.as_slice())).await.is_err() {
                    break;
                }
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

// ── ORM / driver query interception ──────────────────────────────────────────

/// Returns pre-computed responses for queries that MySQL drivers and ORMs send
/// automatically on connect — before any user SQL is executed.
///
/// Without these stubs, most clients (PyMySQL, SQLAlchemy, ActiveRecord, etc.)
/// fail to connect because they receive ERR packets for these mandatory queries.
fn intercept_special_query(sql: &str) -> Option<Vec<(u8, Vec<u8>)>> {
    use super::packets::build_ok_packet;
    use super::result::serialize_query_result;
    use axiomdb_sql::result::{ColumnMeta, QueryResult};
    use axiomdb_types::{DataType, Value};

    let lower = sql.trim().to_ascii_lowercase();

    // SET statements (SET NAMES, SET autocommit, SET character_set_*, etc.)
    if lower.starts_with("set ") {
        return Some(vec![(1u8, build_ok_packet(0, 0, 0))]);
    }

    // SELECT @@version or SELECT VERSION()
    if lower.contains("@@version") && !lower.contains("from ") || lower == "select version()" {
        return Some(single_text_row("version", "8.0.36-AxiomDB-0.1.0"));
    }

    // SELECT @@version_comment
    if lower.contains("@@version_comment") {
        return Some(single_text_row("@@version_comment", "AxiomDB"));
    }

    // SELECT DATABASE() or SELECT current_database()
    if lower.contains("database()") || lower.contains("current_database()") {
        return Some(single_text_row("DATABASE()", "axiomdb"));
    }

    // SELECT @@sql_mode
    if lower.contains("@@sql_mode") {
        return Some(single_text_row("@@sql_mode", ""));
    }

    // SELECT @@lower_case_table_names
    if lower.contains("@@lower_case_table_names") {
        return Some(single_text_row("@@lower_case_table_names", "0"));
    }

    // SELECT @@max_allowed_packet
    if lower.contains("@@max_allowed_packet") {
        return Some(single_text_row("@@max_allowed_packet", "67108864"));
    }

    // SHOW WARNINGS — empty result set
    if lower == "show warnings" {
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

    // SHOW DATABASES
    if lower == "show databases" || lower.starts_with("show databases") {
        let cols = vec![ColumnMeta::computed("Database".to_string(), DataType::Text)];
        let rows = vec![vec![Value::Text("axiomdb".into())]];
        let qr = QueryResult::Rows {
            columns: cols,
            rows,
        };
        return Some(serialize_query_result(qr, 1));
    }

    None
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
