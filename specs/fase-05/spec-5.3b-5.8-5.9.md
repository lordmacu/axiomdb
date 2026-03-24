# Spec: 5.3b + 5.8 + 5.9 — caching_sha2_password, Protocol Tests, Session State

## Context

Phase 5.7 (pymysql test) passed. The remaining blockers for broad ORM/client
compatibility are:

1. **5.3b**: MySQL 8.0+ clients (DBeaver, Workbench, mysqlclient-python, newer
   pymysql) use `caching_sha2_password` by default. Without it, connections fail
   with "Authentication plugin 'caching_sha2_password' cannot be loaded".

2. **5.9**: ORMs issue `SET autocommit=1`, `SELECT @@session.autocommit`,
   `SHOW VARIABLES LIKE 'character_set%'`, etc. before any real SQL. Proper
   session state lets these succeed without hardcoded stubs.

3. **5.8**: Protocol unit tests verify binary correctness of packets without
   running a live server — the safest regression layer for a byte-precise protocol.

---

## 5.3b — caching_sha2_password

### What it is

The default authentication plugin in MySQL 8.0+. Uses SHA256 instead of SHA1.
Two auth paths:

- **Fast auth** (Phase 5.3b scope): Server has a cached hash for this user.
  Client sends: `SHA256(password) XOR SHA256(challenge || SHA256(SHA256(password)))`.
  Identical structure to mysql_native_password but with SHA256 instead of SHA1.
  Server validates inline — no extra round trips.

- **Full auth** (out of scope — requires TLS or RSA key exchange): Server does
  NOT have a cached hash. Falls back to encrypted password exchange. Phase 13.

### Negotiation flow

```
Server Greeting: auth_plugin_name = "caching_sha2_password"
Client Response: HandshakeResponse41 with SHA256-based token
Server → 0x01 byte = fast_auth_success  (if token matches)
Server → OK_Packet                       (connection ready)
     OR
Server → 0x02 byte = full_auth_required (if no cached hash)
Server → ERR_Packet "full auth not supported without TLS"
```

### Phase 5 permissive mode

Phase 5 does not store password hashes — all users in the allowlist are accepted.
Therefore:
- Server sends `0x01` (fast_auth_success) always for allowed users.
- Server then sends OK_Packet.
- Client never needs to send a second packet.

The server must send `0x01` as a SINGLE-BYTE packet (not wrapped in OK):
```
[0x02 0x00 0x00 seq_id 0x01]  ← 1-byte payload = fast_auth_more_data
```

### Greeting advertisement

The server MUST advertise which plugin to use in the Server Greeting. For broad
compatibility, the server should:
1. Always include `caching_sha2_password\0` as `auth_plugin_name` in the greeting.
2. Accept BOTH `mysql_native_password` and `caching_sha2_password` responses —
   clients may override the plugin based on their `default_auth` setting.
3. If the client's HandshakeResponse41 declares `auth_plugin_name =
   mysql_native_password`, use the old verify path.
4. If the client declares `caching_sha2_password` or doesn't declare a plugin,
   respond with the fast auth packet sequence.

### Inputs / Outputs

- Input: HandshakeResponse41 with `auth_plugin_name = "caching_sha2_password"`
  and 32-byte auth_response (SHA256-XOR token)
- Output: `0x01` byte packet (fast_auth_more_data), then OK_Packet (seq+1)
- Error: If client needs full auth → ERR_Packet 1045 with note "TLS required"

### Acceptance criteria (5.3b)

- [ ] DBeaver can connect without changing auth plugin setting
- [ ] `mysql -u root --get-server-public-key` (skip RSA) connects
- [ ] Server correctly sends `0x01` then OK for caching_sha2 clients
- [ ] mysql_native_password clients still work (backward compatible)
- [ ] Unknown plugin name → fall back to permissive accept + OK

---

## 5.9 — Session State

### What to build

A `ConnectionState` struct held per-connection that stores:

```rust
pub struct ConnectionState {
    /// Current database (changed by COM_INIT_DB or `USE db`)
    pub current_database: String,
    /// Autocommit mode — MySQL default is true
    pub autocommit: bool,
    /// Client character set name
    pub character_set_client: String,
    /// Server-side session variables (generic key=value store)
    /// Used for: time_zone, sql_mode, etc.
    pub variables: std::collections::HashMap<String, String>,
}
```

### SET statement handling

Currently `SET ...` returns OK but ignores the value. Phase 5.9 stores the value
and uses it in subsequent `@@variable` queries:

```sql
SET autocommit = 0         → state.autocommit = false
SET NAMES utf8mb4          → state.character_set_client = "utf8mb4"
SET @@session.time_zone = 'UTC'  → state.variables["time_zone"] = "UTC"
SET character_set_client = 'latin1'  → state.character_set_client = "latin1"
```

### @@variable query handling (extend intercept_special_query)

New queries to intercept:

| Query pattern | Response |
|---|---|
| `SELECT @@autocommit` | state.autocommit as "1"/"0" |
| `SELECT @@session.autocommit` | same |
| `SELECT @@transaction_isolation` | "REPEATABLE-READ" |
| `SELECT @@tx_isolation` | "REPEATABLE-READ" (MySQL 5.x alias) |
| `SELECT @@character_set_client` | state.character_set_client |
| `SELECT @@character_set_connection` | state.character_set_client |
| `SELECT @@character_set_results` | state.character_set_client |
| `SELECT @@collation_connection` | "utf8mb4_0900_ai_ci" |
| `SELECT @@time_zone` | state.variables["time_zone"] or "SYSTEM" |
| `SELECT @@sql_mode` | "" (already intercepted) |
| `SHOW VARIABLES LIKE 'character%'` | result set with charset vars |
| `SHOW VARIABLES LIKE 'collation%'` | result set with collation vars |
| `SHOW SESSION VARIABLES LIKE 'Ssl%'` | empty result (no TLS) |
| `SHOW FULL PROCESSLIST` | single row showing current connection |

### COM_INIT_DB

When client sends COM_INIT_DB (USE database), update `state.current_database`.
Then validate the database exists (just check if "axiomdb" or "" — Phase 5).

### SELECT DATABASE() / current_database()

Extend `intercept_special_query`:
- `SELECT DATABASE()` → `state.current_database` (NULL if empty)
- `SELECT current_database()` → same

### Autocommit integration

When `state.autocommit = false` AND an explicit `BEGIN` has not been sent,
the server should NOT auto-wrap each statement. Currently autocommit=true is
assumed. Phase 5.9 respects the SET autocommit value.

### Inputs / Outputs

- `SET autocommit=0` → OK_Packet; subsequent DML not auto-committed
- `SELECT @@autocommit` → result row `["0"]`
- `USE axiomdb` (COM_INIT_DB) → OK; `SELECT DATABASE()` → `["axiomdb"]`
- `SHOW VARIABLES LIKE 'character_set%'` → result set with 4+ rows

### Acceptance criteria (5.9)

- [ ] `SET autocommit=0` + `SET autocommit=1` round-trips correctly
- [ ] `SELECT @@autocommit` returns "0" after SET autocommit=0
- [ ] `SELECT @@session.autocommit` works (session. prefix)
- [ ] `SHOW VARIABLES LIKE 'character%'` returns non-empty result
- [ ] `SELECT DATABASE()` returns correct db after `USE axiomdb`
- [ ] `SET NAMES utf8mb4` → OK (already works) AND stored in state
- [ ] `SHOW FULL PROCESSLIST` returns at least one row (current connection)
- [ ] SQLAlchemy `create_engine("mysql+pymysql://root@127.0.0.1:3306/axiomdb")` connects

---

## 5.8 — Protocol Unit Tests

### What to build

Tests in `crates/axiomdb-network/tests/integration_protocol.rs` that verify
binary packet encoding WITHOUT a live server or network connection.

### Tests

#### Codec tests
- `test_codec_decode_single_packet` — encode + decode round-trip
- `test_codec_decode_fragmented` — payload split across chunks
- `test_codec_decode_needs_more_data` — partial header returns None

#### Handshake tests
- `test_greeting_starts_with_protocol_10`
- `test_greeting_challenge_is_20_bytes`
- `test_greeting_ends_with_null_terminated_plugin_name`
- `test_parse_handshake_response_basic`

#### OK/ERR/EOF tests
- `test_ok_packet_starts_with_0x00`
- `test_ok_affected_rows_lenenc_encoding`
- `test_err_packet_structure` (0xff + code + '#' + state + msg)
- `test_eof_packet_structure`

#### Result set tests
- `test_column_def_encoding` (lenenc strings, 12-byte fixed section)
- `test_row_null_encoded_as_0xfb`
- `test_row_text_value_lenenc_string`
- `test_result_set_packet_sequence` (verify seq_id increments correctly)

#### Type encoding tests
- `test_value_int_to_text`
- `test_value_null_is_0xfb`
- `test_value_bool_true_is_1`
- `test_lenenc_int_boundary_250` (single byte)
- `test_lenenc_int_boundary_251` (0xfc prefix)
- `test_lenenc_int_boundary_65535` (still 0xfc)
- `test_lenenc_int_boundary_65536` (0xfd prefix)

#### Auth tests (already exist in auth.rs but add here for completeness)
- `test_native_password_empty_accepts_empty_response`
- `test_native_password_known_vector`
- `test_sha256_password_token_structure` (just verifies SHA256 output length)

### Acceptance criteria (5.8)

- [ ] All protocol tests pass without a running server
- [ ] Packet round-trip (encode → decode) reproduces original bytes
- [ ] No dependency on real network or storage in these tests

---

## Out of scope

- Full caching_sha2_password with RSA key exchange → Phase 13 (requires TLS)
- Per-session autocommit in multi-connection scenarios → requires Phase 6 concurrency
- `SHOW GLOBAL VARIABLES` full catalog → stub returning empty is acceptable
- `INFORMATION_SCHEMA` queries → Phase 7+
- `SET TRANSACTION ISOLATION LEVEL` changing real MVCC behavior → Phase 7

## ⚠️ DEFERRED

- Full RSA auth (caching_sha2_password full path) → Phase 13
- ON_ERROR session behavior → Phase 5.2c
- Collation/charset transcoding → Phase 5.2a

## Dependencies

- `sha2` crate (for SHA256 in caching_sha2_password) — new workspace dep
- `ConnectionState` struct to replace current per-handler local variables
- Extended `intercept_special_query` function in handler.rs
