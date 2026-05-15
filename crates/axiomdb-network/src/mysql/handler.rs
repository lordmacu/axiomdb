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

use tokio::net::TcpStream;
use tokio::sync::{RwLockReadGuard, RwLockWriteGuard};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_sql::{
    ast::{DropTableStmt, Stmt, TableRef},
    plan_deps::extract_table_deps,
    result::ColumnMeta,
};
use axiomdb_types::DataType;

use super::charset::DEFAULT_SERVER_COLLATION;
use super::connection::ConnectionEngineCtx;
use super::database::{is_read_only_sql, CommitRx};
use super::lifecycle::{
    configure_client_socket, read_auth_packet, read_idle_packet, send_auth_packet,
    send_execute_packet, send_packet_batch, ConnectionIoError, ConnectionLifecycle,
    ConnectionPhase, LifecycleTimeouts,
};

use super::result::serialize_query_result_multi_warn;
use super::status::{ConnectedGuard, RunningGuard, SqlCommandClass};
use super::{
    auth::{gen_challenge, is_allowed_user, verify_native_password, verify_sha256_password},
    codec::{MySqlCodec, MySqlCodecError},
    error::dberror_to_mysql,
    json_error::build_json_error,
    packets::{
        build_auth_more_data, build_err_packet, build_ok_packet, build_packet_too_large_err,
        build_server_greeting, parse_handshake_response,
    },
    prepared::{
        build_prepare_response, parse_execute_packet, substitute_params, substitute_params_in_ast,
    },
    result::serialize_query_result_binary,
    session::ConnectionState,
    status::StatusRegistry,
    SharedDatabase,
};

/// Packets returned by `intercept_special_query`: a sequence of `(seq_id, payload)` pairs.
type InterceptResult = Result<Option<Vec<(u8, Vec<u8>)>>, DbError>;

enum CatalogGuard<'a> {
    Read(#[allow(dead_code)] RwLockReadGuard<'a, ()>),
    Write(#[allow(dead_code)] RwLockWriteGuard<'a, ()>),
}

async fn acquire_catalog_guard<'a>(
    db: &'a SharedDatabase,
    write: bool,
    timeout_dur: std::time::Duration,
) -> Result<CatalogGuard<'a>, DbError> {
    if write {
        tokio::time::timeout(timeout_dur, db.catalog_lock.write())
            .await
            .map(CatalogGuard::Write)
            .map_err(|_| DbError::LockTimeout)
    } else {
        tokio::time::timeout(timeout_dur, db.catalog_lock.read())
            .await
            .map(CatalogGuard::Read)
            .map_err(|_| DbError::LockTimeout)
    }
}

fn sql_changes_schema(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    lower.starts_with("create")
        || lower.starts_with("drop")
        || lower.starts_with("alter")
        || lower.starts_with("truncate")
}

fn stmt_changes_schema(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::CreateMaterializedView(_)
            | Stmt::CreateAggregate(_)
            | Stmt::CreateSequence(_)
            | Stmt::CreateDatabase(_)
            | Stmt::DropTable(_)
            | Stmt::DropMaterializedView(_)
            | Stmt::DropAggregate(_)
            | Stmt::DropSequence(_)
            | Stmt::DropDatabase(_)
            | Stmt::AlterTable(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropIndex(_)
            | Stmt::RefreshMaterializedView(_)
            | Stmt::TruncateTable(_)
    )
}

/// Builds an ERR packet for a database error that occurred while processing `sql`.
///
/// Respects the `error_format` session variable:
/// - `"json"` → ERR message is a JSON string (structured fields for ORM / tooling).
/// - `"text"` (default) → MySQL-compatible plain text message with optional snippet.
fn build_query_err_packet(e: &DbError, sql: &str, session: &ConnectionState) -> Vec<u8> {
    let error_format = session
        .variables
        .get("error_format")
        .map(|s| s.as_str())
        .unwrap_or("text");
    if error_format == "json" {
        let me = dberror_to_mysql(e, None); // code + sqlstate only
        let json_msg = build_json_error(e, Some(sql));
        build_err_packet(me.code, &me.sql_state, &json_msg)
    } else {
        let me = dberror_to_mysql(e, Some(sql));
        build_err_packet(me.code, &me.sql_state, &me.message)
    }
}

async fn rollback_active_session_txn(
    db: &Arc<SharedDatabase>,
    engine: &mut ConnectionEngineCtx,
    conn_id: u32,
    reason: &str,
) {
    if engine.session.conn_txn.is_none() {
        return;
    }

    debug!(
        conn_id,
        reason = reason,
        "rolling back active session transaction during connection cleanup"
    );
    let _catalog_guard = db.catalog_lock.read().await;
    if let Err(e) = db.execute_stmt(Stmt::Rollback, &mut engine.session) {
        warn!(
            conn_id,
            reason = reason,
            err = %e,
            "failed to rollback active session transaction during connection cleanup"
        );
    }
}

async fn drop_session_temp_tables(
    db: &Arc<SharedDatabase>,
    engine: &mut ConnectionEngineCtx,
    conn_id: u32,
    reason: &str,
) {
    let Some(temp_schema) = engine.session.temp_schema_name().map(str::to_string) else {
        return;
    };

    debug!(
        conn_id,
        reason = reason,
        temp_schema = %temp_schema,
        "dropping session temp tables during connection cleanup"
    );

    let tables_by_database = {
        let _catalog_guard = db.catalog_lock.read().await;
        let snap = db.txn.snapshot();
        let mut reader = match CatalogReader::new(&db.storage, snap) {
            Ok(reader) => reader,
            Err(e) => {
                warn!(
                    conn_id,
                    reason = reason,
                    err = %e,
                    "failed to open catalog reader for temp-table cleanup"
                );
                return;
            }
        };

        let mut out = Vec::new();
        match reader.list_databases() {
            Ok(databases) => {
                for database in databases {
                    match reader.list_tables_in_database(&database.name, &temp_schema) {
                        Ok(tables) if !tables.is_empty() => out.push((
                            database.name,
                            tables.into_iter().map(|t| t.table_name).collect::<Vec<_>>(),
                        )),
                        Ok(_) => {}
                        Err(e) => {
                            warn!(
                                conn_id,
                                reason = reason,
                                database = %database.name,
                                temp_schema = %temp_schema,
                                err = %e,
                                "failed to enumerate temp tables in database during cleanup"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    conn_id,
                    reason = reason,
                    err = %e,
                    "failed to enumerate databases for temp-table cleanup"
                );
                return;
            }
        }
        out
    };

    for (database_name, table_names) in tables_by_database {
        let drop_stmt = Stmt::DropTable(DropTableStmt {
            if_exists: true,
            tables: table_names
                .into_iter()
                .map(|table_name| TableRef {
                    database: Some(database_name.clone()),
                    schema: Some(temp_schema.clone()),
                    name: table_name,
                    alias: None,
                    tablesample: None,
                })
                .collect(),
            cascade: false,
        });

        let result = {
            let _catalog_guard = db.catalog_lock.write().await;
            db.execute_stmt(drop_stmt, &mut engine.session)
        };
        match result {
            Ok((_qr, commit_rx)) => {
                if let Err(e) = await_commit_rx(commit_rx).await {
                    warn!(
                        conn_id,
                        reason = reason,
                        database = %database_name,
                        temp_schema = %temp_schema,
                        err = %e,
                        "temp-table cleanup commit failed"
                    );
                }
            }
            Err(e) => {
                warn!(
                    conn_id,
                    reason = reason,
                    database = %database_name,
                    temp_schema = %temp_schema,
                    err = %e,
                    "failed to drop session temp tables during cleanup"
                );
            }
        }
    }

    engine.session.clear_temp_schema();
}

/// Handles one MySQL connection from handshake to disconnection.
pub async fn handle_connection(stream: TcpStream, db: Arc<SharedDatabase>, conn_id: u32) {
    handle_connection_with_timeouts(stream, db, conn_id, LifecycleTimeouts::default()).await;
}

/// Handles one MySQL connection with injectable lifecycle timeouts.
///
/// Used by lifecycle tests so auth/idle deadlines can be exercised without
/// sleeping for the production defaults.
pub async fn handle_connection_with_timeouts(
    stream: TcpStream,
    db: Arc<SharedDatabase>,
    conn_id: u32,
    timeouts: LifecycleTimeouts,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    info!(conn_id, %peer, "connection accepted");

    if let Err(e) = configure_client_socket(&stream) {
        warn!(conn_id, err = %e, "socket configuration failed");
    }

    let (reader, writer) = stream.into_split();
    // Decoder starts with the default 64 MiB limit; synced to the session
    // value after auth and after every SET max_allowed_packet.
    let mut reader = FramedRead::new(
        reader,
        MySqlCodec::new(super::session::ConnectionState::DEFAULT_MAX_ALLOWED_PACKET),
    );
    let mut writer = FramedWrite::new(writer, MySqlCodec::default());
    let mut lifecycle = ConnectionLifecycle::with_timeouts(timeouts);

    // ── Phase 1: Send Server Greeting ─────────────────────────────────────────
    // Advertise caching_sha2_password for MySQL 8.0+ client compatibility.
    // mysql_native_password clients also accepted (plugin negotiated per-connection).
    let challenge = gen_challenge();
    let greeting = build_server_greeting(conn_id, &challenge, "caching_sha2_password");
    lifecycle.enter(ConnectionPhase::Connected);
    if send_auth_packet(&mut writer, &lifecycle, 0u8, greeting.as_slice())
        .await
        .is_err()
    {
        lifecycle.close();
        return;
    }

    // ── Phase 2: Receive HandshakeResponse41 ──────────────────────────────────
    lifecycle.enter(ConnectionPhase::Auth);
    let (_, payload) = match read_auth_packet(&mut reader, &lifecycle).await {
        Ok(p) => p,
        Err(ConnectionIoError::Read(MySqlCodecError::PacketTooLarge { .. })) => {
            // Oversized handshake — send 1153 and close before attempting auth.
            let err = build_packet_too_large_err();
            let _ = send_auth_packet(&mut writer, &lifecycle, 2u8, err.as_slice()).await;
            lifecycle.close();
            return;
        }
        Err(e) => {
            warn!(conn_id, err = %e, "client disconnected during handshake");
            lifecycle.close();
            return;
        }
    };

    let response = match parse_handshake_response(&payload) {
        Some(r) => r,
        None => {
            warn!(conn_id, "malformed HandshakeResponse41");
            let err = build_err_packet(1045, b"28000", "Malformed handshake packet");
            let _ = send_auth_packet(&mut writer, &lifecycle, 2u8, err.as_slice()).await;
            lifecycle.close();
            return;
        }
    };
    lifecycle.set_client_capability_flags(response.capability_flags);

    // Build session from the negotiated collation id. Reject unsupported ids
    // before auth so the client gets a clear error (ER_UNKNOWN_CHARACTER_SET 1115).
    let mut conn_state = match ConnectionState::from_handshake_collation_id(response.character_set)
    {
        Ok(cs) => cs,
        Err(e) => {
            let me = super::error::dberror_to_mysql(&e, None);
            let err = build_err_packet(me.code, &me.sql_state, &me.message);
            let _ = send_auth_packet(&mut writer, &lifecycle, 2u8, err.as_slice()).await;
            lifecycle.close();
            return;
        }
    };

    // Decode the username with the negotiated charset (usernames are ASCII in practice).
    let username = conn_state
        .decode_identifier_text(&response.username)
        .unwrap_or_else(|_| String::from_utf8_lossy(&response.username).into_owned());

    let plugin = response
        .auth_plugin_name
        .as_deref()
        .unwrap_or("caching_sha2_password");
    debug!(conn_id, %username, %plugin, "auth attempt");

    // ── Phase 3: Authenticate ─────────────────────────────────────────────────
    if !is_allowed_user(&username) {
        warn!(conn_id, %username, "user not allowed");
        let err = build_err_packet(
            1045,
            b"28000",
            &format!("Access denied for user '{username}'"),
        );
        let _ = send_auth_packet(&mut writer, &lifecycle, 2u8, err.as_slice()).await;
        lifecycle.close();
        return;
    }

    // Phase 5 permissive: accept all allowed users regardless of password.
    // Real auth in Phase 13.
    let final_auth_seq = if plugin.contains("caching_sha2") {
        // caching_sha2_password fast-auth sequence (4 packets total):
        //   seq=0: Server → HandshakeV10
        //   seq=1: Client → HandshakeResponse41
        //   seq=2: Server → AuthMoreData(0x03)  ← fast_auth_success
        //   seq=3: Client → empty ack (pymysql sends b"" to confirm)
        //   seq=4: Server → OK_Packet
        let _ = verify_sha256_password(&challenge, &response.auth_response);

        // caching_sha2_password fast-auth:
        //   seq=0  Server → HandshakeV10
        //   seq=1  Client → HandshakeResponse41
        //   seq=2  Server → AuthMoreData(0x03)
        //
        // Empty password: pymysql sends a _roundtrip(b"") at seq=3,
        //   then reads OK at seq=4. We must read the ack before responding.
        // Non-empty password: pymysql reads OK directly at seq=3 — no ack.
        let more_data = build_auth_more_data(0x03);
        if send_auth_packet(&mut writer, &lifecycle, 2u8, more_data.as_slice())
            .await
            .is_err()
        {
            lifecycle.close();
            return;
        }

        let ok_seq = if response.auth_response.is_empty() {
            // Empty password: read the client ack at seq=3 before OK at seq=4.
            match read_auth_packet(&mut reader, &lifecycle).await {
                Ok(_) => {}
                Err(_) => {
                    lifecycle.close();
                    return;
                }
            }
            4u8
        } else {
            // Non-empty password: send OK directly at seq=3.
            3u8
        };
        ok_seq
    } else {
        // mysql_native_password (or unknown plugin): send OK directly (seq=2).
        let _ = verify_native_password("", &challenge, &response.auth_response);
        2u8
    };

    let initial_database = if let Some(ref db_bytes) = response.database {
        let db_name = conn_state
            .decode_identifier_text(db_bytes)
            .unwrap_or_else(|_| String::from_utf8_lossy(db_bytes).into_owned());
        let exists = {
            let _catalog_guard = db.catalog_lock.read().await;
            db.database_exists(&db_name)
        };
        match exists {
            Ok(true) => Some(db_name),
            Ok(false) => {
                let err =
                    build_err_packet(1049, b"42000", &format!("Unknown database '{db_name}'"));
                let _ =
                    send_auth_packet(&mut writer, &lifecycle, final_auth_seq, err.as_slice()).await;
                lifecycle.close();
                return;
            }
            Err(e) => {
                let pkt = build_query_err_packet(&e, "", &conn_state);
                let _ =
                    send_auth_packet(&mut writer, &lifecycle, final_auth_seq, pkt.as_slice()).await;
                lifecycle.close();
                return;
            }
        }
    } else {
        None
    };

    let ok = build_ok_packet(0, 0, 0);
    if send_auth_packet(&mut writer, &lifecycle, final_auth_seq, ok.as_slice())
        .await
        .is_err()
    {
        lifecycle.close();
        return;
    }

    info!(conn_id, %username, %plugin, "authenticated");

    // ── Phase 4: Command loop ─────────────────────────────────────────────────
    let mut engine = ConnectionEngineCtx::new();
    if let Some(db_name) = initial_database {
        engine.set_current_database(&mut conn_state, db_name);
    }

    // Clone Arc<AtomicU64> and Arc<StatusRegistry> once per connection — no lock
    // needed after this point for either. (Phase 5.13 + 5.9c)
    let (schema_version, status, snapshot_registry): (
        Arc<AtomicU64>,
        Arc<StatusRegistry>,
        Arc<super::snapshot_registry::SnapshotRegistry>,
    ) = (
        Arc::clone(&db.schema_version),
        Arc::clone(&db.status),
        Arc::clone(&db.snapshot_registry),
    );

    // RAII guard: increments `threads_connected` now, decrements on drop.
    // Placed after auth so only authenticated connections are counted.
    let _connected_guard = ConnectedGuard::new(Arc::clone(&status));

    // Register this connection in the SHOW PROCESSLIST registry (GAP-B.7).
    // RAII guard removes the entry on drop even if the command loop panics.
    let _processlist_guard = super::processlist::ProcesslistGuard::register(
        Arc::clone(&db.connection_registry),
        super::shared_db::ConnectionInfo {
            id: conn_id,
            user: username.clone(),
            host: peer.clone(),
            db: if conn_state.current_database.is_empty() {
                None
            } else {
                Some(conn_state.current_database.clone())
            },
            command: "Sleep".into(),
            command_started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            state: None,
            info: None,
        },
    );

    // Sync decoder limit to the session value after auth.  The session default
    // matches the codec default (67 108 864), but a future SET may change it.
    reader.decoder_mut().set_max_payload_len(
        conn_state
            .max_allowed_packet_bytes()
            .unwrap_or(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET),
    );
    lifecycle.enter(ConnectionPhase::Idle);

    loop {
        let (_, payload) = match read_idle_packet(&mut reader, &lifecycle, &conn_state).await {
            Ok(p) => p,
            Err(ConnectionIoError::Read(MySqlCodecError::PacketTooLarge { .. })) => {
                // Connection stream is unsalvageable — send error then close.
                let err = build_packet_too_large_err();
                let _ =
                    send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, err.as_slice())
                        .await;
                lifecycle.close();
                break;
            }
            Err(ConnectionIoError::Read(e)) => {
                debug!(conn_id, err = %e, "read error");
                lifecycle.close();
                break;
            }
            Err(ConnectionIoError::Timeout(phase)) => {
                debug!(conn_id, ?phase, "connection timeout");
                lifecycle.close();
                break;
            }
            Err(ConnectionIoError::InvalidConfig(e)) => {
                warn!(conn_id, err = %e, "invalid timeout config during idle read");
                lifecycle.close();
                break;
            }
            Err(ConnectionIoError::Closed) => {
                debug!(conn_id, "client disconnected");
                lifecycle.close();
                break;
            }
            Err(ConnectionIoError::Write(_)) => {
                lifecycle.close();
                break;
            }
        };

        if payload.is_empty() {
            lifecycle.close();
            break;
        }

        // Count bytes_received: payload + 4-byte MySQL packet header.
        let pkt_len = (payload.len() + 4) as u64;
        status.bytes_received.fetch_add(pkt_len, Ordering::Relaxed);
        conn_state.session_status.bytes_received += pkt_len;

        let cmd = payload[0];
        let body = &payload[1..];
        lifecycle.enter(ConnectionPhase::Executing);

        match cmd {
            // COM_QUIT
            0x01 => {
                debug!(conn_id, "COM_QUIT");
                lifecycle.close();
                break;
            }

            // COM_INIT_DB (USE database)
            0x02 => {
                let db_name = match conn_state.decode_identifier_text(body) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => {
                        let err =
                            build_err_packet(1064, b"42000", "Invalid charset in database name");
                        let _ = send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            err.as_slice(),
                        )
                        .await;
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                };
                debug!(conn_id, db = %db_name, "COM_INIT_DB");
                let exists = {
                    let _catalog_guard = db.catalog_lock.read().await;
                    db.database_exists(&db_name)
                };
                match exists {
                    Ok(true) => {
                        engine.set_current_database(&mut conn_state, db_name);
                    }
                    Ok(false) => {
                        let err = build_err_packet(
                            1049,
                            b"42000",
                            &format!("Unknown database '{}'", db_name),
                        );
                        let _ = send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            err.as_slice(),
                        )
                        .await;
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                    Err(e) => {
                        let pkt = build_query_err_packet(&e, "", &conn_state);
                        let _ = send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            pkt.as_slice(),
                        )
                        .await;
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                }
                let ok = build_ok_packet(0, 0, 0);
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, ok.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_QUERY
            0x03 => {
                let sql_owned = match conn_state.decode_client_text(body) {
                    Ok(s) => s,
                    Err(_) => {
                        let err = build_err_packet(
                            1064,
                            b"42000",
                            "Query is not valid in connection charset",
                        );
                        let _ = send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            err.as_slice(),
                        )
                        .await;
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                };
                let sql = sql_owned.trim();
                debug!(conn_id, %sql, "COM_QUERY");

                // Intercept queries that ORMs/clients send automatically on connect.
                match intercept_special_query(sql, &mut conn_state, &status, &db.connection_registry) {
                    Ok(Some(packets)) => {
                        engine.sync_from_wire(&conn_state);
                        // Sync decoder limit after SET max_allowed_packet.
                        reader.decoder_mut().set_max_payload_len(
                            conn_state
                                .max_allowed_packet_bytes()
                                .unwrap_or(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET),
                        );
                        let class = SqlCommandClass::from_sql(sql);
                        bump_statement_counters(&status, &mut conn_state.session_status, class);
                        let nbytes = wire_size(&packets);
                        if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                            .await
                            .is_err()
                        {
                            lifecycle.close();
                            break;
                        }
                        bump_bytes_sent(nbytes, &status, &mut conn_state.session_status);
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                    Ok(None) => {} // fall through to engine
                    Err(e) => {
                        // Validation error (e.g., invalid SET max_allowed_packet value).
                        let pkt = build_query_err_packet(&e, sql, &conn_state);
                        let err_bytes = pkt.len() as u64 + 4;
                        if send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            pkt.as_slice(),
                        )
                        .await
                        .is_err()
                        {
                            lifecycle.close();
                            break;
                        }
                        bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                }

                // Split on ';' to support multi-statement COM_QUERY (Phase 5.12).
                // Each non-empty statement is executed and its result set sent
                // with SERVER_MORE_RESULTS_EXISTS in the final EOF/OK, except the
                // last statement which uses normal status flags.
                let stmts: Vec<&str> = split_sql_statements(sql, engine.session.ansi_quotes);
                let stmt_count = stmts.len();
                let mut seq: u8 = 1;
                let mut connection_broken = false;

                // RAII guard: threads_running tracks active command execution.
                let _running = RunningGuard::new(&status);

                'stmts: for (idx, stmt_sql) in stmts.into_iter().enumerate() {
                    let is_last = idx == stmt_count - 1;

                    // Classify statement for counter updates.
                    let class = SqlCommandClass::from_sql(stmt_sql);

                    match intercept_special_query(stmt_sql, &mut conn_state, &status, &db.connection_registry) {
                        Ok(Some(packets)) => {
                            engine.sync_from_wire(&conn_state);
                            reader.decoder_mut().set_max_payload_len(
                                conn_state
                                    .max_allowed_packet_bytes()
                                    .unwrap_or(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET),
                            );
                            bump_statement_counters(&status, &mut conn_state.session_status, class);
                            let nbytes = wire_size(&packets);
                            if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                                .await
                                .is_err()
                            {
                                connection_broken = true;
                                break 'stmts;
                            }
                            bump_bytes_sent(nbytes, &status, &mut conn_state.session_status);
                            if !packets.is_empty() {
                                seq = packets
                                    .last()
                                    .map(|(s, _)| s.wrapping_add(1))
                                    .unwrap_or(seq);
                            }
                            continue 'stmts;
                        }
                        Ok(None) => {} // fall through to engine
                        Err(e) => {
                            let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                            let err_bytes = pkt.len() as u64 + 4;
                            if send_execute_packet(
                                &mut writer,
                                &lifecycle,
                                &conn_state,
                                seq,
                                pkt.as_slice(),
                            )
                            .await
                            .is_err()
                            {
                                connection_broken = true;
                            } else {
                                bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                            }
                            break 'stmts;
                        }
                    }

                    bump_statement_counters(&status, &mut conn_state.session_status, class);

                    let parsed_nonblocking_alter = axiomdb_sql::parse_with_sql_mode(
                        stmt_sql,
                        None,
                        engine.session.sql_mode_flags(),
                    )
                    .ok()
                    .filter(|stmt| {
                        db.is_nonblocking_alter_candidate(
                            stmt,
                            engine.session.effective_database(),
                        )
                        .unwrap_or(false)
                    });

                    if let Some(Stmt::AlterTable(_)) = parsed_nonblocking_alter {
                        let exec_result = db.execute_nonblocking_alter_query_async(
                            stmt_sql,
                            &mut engine.session,
                            &mut engine.schema_cache,
                        )
                        .await;
                        match exec_result {
                            Ok((qr, commit_rx)) => {
                                engine.sync_database_to_wire(&mut conn_state);
                                if let Err(e) = await_commit_rx(commit_rx).await {
                                    let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                                    let err_bytes = pkt.len() as u64 + 4;
                                    if send_execute_packet(
                                        &mut writer,
                                        &lifecycle,
                                        &conn_state,
                                        seq,
                                        pkt.as_slice(),
                                    )
                                    .await
                                    .is_err()
                                    {
                                        connection_broken = true;
                                    } else {
                                        bump_bytes_sent(
                                            err_bytes,
                                            &status,
                                            &mut conn_state.session_status,
                                        );
                                    }
                                    break 'stmts;
                                }
                                let packets = match serialize_query_result_multi_warn(
                                    qr,
                                    seq,
                                    !is_last,
                                    engine.session.warning_count(),
                                    conn_state.results_collation(),
                                ) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                                        let err_bytes = pkt.len() as u64 + 4;
                                        if send_execute_packet(
                                            &mut writer,
                                            &lifecycle,
                                            &conn_state,
                                            seq,
                                            pkt.as_slice(),
                                        )
                                        .await
                                        .is_err()
                                        {
                                            connection_broken = true;
                                        } else {
                                            bump_bytes_sent(
                                                err_bytes,
                                                &status,
                                                &mut conn_state.session_status,
                                            );
                                        }
                                        break 'stmts;
                                    }
                                };
                                let nbytes = wire_size(&packets);
                                if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                                    .await
                                    .is_err()
                                {
                                    connection_broken = true;
                                    break 'stmts;
                                }
                                bump_bytes_sent(nbytes, &status, &mut conn_state.session_status);
                                seq = packets
                                    .last()
                                    .map(|(s, _)| s.wrapping_add(1))
                                    .unwrap_or(seq);
                                continue 'stmts;
                            }
                            Err(e) => {
                                let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                                let err_bytes = pkt.len() as u64 + 4;
                                if send_execute_packet(
                                    &mut writer,
                                    &lifecycle,
                                    &conn_state,
                                    seq,
                                    pkt.as_slice(),
                                )
                                .await
                                .is_err()
                                {
                                    connection_broken = true;
                                } else {
                                    bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                                }
                                break 'stmts;
                            }
                        }
                    }

                    // Phase 7.4 / 40.10: route statements through the shared database.
                    // Read-only queries hold a catalog read lock plus snapshot registration.
                    // DML also uses the catalog read lock; DDL upgrades to the write lock.
                    let is_read_only = !engine.session.in_explicit_txn && is_read_only_sql(stmt_sql);

                    let exec_result = if is_read_only {
                        let _catalog_guard = db.catalog_lock.read().await;
                        let snap_id = db.txn.max_committed() + 1;
                        snapshot_registry.register(conn_id, snap_id);
                        let r = db
                            .execute_read_query(stmt_sql, &mut engine.session, &mut engine.schema_cache)
                            .map(|qr| (qr, None));
                        snapshot_registry.unregister(conn_id);
                        r
                    } else {
                        let timeout_dur = std::time::Duration::from_secs(engine.session.lock_timeout_secs);
                        let lower = stmt_sql.trim_start().to_ascii_lowercase();
                        let is_cacheable = lower.starts_with("select")
                            || lower.starts_with("insert")
                            || lower.starts_with("update")
                            || lower.starts_with("delete");

                        if is_cacheable {
                            match acquire_catalog_guard(&db, false, timeout_dur).await {
                                Err(e) => Err(e),
                                Ok(_catalog_guard) => {
                                    let sv = db
                                        .schema_version
                                        .load(std::sync::atomic::Ordering::Acquire);

                                    let cached_result = {
                                        let snap = db.txn_snapshot_for_cache();
                                        match CatalogReader::new(db.storage_ref(), snap) {
                                            Ok(mut reader) => engine.plan_cache
                                                .lookup(
                                                    stmt_sql,
                                                    engine.session.ansi_quotes,
                                                    sv,
                                                    &mut reader,
                                                )
                                                .unwrap_or(None),
                                            Err(_) => None,
                                        }
                                    };

                                    if let Some((cached_stmt, _params)) = cached_result {
                                        db.execute_stmt(cached_stmt, &mut engine.session)
                                    } else {
                                        let result = db.execute_query(
                                            stmt_sql,
                                            &mut engine.session,
                                            &mut engine.schema_cache,
                                        );
                                        if result.is_ok() {
                                            let (norm_sql, _) = super::plan_cache::normalize_sql(
                                                stmt_sql,
                                                engine.session.ansi_quotes,
                                            );
                                            if let Ok(norm_stmt) =
                                                axiomdb_sql::parse_with_sql_mode(
                                                    &norm_sql,
                                                    None,
                                                    engine.session.sql_mode_flags(),
                                                )
                                            {
                                                let snap = db.txn_snapshot_for_cache();
                                                if let Ok(analyzed) =
                                                    axiomdb_sql::analyze_cached_with_defaults(
                                                        norm_stmt,
                                                        db.storage_ref(),
                                                        snap,
                                                        engine.session.effective_database(),
                                                        engine.session.current_schema(),
                                                        &mut engine.schema_cache,
                                                    )
                                                {
                                                    let snap2 = db.txn_snapshot_for_cache();
                                                    if let Ok(mut reader) =
                                                        CatalogReader::new(db.storage_ref(), snap2)
                                                    {
                                                        if let Ok(deps) = extract_table_deps(
                                                            &analyzed,
                                                            &mut reader,
                                                            engine.session.effective_database(),
                                                        ) {
                                                            engine.plan_cache.store(
                                                                stmt_sql,
                                                                engine.session.ansi_quotes,
                                                                &analyzed,
                                                                deps,
                                                                sv,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        result
                                    }
                                }
                            }
                        } else {
                            let parsed_stmt = axiomdb_sql::parse_with_sql_mode(
                                stmt_sql,
                                None,
                                engine.session.sql_mode_flags(),
                            )
                            .ok();
                            let needs_catalog_write = parsed_stmt
                                .as_ref()
                                .map(stmt_changes_schema)
                                .unwrap_or_else(|| sql_changes_schema(stmt_sql));
                            match acquire_catalog_guard(&db, needs_catalog_write, timeout_dur).await {
                                Err(e) => Err(e),
                                Ok(_catalog_guard) => {
                                    let ddl_table_id = parsed_stmt.as_ref().and_then(|parsed| {
                                        db.ddl_affected_table_id(
                                            parsed,
                                            engine.session.effective_database(),
                                        )
                                    });
                                    let result = db.execute_query(
                                        stmt_sql,
                                        &mut engine.session,
                                        &mut engine.schema_cache,
                                    );
                                    if result.is_ok() {
                                        if let Some(tid) = ddl_table_id {
                                            engine.plan_cache.invalidate_table(tid);
                                        }
                                    }
                                    result
                                }
                            }
                        }
                    };

                    match exec_result {
                        Ok((qr, commit_rx)) => {
                            engine.sync_database_to_wire(&mut conn_state);
                            if let Err(e) = await_commit_rx(commit_rx).await {
                                let me = dberror_to_mysql(&e, Some(stmt_sql));
                                debug!(conn_id, code = me.code, msg = %me.message, "commit error");
                                let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                                let err_bytes = pkt.len() as u64 + 4;
                                if send_execute_packet(
                                    &mut writer,
                                    &lifecycle,
                                    &conn_state,
                                    seq,
                                    pkt.as_slice(),
                                )
                                .await
                                .is_err()
                                {
                                    connection_broken = true;
                                } else {
                                    bump_bytes_sent(
                                        err_bytes,
                                        &status,
                                        &mut conn_state.session_status,
                                    );
                                }
                                break 'stmts;
                            }
                            let packets = match serialize_query_result_multi_warn(
                                qr,
                                seq,
                                !is_last,
                                engine.session.warning_count(),
                                conn_state.results_collation(),
                            ) {
                                Ok(p) => p,
                                Err(e) => {
                                    let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                                    let err_bytes = pkt.len() as u64 + 4;
                                    if send_execute_packet(
                                        &mut writer,
                                        &lifecycle,
                                        &conn_state,
                                        seq,
                                        pkt.as_slice(),
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        bump_bytes_sent(
                                            err_bytes,
                                            &status,
                                            &mut conn_state.session_status,
                                        );
                                    }
                                    break 'stmts;
                                }
                            };
                            seq = packets
                                .last()
                                .map(|(s, _)| s.wrapping_add(1))
                                .unwrap_or(seq);
                            let nbytes = wire_size(&packets);
                            if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                                .await
                                .is_err()
                            {
                                connection_broken = true;
                                break 'stmts;
                            }
                            bump_bytes_sent(nbytes, &status, &mut conn_state.session_status);
                        }
                        Err(e) => {
                            let me = dberror_to_mysql(&e, Some(stmt_sql));
                            debug!(conn_id, code = me.code, msg = %me.message, "query error");
                            let pkt = build_query_err_packet(&e, stmt_sql, &conn_state);
                            let err_bytes = pkt.len() as u64 + 4;
                            if send_execute_packet(
                                &mut writer,
                                &lifecycle,
                                &conn_state,
                                seq,
                                pkt.as_slice(),
                            )
                            .await
                            .is_err()
                            {
                                connection_broken = true;
                            } else {
                                bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                            }
                            break 'stmts;
                        }
                    }
                }
                // RunningGuard dropped here — threads_running decremented.

                if connection_broken {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_PING
            0x0e => {
                let ok = build_ok_packet(0, 0, 0);
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, ok.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_RESET_CONNECTION
            0x1f => {
                rollback_active_session_txn(&db, &mut engine, conn_id, "COM_RESET_CONNECTION")
                    .await;
                drop_session_temp_tables(&db, &mut engine, conn_id, "COM_RESET_CONNECTION").await;
                engine.reset();
                conn_state.reset_for_connection_reuse();
                // Restore the codec limit to the default after session reset.
                reader
                    .decoder_mut()
                    .set_max_payload_len(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET);
                let ok = build_ok_packet(0, 0, 0);
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, ok.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_STMT_PREPARE — parse+analyze once and cache the result.
            0x16 => {
                let sql = match conn_state.decode_client_text(body) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => {
                        let e = build_err_packet(1064, b"42000", "Invalid charset in prepare");
                        let _ = send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            e.as_slice(),
                        )
                        .await;
                        lifecycle.enter(ConnectionPhase::Idle);
                        continue;
                    }
                };
                debug!(conn_id, sql = %sql, "COM_STMT_PREPARE");

                // Parse+analyze once. The analyzed Stmt (with Expr::Param nodes)
                // is cached in PreparedStatement.analyzed_stmt for reuse on every
                // COM_STMT_EXECUTE without re-parsing or re-analyzing.
                // Also extract OID deps at this point (Phase 40.2).
                let (analyzed_stmt, result_cols, prepared_deps) = {
                    let _catalog_guard = db.catalog_lock.read().await;
                    let snap = db.txn.snapshot();
                    match axiomdb_sql::parse_with_sql_mode(&sql, None, engine.session.sql_mode_flags())
                        .and_then(|s| {
                            axiomdb_sql::analyze_with_defaults(
                                s,
                                db.storage_ref(),
                                snap,
                                engine.session.effective_database(),
                                engine.session.current_schema(),
                            )
                        }) {
                        Ok(analyzed) => {
                            let cols = extract_result_columns(&analyzed);
                            let snap2 = db.txn.snapshot();
                            let deps = CatalogReader::new(db.storage_ref(), snap2)
                                .ok()
                                .and_then(|mut r| {
                                    extract_table_deps(
                                        &analyzed,
                                        &mut r,
                                        engine.session.effective_database(),
                                    )
                                    .ok()
                                })
                                .unwrap_or_default();
                            (Some(analyzed), cols, deps)
                        }
                        Err(_) => (None, vec![], axiomdb_sql::plan_deps::PlanDeps::default()),
                    }
                };

                let current_version = schema_version.load(Ordering::Acquire);
                let (stmt_id, param_count) = conn_state.prepare_statement(
                    sql,
                    current_version,
                    engine.session.effective_database(),
                );
                let prepared_ansi_quotes = conn_state.ansi_quotes();
                // Store the cached analyzed statement, schema version, and OID deps.
                if let Some(ps) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    ps.analyzed_stmt = analyzed_stmt;
                    ps.compiled_at_version = current_version;
                    ps.deps = prepared_deps;
                    ps.compiled_database = engine.session.effective_database().to_string();
                    ps.compiled_ansi_quotes = prepared_ansi_quotes;
                }
                let packets = build_prepare_response(
                    stmt_id,
                    param_count,
                    &result_cols,
                    1,
                    conn_state.results_collation(),
                );
                if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_STMT_EXECUTE — use cached plan, skip parse+analyze.
            0x17 => {
                if body.len() < 4 {
                    let e = build_err_packet(1105, b"HY000", "Malformed COM_STMT_EXECUTE");
                    let _ = send_execute_packet(
                        &mut writer,
                        &lifecycle,
                        &conn_state,
                        1u8,
                        e.as_slice(),
                    )
                    .await;
                    lifecycle.enter(ConnectionPhase::Idle);
                    continue;
                }
                let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);

                // RAII guard: threads_running incremented while executing.
                let _running = RunningGuard::new(&status);

                // Classify the statement for Com_* counters before the borrow
                // of conn_state.prepared_statements (two borrows can't overlap).
                let stmt_class = conn_state
                    .prepared_statements
                    .get(&stmt_id)
                    .map(|ps| SqlCommandClass::from_sql(&ps.sql_template))
                    .unwrap_or(SqlCommandClass::Other);

                // Pre-compute values that borrow conn_state immutably before the
                // mutable borrow of prepared_statements below.
                let next_seq = conn_state.next_execute_seq();
                let client_charset = conn_state.client_charset();

                let result = if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    // Parse the execute packet and immediately clear long-data state
                    // regardless of parse success or failure (long data is single-use).
                    let parse_result = parse_execute_packet(body, stmt, client_charset);
                    stmt.clear_long_data_state();
                    match parse_result {
                        Ok(exec) => {
                            let current_version = schema_version.load(Ordering::Acquire);
                            let db_changed = stmt.compiled_database != engine.session.effective_database();
                            let needs_reanalyze = if stmt.analyzed_stmt.is_none() || db_changed {
                                true
                            } else if stmt.compiled_at_version != current_version {
                                if stmt.deps.is_empty() {
                                    true
                                } else {
                                    let _catalog_guard = db.catalog_lock.read().await;
                                    let snap = db.txn.snapshot();
                                    CatalogReader::new(db.storage_ref(), snap)
                                        .and_then(|mut r| stmt.deps.is_stale(&mut r))
                                        .unwrap_or(true)
                                }
                            } else {
                                false
                            };

                            if needs_reanalyze {
                                debug!(
                                    conn_id,
                                    stmt_id,
                                    old_ver = stmt.compiled_at_version,
                                    new_ver = current_version,
                                    "plan stale: re-analyzing"
                                );
                                let (new_plan, new_deps) = {
                                    let _catalog_guard = db.catalog_lock.read().await;
                                    let snap = db.txn.snapshot();
                                    match axiomdb_sql::parse_with_sql_mode(
                                        &stmt.sql_template,
                                        None,
                                        axiomdb_sql::SqlModeFlags {
                                            ansi_quotes: stmt.compiled_ansi_quotes,
                                        },
                                    )
                                    .and_then(|s| {
                                        axiomdb_sql::analyze_with_defaults(
                                            s,
                                            db.storage_ref(),
                                            snap,
                                            engine.session.effective_database(),
                                            engine.session.current_schema(),
                                        )
                                    }) {
                                        Ok(analyzed) => {
                                            let _cols = extract_result_columns(&analyzed);
                                            let snap2 = db.txn.snapshot();
                                            let deps = CatalogReader::new(db.storage_ref(), snap2)
                                                .ok()
                                                .and_then(|mut r| {
                                                    extract_table_deps(
                                                        &analyzed,
                                                        &mut r,
                                                        engine.session.effective_database(),
                                                    )
                                                    .ok()
                                                })
                                                .unwrap_or_default();
                                            (Some(analyzed), deps)
                                        }
                                        Err(_) => {
                                            (None, axiomdb_sql::plan_deps::PlanDeps::default())
                                        }
                                    }
                                };
                                stmt.analyzed_stmt = new_plan;
                                stmt.compiled_at_version = current_version;
                                stmt.deps = new_deps;
                                stmt.generation = stmt.generation.saturating_add(1);
                                stmt.compiled_database = engine.session.effective_database().to_string();
                            } else if stmt.compiled_at_version != current_version {
                                stmt.compiled_at_version = current_version;
                            }

                            stmt.last_used_seq = next_seq;

                            if let Some(cached) = stmt.analyzed_stmt.clone() {
                                debug!(conn_id, stmt_id, "COM_STMT_EXECUTE (plan cache hit)");
                                match substitute_params_in_ast(cached, &exec.params) {
                                    Ok(ready_stmt) => {
                                        if db
                                            .is_nonblocking_alter_candidate(
                                                &ready_stmt,
                                                engine.session.effective_database(),
                                            )
                                            .unwrap_or(false)
                                        {
                                            db.execute_nonblocking_alter_stmt_async(
                                                ready_stmt,
                                                &mut engine.session,
                                            )
                                            .await
                                        } else if stmt_changes_schema(&ready_stmt) {
                                            let _catalog_guard = db.catalog_lock.write().await;
                                            db.execute_stmt(ready_stmt, &mut engine.session)
                                        } else {
                                            let _catalog_guard = db.catalog_lock.read().await;
                                            db.execute_stmt(ready_stmt, &mut engine.session)
                                        }
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                let sql_template = stmt.sql_template.clone();
                                match substitute_params(
                                    &sql_template,
                                    &exec.params,
                                    stmt.compiled_ansi_quotes,
                                ) {
                                    Ok(final_sql) => {
                                        debug!(conn_id, sql = %final_sql, "COM_STMT_EXECUTE (no cache)");
                                        let parsed_stmt = axiomdb_sql::parse_with_sql_mode(
                                            &final_sql,
                                            None,
                                            engine.session.sql_mode_flags(),
                                        )
                                        .ok();
                                        if parsed_stmt
                                            .as_ref()
                                            .and_then(|stmt| {
                                                db.is_nonblocking_alter_candidate(
                                                    stmt,
                                                    engine.session.effective_database(),
                                                )
                                                .ok()
                                            })
                                            .unwrap_or(false)
                                        {
                                            db.execute_nonblocking_alter_query_async(
                                                &final_sql,
                                                &mut engine.session,
                                                &mut engine.schema_cache,
                                            )
                                            .await
                                        } else if sql_changes_schema(&final_sql) {
                                            let _catalog_guard = db.catalog_lock.write().await;
                                            db.execute_query(
                                                &final_sql,
                                                &mut engine.session,
                                                &mut engine.schema_cache,
                                            )
                                        } else {
                                            let _catalog_guard = db.catalog_lock.read().await;
                                            db.execute_query(
                                                &final_sql,
                                                &mut engine.session,
                                                &mut engine.schema_cache,
                                            )
                                        }
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

                // Count this execution regardless of success/failure.
                bump_statement_counters(&status, &mut conn_state.session_status, stmt_class);

                match result {
                    Ok((qr, commit_rx)) => {
                        conn_state.current_database =
                            engine.session.selected_database().unwrap_or("").to_string();
                        // Await fsync confirmation outside the lock (fsync pipeline).
                        if let Err(e) = await_commit_rx(commit_rx).await {
                            let me = dberror_to_mysql(&e, None);
                            let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                            let err_bytes = pkt.len() as u64 + 4;
                            if send_execute_packet(
                                &mut writer,
                                &lifecycle,
                                &conn_state,
                                1u8,
                                pkt.as_slice(),
                            )
                            .await
                            .is_ok()
                            {
                                bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                            }
                            lifecycle.enter(ConnectionPhase::Idle);
                            continue;
                        }
                        let packets = match serialize_query_result_binary(
                            qr,
                            1,
                            conn_state.results_collation(),
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                let me = dberror_to_mysql(&e, None);
                                let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                                let err_bytes = pkt.len() as u64 + 4;
                                if send_execute_packet(
                                    &mut writer,
                                    &lifecycle,
                                    &conn_state,
                                    1u8,
                                    pkt.as_slice(),
                                )
                                .await
                                .is_ok()
                                {
                                    bump_bytes_sent(
                                        err_bytes,
                                        &status,
                                        &mut conn_state.session_status,
                                    );
                                }
                                lifecycle.enter(ConnectionPhase::Idle);
                                continue;
                            }
                        };
                        let nbytes = wire_size(&packets);
                        if send_packet_batch(&mut writer, &lifecycle, &conn_state, &packets)
                            .await
                            .is_err()
                        {
                            lifecycle.close();
                            break;
                        }
                        bump_bytes_sent(nbytes, &status, &mut conn_state.session_status);
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
                            super::error::dberror_to_mysql(&e, None)
                        };
                        let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                        let err_bytes = pkt.len() as u64 + 4;
                        if send_execute_packet(
                            &mut writer,
                            &lifecycle,
                            &conn_state,
                            1u8,
                            pkt.as_slice(),
                        )
                        .await
                        .is_ok()
                        {
                            bump_bytes_sent(err_bytes, &status, &mut conn_state.session_status);
                        }
                    }
                }
                // RunningGuard dropped here.
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_STMT_SEND_LONG_DATA — no response, no engine lock
            // Payload: [stmt_id:4][param_id:2][chunk_bytes...]
            0x18 => {
                conn_state.session_status.com_stmt_send_long_data += 1;
                status
                    .com_stmt_send_long_data
                    .fetch_add(1, Ordering::Relaxed);

                if body.len() < 6 {
                    // Malformed: ignore silently per MySQL wire contract
                    lifecycle.enter(ConnectionPhase::Idle);
                    continue;
                }
                let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let param_idx = u16::from_le_bytes([body[4], body[5]]) as usize;
                let chunk = &body[6..];
                let limit = conn_state
                    .max_allowed_packet_bytes()
                    .unwrap_or(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET);

                if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    stmt.append_long_data(param_idx, chunk, limit);
                }
                // Unknown stmt_id: ignore silently (no response either way)
                lifecycle.enter(ConnectionPhase::Idle);
                continue; // never send an OK packet for this command
            }

            // COM_STMT_CLOSE — no response
            0x19 => {
                if body.len() >= 4 {
                    let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    conn_state.prepared_statements.remove(&stmt_id);
                    debug!(conn_id, stmt_id, "COM_STMT_CLOSE");
                }
                // No response for COM_STMT_CLOSE
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_STMT_RESET — clears pending long-data for the addressed statement
            0x1a => {
                if body.len() < 4 {
                    let err = build_err_packet(1105, b"HY000", "Malformed COM_STMT_RESET");
                    let _ = send_execute_packet(
                        &mut writer,
                        &lifecycle,
                        &conn_state,
                        1u8,
                        err.as_slice(),
                    )
                    .await;
                    lifecycle.enter(ConnectionPhase::Idle);
                    continue;
                }
                let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    stmt.clear_long_data_state();
                    let ok = build_ok_packet(0, 0, 0);
                    let _ = send_execute_packet(
                        &mut writer,
                        &lifecycle,
                        &conn_state,
                        1u8,
                        ok.as_slice(),
                    )
                    .await;
                } else {
                    let err = build_err_packet(
                        1243,
                        b"HY000",
                        &format!("Unknown prepared statement handler: stmt_id={stmt_id}"),
                    );
                    let _ = send_execute_packet(
                        &mut writer,
                        &lifecycle,
                        &conn_state,
                        1u8,
                        err.as_slice(),
                    )
                    .await;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_STATISTICS (0x09) — legacy monitoring agents expect a plain-text
            // statistics string (5.4c). The response payload is the raw UTF-8 string;
            // the codec adds the MySQL 4-byte packet header automatically.
            0x09 => {
                let uptime = status.uptime_secs();
                let threads = status.threads_connected.load(Ordering::Relaxed);
                let questions = status.questions.load(Ordering::Relaxed);
                let qps = if uptime > 0 {
                    questions as f64 / uptime as f64
                } else {
                    0.0
                };
                let stats_str = format!(
                    "Uptime: {uptime}  Threads: {threads}  Questions: {questions}  \
                     Slow queries: 0  Opens: 0  Flush tables: 1  Open tables: 0  \
                     Queries per second avg: {qps:.3}"
                );
                if send_execute_packet(
                    &mut writer,
                    &lifecycle,
                    &conn_state,
                    1u8,
                    stats_str.as_bytes(),
                )
                .await
                .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // COM_CHANGE_USER (0x11) — connection pool recycling (HikariCP / c3p0).
            // Reset session state and reply OK, identical to COM_RESET_CONNECTION (5.11d).
            0x11 => {
                debug!(conn_id, "COM_CHANGE_USER — reset session");
                rollback_active_session_txn(&db, &mut engine, conn_id, "COM_CHANGE_USER").await;
                drop_session_temp_tables(&db, &mut engine, conn_id, "COM_CHANGE_USER").await;
                engine.reset();
                conn_state.reset_for_connection_reuse();
                reader
                    .decoder_mut()
                    .set_max_payload_len(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET);
                let ok = build_ok_packet(0, 0, 0);
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, ok.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            // ── Deprecated / unsupported COM_* commands ─────────────────
            // Return ERR 1235 (ER_NOT_SUPPORTED_YET) with a specific message
            // instead of the generic 1047 "Unknown command".  The connection
            // stays alive — MySQL behaviour for deprecated commands.
            0x04 | // COM_FIELD_LIST (deprecated MySQL 5.7.11)
            0x07 | // COM_REFRESH (deprecated MySQL 5.7.11)
            0x08 | // COM_SHUTDOWN (deprecated MySQL 5.7.9)
            0x0a | // COM_PROCESS_INFO (deprecated MySQL 5.7.11)
            0x0d | // COM_DEBUG
            0x05 | // COM_CREATE_DB (deprecated)
            0x06 | // COM_DROP_DB (deprecated)
            0x12 | // COM_BINLOG_DUMP
            0x1c   // COM_STMT_FETCH (server-side cursors)
            => {
                let name = match cmd {
                    0x04 => "COM_FIELD_LIST",
                    0x05 => "COM_CREATE_DB",
                    0x06 => "COM_DROP_DB",
                    0x07 => "COM_REFRESH",
                    0x08 => "COM_SHUTDOWN",
                    0x0a => "COM_PROCESS_INFO",
                    0x0d => "COM_DEBUG",
                    0x12 => "COM_BINLOG_DUMP",
                    0x1c => "COM_STMT_FETCH",
                    _ => "UNKNOWN",
                };
                debug!(conn_id, cmd = cmd, name, "unsupported command");
                let err = build_err_packet(
                    1235,
                    b"0A000",
                    &format!("{name} is not supported"),
                );
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, err.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }

            other => {
                warn!(conn_id, cmd = other, "unknown command");
                let err = build_err_packet(1047, b"HY000", "Unknown command");
                if send_execute_packet(&mut writer, &lifecycle, &conn_state, 1u8, err.as_slice())
                    .await
                    .is_err()
                {
                    lifecycle.close();
                    break;
                }
                lifecycle.enter(ConnectionPhase::Idle);
            }
        }
    }

    rollback_active_session_txn(&db, &mut engine, conn_id, "connection_close").await;
    drop_session_temp_tables(&db, &mut engine, conn_id, "connection_close").await;
    info!(conn_id, "connection closed");
}

include!("handler_sql_intercept.rs");
include!("handler_util.rs");
