//! MySQL packet serialization — HandshakeV10, OK, ERR, EOF, result set helpers.
//!
//! All functions return `Vec<u8>` (the packet payload, without the 4-byte
//! framing header). The codec adds the header.

// ── Length-encoded primitives ─────────────────────────────────────────────────

/// Appends a length-encoded integer to `buf`.
///
/// Encoding:
/// - 0–250: single byte
/// - 251–65535: 0xFC + u16 LE
/// - 65536–16777215: 0xFD + u24 LE
/// - else: 0xFE + u64 LE
pub fn write_lenenc_int(buf: &mut Vec<u8>, n: u64) {
    match n {
        0..=250 => buf.push(n as u8),
        251..=65535 => {
            buf.push(0xfc);
            buf.extend_from_slice(&(n as u16).to_le_bytes());
        }
        65536..=16_777_215 => {
            buf.push(0xfd);
            let b = (n as u32).to_le_bytes();
            buf.extend_from_slice(&b[..3]);
        }
        _ => {
            buf.push(0xfe);
            buf.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Appends a length-encoded string (lenenc_int + bytes) to `buf`.
pub fn write_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

/// Appends a null-terminated string to `buf`.
pub fn write_nul_str(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(s);
    buf.push(0x00);
}

// ── Capability flags ──────────────────────────────────────────────────────────

pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH;

// ── HandshakeV10 ──────────────────────────────────────────────────────────────

/// Builds a MySQL HandshakeV10 payload (sent by server after TCP accept).
///
/// `challenge` must be exactly 20 bytes of random data.
/// The client will use them to compute the auth response.
pub fn build_server_greeting(conn_id: u32, challenge: &[u8; 20]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // Protocol version = 10
    buf.push(10u8);
    // Server version string (null-terminated) — report as MySQL 8.0 for compat
    write_nul_str(&mut buf, b"8.0.36-AxiomDB-0.1.0");
    // Connection ID
    buf.extend_from_slice(&conn_id.to_le_bytes());
    // auth_plugin_data part 1 (first 8 bytes of challenge)
    buf.extend_from_slice(&challenge[..8]);
    // Filler
    buf.push(0x00);

    let cap_lower = (SERVER_CAPABILITIES & 0xFFFF) as u16;
    let cap_upper = (SERVER_CAPABILITIES >> 16) as u16;

    buf.extend_from_slice(&cap_lower.to_le_bytes());
    // character_set = 255 (utf8mb4_0900_ai_ci)
    buf.push(255u8);
    // status_flags = SERVER_STATUS_AUTOCOMMIT
    buf.extend_from_slice(&0x0002u16.to_le_bytes());
    buf.extend_from_slice(&cap_upper.to_le_bytes());

    // auth_plugin_data_len: total challenge length + 1 (for null terminator)
    buf.push(21u8); // 8 + 12 + 1 null = 21
                    // reserved (10 bytes of zeros)
    buf.extend_from_slice(&[0u8; 10]);
    // auth_plugin_data part 2 (remaining 12 bytes + null terminator)
    buf.extend_from_slice(&challenge[8..]);
    buf.push(0x00); // null terminator for part 2
                    // auth_plugin_name (null-terminated)
    write_nul_str(&mut buf, b"mysql_native_password");

    buf
}

// ── Parsed HandshakeResponse41 ────────────────────────────────────────────────

pub struct HandshakeResponse {
    pub capability_flags: u32,
    pub character_set: u8,
    pub username: String,
    pub auth_response: Vec<u8>,
    pub database: Option<String>,
}

/// Parses a HandshakeResponse41 packet payload.
///
/// Returns a `HandshakeResponse` on success, or `None` if the packet is
/// malformed (in which case the server should close the connection).
pub fn parse_handshake_response(payload: &[u8]) -> Option<HandshakeResponse> {
    if payload.len() < 32 {
        return None;
    }

    let capability_flags = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let _max_packet_size = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let character_set = payload[8];
    // bytes 9..32 are reserved (zeros)
    let mut pos = 32usize;

    // username: null-terminated
    let username_end = payload[pos..].iter().position(|&b| b == 0)?;
    let username = String::from_utf8(payload[pos..pos + username_end].to_vec()).ok()?;
    pos += username_end + 1;

    // auth_response: length-encoded string (CLIENT_SECURE_CONNECTION is always set)
    if pos >= payload.len() {
        return None;
    }
    let auth_len = payload[pos] as usize;
    pos += 1;
    let auth_response = if pos + auth_len <= payload.len() {
        payload[pos..pos + auth_len].to_vec()
    } else {
        vec![]
    };
    pos += auth_len;

    // database (optional, if CLIENT_CONNECT_WITH_DB)
    let database = if capability_flags & CLIENT_CONNECT_WITH_DB != 0 && pos < payload.len() {
        let db_end = payload[pos..]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(payload.len() - pos);
        let db = String::from_utf8(payload[pos..pos + db_end].to_vec()).ok()?;
        Some(db)
    } else {
        None
    };

    Some(HandshakeResponse {
        capability_flags,
        character_set,
        username,
        auth_response,
        database,
    })
}

// ── OK Packet ─────────────────────────────────────────────────────────────────

/// Builds an OK_Packet payload.
pub fn build_ok_packet(affected_rows: u64, last_insert_id: u64, warnings: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.push(0x00); // OK header
    write_lenenc_int(&mut buf, affected_rows);
    write_lenenc_int(&mut buf, last_insert_id);
    // status_flags = SERVER_STATUS_AUTOCOMMIT
    buf.extend_from_slice(&0x0002u16.to_le_bytes());
    buf.extend_from_slice(&warnings.to_le_bytes());
    buf
}

// ── ERR Packet ────────────────────────────────────────────────────────────────

/// Builds an ERR_Packet payload.
pub fn build_err_packet(error_code: u16, sql_state: &[u8; 5], message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9 + message.len());
    buf.push(0xff); // ERR header
    buf.extend_from_slice(&error_code.to_le_bytes());
    buf.push(b'#'); // SQLSTATE marker
    buf.extend_from_slice(sql_state);
    buf.extend_from_slice(message.as_bytes());
    buf
}

// ── EOF Packet ────────────────────────────────────────────────────────────────

/// Builds an EOF_Packet payload (used between column defs and rows, and after rows).
pub fn build_eof_packet() -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.push(0xfe); // EOF header
    buf.extend_from_slice(&0u16.to_le_bytes()); // warnings = 0
    buf.extend_from_slice(&0x0002u16.to_le_bytes()); // status = SERVER_STATUS_AUTOCOMMIT
    buf
}
