# Spec: Phase 5 Critical Path — MySQL Wire Protocol (5.1 → 5.7)

## What to build

A minimal but complete MySQL server that accepts real client connections and
executes SQL queries over TCP. After this spec, any MySQL-compatible client
(pymysql, PHP PDO, DBeaver, TablePlus, mysql CLI) can connect to port 3306
and run queries against AxiomDB.

The scope is deliberately narrow: text protocol only, single-connection
(single-writer), no TLS, no compression. Everything needed to say
**"it works with a real client"**.

---

## The MySQL Wire Protocol — Minimum Viable Server

### Packet framing

Every MySQL message (in both directions) is a **packet**:

```
[payload_length: u24 LE]  [sequence_id: u8]  [payload: payload_length bytes]
```

- Maximum payload per packet: 16MB − 1 (0xFFFFFF). Longer payloads are split
  into chained packets, each with incremented sequence_id. Phase 5 enforces
  `max_allowed_packet = 4MB` and returns ERR if exceeded.
- `sequence_id` resets to 0 at the start of each command. The server
  increments it by 1 for each response packet.

### Connection lifecycle (Phase 5)

```
Client connects (TCP)
  ↓
Server sends HandshakeV10           (seq=0)
  ↓
Client sends HandshakeResponse41    (seq=1)
  ↓
Server sends OK_Packet              (seq=2)  ← connection ready
  ↓
LOOP:
  Client sends COM_QUERY | COM_PING | COM_QUIT | COM_INIT_DB  (seq=0)
    ↓
  Server sends result set | OK_Packet | ERR_Packet
```

---

## 5.1 — TCP Listener with Tokio

Accept incoming TCP connections on `127.0.0.1:3306`.

For each accepted connection, spawn a `tokio::task` that runs the full
connection handler (handshake → auth → command loop).

The database is wrapped in `Arc<Mutex<Database>>` where `Database` holds:
```rust
struct Database {
    storage: MmapStorage,
    txn: TxnManager,
    ctx: SessionContext,
}
```

`Arc<Mutex<Database>>` is locked for the duration of each SQL execution
(single-writer constraint, Phase 4). Future phases will refine this.

**Acceptance criteria 5.1:**
- [ ] Server binds and listens on `:3306`
- [ ] Multiple clients can connect concurrently (one at a time executes)
- [ ] Graceful shutdown on Ctrl+C (SIGINT)

---

## 5.2 — MySQL Handshake

### Server sends: HandshakeV10

After accepting a TCP connection, the server sends a HandshakeV10 packet:

```
protocol_version: u8 = 10
server_version: null-terminated string  = "8.0.36-AxiomDB-0.1.0\0"
connection_id: u32 LE
auth_plugin_data_part1: [u8; 8]  (random bytes)
filler: u8 = 0x00
capability_flags_lower: u16 LE
character_set: u8 = 255  (utf8mb4, collation_id 255)
status_flags: u16 LE = 0x0002  (SERVER_STATUS_AUTOCOMMIT)
capability_flags_upper: u16 LE
auth_plugin_data_len: u8 = 21  (8+13)
reserved: [u8; 10] = zeros
auth_plugin_data_part2: [u8; 13]  (random bytes, last is 0x00)
auth_plugin_name: "mysql_native_password\0"
```

**Capability flags to advertise (minimum for client compat):**
```
CLIENT_LONG_PASSWORD       = 0x00000001
CLIENT_FOUND_ROWS          = 0x00000002
CLIENT_LONG_FLAG           = 0x00000004
CLIENT_CONNECT_WITH_DB     = 0x00000008
CLIENT_PROTOCOL_41         = 0x00000200
CLIENT_TRANSACTIONS        = 0x00002000
CLIENT_SECURE_CONNECTION   = 0x00008000
CLIENT_MULTI_RESULTS       = 0x00020000
CLIENT_PS_MULTI_RESULTS    = 0x00040000
CLIENT_PLUGIN_AUTH         = 0x00080000
```

### Client sends: HandshakeResponse41

```
capability_flags: u32 LE
max_packet_size: u32 LE
character_set: u8
reserved: [u8; 23] = zeros
username: null-terminated string
auth_response_length: u8
auth_response: [u8; 20]  (SHA1-based auth data)
database: null-terminated string  (optional, if CLIENT_CONNECT_WITH_DB)
auth_plugin_name: null-terminated string  (optional, if CLIENT_PLUGIN_AUTH)
```

**Acceptance criteria 5.2:**
- [ ] Server sends valid HandshakeV10 (sequence=0)
- [ ] Server correctly parses HandshakeResponse41
- [ ] Server records client charset from response
- [ ] `mysql -h 127.0.0.1 -P 3306 -u root --password=` fails gracefully (before auth)

---

## 5.3 — Authentication (mysql_native_password)

### Algorithm

The server sends 20 bytes of random challenge (`auth_plugin_data` = part1+part2,
trimmed to 20 bytes before the null terminator).

The client computes:
```
token = SHA1(password) XOR SHA1(challenge || SHA1(SHA1(password)))
```

The server verifies by comparing `token XOR SHA1(challenge + SHA1(SHA1(stored_hash)))`.

### Phase 5 authentication rules

For Phase 5, use a simple in-memory user store:
```rust
const ALLOWED_USERS: &[(&str, Option<&str>)] = &[
    ("root", None),      // no password
    ("axiomdb", None),   // no password
];
```

Accept any connection where:
- Username is in `ALLOWED_USERS`
- Password matches (or is None → accept empty and any password)

This is deliberately permissive for development. Real auth in Phase 13.

**On auth success:** send OK_Packet (seq=2)
**On auth failure:** send ERR_Packet (seq=2) with `ER_ACCESS_DENIED_ERROR (1045)`

**Acceptance criteria 5.3:**
- [ ] `mysql -h 127.0.0.1 -P 3306 -u root` connects successfully
- [ ] `mysql -h 127.0.0.1 -P 3306 -u root -p anypwd` also connects (permissive)
- [ ] Unknown user → ERR 1045 Access denied
- [ ] `pymysql.connect(host='127.0.0.1', port=3306, user='root')` connects

---

## 5.4 — COM_QUERY Handler

After auth, the server enters the **command loop**. Each iteration:
1. Read a packet (seq=0 always from client for a new command)
2. Dispatch on the command byte

### Commands in Phase 5

| Byte | Command | Handler |
|---|---|---|
| 0x03 | COM_QUERY | Parse SQL → execute → send result |
| 0x0e | COM_PING | Send OK_Packet |
| 0x01 | COM_QUIT | Close connection |
| 0x02 | COM_INIT_DB | Change current database (SET schema stub) |
| other | → | Send ERR_Packet `ER_UNKNOWN_COM_ERROR (1047)` |

### COM_QUERY flow

```
1. Read payload[1..] as UTF-8 SQL string
2. parse(sql) → Stmt  (error → ERR_Packet with parse error)
3. analyze(stmt, storage, snap) → analyzed  (error → ERR_Packet)
4. execute_with_ctx(analyzed, storage, txn, ctx) → QueryResult
5. Serialize QueryResult:
   - Rows     → result set (column defs + rows + EOF)
   - Affected → OK_Packet
   - Empty    → OK_Packet
```

**Autocommit behavior:** Phase 5 runs in autocommit mode by default.
Each COM_QUERY wraps the statement in an implicit BEGIN/COMMIT unless
an explicit `BEGIN` is in progress.

**Session context:** Per-connection `SessionContext` is created at handshake
time and persists for the connection lifetime.

**Acceptance criteria 5.4:**
- [ ] `SELECT 1` returns a result row `[1]`
- [ ] `SELECT version()` returns `"8.0.36-AxiomDB-0.1.0"`
- [ ] `CREATE TABLE t (id INT)` returns OK (0 rows affected)
- [ ] `INSERT INTO t VALUES (1)` returns OK (1 row affected)
- [ ] Syntax error returns ERR with SQLSTATE 42601

---

## 5.5 — Result Set Serialization (Text Protocol)

For `QueryResult::Rows`, send the **MySQL text protocol result set**:

### Step 1: Column count packet
```
lenenc_int(num_columns)
```

### Step 2: Column definition packets (one per column)

```
catalog: lenenc_str = "def"
schema:  lenenc_str = schema_name
table:   lenenc_str = table_name (or alias)
org_table: lenenc_str = table_name
name:    lenenc_str = column_name
org_name: lenenc_str = column_name
next_length: lenenc_int = 0x0c
character_set: u16 = 255  (utf8mb4)
column_length: u32  (display width, type-dependent)
type: u8  (MySQL column type code — see below)
flags: u16
decimals: u8
filler: u16 = 0x0000
```

**AxiomDB DataType → MySQL type code:**
| AxiomDB | MySQL code | Name |
|---|---|---|
| Int | 0x03 | FIELD_TYPE_LONG |
| BigInt | 0x08 | FIELD_TYPE_LONGLONG |
| Real | 0x05 | FIELD_TYPE_DOUBLE |
| Decimal | 0x00 | FIELD_TYPE_DECIMAL |
| Bool | 0x10 | FIELD_TYPE_BIT (display as TINYINT) |
| Text | 0xfd | FIELD_TYPE_VAR_STRING |
| Bytes | 0xfc | FIELD_TYPE_BLOB |
| Date | 0x0a | FIELD_TYPE_DATE |
| Timestamp | 0x07 | FIELD_TYPE_TIMESTAMP |
| Uuid | 0xfd | FIELD_TYPE_VAR_STRING |

### Step 3: EOF packet (after column defs)

```
[0xfe] [warnings: u16 = 0] [status_flags: u16 = 0x0002]
```

### Step 4: Row data packets (one per row)

Each column value is encoded as a **length-encoded string**:
- NULL → single byte `0xfb`
- Non-null → lenenc_int(len) + utf8_bytes

**Value → text encoding:**
| Type | Text representation |
|---|---|
| Int(n) | `n.to_string()` |
| BigInt(n) | `n.to_string()` |
| Real(f) | `format!("{f}")` |
| Decimal(m,s) | decimal string with s fractional digits |
| Bool(true) | `"1"` |
| Bool(false) | `"0"` |
| Text(s) | s as UTF-8 bytes |
| Bytes(b) | raw bytes |
| Null | `0xfb` (NULL marker, not a string) |
| Date(d) | `"YYYY-MM-DD"` |
| Timestamp(t) | `"YYYY-MM-DD HH:MM:SS"` |
| Uuid(u) | `"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"` |

### Step 5: EOF packet (end of rows)

Same as step 3.

**Acceptance criteria 5.5:**
- [ ] `SELECT 1, 'hello', NULL, TRUE` returns row `["1", "hello", NULL, "1"]`
- [ ] Multi-row SELECT returns all rows correctly
- [ ] Column metadata (name, type) is correct in result
- [ ] NULL columns encode as 0xfb

---

## 5.6 — Error Packets

For any error from parse/analyze/execute, send an **ERR_Packet**:

```
[0xff]
[error_code: u16 LE]
['#': u8]
[sql_state: 5 ASCII bytes]
[error_message: string<EOF>]
```

**DbError → MySQL error mapping:**
| DbError | MySQL code | SQLSTATE |
|---|---|---|
| ParseError | 1064 `ER_PARSE_ERROR` | 42000 |
| TableNotFound | 1146 `ER_NO_SUCH_TABLE` | 42S02 |
| ColumnNotFound | 1054 `ER_BAD_FIELD_ERROR` | 42S22 |
| ColumnAlreadyExists | 1060 `ER_DUP_FIELDNAME` | 42701 |
| TableAlreadyExists | 1050 `ER_TABLE_EXISTS_ERROR` | 42S01 |
| UniqueViolation | 1062 `ER_DUP_ENTRY` | 23000 |
| NotNullViolation | 1048 `ER_BAD_NULL_ERROR` | 23000 |
| CardinalityViolation | 1242 `ER_SUBQUERY_NO_1_ROW` | 21000 |
| DivisionByZero | 1365 `ER_DIVISION_BY_ZERO` | 22012 |
| NotImplemented | 1235 `ER_NOT_SUPPORTED_YET` | 0A000 |
| Internal | 1105 `ER_UNKNOWN_ERROR` | HY000 |
| other | 1105 | HY000 |

**Acceptance criteria 5.6:**
- [ ] `SELECT * FROM nonexistent` → error 1146 (Table doesn't exist)
- [ ] Syntax error → error 1064
- [ ] MySQL client displays error message correctly
- [ ] `SHOW WARNINGS` stub (returns empty, no crash)

---

## 5.7 — Test with Real Client

At least two clients must be able to connect and execute queries:

### Test 1: Python pymysql

```python
import pymysql
conn = pymysql.connect(host='127.0.0.1', port=3306, user='root', db='')
cursor = conn.cursor()
cursor.execute("CREATE TABLE test (id INT, name TEXT)")
cursor.execute("INSERT INTO test VALUES (1, 'Alice')")
cursor.execute("SELECT * FROM test")
rows = cursor.fetchall()
assert rows == ((1, 'Alice'),)
cursor.execute("SELECT version()")
assert cursor.fetchone()[0].startswith('8.0')
conn.close()
```

### Test 2: mysql CLI

```bash
mysql -h 127.0.0.1 -P 3306 -u root -e "SHOW TABLES"
mysql -h 127.0.0.1 -P 3306 -u root -e "SELECT 1+1"
```

**Acceptance criteria 5.7:**
- [ ] pymysql test above passes completely
- [ ] mysql CLI connects without `--protocol tcp` or extra flags
- [ ] `SELECT @@version` returns a non-empty string (ORMs check this on connect)
- [ ] `SET NAMES utf8mb4` returns OK without error
- [ ] `SET autocommit=1` returns OK without error

---

## Out of scope (Phase 5 critical path)

- TLS / SSL — Phase 13
- PostgreSQL wire protocol — Phase 9
- Prepared statements (COM_STMT_PREPARE) — Phase 5.10
- Multi-statement queries — Phase 5.12
- Compression — Phase N
- Real authentication (bcrypt, argon2) — Phase 13
- `SHOW VARIABLES` full response — partial stub OK
- `SHOW STATUS` — stub OK (return empty)
- `SHOW DATABASES` — stub returning ["axiomdb"] OK

## ⚠️ DEFERRED

- Binary result set protocol (for prepared stmts) → Phase 5.10
- caching_sha2_password → Phase 5.3b
- Session variables persistence (SET autocommit etc.) → Phase 5.9
- ON_ERROR behavior → Phase 5.2c
- Charset transcoding (Latin1, CP1252) → Phase 5.2a

## Dependencies

- `axiomdb-sql`: parse + analyze + execute_with_ctx — already complete
- `tokio` with features = ["full"] — already in server deps
- `tokio-util` with `codec` feature — for packet framing
- `bytes` — for byte manipulation
- `sha1` crate — for mysql_native_password
- `rand` — for challenge bytes in handshake
- New crate: `axiomdb-mysql-wire` — or use existing `axiomdb-network`
