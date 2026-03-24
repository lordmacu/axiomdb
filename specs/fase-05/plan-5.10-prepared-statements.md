# Plan: 5.10 — COM_STMT_PREPARE / COM_STMT_EXECUTE

## Files to create/modify

| File | Action | What |
|---|---|---|
| `Cargo.toml` (workspace) | check | No new deps needed (hex encoding inline) |
| `crates/axiomdb-network/src/mysql/session.rs` | modify | Add `PreparedStatement` struct + fields to `ConnectionState` |
| `crates/axiomdb-network/src/mysql/prepared.rs` | create | Binary param decoding + SQL substitution + PREPARE response builder |
| `crates/axiomdb-network/src/mysql/handler.rs` | modify | Add 0x16/0x17/0x19/0x1a handlers in command loop |
| `crates/axiomdb-network/src/mysql/mod.rs` | modify | Export `prepared` module |
| `crates/axiomdb-network/tests/integration_protocol.rs` | modify | Add prepared statement tests |

---

## Phase A — session.rs: PreparedStatement + ConnectionState fields

```rust
/// A compiled prepared statement stored per-connection.
pub struct PreparedStatement {
    pub stmt_id: u32,
    /// SQL with ? placeholders, as sent by the client.
    pub sql_template: String,
    /// Number of ? placeholders (detected at prepare time).
    pub param_count: u16,
    /// MySQL type codes for each parameter (set on first COM_STMT_EXECUTE).
    pub param_types: Vec<u16>,
}

// Add to ConnectionState:
pub struct ConnectionState {
    // ... existing fields ...
    pub prepared_statements: std::collections::HashMap<u32, PreparedStatement>,
    pub next_stmt_id: u32,
}
```

Add to `ConnectionState::new()`:
```rust
prepared_statements: HashMap::new(),
next_stmt_id: 1,
```

Add method:
```rust
pub fn prepare_statement(&mut self, sql: String) -> (u32, u16) {
    let param_count = count_params(&sql);
    let stmt_id = self.next_stmt_id;
    self.next_stmt_id = self.next_stmt_id.wrapping_add(1).max(1); // never 0
    self.prepared_statements.insert(stmt_id, PreparedStatement {
        stmt_id,
        sql_template: sql,
        param_count,
        param_types: vec![],
    });
    (stmt_id, param_count)
}

/// Count unquoted ? in SQL (placeholders, not ? inside string literals).
fn count_params(sql: &str) -> u16 {
    let mut count = 0u16;
    let mut in_string = false;
    let mut prev = '\0';
    for ch in sql.chars() {
        match ch {
            '\'' if !in_string => in_string = true,
            '\'' if in_string && prev != '\\' => in_string = false,
            '?' if !in_string => count += 1,
            _ => {}
        }
        prev = ch;
    }
    count
}
```

---

## Phase B — prepared.rs: Core logic

### Build PREPARE response

```rust
/// Builds the COM_STMT_PREPARE response packet sequence.
///
/// Sequence:
///   seq=1: Statement OK (stmt_id, num_cols, num_params)
///   seq=2..N: parameter column defs (if num_params > 0)
///   seq=N+1: EOF (if num_params > 0)
///   seq=N+2..: result column defs (if num_cols > 0)
///   seq=last: EOF (if num_cols > 0)
pub fn build_prepare_response(
    stmt_id: u32,
    num_params: u16,
    result_cols: &[ColumnMeta],
    seq_start: u8,
) -> Vec<(u8, Vec<u8>)> {
    let mut packets = Vec::new();
    let mut seq = seq_start;

    // Statement OK packet
    let mut ok = Vec::with_capacity(12);
    ok.push(0x00); // status = OK
    ok.extend_from_slice(&stmt_id.to_le_bytes());
    ok.extend_from_slice(&(result_cols.len() as u16).to_le_bytes()); // num_cols
    ok.extend_from_slice(&num_params.to_le_bytes());
    ok.push(0x00); // reserved
    ok.extend_from_slice(&0u16.to_le_bytes()); // warning_count
    packets.push((seq, ok));
    seq += 1;

    // Parameter column definitions (stubs — all TEXT type)
    for _ in 0..num_params {
        packets.push((seq, build_stub_column_def("?")));
        seq += 1;
    }
    if num_params > 0 {
        packets.push((seq, build_eof_packet()));
        seq += 1;
    }

    // Result column definitions
    for col in result_cols {
        packets.push((seq, build_column_def(col)));
        seq += 1;
    }
    if !result_cols.is_empty() {
        packets.push((seq, build_eof_packet()));
    }

    packets
}

fn build_stub_column_def(name: &str) -> Vec<u8> {
    // Minimal column def — type=VARCHAR, nullable
    let mut buf = Vec::new();
    write_lenenc_str(&mut buf, b"def");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, name.as_bytes());
    write_lenenc_str(&mut buf, name.as_bytes());
    write_lenenc_int(&mut buf, 0x0c);
    buf.extend_from_slice(&255u16.to_le_bytes()); // charset = utf8mb4
    buf.extend_from_slice(&255u32.to_le_bytes()); // column_length
    buf.push(0xfd); // type = VAR_STRING
    buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    buf.push(0u8); // decimals
    buf.extend_from_slice(&0u16.to_le_bytes()); // filler
    buf
}
```

### Parse COM_STMT_EXECUTE payload

```rust
pub struct ExecutePacket {
    pub stmt_id: u32,
    pub params: Vec<Value>,
}

/// Parses a COM_STMT_EXECUTE payload (after the 0x17 command byte).
pub fn parse_execute_packet(payload: &[u8], param_count: u16, stmt: &mut PreparedStatement) -> Result<ExecutePacket, &'static str> {
    if payload.len() < 9 {
        return Err("execute packet too short");
    }

    let stmt_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    // payload[4] = flags (ignored)
    // payload[5..9] = iteration_count (always 1, ignored)
    let mut pos = 9usize;

    let n = param_count as usize;
    if n == 0 {
        return Ok(ExecutePacket { stmt_id, params: vec![] });
    }

    // Null bitmap: ceil(n/8) bytes
    let bitmap_len = (n + 7) / 8;
    if pos + bitmap_len > payload.len() {
        return Err("null bitmap truncated");
    }
    let null_bitmap = &payload[pos..pos + bitmap_len];
    pos += bitmap_len;

    // new_params_bound_flag
    if pos >= payload.len() { return Err("missing bound flag"); }
    let bound = payload[pos] == 1;
    pos += 1;

    // Update stored types if bound
    if bound {
        if pos + n * 2 > payload.len() { return Err("type list truncated"); }
        stmt.param_types = (0..n)
            .map(|i| u16::from_le_bytes([payload[pos + i*2], payload[pos + i*2 + 1]]))
            .collect();
        pos += n * 2;
    }

    // Decode values
    let mut params = Vec::with_capacity(n);
    for i in 0..n {
        if is_null(null_bitmap, i) {
            params.push(Value::Null);
            continue;
        }
        let type_code = stmt.param_types.get(i).copied().unwrap_or(0xfd);
        let (value, consumed) = decode_binary_value(&payload[pos..], type_code)?;
        params.push(value);
        pos += consumed;
    }

    Ok(ExecutePacket { stmt_id, params })
}

fn is_null(bitmap: &[u8], idx: usize) -> bool {
    let byte = idx / 8;
    let bit = idx % 8;
    byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1
}

/// Decodes one binary-encoded parameter value.
/// Returns (value, bytes_consumed).
fn decode_binary_value(buf: &[u8], type_code: u16) -> Result<(Value, usize), &'static str> {
    let type_base = type_code & 0x00FF; // strip unsigned flag
    match type_base {
        0x01 => { // TINY
            if buf.is_empty() { return Err("TINY truncated"); }
            Ok((Value::Int(buf[0] as i8 as i32), 1))
        }
        0x02 => { // SHORT
            if buf.len() < 2 { return Err("SHORT truncated"); }
            Ok((Value::Int(i16::from_le_bytes([buf[0], buf[1]]) as i32), 2))
        }
        0x03 | 0x09 => { // LONG / INT24
            if buf.len() < 4 { return Err("LONG truncated"); }
            Ok((Value::Int(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])), 4))
        }
        0x08 => { // LONGLONG
            if buf.len() < 8 { return Err("LONGLONG truncated"); }
            Ok((Value::BigInt(i64::from_le_bytes(buf[..8].try_into().unwrap())), 8))
        }
        0x04 => { // FLOAT
            if buf.len() < 4 { return Err("FLOAT truncated"); }
            let f = f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            Ok((Value::Real(f as f64), 4))
        }
        0x05 => { // DOUBLE
            if buf.len() < 8 { return Err("DOUBLE truncated"); }
            Ok((Value::Real(f64::from_le_bytes(buf[..8].try_into().unwrap())), 8))
        }
        0x00 | 0xf6 => { // DECIMAL / NEWDECIMAL — lenenc string
            let (s, consumed) = read_lenenc_str(buf)?;
            Ok((Value::Text(s), consumed))
        }
        0x0a => { // DATE
            let (len, llen) = read_lenenc_int(buf)?;
            let len = len as usize;
            let data = &buf[llen..llen+len];
            let date_val = decode_date(data);
            Ok((date_val, llen + len))
        }
        0x07 | 0x0c => { // TIMESTAMP / DATETIME
            let (len, llen) = read_lenenc_int(buf)?;
            let len = len as usize;
            let data = &buf[llen..llen+len];
            let ts_val = decode_datetime(data);
            Ok((ts_val, llen + len))
        }
        0x0f | 0xfd | 0xfe | 0xfc | 0xf5 => { // VARCHAR / VAR_STRING / STRING / BLOB
            let (s, consumed) = read_lenenc_str(buf)?;
            Ok((Value::Text(s), consumed))
        }
        _ => { // Unknown type — read as string
            let (s, consumed) = read_lenenc_str(buf)?;
            Ok((Value::Text(s), consumed))
        }
    }
}

fn read_lenenc_int(buf: &[u8]) -> Result<(u64, usize), &'static str> {
    if buf.is_empty() { return Err("lenenc truncated"); }
    match buf[0] {
        0..=250 => Ok((buf[0] as u64, 1)),
        0xfc => {
            if buf.len() < 3 { return Err("lenenc 2b truncated"); }
            Ok((u16::from_le_bytes([buf[1], buf[2]]) as u64, 3))
        }
        0xfd => {
            if buf.len() < 4 { return Err("lenenc 3b truncated"); }
            Ok((u32::from_le_bytes([buf[1], buf[2], buf[3], 0]) as u64, 4))
        }
        0xfe => {
            if buf.len() < 9 { return Err("lenenc 8b truncated"); }
            Ok((u64::from_le_bytes(buf[1..9].try_into().unwrap()), 9))
        }
        _ => Err("invalid lenenc byte"),
    }
}

fn read_lenenc_str(buf: &[u8]) -> Result<(String, usize), &'static str> {
    let (len, llen) = read_lenenc_int(buf)?;
    let len = len as usize;
    if buf.len() < llen + len { return Err("lenenc string truncated"); }
    let s = String::from_utf8_lossy(&buf[llen..llen+len]).into_owned();
    Ok((s, llen + len))
}

fn decode_date(buf: &[u8]) -> Value {
    if buf.len() < 4 { return Value::Null; }
    let year = u16::from_le_bytes([buf[0], buf[1]]) as i32;
    let month = buf[2] as i32;
    let day = buf[3] as i32;
    // Convert to days since Unix epoch
    let days = ymd_to_days(year, month, day);
    Value::Date(days)
}

fn decode_datetime(buf: &[u8]) -> Value {
    if buf.len() < 4 { return Value::Null; }
    let year = u16::from_le_bytes([buf[0], buf[1]]) as i64;
    let month = buf[2] as i64;
    let day = buf[3] as i64;
    let hour   = if buf.len() > 4 { buf[4] as i64 } else { 0 };
    let minute = if buf.len() > 5 { buf[5] as i64 } else { 0 };
    let second = if buf.len() > 6 { buf[6] as i64 } else { 0 };
    let days = ymd_to_days(year as i32, month as i32, day as i32) as i64;
    let secs = days * 86400 + hour * 3600 + minute * 60 + second;
    Value::Timestamp(secs * 1_000_000)
}

/// Converts year/month/day to days since Unix epoch (1970-01-01).
fn ymd_to_days(year: i32, month: i32, day: i32) -> i32 {
    // Algorithm: https://howardhinnant.github.io/date_algorithms.html
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let y = if month <= 2 { year - 1 } else { year } - 1970;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m as u32 + 2) / 5 + day as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe as i32 - 719468) as i32
}
```

### SQL Substitution

```rust
/// Replaces ? placeholders with SQL literals.
/// Safe: strings are single-quote escaped, not raw user input.
pub fn substitute_params(template: &str, params: &[Value]) -> Result<String, DbError> {
    let mut result = String::with_capacity(template.len() + params.len() * 8);
    let mut param_idx = 0;
    let mut in_string = false;

    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            b'\'' => {
                in_string = !in_string;
                result.push('\'');
            }
            b'?' if !in_string => {
                if param_idx >= params.len() {
                    return Err(DbError::ParseError {
                        message: format!("not enough parameters: need >{param_idx} but got {}", params.len()),
                    });
                }
                result.push_str(&value_to_sql_literal(&params[param_idx]));
                param_idx += 1;
            }
            _ => result.push(ch as char),
        }
        i += 1;
    }
    Ok(result)
}

fn value_to_sql_literal(v: &Value) -> String {
    match v {
        Value::Null         => "NULL".into(),
        Value::Bool(b)      => if *b { "1".into() } else { "0".into() },
        Value::Int(n)       => n.to_string(),
        Value::BigInt(n)    => n.to_string(),
        Value::Real(f)      => format!("{f}"),
        Value::Text(s)      => format!("'{}'", s.replace('\'', "''")),
        Value::Bytes(b)     => {
            // Hex literal: x'deadbeef'
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("x'{hex}'")
        }
        Value::Decimal(m,s) => {
            // re-use the format_decimal from result.rs
            crate::mysql::result::format_decimal_pub(*m, *s)
        }
        Value::Date(d)      => format!("'{}'", crate::mysql::result::format_date_pub(*d)),
        Value::Timestamp(t) => format!("'{}'", crate::mysql::result::format_timestamp_pub(*t)),
        Value::Uuid(u)      => {
            let s: String = {
                let b = u;
                format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15])
            };
            format!("'{s}'")
        }
    }
}
```

**Note:** `format_decimal_pub`, `format_date_pub`, `format_timestamp_pub` need to be
exported from `result.rs` (currently private). Add `pub(crate)` visibility.

---

## Phase C — handler.rs: New command cases

Add to the command loop match:

```rust
// COM_STMT_PREPARE
0x16 => {
    let sql = match std::str::from_utf8(body) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            let e = build_err_packet(1064, b"42000", "Invalid UTF-8 in prepare");
            let _ = writer.send((1u8, e.as_slice())).await;
            continue;
        }
    };
    debug!(conn_id, sql = %sql, "COM_STMT_PREPARE");

    // Parse to validate SQL and get result column metadata.
    // For simplicity in Phase 5.10: parse, analyze, but don't cache the plan.
    let (stmt_id, param_count) = conn_state.prepare_statement(sql.clone());

    // Try to analyze to get result column info.
    let result_cols: Vec<ColumnMeta> = {
        let guard = db.lock().await;
        let snap = guard.txn.active_snapshot().unwrap_or_else(|_| guard.txn.snapshot());
        match parse(&sql, None).and_then(|s| analyze(s, &guard.storage, snap)) {
            Ok(analyzed) => extract_result_columns(&analyzed),
            Err(_) => vec![], // columns unknown at prepare time, that's OK
        }
    };

    let packets = build_prepare_response(stmt_id, param_count, &result_cols, 1);
    for (seq, pkt) in packets {
        if writer.send((seq, pkt.as_slice())).await.is_err() { break; }
    }
}

// COM_STMT_EXECUTE
0x17 => {
    let result = {
        // Find the prepared statement
        let param_count = match conn_state.prepared_statements.get(
            &u32::from_le_bytes(body.get(0..4).and_then(|b| b.try_into().ok()).unwrap_or([0;4]))
        ) {
            Some(s) => s.param_count,
            None => {
                let e = build_err_packet(1243, b"HY000", "Unknown prepared statement handler");
                let _ = writer.send((1u8, e.as_slice())).await;
                continue;
            }
        };

        let stmt_id = u32::from_le_bytes(body.get(0..4).unwrap_or(&[0,0,0,0]).try_into().unwrap_or([0;4]));

        // Parse execute packet
        let stmt = conn_state.prepared_statements.get_mut(&stmt_id).unwrap();
        match parse_execute_packet(body, param_count, stmt) {
            Ok(exec) => {
                // Substitute parameters into SQL
                match substitute_params(&stmt.sql_template.clone(), &exec.params) {
                    Ok(final_sql) => {
                        let mut guard = db.lock().await;
                        guard.execute_query(&final_sql, &mut session)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(msg) => Err(DbError::ParseError { message: msg.into() }),
        }
    };

    match result {
        Ok(qr) => {
            for (seq, pkt) in serialize_query_result(qr, 1) {
                if writer.send((seq, pkt.as_slice())).await.is_err() { break; }
            }
        }
        Err(e) => {
            let me = dberror_to_mysql(&e);
            let pkt = build_err_packet(me.code, &me.sql_state, &me.message);
            let _ = writer.send((1u8, pkt.as_slice())).await;
        }
    }
}

// COM_STMT_CLOSE
0x19 => {
    if body.len() >= 4 {
        let stmt_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        conn_state.prepared_statements.remove(&stmt_id);
    }
    // No response for COM_STMT_CLOSE
}

// COM_STMT_RESET
0x1a => {
    let ok = build_ok_packet(0, 0, 0);
    let _ = writer.send((1u8, ok.as_slice())).await;
}
```

---

## Phase D — Export format helpers from result.rs

Add `pub(crate)` to `format_decimal`, `format_date`, `format_timestamp` in `result.rs`.

---

## Phase E — Tests

Add to `tests/integration_protocol.rs`:
```rust
#[test] fn test_count_params_basic()
#[test] fn test_count_params_inside_string_ignored()
#[test] fn test_substitute_params_int()
#[test] fn test_substitute_params_text_escaping()
#[test] fn test_substitute_params_null()
#[test] fn test_decode_tiny_param()
#[test] fn test_decode_long_param()
#[test] fn test_decode_string_param()
#[test] fn test_null_bitmap_basic()
#[test] fn test_prepare_response_packet_structure()
```

Live test (pymysql):
```python
cursor.execute("SELECT * FROM t WHERE id = %s", (42,))
cursor.execute("INSERT INTO t VALUES (%s, %s)", (1, 'hello'))
cursor.execute("SELECT * FROM t WHERE name = %s AND active = %s", ('Alice', True))
```

---

## Anti-patterns to avoid

- **DO NOT** use `format!` to substitute params without escaping — always use
  `value_to_sql_literal` which escapes single quotes in strings.

- **DO NOT** advance `pos` past the payload length when decoding params — check
  bounds before every read and return `Err("truncated")` if insufficient.

- **DO NOT** send a response for COM_STMT_CLOSE — this is one of the few MySQL
  commands that has no server response. Sending OK causes clients to hang waiting
  for a second acknowledgment.

- **DO NOT** increment `next_stmt_id` to 0 — stmt_id=0 is reserved in MySQL
  protocol. Always clamp to ≥ 1.

## Risks

- **`extract_result_columns` accuracy** — getting the exact result column types
  at PREPARE time requires executing a dry-run or using the analyzer's type
  inference. For Phase 5.10, return an empty list if analysis fails; clients
  don't require perfect column metadata at prepare time.

- **String literal scanning for `?`** — the current `substitute_params` handles
  single-quoted strings but not double-quoted identifiers or `$$...$$` style.
  For Phase 5.10 this is sufficient (MySQL uses single quotes for strings).
