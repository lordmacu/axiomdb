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
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, info, warn};

use axiomdb_core::error::DbError;
use axiomdb_sql::{ast::Stmt, result::ColumnMeta, SchemaCache, SessionContext};
use axiomdb_types::DataType;

use super::charset::DEFAULT_SERVER_COLLATION;
use super::database::CommitRx;
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
    database::Database,
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
};

/// Packets returned by `intercept_special_query`: a sequence of `(seq_id, payload)` pairs.
type InterceptResult = Result<Option<Vec<(u8, Vec<u8>)>>, DbError>;

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

/// Handles one MySQL connection from handshake to disconnection.
pub async fn handle_connection(stream: TcpStream, db: Arc<Mutex<Database>>, conn_id: u32) {
    handle_connection_with_timeouts(stream, db, conn_id, LifecycleTimeouts::default()).await;
}

/// Handles one MySQL connection with injectable lifecycle timeouts.
///
/// Used by lifecycle tests so auth/idle deadlines can be exercised without
/// sleeping for the production defaults.
pub async fn handle_connection_with_timeouts(
    stream: TcpStream,
    db: Arc<Mutex<Database>>,
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
            let guard = db.lock().await;
            guard.database_exists(&db_name)
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
    let mut session = SessionContext::new();
    if let Some(db_name) = initial_database {
        conn_state.current_database = db_name.clone();
        session.set_current_database(db_name);
    }
    // Per-connection schema cache — avoids repeated catalog heap scans for the
    // same table across queries. Warm on second query to the same table.
    // Automatically invalidated by analyze_cached() on DDL statements.
    let mut schema_cache = SchemaCache::new();

    // Clone Arc<AtomicU64> and Arc<StatusRegistry> once per connection — no lock
    // needed after this point for either. (Phase 5.13 + 5.9c)
    let (schema_version, status): (Arc<AtomicU64>, Arc<StatusRegistry>) = {
        let guard = db.lock().await;
        (Arc::clone(&guard.schema_version), Arc::clone(&guard.status))
    };

    // RAII guard: increments `threads_connected` now, decrements on drop.
    // Placed after auth so only authenticated connections are counted.
    let _connected_guard = ConnectedGuard::new(Arc::clone(&status));

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
                    let guard = db.lock().await;
                    guard.database_exists(&db_name)
                };
                match exists {
                    Ok(true) => {
                        conn_state.current_database = db_name.clone();
                        session.set_current_database(db_name);
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
                match intercept_special_query(sql, &mut conn_state, &status) {
                    Ok(Some(packets)) => {
                        // Sync session autocommit flag after SET statements so that the
                        // executor respects the new mode on the next DML statement.
                        session.autocommit = conn_state.autocommit;
                        // Sync strict_mode from sql_mode so the executor uses the new setting.
                        session.strict_mode = axiomdb_sql::session::sql_mode_is_strict(
                            conn_state
                                .get_variable("sql_mode")
                                .as_deref()
                                .unwrap_or("STRICT_TRANS_TABLES"),
                        );
                        // Sync on_error, compat_mode, and explicit_collation so the executor
                        // and pipeline use the new session semantics immediately.
                        session.on_error = conn_state.on_error();
                        session.compat_mode = conn_state.compat_mode();
                        session.explicit_collation = conn_state.explicit_collation();
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
                let stmts: Vec<&str> = split_sql_statements(sql);
                let stmt_count = stmts.len();
                let mut seq: u8 = 1;
                let mut connection_broken = false;

                // RAII guard: threads_running tracks active command execution.
                let _running = RunningGuard::new(&status);

                'stmts: for (idx, stmt_sql) in stmts.into_iter().enumerate() {
                    let is_last = idx == stmt_count - 1;

                    // Classify statement for counter updates.
                    let class = SqlCommandClass::from_sql(stmt_sql);

                    match intercept_special_query(stmt_sql, &mut conn_state, &status) {
                        Ok(Some(packets)) => {
                            session.autocommit = conn_state.autocommit;
                            session.strict_mode = axiomdb_sql::session::sql_mode_is_strict(
                                conn_state
                                    .get_variable("sql_mode")
                                    .as_deref()
                                    .unwrap_or("STRICT_TRANS_TABLES"),
                            );
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

                    let exec_result = {
                        let mut guard = db.lock().await;
                        guard.execute_query(stmt_sql, &mut session, &mut schema_cache)
                    };

                    match exec_result {
                        Ok((qr, commit_rx)) => {
                            conn_state.current_database =
                                session.selected_database().unwrap_or("").to_string();
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
                                session.warning_count(),
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
                session = SessionContext::new();
                conn_state = ConnectionState::new();
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
                let (analyzed_stmt, result_cols) = {
                    let guard = db.lock().await;
                    let snap = guard
                        .txn
                        .active_snapshot()
                        .unwrap_or_else(|_| guard.txn.snapshot());
                    match axiomdb_sql::parse(&sql, None).and_then(|s| {
                        axiomdb_sql::analyze_with_defaults(
                            s,
                            &guard.storage,
                            snap,
                            session.effective_database(),
                            "public",
                        )
                    }) {
                        Ok(analyzed) => {
                            let cols = extract_result_columns(&analyzed);
                            (Some(analyzed), cols)
                        }
                        Err(_) => (None, vec![]),
                    }
                };

                let current_version = schema_version.load(Ordering::Acquire);
                let (stmt_id, param_count) = conn_state.prepare_statement(
                    sql,
                    current_version,
                    session.effective_database(),
                );
                // Store the cached analyzed statement and its schema version.
                if let Some(ps) = conn_state.prepared_statements.get_mut(&stmt_id) {
                    ps.analyzed_stmt = analyzed_stmt;
                    ps.compiled_at_version = current_version;
                    ps.compiled_database = session.effective_database().to_string();
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
                            // ── Plan cache version check (Phase 5.13) ─────────────
                            // If the schema changed since this plan was compiled, re-analyze
                            // before using the cached plan. Lock is held only for analysis.
                            let current_version = schema_version.load(Ordering::Acquire);
                            if stmt.compiled_at_version != current_version
                                || stmt.compiled_database != session.effective_database()
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
                                    match axiomdb_sql::parse(&stmt.sql_template, None).and_then(
                                        |s| {
                                            axiomdb_sql::analyze_with_defaults(
                                                s,
                                                &guard.storage,
                                                snap,
                                                session.effective_database(),
                                                "public",
                                            )
                                        },
                                    ) {
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
                                stmt.compiled_database = session.effective_database().to_string();
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

                // Count this execution regardless of success/failure.
                bump_statement_counters(&status, &mut conn_state.session_status, stmt_class);

                match result {
                    Ok((qr, commit_rx)) => {
                        conn_state.current_database =
                            session.selected_database().unwrap_or("").to_string();
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
    if lower.starts_with("show") && lower.contains("status") {
        use super::status::{build_status_rows, parse_show_status};
        if let Some(query) = parse_show_status(&lower) {
            let qr = build_status_rows(&query, status, &conn_state.session_status);
            return Ok(Some(
                serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
                    .expect("utf8mb4 encoding always valid for ASCII data"),
            ));
        }
        // Unrecognised SHOW ... STATUS variant — return empty two-column rowset.
        let cols = vec![
            ColumnMeta::computed("Variable_name".to_string(), DataType::Text),
            ColumnMeta::computed("Value".to_string(), DataType::Text),
        ];
        let qr = QueryResult::Rows {
            columns: cols,
            rows: vec![],
        };
        return Ok(Some(
            serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION)
                .expect("utf8mb4 encoding always valid for ASCII data"),
        ));
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

// ── Status counter helpers ─────────────────────────────────────────────────────

/// Increments Questions, Com_select, and Com_insert for one processed statement.
fn bump_statement_counters(
    status: &Arc<StatusRegistry>,
    sess: &mut super::status::SessionStatus,
    class: SqlCommandClass,
) {
    status.questions.fetch_add(1, Ordering::Relaxed);
    sess.questions += 1;
    match class {
        SqlCommandClass::Select => {
            status.com_select.fetch_add(1, Ordering::Relaxed);
            sess.com_select += 1;
        }
        SqlCommandClass::Insert => {
            status.com_insert.fetch_add(1, Ordering::Relaxed);
            sess.com_insert += 1;
        }
        SqlCommandClass::Other => {}
    }
}

/// Increments Bytes_sent by `nbytes` in both the global registry and the
/// per-connection session counters.
fn bump_bytes_sent(
    nbytes: u64,
    status: &Arc<StatusRegistry>,
    sess: &mut super::status::SessionStatus,
) {
    status.bytes_sent.fetch_add(nbytes, Ordering::Relaxed);
    sess.bytes_sent += nbytes;
}

/// Total wire size of a packet batch (payload + 4-byte MySQL header per packet).
fn wire_size(packets: &[(u8, Vec<u8>)]) -> u64 {
    packets.iter().map(|(_, p)| p.len() as u64 + 4).sum()
}

#[cfg(test)]
mod tests {
    use super::{show_variables_result, ConnectionState};

    #[test]
    fn test_show_variables_includes_on_error() {
        let conn = ConnectionState::new();
        let packets = show_variables_result("show variables like 'on_error'", &conn);
        let payloads: Vec<u8> = packets.into_iter().flat_map(|(_, p)| p).collect();
        assert!(
            payloads.windows("on_error".len()).any(|w| w == b"on_error"),
            "SHOW VARIABLES LIKE 'on_error' must include the variable name"
        );
        assert!(
            payloads
                .windows("rollback_statement".len())
                .any(|w| w == b"rollback_statement"),
            "SHOW VARIABLES LIKE 'on_error' must expose the live default value"
        );
    }
}
