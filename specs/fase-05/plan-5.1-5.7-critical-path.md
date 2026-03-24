# Plan: Phase 5 Critical Path — MySQL Wire Protocol (5.1 → 5.7)

## Files to create/modify

| File | Action | What |
|---|---|---|
| `Cargo.toml` (workspace) | modify | Add `sha1`, `rand`, `bytes`, `tokio-util` workspace deps |
| `crates/axiomdb-network/Cargo.toml` | modify | Add wire-protocol deps |
| `crates/axiomdb-server/Cargo.toml` | modify | Add axiomdb-network, axiomdb-sql, etc. |
| `crates/axiomdb-network/src/lib.rs` | modify | Expose `mysql` module |
| `crates/axiomdb-network/src/mysql/mod.rs` | create | Public re-exports |
| `crates/axiomdb-network/src/mysql/codec.rs` | create | Packet framing (3-byte len + seq_id) |
| `crates/axiomdb-network/src/mysql/packets.rs` | create | All packet structs + serialization |
| `crates/axiomdb-network/src/mysql/auth.rs` | create | mysql_native_password verification |
| `crates/axiomdb-network/src/mysql/result.rs` | create | QueryResult → text protocol wire |
| `crates/axiomdb-network/src/mysql/error.rs` | create | DbError → MySQL error code/SQLSTATE |
| `crates/axiomdb-network/src/mysql/handler.rs` | create | Connection handler (handshake + command loop) |
| `crates/axiomdb-network/src/mysql/database.rs` | create | Database wrapper (storage + txn + ctx) |
| `crates/axiomdb-server/src/main.rs` | modify | TCP listener + spawn connections |
| `crates/axiomdb-network/tests/integration_mysql.rs` | create | Protocol unit tests |

---

## New workspace dependencies

```toml
# Workspace Cargo.toml additions:
sha1    = "0.10"   # mysql_native_password (SHA1-based auth)
rand    = "0.8"    # challenge bytes for handshake
bytes   = "1"      # BytesMut for packet manipulation
tokio-util = { version = "0.7", features = ["codec"] }  # packet framing
```

---

## Implementation phases

### Phase A — Dependencies + crate wiring

1. Add `sha1`, `rand`, `bytes`, `tokio-util` to `[workspace.dependencies]`
2. Update `axiomdb-network/Cargo.toml`:
   ```toml
   [dependencies]
   axiomdb-core    = { workspace = true }
   axiomdb-catalog = { workspace = true }
   axiomdb-storage = { workspace = true }
   axiomdb-wal     = { workspace = true }
   axiomdb-sql     = { workspace = true }
   axiomdb-types   = { workspace = true }
   thiserror = { workspace = true }
   tokio     = { workspace = true }
   tokio-util = { workspace = true }
   bytes     = { workspace = true }
   sha1      = { workspace = true }
   rand      = { workspace = true }
   tracing   = { workspace = true }
   ```
3. Update `axiomdb-server/Cargo.toml`:
   ```toml
   [dependencies]
   axiomdb-network = { path = "../axiomdb-network" }
   axiomdb-core    = { workspace = true }
   tokio           = { workspace = true }
   tracing         = { workspace = true }
   tracing-subscriber = { workspace = true }
   ```

---

### Phase B — Packet codec (`codec.rs`)

MySQL packet framing: 3-byte payload_length (LE) + 1-byte sequence_id + payload.

```rust
use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

pub struct MySqlCodec;

/// A decoded MySQL packet: (sequence_id, payload bytes).
pub type MysqlPacket = (u8, bytes::Bytes);

impl Decoder for MySqlCodec {
    type Item = MysqlPacket;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 { return Ok(None); }
        let len = u32::from_le_bytes([src[0], src[1], src[2], 0]) as usize;
        if src.len() < 4 + len { return Ok(None); }
        src.advance(3);
        let seq_id = src.get_u8();
        let payload = src.split_to(len).freeze();
        Ok(Some((seq_id, payload)))
    }
}

impl Encoder<(u8, &[u8])> for MySqlCodec {
    type Error = std::io::Error;

    fn encode(&mut self, (seq_id, payload): (u8, &[u8]), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len = payload.len() as u32;
        dst.put_u8((len & 0xFF) as u8);
        dst.put_u8(((len >> 8) & 0xFF) as u8);
        dst.put_u8(((len >> 16) & 0xFF) as u8);
        dst.put_u8(seq_id);
        dst.put_slice(payload);
        Ok(())
    }
}
```

---

### Phase C — Packet structs + serialization (`packets.rs`)

#### Length-encoded int/string helpers

```rust
fn write_lenenc_int(buf: &mut Vec<u8>, n: u64) {
    match n {
        0..=250 => buf.push(n as u8),
        251..=65535 => { buf.push(0xfc); buf.extend_from_slice(&(n as u16).to_le_bytes()); }
        65536..=16777215 => { buf.push(0xfd); let b = (n as u32).to_le_bytes(); buf.extend_from_slice(&b[..3]); }
        _ => { buf.push(0xfe); buf.extend_from_slice(&n.to_le_bytes()); }
    }
}

fn write_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}
```

#### HandshakeV10 serialization

```rust
pub fn build_server_greeting(conn_id: u32, challenge: &[u8; 20]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(10u8);                              // protocol v10
    buf.extend_from_slice(b"8.0.36-AxiomDB-0.1.0\0");
    buf.extend_from_slice(&conn_id.to_le_bytes());
    buf.extend_from_slice(&challenge[..8]);      // auth_plugin_data_part1
    buf.push(0x00);                              // filler
    let cap_low: u16 = 0x0200 | 0x8000 | 0x0002 | 0x0004 | 0x0008 | 0x0001;
    buf.extend_from_slice(&cap_low.to_le_bytes());
    buf.push(255u8);                             // character_set = utf8mb4
    buf.extend_from_slice(&0x0002u16.to_le_bytes()); // status: autocommit
    let cap_high: u16 = 0x0002 | 0x0004 | 0x0008;   // MULTI_RESULTS, PS_MULTI, PLUGIN_AUTH
    buf.extend_from_slice(&cap_high.to_le_bytes());
    buf.push(21u8);                              // auth_plugin_data_len = 8+13
    buf.extend_from_slice(&[0u8; 10]);           // reserved
    buf.extend_from_slice(&challenge[8..]);      // auth_plugin_data_part2 (12 bytes)
    buf.push(0x00);                              // null terminator
    buf.extend_from_slice(b"mysql_native_password\0");
    buf
}
```

#### OK Packet

```rust
pub fn build_ok_packet(affected_rows: u64, last_insert_id: u64, warnings: u16) -> Vec<u8> {
    let mut buf = vec![0x00u8]; // OK header
    write_lenenc_int(&mut buf, affected_rows);
    write_lenenc_int(&mut buf, last_insert_id);
    buf.extend_from_slice(&0x0002u16.to_le_bytes()); // SERVER_STATUS_AUTOCOMMIT
    buf.extend_from_slice(&warnings.to_le_bytes());
    buf
}
```

#### ERR Packet

```rust
pub fn build_err_packet(error_code: u16, sql_state: &[u8; 5], message: &str) -> Vec<u8> {
    let mut buf = vec![0xffu8];
    buf.extend_from_slice(&error_code.to_le_bytes());
    buf.push(b'#');
    buf.extend_from_slice(sql_state);
    buf.extend_from_slice(message.as_bytes());
    buf
}
```

#### EOF Packet

```rust
pub fn build_eof_packet(warnings: u16, status: u16) -> Vec<u8> {
    let mut buf = vec![0xfeu8];
    buf.extend_from_slice(&warnings.to_le_bytes());
    buf.extend_from_slice(&status.to_le_bytes());
    buf
}
```

---

### Phase D — Authentication (`auth.rs`)

```rust
use sha1::{Digest, Sha1};

/// Verifies mysql_native_password auth response.
///
/// Returns true if:
/// - password is empty AND auth_response is empty
/// - OR: auth_response == SHA1(password) XOR SHA1(challenge || SHA1(SHA1(password)))
pub fn verify_native_password(
    password: &str,
    challenge: &[u8; 20],
    auth_response: &[u8],
) -> bool {
    if password.is_empty() {
        return auth_response.is_empty();
    }
    let sha1_pwd: [u8; 20] = Sha1::digest(password.as_bytes()).into();
    let sha1_sha1_pwd: [u8; 20] = Sha1::digest(sha1_pwd).into();
    let mut h = Sha1::new();
    h.update(challenge);
    h.update(sha1_sha1_pwd);
    let xor_key: [u8; 20] = h.finalize().into();
    let expected: Vec<u8> = sha1_pwd.iter().zip(xor_key.iter()).map(|(a,b)| a^b).collect();
    auth_response == expected.as_slice()
}
```

---

### Phase E — Result set serialization (`result.rs`)

```rust
use axiomdb_sql::{result::{ColumnMeta, QueryResult, Row}, QueryResult};
use axiomdb_types::{DataType, Value};

pub fn build_result_set(cols: &[ColumnMeta], rows: &[Row], seq_start: u8) -> Vec<(u8, Vec<u8>)> {
    let mut packets: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut seq = seq_start;

    // Column count
    let mut col_count = Vec::new();
    write_lenenc_int(&mut col_count, cols.len() as u64);
    packets.push((seq, col_count)); seq += 1;

    // Column definitions
    for col in cols {
        packets.push((seq, build_column_def(col))); seq += 1;
    }

    // EOF after column defs
    packets.push((seq, build_eof_packet(0, 0x0002))); seq += 1;

    // Row data
    for row in rows {
        packets.push((seq, build_row_packet(row))); seq += 1;
    }

    // EOF after rows
    packets.push((seq, build_eof_packet(0, 0x0002)));
    packets
}

fn build_column_def(col: &ColumnMeta) -> Vec<u8> {
    let mut buf = Vec::new();
    write_lenenc_str(&mut buf, b"def");                  // catalog
    write_lenenc_str(&mut buf, b"");                     // schema
    write_lenenc_str(&mut buf, b"");                     // table
    write_lenenc_str(&mut buf, b"");                     // org_table
    write_lenenc_str(&mut buf, col.name.as_bytes());     // name
    write_lenenc_str(&mut buf, col.name.as_bytes());     // org_name
    write_lenenc_int(&mut buf, 0x0c);                    // fixed_length = 12
    buf.extend_from_slice(&255u16.to_le_bytes());        // charset = utf8mb4
    buf.extend_from_slice(&255u32.to_le_bytes());        // column_length
    buf.push(datatype_to_mysql_type(col.data_type));     // type
    buf.extend_from_slice(&0u16.to_le_bytes());          // flags
    buf.push(0u8);                                       // decimals
    buf.extend_from_slice(&0u16.to_le_bytes());          // filler
    buf
}

fn build_row_packet(row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in row {
        match value {
            Value::Null => buf.push(0xfb),
            v => {
                let s = value_to_text(v);
                write_lenenc_str(&mut buf, s.as_bytes());
            }
        }
    }
    buf
}

fn datatype_to_mysql_type(dt: DataType) -> u8 {
    match dt {
        DataType::Bool      => 0x10, // BIT (displayed as TINYINT)
        DataType::Int       => 0x03, // LONG
        DataType::BigInt    => 0x08, // LONGLONG
        DataType::Real      => 0x05, // DOUBLE
        DataType::Decimal   => 0x00, // DECIMAL
        DataType::Text      => 0xfd, // VAR_STRING
        DataType::Bytes     => 0xfc, // BLOB
        DataType::Date      => 0x0a, // DATE
        DataType::Timestamp => 0x07, // TIMESTAMP
        DataType::Uuid      => 0xfd, // VAR_STRING
    }
}

fn value_to_text(v: &Value) -> String {
    match v {
        Value::Bool(b)       => if *b { "1".into() } else { "0".into() },
        Value::Int(n)        => n.to_string(),
        Value::BigInt(n)     => n.to_string(),
        Value::Real(f)       => f.to_string(),
        Value::Text(s)       => s.clone(),
        Value::Bytes(b)      => String::from_utf8_lossy(b).into_owned(),
        Value::Decimal(m, s) => format_decimal(*m, *s),
        Value::Date(d)       => format_date(*d),
        Value::Timestamp(t)  => format_timestamp(*t),
        Value::Uuid(u)       => format_uuid(u),
        Value::Null          => unreachable!("NULL handled above"),
    }
}
```

---

### Phase F — Error mapping (`error.rs`)

```rust
use axiomdb_core::error::DbError;

pub struct MysqlError {
    pub code: u16,
    pub sql_state: [u8; 5],
    pub message: String,
}

pub fn dberror_to_mysql(e: &DbError) -> MysqlError {
    let (code, state, msg) = match e {
        DbError::ParseError { message }        => (1064, *b"42000", message.as_str()),
        DbError::TableNotFound { name }        => (1146, *b"42S02", &format!("Table '{}' doesn't exist", name)[..]),
        DbError::ColumnNotFound { name, .. }   => (1054, *b"42S22", &format!("Unknown column '{}'", name)[..]),
        DbError::ColumnAlreadyExists { name, ..} => (1060, *b"42701", &format!("Duplicate column name '{}'", name)[..]),
        DbError::TableAlreadyExists { name, ..} => (1050, *b"42S01", &format!("Table '{}' already exists", name)[..]),
        DbError::UniqueViolation { table, column } => (1062, *b"23000", &format!("Duplicate entry for key '{}.{}'", table, column)[..]),
        DbError::NotNullViolation { table, column } => (1048, *b"23000", &format!("Column '{}.{}' cannot be null", table, column)[..]),
        DbError::CardinalityViolation { .. }   => (1242, *b"21000", "Subquery returns more than 1 row"),
        DbError::DivisionByZero                => (1365, *b"22012", "Division by 0"),
        DbError::NotImplemented { feature }    => (1235, *b"0A000", &format!("Not supported: {}", feature)[..]),
        _                                       => (1105, *b"HY000", &e.to_string()[..]),
    };
    MysqlError { code, sql_state: state, message: msg.to_string() }
}
```

---

### Phase G — Connection handler (`handler.rs`)

```rust
use tokio::net::TcpStream;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio::io::AsyncWriteExt;
use futures::{SinkExt, StreamExt};

pub async fn handle_connection(
    stream: TcpStream,
    db: std::sync::Arc<tokio::sync::Mutex<Database>>,
    conn_id: u32,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = FramedRead::new(reader, MySqlCodec);
    let mut writer = FramedWrite::new(writer, MySqlCodec);

    // ── Handshake ─────────────────────────────────────────────────────────────
    let challenge = gen_challenge();
    let greeting = build_server_greeting(conn_id, &challenge);
    if writer.send((0, greeting.as_slice())).await.is_err() { return; }

    // ── Auth ──────────────────────────────────────────────────────────────────
    let (_, payload) = match reader.next().await {
        Some(Ok(p)) => p,
        _ => return,
    };
    let (username, auth_resp) = parse_handshake_response41(&payload);

    if !is_allowed_user(&username, &auth_resp, &challenge) {
        let err = build_err_packet(1045, b"28000", "Access denied");
        let _ = writer.send((2, err.as_slice())).await;
        return;
    }
    let _ = writer.send((2, build_ok_packet(0, 0, 0).as_slice())).await;

    // ── Command loop ──────────────────────────────────────────────────────────
    let mut session = axiomdb_sql::SessionContext::new();

    loop {
        let (_, payload) = match reader.next().await {
            Some(Ok(p)) => p,
            _ => break,
        };
        if payload.is_empty() { break; }

        let cmd = payload[0];
        let body = &payload[1..];

        match cmd {
            0x01 => break,  // COM_QUIT
            0x0e => {        // COM_PING
                let _ = writer.send((1, build_ok_packet(0, 0, 0).as_slice())).await;
            }
            0x02 => {        // COM_INIT_DB — ignore, send OK
                let _ = writer.send((1, build_ok_packet(0, 0, 0).as_slice())).await;
            }
            0x03 => {        // COM_QUERY
                let sql = match std::str::from_utf8(body) {
                    Ok(s) => s,
                    Err(_) => {
                        let e = build_err_packet(1064, b"42000", "Invalid UTF-8 in query");
                        let _ = writer.send((1, e.as_slice())).await;
                        continue;
                    }
                };

                // Pre-intercept queries that ORMs always send
                if let Some(response) = intercept_orm_query(sql) {
                    for (seq, pkt) in response {
                        let _ = writer.send((seq, pkt.as_slice())).await;
                    }
                    continue;
                }

                let result = {
                    let mut guard = db.lock().await;
                    guard.execute_query(sql, &mut session)
                };

                match result {
                    Ok(qr) => {
                        for (seq, pkt) in serialize_query_result(qr, 1) {
                            let _ = writer.send((seq, pkt.as_slice())).await;
                        }
                    }
                    Err(e) => {
                        let me = dberror_to_mysql(&e);
                        let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
                        let _ = writer.send((1, pkt.as_slice())).await;
                    }
                }
            }
            _ => {
                let e = build_err_packet(1047, b"HY000", "Unknown command");
                let _ = writer.send((1, e.as_slice())).await;
            }
        }
    }
}
```

**ORM query interception** — ORMs send these on connect, must not fail:
```rust
fn intercept_orm_query(sql: &str) -> Option<Vec<(u8, Vec<u8>)>> {
    let lower = sql.trim().to_ascii_lowercase();
    // SET NAMES / SET autocommit / SET character_set_* → OK
    if lower.starts_with("set ") { return Some(ok_response()); }
    // SELECT @@version_comment → single-row result
    if lower.contains("@@version_comment") { return Some(version_comment_result()); }
    // SELECT @@version → single-row result
    if lower.contains("@@version") && !lower.contains("from") {
        return Some(version_result());
    }
    None
}
```

---

### Phase H — TCP Listener + main.rs

```rust
// axiomdb-server/src/main.rs
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() {
    // init tracing...

    let data_dir = std::env::var("AXIOMDB_DATA").unwrap_or_else(|_| "./data".into());
    let db = Arc::new(Mutex::new(
        axiomdb_network::mysql::Database::open(std::path::Path::new(&data_dir))
            .expect("failed to open database")
    ));

    let listener = TcpListener::bind("0.0.0.0:3306").await
        .expect("failed to bind :3306");

    info!("AxiomDB listening on :3306");
    let mut conn_id = 1u32;

    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        info!(%addr, conn_id, "new connection");
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            axiomdb_network::mysql::handle_connection(stream, db, conn_id).await;
        });
        conn_id = conn_id.wrapping_add(1);
    }
}
```

---

### Phase I — Tests

```
crates/axiomdb-network/tests/integration_mysql.rs
```

Tests WITHOUT a real network (unit-level) — verify binary encoding:
- `test_handshake_packet_structure` — bytes match MySQL spec exactly
- `test_ok_packet` — correct lenenc encoding of affected_rows
- `test_err_packet` — correct error code, SQLSTATE, message
- `test_result_set_single_row` — column def + row + EOF bytes
- `test_native_password_empty` — empty password accepted
- `test_native_password_known_hash` — known input → expected output

Integration test (requires server running — marked `#[ignore]`):
- `test_pymysql_connect` — Python subprocess runs pymysql test
- `test_mysql_cli_select_1` — `mysql -e "SELECT 1"` returns row

---

## Anti-patterns to avoid

- **DO NOT use `unwrap()` on packet parsing** — malformed client data must send
  ERR_Packet, not crash the server task. Invalid bytes from a client should
  only terminate that connection, never the whole server.

- **DO NOT hold the DB Mutex across `await` points** — the lock must be held
  only during the synchronous `execute_query` call. Acquiring it and then
  awaiting on network I/O would deadlock other connections.

- **DO NOT trust `sequence_id` from client** — MySQL protocol allows clients
  to send arbitrary seq IDs. For Phase 5, accept any seq from client, but
  always respond with seq = client_seq + 1 for the first response packet
  (or seq = last_server_seq + 1 for subsequent packets in the same response).

- **DO NOT send Server Greeting without the null-terminator on the auth plugin
  name** — clients parse "mysql_native_password\0" as null-terminated. Missing
  the null causes DBeaver, Workbench, and some drivers to hang.

## Risks

- **ORM queries sent before any user query** — SQLAlchemy, ActiveRecord, and
  most ORMs send `SELECT @@version`, `SET NAMES utf8mb4`, `SET autocommit=1`,
  `SHOW FULL TABLES`, `INFORMATION_SCHEMA` queries on connect. Without
  `intercept_orm_query()`, the connection will receive ERR packets and refuse
  to work. These must be intercepted and stubbed before Phase 5.7.

- **sequence_id mismatch** — if any response packet has the wrong sequence_id,
  MySQL clients silently disconnect (no error message). Every response chain
  must start at seq=1 and increment per packet.

- **lenenc_int boundary at 251** — values 251, 252, 253 (0xfb, 0xfc, 0xfd) are
  special markers in lenenc encoding. A string of length exactly 251 must be
  encoded as 0xfc + 0x00 + 0xfb, NOT as 0xfb (which means NULL). Off-by-one
  here corrupts all rows silently.
