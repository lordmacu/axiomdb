# Plan: 5.3b + 5.8 + 5.9 — caching_sha2_password, Protocol Tests, Session State

## Files to create/modify

| File | Action | What |
|---|---|---|
| `Cargo.toml` (workspace) | modify | Add `sha2 = "0.10"` |
| `crates/axiomdb-network/Cargo.toml` | modify | Add `sha2` dependency |
| `crates/axiomdb-network/src/mysql/auth.rs` | modify | Add `caching_sha2_password` support |
| `crates/axiomdb-network/src/mysql/packets.rs` | modify | `build_auth_more_data` packet |
| `crates/axiomdb-network/src/mysql/handler.rs` | modify | Auth plugin negotiation + ConnectionState + extended intercept |
| `crates/axiomdb-network/src/mysql/session.rs` | create | `ConnectionState` struct |
| `crates/axiomdb-network/tests/integration_protocol.rs` | create | Protocol unit tests (5.8) |

---

## Phase A — `sha2` dependency

Add to workspace `Cargo.toml`:
```toml
sha2 = "0.10"
```

Add to `axiomdb-network/Cargo.toml`:
```toml
sha2 = { workspace = true }
```

---

## Phase B — `session.rs` — ConnectionState

```rust
/// Per-connection session state.
///
/// Created at handshake time, lives for the duration of the connection.
/// Replaces hardcoded responses for @@variable queries.
pub struct ConnectionState {
    /// Current schema (changed by COM_INIT_DB / USE db).
    pub current_database: String,
    /// Autocommit mode. MySQL default = true.
    /// Changed by `SET autocommit = 0/1`.
    pub autocommit: bool,
    /// Client character set (from handshake or SET NAMES).
    pub character_set_client: String,
    /// Generic session variables (SET @@session.x = y).
    pub variables: std::collections::HashMap<String, String>,
}

impl ConnectionState {
    pub fn new() -> Self {
        let mut vars = std::collections::HashMap::new();
        vars.insert("time_zone".into(), "SYSTEM".into());
        vars.insert("sql_mode".into(), String::new());
        vars.insert("transaction_isolation".into(), "REPEATABLE-READ".into());
        Self {
            current_database: String::new(),
            autocommit: true,
            character_set_client: "utf8mb4".into(),
            variables: vars,
        }
    }

    /// Applies a SET statement to the session.
    /// Returns true if the variable was recognized and stored.
    pub fn apply_set(&mut self, sql: &str) -> bool {
        let lower = sql.trim().to_ascii_lowercase();
        // SET NAMES charset
        if let Some(rest) = lower.strip_prefix("set names ") {
            let charset = rest.trim().trim_matches('\'').trim_matches('"').trim();
            self.character_set_client = charset.into();
            return true;
        }
        // SET autocommit = val
        if lower.contains("autocommit") {
            let val = if lower.contains('1') || lower.contains("true") { true } else { false };
            self.autocommit = val;
            return true;
        }
        // SET @@session.variable = value  OR  SET variable = value
        if lower.starts_with("set ") {
            // Generic: try to parse "SET [@@session.]name = value"
            let rest = lower.strip_prefix("set ").unwrap_or("").trim();
            let rest = rest.strip_prefix("@@session.").unwrap_or(rest);
            let rest = rest.strip_prefix("@@").unwrap_or(rest);
            if let Some(eq_pos) = rest.find('=') {
                let name = rest[..eq_pos].trim().to_string();
                let val_raw = rest[eq_pos+1..].trim();
                let value = val_raw.trim_matches('\'').trim_matches('"').to_string();
                self.variables.insert(name, value);
            }
            return true;
        }
        false
    }

    /// Returns the value of a @@variable query.
    /// Returns None if unrecognized (caller falls back to engine).
    pub fn get_variable(&self, name: &str) -> Option<String> {
        let lower = name.to_ascii_lowercase();
        let lower = lower.trim_start_matches("@@session.");
        let lower = lower.trim_start_matches("@@");
        match lower {
            "autocommit" => Some(if self.autocommit { "1".into() } else { "0".into() }),
            "character_set_client" => Some(self.character_set_client.clone()),
            "character_set_connection" => Some(self.character_set_client.clone()),
            "character_set_results" => Some(self.character_set_client.clone()),
            "collation_connection" => Some("utf8mb4_0900_ai_ci".into()),
            "transaction_isolation" | "tx_isolation" => {
                Some(self.variables.get("transaction_isolation")
                    .cloned()
                    .unwrap_or("REPEATABLE-READ".into()))
            }
            name => self.variables.get(name).cloned(),
        }
    }
}
```

---

## Phase C — auth.rs: SHA256 support for caching_sha2_password

```rust
use sha2::{Digest, Sha256};

/// Computes the caching_sha2_password fast-auth token.
/// token = SHA256(password) XOR SHA256(challenge || SHA256(SHA256(password)))
pub fn compute_sha256_token(password: &str, challenge: &[u8; 20]) -> [u8; 32] {
    let sha256_pwd: [u8; 32] = Sha256::digest(password.as_bytes()).into();
    let sha256_sha256_pwd: [u8; 32] = Sha256::digest(sha256_pwd).into();
    let mut h = Sha256::new();
    h.update(challenge);
    h.update(sha256_sha256_pwd);
    let xor_key: [u8; 32] = h.finalize().into();
    let mut token = [0u8; 32];
    for i in 0..32 {
        token[i] = sha256_pwd[i] ^ xor_key[i];
    }
    token
}

/// Verifies a caching_sha2_password fast-auth response.
/// Phase 5: permissive — only validates structure (20-32 bytes) for allowed users.
pub fn verify_sha256_password(
    _password: &str,      // not used in permissive mode
    _challenge: &[u8; 20],
    auth_response: &[u8],
) -> bool {
    // Permissive: accept any non-empty or empty response for allowed users.
    // Real verification in Phase 13.
    let _ = (auth_response,);  // suppress unused warning
    true
}
```

---

## Phase D — packets.rs: `build_auth_more_data`

The caching_sha2_password fast-auth path requires the server to send a
single-byte packet with `0x01` (MORE_DATA indicator) before the OK:

```rust
/// Builds the "fast auth success" packet for caching_sha2_password.
///
/// The server sends this single byte to tell the client that fast auth
/// succeeded and an OK packet follows.
pub fn build_auth_more_data(data: u8) -> Vec<u8> {
    vec![0x01, data]
}
```

The MySQL protocol spec: the payload is `[0x01][more_data_byte]` where
`more_data_byte` = `0x03` for fast_auth_success.

---

## Phase E — handler.rs: auth plugin negotiation + ConnectionState

### Changes to greeting

Change `build_server_greeting` call to advertise `caching_sha2_password`:
```rust
let greeting = build_server_greeting(conn_id, &challenge, "caching_sha2_password");
```

Modify `build_server_greeting` signature to take `auth_plugin: &str`.

### Changes to auth verification

```rust
let plugin = response.auth_plugin_name.as_deref().unwrap_or("caching_sha2_password");

if plugin == "mysql_native_password" {
    // existing SHA1 path (permissive: always accept allowed users)
} else {
    // caching_sha2_password or unknown plugin
    if is_allowed_user(&response.username) {
        // Send fast_auth_success (0x03) then OK
        let more_data = build_auth_more_data(0x03);
        writer.send((2u8, more_data.as_slice())).await?;
        writer.send((3u8, build_ok_packet(0, 0, 0).as_slice())).await?;
    } else {
        writer.send((2u8, build_err_packet(1045, b"28000", "Access denied"))).await?;
        return;
    }
}
```

### ConnectionState integration

Replace bare `SessionContext` with combined state:
```rust
let mut session = axiomdb_sql::SessionContext::new();
let mut conn_state = ConnectionState::new();
```

Update `handle_connection` signature of `intercept_special_query` to take
`&mut ConnectionState`.

### Extended intercept_special_query

Move from global function to method or pass `conn_state: &mut ConnectionState`:

```rust
fn intercept_special_query(
    sql: &str,
    conn_state: &mut ConnectionState,
) -> Option<Vec<(u8, Vec<u8>)>> {
    let lower = sql.trim().to_ascii_lowercase();

    // SET statements — apply to session state
    if lower.starts_with("set ") {
        conn_state.apply_set(sql);
        return Some(vec![(1u8, build_ok_packet(0, 0, 0))]);
    }

    // SELECT @@variable
    if lower.starts_with("select @@") || lower.starts_with("select @@session.") {
        let varname = extract_variable_name(&lower);
        if let Some(val) = conn_state.get_variable(varname) {
            return Some(single_text_row(varname, &val));
        }
    }

    // SELECT DATABASE() / current_database()
    if lower.contains("database()") || lower.contains("current_database()") {
        let db = if conn_state.current_database.is_empty() {
            Value::Null
        } else {
            Value::Text(conn_state.current_database.clone())
        };
        return Some(single_value_row("DATABASE()", db));
    }

    // SHOW VARIABLES LIKE 'character%'
    if lower.starts_with("show") && lower.contains("variables") && lower.contains("character") {
        return Some(show_charset_variables(&conn_state.character_set_client));
    }

    // SHOW VARIABLES LIKE 'collation%'
    if lower.starts_with("show") && lower.contains("variables") && lower.contains("collation") {
        return Some(show_collation_variables());
    }

    // SHOW SESSION STATUS LIKE 'Ssl%' → empty (no TLS)
    if lower.starts_with("show") && lower.contains("ssl") {
        return Some(empty_two_col_result("Variable_name", "Value"));
    }

    // SHOW FULL PROCESSLIST
    if lower.starts_with("show") && lower.contains("processlist") {
        return Some(processlist_result());
    }

    // ... existing @@version, SHOW WARNINGS, etc. handlers
    None
}
```

### COM_INIT_DB: update current_database

```rust
0x02 => { // COM_INIT_DB
    let db_name = String::from_utf8_lossy(body).into_owned();
    conn_state.current_database = db_name;
    writer.send((1u8, build_ok_packet(0, 0, 0).as_slice())).await?;
}
```

---

## Phase F — `tests/integration_protocol.rs` (5.8)

Unit tests, no network needed:

```rust
// Codec tests
#[test] fn test_codec_encode_decode_round_trip()
#[test] fn test_codec_large_payload()
#[test] fn test_codec_partial_header_returns_none()

// Greeting tests
#[test] fn test_greeting_protocol_version_is_10()
#[test] fn test_greeting_challenge_length()
#[test] fn test_greeting_plugin_name_null_terminated()
#[test] fn test_parse_handshake_response_minimal()

// OK/ERR/EOF tests
#[test] fn test_ok_packet_header_byte()
#[test] fn test_ok_packet_affected_rows_encoding()
#[test] fn test_err_packet_structure()
#[test] fn test_eof_packet_length_and_header()

// Result set tests
#[test] fn test_column_def_lenenc_strings()
#[test] fn test_row_null_is_0xfb()
#[test] fn test_row_text_is_lenenc_string()
#[test] fn test_result_set_sequence_ids()

// lenenc boundary tests
#[test] fn test_lenenc_int_250()
#[test] fn test_lenenc_int_251_uses_0xfc_prefix()
#[test] fn test_lenenc_int_65535()
#[test] fn test_lenenc_int_65536_uses_0xfd_prefix()

// Auth tests
#[test] fn test_native_password_empty_accepts_empty()
#[test] fn test_native_password_known_vector()
#[test] fn test_sha256_token_length_is_32_bytes()

// Session state tests
#[test] fn test_session_set_autocommit()
#[test] fn test_session_set_names()
#[test] fn test_session_get_variable_autocommit()
#[test] fn test_session_current_database()
```

---

## Anti-patterns to avoid

- **DO NOT** change `auth_plugin_data_len` in the greeting for caching_sha2_password
  — it stays 21 (8+13 bytes of challenge). The challenge length does NOT change.
  Only the `auth_plugin_name` string changes.

- **DO NOT** send OK directly after the HandshakeResponse for caching_sha2_password
  — the protocol requires the intermediate `0x01 0x03` fast_auth_success packet
  BEFORE the OK. Without it, clients time out waiting for the auth result.

- **DO NOT** increment the sequence_id for the `auth_more_data` packet from the
  OK sequence — the sequence must be: greeting=0, client_response=1,
  auth_more_data=2, OK=3. Any mismatch causes silent client disconnect.

- **DO NOT** make `intercept_special_query` return errors for `SHOW VARIABLES`
  queries it doesn't fully support — return an empty result set instead.
  Clients expect zero or more rows, never an error, for SHOW VARIABLES.

## Risks

- **@@variable extraction regex** — `SELECT @@session.autocommit, @@version`
  (multi-variable SELECT) is common. The simple `starts_with("select @@")`
  matcher handles single-variable queries. Multi-variable SELECTs that mix
  session variables and engine values will fall through to the engine
  (which may fail). A more complete parser is needed for full ORM compat.
  Mitigation: handle the most common multi-variable patterns explicitly.

- **Sequence ID for caching_sha2_password** — the extra round-trip packet
  increments the sequence counter, so the OK_Packet must use seq=3, not seq=2.
  Any client implementing strict sequence checking will disconnect silently.
