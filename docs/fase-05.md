# Phase 5 — MySQL Wire Protocol

## Subfases completed in this session: 5.11b

## What was built

### 5.11b — COM_STMT_SEND_LONG_DATA

AxiomDB now closes the MySQL prepared-statement large-parameter path end to end.

Supported behavior:

- `COM_STMT_SEND_LONG_DATA (0x18)` appends raw bytes per `stmt_id` + `param_id`
- long-data buffers live inside `PreparedStatement`, not in the engine
- chunks are stored as raw bytes and decoded only at `COM_STMT_EXECUTE`
- long data takes precedence over both the inline execute payload and the null bitmap
- `COM_STMT_RESET` clears only stmt-local long-data state and deferred errors
- `COM_STMT_CLOSE` drops the statement and therefore its long-data state too
- `SHOW STATUS LIKE 'Com_stmt_send_long_data'` exposes chunk counts in session/global scope

Implementation highlights:

- `axiomdb-network/src/mysql/handler.rs`
  - handles `0x18` with no response and no `Database` mutex
  - `0x1a` now resets stmt-local long-data state for the addressed statement
- `axiomdb-network/src/mysql/session.rs`
  - `PreparedStatement.pending_long_data: Vec<Option<Vec<u8>>>`
  - `PreparedStatement.pending_long_data_error: Option<String>`
  - `append_long_data(...)` and `clear_long_data_state()`
- `axiomdb-network/src/mysql/prepared.rs`
  - `parse_execute_packet()` merges long-data buffers before inline decoding
  - text-like long data decodes with the negotiated client charset
  - binary-like long data becomes `Value::Bytes`
- `axiomdb-network/src/mysql/status.rs`
  - adds `Com_stmt_send_long_data` to global + session status snapshots

## Tests

- Unit tests:
  - `crates/axiomdb-network/src/mysql/prepared.rs` — chunk accumulation, empty long data, precedence, binary preservation, deferred overflow, clear-after-execute
- Protocol tests:
  - `crates/axiomdb-network/tests/integration_protocol.rs` — prepared-state initialization plus public packet decode tests for long-text precedence and binary decode
- Wire smoke test:
  - `tools/wire-test.py` now covers:
    - multibyte text split across long-data chunks
    - `COM_STMT_RESET`
    - binary `BLOB` params with `0x00`
    - deferred `max_allowed_packet` overflow on execute
    - session/global `Com_stmt_send_long_data`

## Follow-up subfases still open in Phase 5

- `5.11c` — explicit connection state machine
- `5.15` — DSN parsing
- `5.17` — in-place B+Tree write-path expansion
