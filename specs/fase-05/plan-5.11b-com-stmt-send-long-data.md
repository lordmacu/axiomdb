# Plan: 5.11b — COM_STMT_SEND_LONG_DATA

## Files to create/modify

| File | Action | What |
|---|---|---|
| `crates/axiomdb-network/src/mysql/session.rs` | modify | Extend `PreparedStatement` with stmt-local pending long-data buffers and deferred long-data error state; add helper methods to append/reset those buffers without touching the engine |
| `crates/axiomdb-network/src/mysql/prepared.rs` | modify | Merge pending long data into `COM_STMT_EXECUTE` parameter decoding; classify text-like vs binary-like MySQL param types; correct inline BLOB decode to `Value::Bytes` |
| `crates/axiomdb-network/src/mysql/handler.rs` | modify | Implement `COM_STMT_SEND_LONG_DATA` (`0x18`), make `COM_STMT_RESET` statement-scoped and real, clear pending long-data state after execute attempts, and bump status counters |
| `crates/axiomdb-network/src/mysql/status.rs` | modify | Add global/session `Com_stmt_send_long_data` counters and surface them through `SHOW STATUS` |
| `crates/axiomdb-network/tests/integration_protocol.rs` | modify | Add packet-level tests for pending long data, empty long data, precedence over inline/null, execute-time clearing, and inline BLOB decode regression |
| `tools/wire-test.py` | modify | Add live low-level MySQL wire tests that send `COM_STMT_SEND_LONG_DATA` manually and verify text/binary chunk reconstruction, reset, and `SHOW STATUS` visibility |

## Research synthesis

### AxiomDB file that constrains the design
- `crates/axiomdb-network/src/mysql/handler.rs`
  - `COM_STMT_PREPARE` and `COM_STMT_EXECUTE` already live entirely in the connection task.
  - `COM_STMT_RESET` is currently a stub that ignores `stmt_id`; this subphase must make it real because long-data state is statement-scoped.
- `crates/axiomdb-network/src/mysql/session.rs`
  - `PreparedStatement` is already the per-statement ownership boundary, so pending long data belongs there.
- `crates/axiomdb-network/src/mysql/prepared.rs`
  - `parse_execute_packet()` is the only correct merge point because it already owns the inline param decode path and the `stmt.param_types` state.
- `crates/axiomdb-network/src/mysql/charset.rs`
  - Client-charset decoding already exists and must be reused only after all long-data chunks are assembled.

### Research file paths and what we borrow
- `research/mariadb-server/sql/sql_prepare.cc`
  - Borrowed:
    - packet format `[stmt_id:4][param_id:2][data]`
    - no reply on `COM_STMT_SEND_LONG_DATA`
    - long data takes precedence over inline execute bytes and null bitmap
    - `COM_STMT_RESET` clears long-data state
- `research/mariadb-server/sql/item.cc`
  - Borrowed:
    - accumulate raw bytes first, decode later
    - enforce `max_allowed_packet` on accumulated bytes per parameter
- `research/mariadb-server/tests/mysql_client_test.c`
  - Borrowed test oracles:
    - empty long data is meaningful
    - long data does not persist to the next execute
    - reset clears long data
- `research/mariadb-server/libmysqld/libmysql.c`
  - Borrowed:
    - client-side model clears long-data flags after execute
    - long data is valid only for string/binary parameter families
- `research/oceanbase/src/observer/mysql/obmp_stmt_send_long_data.cpp`
  - Borrowed:
    - keep `send_long_data` off the engine path
    - record stmt-local error state without immediate reply
- `research/oceanbase/src/observer/mysql/obmp_stmt_execute.cpp`
  - Borrowed:
    - validate the final parameter type at execute time, not at chunk arrival

### What we reject and how AxiomDB adapts it
- Reject:
  - OceanBase's separate piece-cache subsystem
  - eager text decoding on chunk arrival
  - SQL-string rewriting in the handler
- Adaptation:
  - AxiomDB stores `Option<Vec<u8>>` per placeholder inside `PreparedStatement`
  - `COM_STMT_SEND_LONG_DATA` only appends raw bytes and updates counters
  - `COM_STMT_EXECUTE` merges long-data buffers into the existing `Vec<Value>` decode path

## Algorithm / Data structure

### 1. `PreparedStatement` owns pending long-data state

Add explicit stmt-local state in `session.rs`:

```rust
#[derive(Debug, Clone)]
pub struct PendingLongDataError {
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct PreparedStatement {
    pub stmt_id: u32,
    pub sql_template: String,
    pub param_count: u16,
    pub param_types: Vec<u16>,
    pub analyzed_stmt: Option<Stmt>,
    pub compiled_at_version: u64,
    pub last_used_seq: u64,
    pub pending_long_data: Vec<Option<Vec<u8>>>,
    pub pending_long_data_error: Option<PendingLongDataError>,
}
```

`Vec<Option<Vec<u8>>>` is required, not `Vec<Vec<u8>>`, because the subphase
must distinguish:
- `None` → no long data was provided for that param
- `Some(vec![])` → explicit empty long data, which is a real value

Helper methods in `session.rs`:

```rust
impl PreparedStatement {
    fn append_long_data(&mut self, param_idx: usize, chunk: &[u8], max_len: usize) {
        // first error wins until reset/execute
        // param idx out of range -> store deferred error, no panic
        // accumulated len > max_len -> store deferred error, do not append
        // first zero-length chunk still creates Some(Vec::new())
    }

    fn clear_long_data_state(&mut self) {
        self.pending_long_data.fill(None);
        self.pending_long_data_error = None;
    }
}
```

`ConnectionState::prepare_statement()` must initialize:

```rust
pending_long_data: vec![None; param_count as usize],
pending_long_data_error: None,
```

### 2. `COM_STMT_SEND_LONG_DATA` stays on the session path

Exact handler behavior in `handler.rs`:

```rust
// 0x18 = COM_STMT_SEND_LONG_DATA
0x18 => {
    let _running = RunningGuard::new(&status);
    bump_stmt_send_long_data_counters(&status, &mut conn_state.session_status);

    if body.len() < 6 {
        continue; // MariaDB-compatible: no response, no engine work
    }

    let stmt_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let param_id = u16::from_le_bytes(body[4..6].try_into().unwrap()) as usize;
    let chunk = &body[6..];
    let limit = conn_state
        .max_allowed_packet_bytes()
        .unwrap_or(ConnectionState::DEFAULT_MAX_ALLOWED_PACKET);

    if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
        stmt.append_long_data(param_id, chunk, limit);
    }

    continue; // never send an OK packet here
}
```

Chosen behavior for malformed / unknown `stmt_id`:
- malformed body `< 6` bytes: ignore silently, no response
- unknown statement id: ignore silently, no response

That matches the no-reply contract and avoids inventing a special immediate
error path that would diverge from MariaDB.

### 3. `COM_STMT_EXECUTE` merges long data before engine execution

`prepared.rs` becomes the single source of truth for both inline and long-data
param decoding.

Replace the current catch-all string/blob arm with explicit helpers:

```rust
fn is_text_like_param_type(type_base: u8) -> bool {
    matches!(type_base, 0x0f | 0xfd | 0xfe)
}

fn is_binary_like_param_type(type_base: u8) -> bool {
    matches!(type_base, 0xf9 | 0xfa | 0xfb | 0xfc)
}
```

Inline decode rules become:

```rust
match type_base {
    // numeric + temporal stay as today
    0x0f | 0xfd | 0xfe => read_lenenc_text(...),   // Value::Text
    0xf9 | 0xfa | 0xfb | 0xfc => read_lenenc_bytes(...), // Value::Bytes
    _ => fallback_or_error_as_today,
}
```

Long-data merge rule inside `parse_execute_packet()`:

```rust
if let Some(err) = &stmt.pending_long_data_error {
    return Err(DbError::InvalidValue {
        reason: err.reason.clone(),
    });
}

for i in 0..param_count {
    if let Some(raw) = stmt.pending_long_data[i].as_ref() {
        let type_code = stmt.param_types.get(i).copied().ok_or_else(|| {
            DbError::InvalidValue {
                reason: format!("missing type metadata for long-data param {i}"),
            }
        })?;
        params.push(decode_long_data_value(raw, type_code, client_charset)?);
        continue;
    }

    if is_null(&null_bitmap, i) {
        params.push(Value::Null);
        continue;
    }

    let type_code = stmt.param_types.get(i).copied().unwrap_or(0xfd);
    let (value, consumed) = decode_binary_value(&payload[pos..], type_code, client_charset)?;
    params.push(value);
    pos += consumed;
}
```

`decode_long_data_value()` is fixed in this plan:

```rust
fn decode_long_data_value(
    raw: &[u8],
    type_code: u16,
    client_charset: &'static CharsetDef,
) -> Result<Value, DbError> {
    match type_code as u8 {
        0x0f | 0xfd | 0xfe => {
            let s = charset::decode_text(client_charset, raw)?.into_owned();
            Ok(Value::Text(s))
        }
        0xf9 | 0xfa | 0xfb | 0xfc => Ok(Value::Bytes(raw.to_vec())),
        other => Err(DbError::InvalidValue {
            reason: format!(
                "COM_STMT_SEND_LONG_DATA is only valid for string/binary params, got MySQL type code {other:#04x}"
            ),
        }),
    }
}
```

### 4. Long-data clearing point is fixed

The clearing point is not left open. It happens in `handler.rs` immediately
after `parse_execute_packet()` returns, before the engine executes:

```rust
let parse_result = parse_execute_packet(body, stmt, client_charset);
stmt.clear_long_data_state();

match parse_result {
    Ok(exec) => { /* execute with exec.params */ }
    Err(e) => { /* return ERR */ }
}
```

This guarantees:
- long data is single-use per execute
- failed execute parse still clears the pending buffers
- execution itself does not depend on stmt-local long-data state anymore

### 5. `COM_STMT_RESET` becomes real

Replace the current stub in `handler.rs` with the exact stmt-scoped behavior:

```rust
0x1a => {
    if body.len() < 4 {
        let e = build_err_packet(1105, b"HY000", "Malformed COM_STMT_RESET");
        let _ = writer.send((1u8, e.as_slice())).await;
        continue;
    }

    let stmt_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
    if let Some(stmt) = conn_state.prepared_statements.get_mut(&stmt_id) {
        stmt.clear_long_data_state();
        let ok = build_ok_packet(0, 0, 0);
        let _ = writer.send((1u8, ok.as_slice())).await;
    } else {
        let e = build_err_packet(
            1243,
            b"HY000",
            &format!("Unknown prepared statement handler: stmt_id={stmt_id}"),
        );
        let _ = writer.send((1u8, e.as_slice())).await;
    }
}
```

This subphase intentionally does not make `COM_STMT_RESET` drop `param_types`,
`analyzed_stmt`, or plan-cache metadata.

### 6. Status integration is explicit

`status.rs` gets:

```rust
pub struct StatusRegistry {
    ...
    pub com_stmt_send_long_data: AtomicU64,
}

#[derive(Debug, Default, Clone)]
pub struct SessionStatus {
    ...
    pub com_stmt_send_long_data: u64,
}
```

And `SHOW STATUS` row generation must emit:
- `Com_stmt_send_long_data`

The counter increments once per `COM_STMT_SEND_LONG_DATA` chunk, not per
statement execute and not per final parameter.

## Implementation phases

1. Extend `PreparedStatement` state in `session.rs` and initialize/reset it correctly.
2. Add explicit text-like vs binary-like param classification in `prepared.rs` and fix inline BLOB decode to `Value::Bytes`.
3. Add long-data merge + deferred-error handling to `parse_execute_packet()`.
4. Implement `COM_STMT_SEND_LONG_DATA` in `handler.rs` without engine locking and with per-chunk status updates.
5. Replace stub `COM_STMT_RESET` with stmt-scoped reset semantics.
6. Add protocol/unit/wire tests for text chunks, binary chunks, empty long data, clearing-after-execute, reset, and `SHOW STATUS`.

## Tests to write

- unit:
  - `PreparedStatement::append_long_data()` distinguishes `None` from `Some(vec![])`
  - accumulated length over `max_allowed_packet` stores deferred error and does not append
  - inline `MYSQL_TYPE_BLOB` decode now returns `Value::Bytes`
  - long-data text decode uses client charset after concatenation
  - long-data binary decode preserves raw bytes including `0x00`
  - pending long data takes precedence over null bitmap and over inline payload

- integration:
  - `parse_execute_packet()` with param0 long data + param1 inline proves the parser skips inline bytes for long-data params
  - execute without new long data after a prior execute does not reuse old long data
  - `COM_STMT_RESET` clears pending long data for one statement without dropping the prepared statement
  - `SHOW STATUS` session/global rows include `Com_stmt_send_long_data`

- wire:
  - low-level `COM_STMT_PREPARE` + `COM_STMT_SEND_LONG_DATA` + `COM_STMT_EXECUTE`
  - split a multibyte UTF-8 code point across two long-data chunks and verify round-trip text
  - send a BLOB containing `0x00` and verify round-trip bytes via `HEX(...)`
  - send empty long data and verify empty string/bytes insertion
  - send long data, call `COM_STMT_RESET`, then execute with inline params and verify old long data is gone
  - verify `SHOW STATUS LIKE 'Com_stmt_send_long_data'` increments by chunk count

- bench:
  - none for this subphase; correctness and protocol compatibility are the goal

## Anti-patterns to avoid

- Do not decode long text on chunk arrival; that breaks multibyte boundaries.
- Do not store pending long data at the connection level; it must be statement-scoped.
- Do not keep long data alive after `COM_STMT_EXECUTE`; it is per-execution state.
- Do not implement long-data params by rewriting SQL strings in `handler.rs`.
- Do not keep `COM_STMT_RESET` as a connection-wide OK stub; it must parse and act on `stmt_id`.
- Do not conflate “no long data” with “empty long data”.

## Risks

- Memory growth from repeated chunks.
  - Mitigation: per-parameter accumulated bytes are capped by session `max_allowed_packet`.

- Charset corruption if a multibyte character is split across chunks.
  - Mitigation: buffer raw bytes and decode only once at execute time via `charset::decode_text()`.

- Stale long data leaking into the next execute.
  - Mitigation: clear stmt-local long-data state immediately after `parse_execute_packet()` returns.

- Divergent semantics between inline BLOB params and long-data BLOB params.
  - Mitigation: use one explicit text-vs-bytes classification helper in `prepared.rs` for both paths.

- Regressing `COM_STMT_RESET` behavior because it is currently a stub.
  - Mitigation: add both unit and live wire tests that verify stmt-scoped reset semantics and unknown-stmt errors.
