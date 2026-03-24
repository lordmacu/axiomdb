# Spec: 5.10 — COM_STMT_PREPARE / COM_STMT_EXECUTE (Prepared Statements)

## What to build

Full MySQL prepared statement protocol support:
- `COM_STMT_PREPARE (0x16)` — parse SQL with `?` placeholders, return stmt metadata
- `COM_STMT_EXECUTE (0x17)` — decode binary parameters, execute, return result
- `COM_STMT_CLOSE (0x19)` — release statement from per-connection cache
- `COM_STMT_RESET (0x1a)` — acknowledge (no state to reset in Phase 5.10)

After this, all ORMs that use prepared statements (SQLAlchemy, ActiveRecord,
Django ORM, Prisma, GORM, Hibernate, etc.) can execute parameterized queries.

## Why this matters

Every ORM sends parameterized queries via prepared statements:
```python
# SQLAlchemy
session.execute(select(User).where(User.id == 42))
# Generates: COM_STMT_PREPARE("SELECT * FROM users WHERE id = ?")
#            COM_STMT_EXECUTE(stmt_id, params=[42])
```

Without prepared statement support, ORMs fall back to plain COM_QUERY with
SQL string interpolation (unsafe) or fail entirely.

---

## Wire Protocol

### COM_STMT_PREPARE (0x16)

**Client → Server:**
```
[0x16][sql bytes as UTF-8, no null terminator]
```

**Server → Client (success):**
```
Packet 1 — Statement OK (seq=1):
  [0x00]          ← status = OK
  [stmt_id: u32 LE]
  [num_cols: u16 LE]   ← columns in the result set (0 for INSERT/UPDATE/DELETE)
  [num_params: u16 LE] ← number of ? placeholders
  [reserved: u8 = 0]
  [warning_count: u16 LE = 0]

IF num_params > 0:
  Packets 2..N+1: column definition for each parameter (generic — Phase 5.10 uses placeholder)
  Packet N+2: EOF

IF num_cols > 0:
  Packets ...: column definition for each result column
  Final: EOF
```

For Phase 5.10, parameter column definitions are stubs (all Text type) since
the actual type is determined at execute time from the client's type list.

**Server → Client (error):**
```
ERR_Packet with parse error code
```

### COM_STMT_EXECUTE (0x17)

**Client → Server:**
```
[0x17]
[stmt_id: u32 LE]
[flags: u8]         ← 0x00 = no cursor; ignored in Phase 5.10
[iteration_count: u32 LE = 1]   ← always 1
[null_bitmap: ceil(N/8) bytes]  ← bit i=1 means param i is NULL
[new_params_bound_flag: u8]     ← 1 if type list follows (usually 1 on first exec)
IF new_params_bound_flag == 1:
  N × [type_code: u16 LE]       ← MySQL column type; high bit = unsigned flag
[binary parameter values...]    ← one per non-null param, type-specific encoding
```

**Server → Client:** Same as COM_QUERY response (text protocol):
- SELECT → result set (column defs + EOF + rows + EOF)
- INSERT/UPDATE/DELETE → OK_Packet with affected_rows + last_insert_id
- Error → ERR_Packet

### COM_STMT_CLOSE (0x19)

```
[0x19][stmt_id: u32 LE]
```
No response. Server removes the statement from its per-connection cache.

### COM_STMT_RESET (0x1a)

```
[0x1a][stmt_id: u32 LE]
```
Server responds with OK_Packet. No cursor state in Phase 5.10.

---

## Null Bitmap

For N parameters: `ceil(N / 8)` bytes.
Bit `(i % 8)` of byte `(i / 8)` is set if parameter `i` (0-indexed) is NULL.

```rust
fn is_null(null_bitmap: &[u8], param_idx: usize) -> bool {
    let byte_idx = param_idx / 8;
    let bit_idx = param_idx % 8;
    byte_idx < null_bitmap.len() && (null_bitmap[byte_idx] >> bit_idx) & 1 == 1
}
```

---

## Binary Parameter Decoding

Each non-null parameter is encoded according to its MySQL type:

| MySQL Type Code | Name | Bytes | Decode to |
|---|---|---|---|
| 0x00 | DECIMAL | lenenc+str | Value::Text (then parse) |
| 0x01 | TINY | 1 | Value::Int (i8 as i32) |
| 0x02 | SHORT | 2 LE | Value::Int (i16 as i32) |
| 0x03 | LONG | 4 LE | Value::Int (i32) |
| 0x04 | FLOAT | 4 LE | Value::Real (f32 as f64) |
| 0x05 | DOUBLE | 8 LE | Value::Real (f64) |
| 0x06 | NULL | 0 | Value::Null (null bitmap) |
| 0x07 | TIMESTAMP | lenenc | Value::Timestamp |
| 0x08 | LONGLONG | 8 LE | Value::BigInt (i64) |
| 0x09 | INT24 | 4 LE | Value::Int (i32, only lower 3 bytes used) |
| 0x0a | DATE | lenenc | Value::Date |
| 0x0b | TIME | lenenc | → unused, emit NULL |
| 0x0c | DATETIME | lenenc | Value::Timestamp |
| 0x0f | VARCHAR | lenenc+str | Value::Text |
| 0xf6 | NEWDECIMAL | lenenc+str | Value::Text |
| 0xfc | BLOB | lenenc+bytes | Value::Bytes or Value::Text |
| 0xfd | VAR_STRING | lenenc+bytes | Value::Text |
| 0xfe | STRING | lenenc+bytes | Value::Text |

**DATE format:** `[len: u8][year: u16 LE][month: u8][day: u8]`
**DATETIME/TIMESTAMP format:** `[len: u8][year: u16][month][day][hour][min][sec][microsec: u32 LE]`

---

## Parameter Substitution

After decoding all parameters into `Vec<Value>`, replace `?` placeholders in the
SQL template to form a valid SQL string:

```rust
fn substitute_params(template: &str, params: &[Value]) -> Result<String, DbError> {
    let mut result = String::with_capacity(template.len() + params.len() * 8);
    let mut param_idx = 0;
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '?' {
            if param_idx >= params.len() {
                return Err(DbError::ParseError { message: "too few parameters".into() });
            }
            result.push_str(&value_to_sql_literal(&params[param_idx]));
            param_idx += 1;
        } else if ch == '\'' {
            // Pass through string literals unchanged (? inside strings is not a param)
            result.push(ch);
            while let Some(c) = chars.next() {
                result.push(c);
                if c == '\'' { break; }
            }
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}

fn value_to_sql_literal(v: &Value) -> String {
    match v {
        Value::Null        => "NULL".into(),
        Value::Bool(b)     => if *b { "1".into() } else { "0".into() },
        Value::Int(n)      => n.to_string(),
        Value::BigInt(n)   => n.to_string(),
        Value::Real(f)     => format!("{f}"),
        Value::Text(s)     => format!("'{}'", s.replace('\'', "''")),  // escape single quotes
        Value::Bytes(b)    => format!("x'{}'", hex::encode(b)),
        Value::Decimal(m,s) => format_decimal(*m, *s),
        Value::Date(d)     => format!("'{}'", format_date_sql(*d)),
        Value::Timestamp(t) => format!("'{}'", format_timestamp_sql(*t)),
        Value::Uuid(u)     => format!("'{}'", format_uuid(u)),
    }
}
```

---

## Per-Connection Statement Cache

```rust
pub struct PreparedStatement {
    pub stmt_id: u32,
    /// Original SQL with ? placeholders.
    pub sql_template: String,
    /// Number of ? placeholders.
    pub param_count: u16,
    /// Client-declared parameter types (populated from first COM_STMT_EXECUTE).
    pub param_types: Vec<u16>,
}
```

The cache lives in `ConnectionState`:
```rust
pub struct ConnectionState {
    // ... existing fields ...
    pub prepared_statements: HashMap<u32, PreparedStatement>,
    pub next_stmt_id: u32,
}
```

`next_stmt_id` increments monotonically per connection (wraps at u32::MAX).

---

## Inputs / Outputs

### COM_STMT_PREPARE

- Input: `"SELECT * FROM users WHERE id = ? AND active = ?"`
- Output: Statement OK with `stmt_id=1, num_cols=4, num_params=2`
  + 2 parameter column defs (stubs) + EOF
  + 4 result column defs + EOF

- Input: `"INSERT INTO t VALUES (?, ?)"`
- Output: Statement OK with `stmt_id=2, num_cols=0, num_params=2`
  + 2 parameter column defs + EOF
  + (no result column defs — num_cols=0)

- Input: malformed SQL
- Output: ERR_Packet (1064 parse error)

### COM_STMT_EXECUTE

- Input: stmt_id=1, params=[Value::Int(42), Value::Bool(true)]
- Output: result set (text protocol) for `SELECT * FROM users WHERE id = 42 AND active = 1`

- Input: stmt_id=2, params=[Value::Int(1), Value::Text("Alice")]
- Output: OK_Packet (affected_rows=1, last_insert_id=auto)

- Input: stmt_id=999 (unknown)
- Output: ERR_Packet 1243 (unknown prepared statement handler)

### COM_STMT_CLOSE

- Input: stmt_id=1
- Effect: stmt removed from connection's cache
- Output: (none — server does not respond)

---

## Acceptance Criteria

- [ ] `pymysql.connect(cursorclass=pymysql.cursors.DictCursor)` + parameterized queries work
- [ ] `cursor.execute("SELECT * FROM t WHERE id = %s", (42,))` executes correctly
- [ ] `cursor.execute("INSERT INTO t VALUES (%s, %s)", (1, 'hello'))` inserts correctly
- [ ] Null parameter encoded as NULL in the query
- [ ] Multiple executions of same stmt_id work
- [ ] COM_STMT_CLOSE removes the statement
- [ ] Unknown stmt_id → ERR 1243
- [ ] SQLAlchemy `create_engine("mysql+pymysql://root@127.0.0.1:13306/axiomdb")` + `text()` queries work
- [ ] Protocol unit tests: parse_execute_packet, decode_binary_params, substitute_params

---

## Out of Scope

- Binary result set protocol (5.5a) — text protocol is sufficient for Phase 5.10
- COM_STMT_SEND_LONG_DATA (5.11b) — chunked large parameters
- Named parameters (`:name` style) — MySQL uses `?` only
- Plan reuse without re-parsing (5.13) — parse on every EXECUTE in Phase 5.10
- Cursor support (cursor flag in EXECUTE) — Phase N

## ⚠️ DEFERRED

- Binary result encoding → Phase 5.5a
- Plan cache (parse once) → Phase 5.13
- COM_STMT_SEND_LONG_DATA → Phase 5.11b

## Dependencies

- `hex` crate (for `x'...'` binary literal encoding in `value_to_sql_literal`)
  OR just encode bytes as escaped string
- `PreparedStatement` struct in `session.rs`
- `ConnectionState.prepared_statements: HashMap<u32, PreparedStatement>`
- New functions: `parse_execute_packet`, `decode_binary_params`, `substitute_params`
- New handlers in `handler.rs`: 0x16, 0x17, 0x19, 0x1a
