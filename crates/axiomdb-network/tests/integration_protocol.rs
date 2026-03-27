//! Protocol unit tests (Phase 5.8).
//!
//! Verifies binary correctness of MySQL wire protocol packets without
//! running a live server or establishing a real TCP connection.

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use axiomdb_network::mysql::{
    auth::{gen_challenge, is_allowed_user, verify_native_password},
    charset::DEFAULT_SERVER_COLLATION,
    codec::MySqlCodec,
    packets::{
        build_auth_more_data, build_eof_packet, build_err_packet, build_ok_packet,
        build_server_greeting, parse_handshake_response, write_lenenc_int, write_lenenc_str,
        CLIENT_INTERACTIVE, CLIENT_PLUGIN_AUTH,
    },
    result::serialize_query_result,
    session::ConnectionState,
};
use axiomdb_sql::result::{ColumnMeta, QueryResult};
use axiomdb_types::{DataType, Value};

// ── Codec tests ───────────────────────────────────────────────────────────────

#[test]
fn test_codec_encode_decode_round_trip() {
    let payload = b"Hello, MySQL!";
    let seq_id = 42u8;

    let mut dst = BytesMut::new();
    let mut codec = MySqlCodec::default();
    codec
        .encode((seq_id, payload.as_slice()), &mut dst)
        .unwrap();

    // Verify encoded length (3-byte payload_len + 1 seq_id + payload)
    assert_eq!(dst.len(), 4 + payload.len());
    assert_eq!(dst[3], seq_id);

    let (decoded_seq, decoded_payload) = codec.decode(&mut dst).unwrap().unwrap();
    assert_eq!(decoded_seq, seq_id);
    assert_eq!(decoded_payload.as_ref(), payload);
}

#[test]
fn test_codec_partial_header_returns_none() {
    let mut codec = MySqlCodec::default();
    let mut buf = BytesMut::from(&[0x01, 0x00][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn test_codec_partial_payload_returns_none() {
    let mut codec = MySqlCodec::default();
    // Header says 10 bytes, but only 3 in buffer
    let mut buf = BytesMut::from(&[0x0a, 0x00, 0x00, 0x01, 0x41, 0x42, 0x43][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn test_codec_empty_payload() {
    let mut dst = BytesMut::new();
    let mut codec = MySqlCodec::default();
    codec.encode((0u8, &[][..]), &mut dst).unwrap();
    assert_eq!(dst.len(), 4); // header only
    assert_eq!(&dst[..4], &[0x00, 0x00, 0x00, 0x00]);
}

// ── Greeting tests ────────────────────────────────────────────────────────────

#[test]
fn test_greeting_protocol_version_is_10() {
    let challenge = [0u8; 20];
    let greeting = build_server_greeting(1, &challenge, "caching_sha2_password");
    assert_eq!(greeting[0], 10, "protocol version must be 10");
}

#[test]
fn test_greeting_server_version_contains_axiomdb() {
    let challenge = [0u8; 20];
    let greeting = build_server_greeting(1, &challenge, "caching_sha2_password");
    let version_end = greeting[1..].iter().position(|&b| b == 0).unwrap() + 1;
    let version = std::str::from_utf8(&greeting[1..version_end]).unwrap();
    assert!(
        version.contains("AxiomDB"),
        "version should contain AxiomDB, got: {version}"
    );
}

#[test]
fn test_greeting_challenge_in_correct_positions() {
    let challenge: [u8; 20] = [
        1, 2, 3, 4, 5, 6, 7, 8, // part1 (8 bytes)
        9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, // part2 (12 bytes)
    ];
    let greeting = build_server_greeting(1, &challenge, "mysql_native_password");

    // Find where part1 starts (after protocol_version + server_version_null + conn_id)
    let ver_end = greeting[1..].iter().position(|&b| b == 0).unwrap() + 2; // +1 for protocol, +1 for null
    let part1_start = ver_end + 4; // skip conn_id (4 bytes)
    assert_eq!(&greeting[part1_start..part1_start + 8], &challenge[..8]);
}

#[test]
fn test_greeting_ends_with_null_terminated_plugin_name() {
    let challenge = [0u8; 20];
    let plugin = "mysql_native_password";
    let greeting = build_server_greeting(1, &challenge, plugin);
    // The last bytes should be the plugin name + null terminator
    let end = &greeting[greeting.len() - plugin.len() - 1..];
    assert_eq!(&end[..plugin.len()], plugin.as_bytes());
    assert_eq!(
        end[plugin.len()],
        0x00,
        "plugin name must be null-terminated"
    );
}

#[test]
fn test_parse_handshake_response_minimal() {
    // Build a minimal HandshakeResponse41 payload
    let mut payload = Vec::new();
    // capability_flags (4 bytes) — include CLIENT_PROTOCOL_41 and CLIENT_SECURE_CONNECTION
    let caps: u32 = 0x0000_0200 | 0x0000_8000;
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&[0u8; 4]); // max_packet_size
    payload.push(255u8); // character_set
    payload.extend_from_slice(&[0u8; 23]); // reserved
    payload.extend_from_slice(b"root\0"); // username
    payload.push(0u8); // auth_response_len = 0
                       // no database, no plugin name

    let response = parse_handshake_response(&payload).unwrap();
    assert_eq!(response.username, b"root");
    assert_eq!(response.auth_response, Vec::<u8>::new());
}

#[test]
fn test_parse_handshake_response_preserves_interactive_capability() {
    let mut payload = Vec::new();
    let caps: u32 = 0x0000_0200 | 0x0000_8000 | CLIENT_INTERACTIVE | CLIENT_PLUGIN_AUTH;
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&[0u8; 4]);
    payload.push(255u8);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"root\0");
    payload.push(0u8);
    payload.extend_from_slice(b"caching_sha2_password\0");

    let response = parse_handshake_response(&payload).unwrap();
    assert_ne!(response.capability_flags & CLIENT_INTERACTIVE, 0);
}

// ── OK / ERR / EOF packet tests ───────────────────────────────────────────────

#[test]
fn test_ok_packet_starts_with_0x00() {
    let ok = build_ok_packet(0, 0, 0);
    assert_eq!(ok[0], 0x00);
}

#[test]
fn test_ok_packet_affected_rows_encoded() {
    let ok = build_ok_packet(42, 0, 0);
    assert_eq!(ok[1], 42u8); // lenenc_int(42) = single byte
}

#[test]
fn test_ok_packet_large_affected_rows() {
    let ok = build_ok_packet(300, 0, 0);
    // 300 requires 0xfc prefix
    assert_eq!(ok[1], 0xfc);
    assert_eq!(u16::from_le_bytes([ok[2], ok[3]]), 300);
}

#[test]
fn test_err_packet_structure() {
    let err = build_err_packet(1064, b"42000", "syntax error");
    assert_eq!(err[0], 0xff, "ERR header must be 0xff");
    assert_eq!(u16::from_le_bytes([err[1], err[2]]), 1064);
    assert_eq!(err[3], b'#');
    assert_eq!(&err[4..9], b"42000");
    assert_eq!(std::str::from_utf8(&err[9..]).unwrap(), "syntax error");
}

#[test]
fn test_eof_packet_structure() {
    let eof = build_eof_packet();
    assert_eq!(eof[0], 0xfe, "EOF header must be 0xfe");
    assert_eq!(eof.len(), 5); // 0xfe + 2 warnings + 2 status
}

#[test]
fn test_auth_more_data_packet() {
    let pkt = build_auth_more_data(0x03);
    assert_eq!(pkt[0], 0x01, "AUTH_MORE_DATA marker");
    assert_eq!(pkt[1], 0x03, "fast_auth_success");
}

#[test]
fn test_apply_set_validates_timeout_session_variables() {
    for name in [
        "wait_timeout",
        "interactive_timeout",
        "net_read_timeout",
        "net_write_timeout",
    ] {
        let mut conn = ConnectionState::new();
        let err = conn
            .apply_set(&format!("SET {name} = 0"))
            .expect_err("timeout variable must reject zero");
        assert!(
            err.to_string().contains("positive integer"),
            "unexpected error for {name}: {err}"
        );

        conn.apply_set(&format!("SET {name} = '7'"))
            .expect("quoted timeout value should be accepted");
        assert_eq!(
            conn.get_variable(name).as_deref(),
            Some("7"),
            "{name} must be normalized back into the variables map"
        );
    }
}

// ── lenenc_int boundary tests ─────────────────────────────────────────────────

#[test]
fn test_lenenc_int_250_is_single_byte() {
    let mut buf = Vec::new();
    write_lenenc_int(&mut buf, 250);
    assert_eq!(buf, [250u8]);
}

#[test]
fn test_lenenc_int_251_uses_0xfc_prefix() {
    let mut buf = Vec::new();
    write_lenenc_int(&mut buf, 251);
    assert_eq!(buf[0], 0xfc);
    assert_eq!(u16::from_le_bytes([buf[1], buf[2]]), 251);
}

#[test]
fn test_lenenc_int_65535() {
    let mut buf = Vec::new();
    write_lenenc_int(&mut buf, 65535);
    assert_eq!(buf[0], 0xfc);
    assert_eq!(u16::from_le_bytes([buf[1], buf[2]]), 65535);
}

#[test]
fn test_lenenc_int_65536_uses_0xfd_prefix() {
    let mut buf = Vec::new();
    write_lenenc_int(&mut buf, 65536);
    assert_eq!(buf[0], 0xfd);
    let val = u32::from_le_bytes([buf[1], buf[2], buf[3], 0]);
    assert_eq!(val, 65536);
}

#[test]
fn test_lenenc_str() {
    let mut buf = Vec::new();
    write_lenenc_str(&mut buf, b"hello");
    assert_eq!(buf[0], 5); // length
    assert_eq!(&buf[1..], b"hello");
}

// ── Result set encoding tests ─────────────────────────────────────────────────

#[test]
fn test_result_set_row_null_is_0xfb() {
    let cols = vec![ColumnMeta::computed("x".to_string(), DataType::Text)];
    let rows = vec![vec![Value::Null]];
    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    let packets = serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION).unwrap();

    // Find the row packet (after col_count, col_def, EOF)
    // Sequence: seq=1 col_count, seq=2 col_def, seq=3 EOF, seq=4 row, seq=5 EOF
    let row_pkt = packets
        .iter()
        .find(|(seq, _)| *seq == 4)
        .expect("row packet at seq=4");
    assert_eq!(row_pkt.1[0], 0xfb, "NULL must be encoded as 0xfb");
}

#[test]
fn test_result_set_row_text_is_lenenc_string() {
    let cols = vec![ColumnMeta::computed("v".to_string(), DataType::Text)];
    let rows = vec![vec![Value::Text("hi".into())]];
    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    let packets = serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION).unwrap();

    let row_pkt = packets
        .iter()
        .find(|(seq, _)| *seq == 4)
        .expect("row packet at seq=4");
    assert_eq!(row_pkt.1[0], 2); // lenenc length = 2
    assert_eq!(&row_pkt.1[1..3], b"hi");
}

#[test]
fn test_result_set_sequence_ids_increment() {
    let cols = vec![ColumnMeta::computed("x".to_string(), DataType::Text)];
    let rows = vec![vec![Value::Text("a".into())], vec![Value::Text("b".into())]];
    let qr = QueryResult::Rows {
        columns: cols,
        rows,
    };
    let packets = serialize_query_result(qr, 1, DEFAULT_SERVER_COLLATION).unwrap();

    // Verify sequence IDs are 1, 2, 3, 4, 5, 6 (col_count, col_def, EOF, row1, row2, EOF)
    let seqs: Vec<u8> = packets.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
}

// ── Auth tests ────────────────────────────────────────────────────────────────

#[test]
fn test_native_password_empty_accepts_empty() {
    let challenge = gen_challenge();
    assert!(verify_native_password("", &challenge, &[]));
}

#[test]
fn test_native_password_empty_rejects_nonempty() {
    let challenge = gen_challenge();
    assert!(!verify_native_password("", &challenge, &[0u8; 20]));
}

#[test]
fn test_is_allowed_user_root() {
    assert!(is_allowed_user("root"));
    assert!(is_allowed_user("axiomdb"));
    assert!(!is_allowed_user("hacker"));
}

// ── Session state tests ───────────────────────────────────────────────────────

#[test]
fn test_session_default_autocommit() {
    let s = ConnectionState::new();
    assert!(s.autocommit);
    assert_eq!(s.get_variable("autocommit"), Some("1".into()));
}

#[test]
fn test_session_set_autocommit_false() {
    let mut s = ConnectionState::new();
    s.apply_set("SET autocommit=0").unwrap();
    assert!(!s.autocommit);
    assert_eq!(s.get_variable("@@autocommit"), Some("0".into()));
}

#[test]
fn test_session_set_autocommit_back_to_true() {
    let mut s = ConnectionState::new();
    s.apply_set("SET autocommit=0").unwrap();
    s.apply_set("SET autocommit=1").unwrap();
    assert!(s.autocommit);
}

#[test]
fn test_session_set_names() {
    let mut s = ConnectionState::new();
    s.apply_set("SET NAMES latin1").unwrap();
    assert_eq!(s.character_set_client_name(), "latin1");
}

#[test]
fn test_session_get_charset_variables() {
    let s = ConnectionState::new();
    assert_eq!(
        s.get_variable("character_set_client"),
        Some("utf8mb4".into())
    );
    assert_eq!(
        s.get_variable("collation_connection"),
        Some("utf8mb4_0900_ai_ci".into())
    );
}

#[test]
fn test_session_current_database_starts_empty() {
    let s = ConnectionState::new();
    assert!(s.current_database.is_empty());
}

#[test]
fn test_session_unknown_variable_is_none() {
    let s = ConnectionState::new();
    assert_eq!(s.get_variable("totally_unknown_var"), None);
}

#[test]
fn test_session_transaction_isolation() {
    let s = ConnectionState::new();
    assert_eq!(
        s.get_variable("transaction_isolation"),
        Some("REPEATABLE-READ".into())
    );
    // MySQL 5.x alias
    assert_eq!(
        s.get_variable("tx_isolation"),
        Some("REPEATABLE-READ".into())
    );
}

#[test]
fn test_session_default_on_error_variable() {
    let s = ConnectionState::new();
    assert_eq!(
        s.get_variable("@@on_error"),
        Some("rollback_statement".into())
    );
    assert_eq!(
        s.get_variable("@@session.on_error"),
        Some("rollback_statement".into())
    );
}

#[test]
fn test_session_set_on_error_updates_visible_variable() {
    let mut s = ConnectionState::new();
    s.apply_set("SET on_error = savepoint").unwrap();
    assert_eq!(s.get_variable("on_error"), Some("savepoint".into()));
    assert_eq!(s.get_variable("@@on_error"), Some("savepoint".into()));
}

#[test]
fn test_session_invalid_on_error_preserves_previous_value() {
    let mut s = ConnectionState::new();
    s.apply_set("SET on_error = rollback_transaction").unwrap();
    assert!(s.apply_set("SET on_error = banana").is_err());
    assert_eq!(
        s.get_variable("@@on_error"),
        Some("rollback_transaction".into())
    );
}

// ── 4.10d — COM_STMT_EXECUTE packet decoding for LIMIT/OFFSET params ─────────

#[test]
fn test_execute_packet_mysql_type_string_limit_param() {
    // Verifies that parse_execute_packet correctly decodes a single
    // MYSQL_TYPE_STRING (0xfd) parameter carrying the value "2".
    // This is the MariaDB client compatibility case: some clients bind
    // LIMIT ? as a string type rather than an integer type.
    use axiomdb_network::mysql::{
        charset::UTF8MB4_CHARSET, prepared::parse_execute_packet, session::PreparedStatement,
    };

    let mut stmt = PreparedStatement {
        stmt_id: 1,
        sql_template: "SELECT a FROM t LIMIT ?".into(),
        param_count: 1,
        param_types: vec![0xfd], // MYSQL_TYPE_STRING
        analyzed_stmt: None,
        compiled_at_version: 0,
        last_used_seq: 0,
        pending_long_data: vec![None; 1],
        pending_long_data_error: None,
    };

    // Minimal COM_STMT_EXECUTE payload layout:
    //   bytes 0..4   stmt_id (u32 LE) = 1
    //   byte 4       flags = 0
    //   bytes 5..9   iteration_count (u32 LE) = 1
    //   byte 9       null_bitmap: 1 param, not null → 0x00
    //   byte 10      new_params_bound_flag = 1
    //   bytes 11..13 type list: 0xfd 0x00 (MYSQL_TYPE_STRING, unsigned=0)
    //   byte 13      lenenc length = 1
    //   byte 14      payload = b'2'
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // stmt_id
    payload.push(0); // flags
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
    payload.push(0x00); // null_bitmap (param 0 is not null)
    payload.push(1); // new_params_bound_flag
    payload.push(0xfd); // MYSQL_TYPE_STRING
    payload.push(0x00); // unsigned flag
    payload.push(1); // lenenc: length = 1
    payload.push(b'2'); // the string "2"

    let exec = parse_execute_packet(&payload, &mut stmt, &UTF8MB4_CHARSET).unwrap();
    assert_eq!(exec.params.len(), 1);
    assert_eq!(exec.params[0], Value::Text("2".into()));
}

// ── 5.11b — COM_STMT_SEND_LONG_DATA packet semantics ────────────────────────

#[test]
fn test_prepare_statement_initializes_long_data_state() {
    use axiomdb_network::mysql::session::ConnectionState;

    let mut s = ConnectionState::new();
    let (stmt_id, param_count) = s.prepare_statement("INSERT INTO t VALUES (?, ?, ?)".into(), 7);

    assert_eq!(param_count, 3);
    let stmt = s.prepared_statements.get(&stmt_id).unwrap();
    assert_eq!(stmt.pending_long_data.len(), 3);
    assert!(stmt.pending_long_data.iter().all(|slot| slot.is_none()));
    assert!(stmt.pending_long_data_error.is_none());
}

#[test]
fn test_execute_packet_long_data_text_wins_over_null_and_inline() {
    use axiomdb_network::mysql::{
        charset::UTF8MB4_CHARSET, prepared::parse_execute_packet, session::PreparedStatement,
    };

    let mut stmt = PreparedStatement {
        stmt_id: 1,
        sql_template: "INSERT INTO t VALUES (?)".into(),
        param_count: 1,
        param_types: vec![0xfd], // MYSQL_TYPE_VAR_STRING
        analyzed_stmt: None,
        compiled_at_version: 0,
        last_used_seq: 0,
        pending_long_data: vec![Some("mañana".as_bytes().to_vec())],
        pending_long_data_error: None,
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // stmt_id
    payload.push(0); // flags
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
    payload.push(0x01); // null bitmap: param 0 marked NULL
    payload.push(1); // new_params_bound_flag
    payload.push(0xfd); // MYSQL_TYPE_VAR_STRING
    payload.push(0x00); // unsigned flag
    payload.push(7); // inline lenenc string length
    payload.extend_from_slice(b"ignored");

    let exec = parse_execute_packet(&payload, &mut stmt, &UTF8MB4_CHARSET).unwrap();
    assert_eq!(exec.params, vec![Value::Text("mañana".into())]);
}

#[test]
fn test_execute_packet_long_data_blob_decodes_as_bytes() {
    use axiomdb_network::mysql::{
        charset::UTF8MB4_CHARSET, prepared::parse_execute_packet, session::PreparedStatement,
    };

    let raw = b"\x00\xff\x00\x42".to_vec();
    let mut stmt = PreparedStatement {
        stmt_id: 1,
        sql_template: "INSERT INTO t VALUES (?)".into(),
        param_count: 1,
        param_types: vec![0xfc], // MYSQL_TYPE_BLOB
        analyzed_stmt: None,
        compiled_at_version: 0,
        last_used_seq: 0,
        pending_long_data: vec![Some(raw.clone())],
        pending_long_data_error: None,
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // stmt_id
    payload.push(0); // flags
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
    payload.push(0x00); // null bitmap
    payload.push(1); // new_params_bound_flag
    payload.push(0xfc); // MYSQL_TYPE_BLOB
    payload.push(0x00); // unsigned flag

    let exec = parse_execute_packet(&payload, &mut stmt, &UTF8MB4_CHARSET).unwrap();
    assert_eq!(exec.params, vec![Value::Bytes(raw)]);
}
