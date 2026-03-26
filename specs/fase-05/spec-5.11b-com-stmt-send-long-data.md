# Spec: 5.11b — COM_STMT_SEND_LONG_DATA

## These files were reviewed before writing this spec

### AxiomDB codebase
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/status.rs`
- `crates/axiomdb-network/src/mysql/codec.rs`
- `crates/axiomdb-network/src/mysql/charset.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `crates/axiomdb-types/src/coerce.rs`
- `crates/axiomdb-sql/src/table.rs`
- `tools/wire-test.py`

### Research consulted
- `research/mariadb-server/sql/sql_prepare.cc`
- `research/mariadb-server/sql/item.cc`
- `research/mariadb-server/include/mysql.h`
- `research/mariadb-server/include/mysql_com.h`
- `research/mariadb-server/tests/mysql_client_test.c`
- `research/mariadb-server/libmysqld/libmysql.c`
- `research/oceanbase/src/observer/mysql/obmp_stmt_send_long_data.cpp`
- `research/oceanbase/src/observer/mysql/obmp_stmt_execute.cpp`
- `research/oceanbase/src/sql/session/ob_sql_session_info.cpp`

## Research synthesis

### AxiomDB-first constraints
- `crates/axiomdb-network/src/mysql/handler.rs` shows `COM_STMT_*` handling lives entirely in the connection task; `COM_STMT_SEND_LONG_DATA` must not take the `Database` mutex.
- `crates/axiomdb-network/src/mysql/session.rs` already makes `PreparedStatement` the per-statement state holder, so pending long-data state must live there rather than in a new runtime cache.
- `crates/axiomdb-network/src/mysql/prepared.rs` is already the single place that knows how to decode `COM_STMT_EXECUTE` parameters, so long-data merge must happen there instead of patching the executor.
- `crates/axiomdb-network/src/mysql/charset.rs` already owns client-charset decoding, so long text chunks must be decoded there only after all pieces are assembled.

### What we borrow from research
- `research/mariadb-server/sql/sql_prepare.cc` provides the wire contract:
  `COM_STMT_SEND_LONG_DATA` appends raw bytes per placeholder, sends no reply,
  and returns deferred errors at `COM_STMT_EXECUTE`.
- `research/mariadb-server/sql/item.cc` shows why long text must be buffered as
  raw bytes until execute time: chunks can split multibyte characters, and the
  accumulated value is bounded by `max_allowed_packet`.
- `research/mariadb-server/tests/mysql_client_test.c` proves the behavioral edge
  cases that matter here: empty long data is meaningful, long data does not
  persist across executes, and `mysql_stmt_reset()` clears pending long data.
- `research/mariadb-server/libmysqld/libmysql.c` confirms the per-execute reset
  model from the client side (`RESET_LONG_DATA` after execute).
- `research/oceanbase/src/observer/mysql/obmp_stmt_send_long_data.cpp` and
  `research/oceanbase/src/observer/mysql/obmp_stmt_execute.cpp` reinforce the
  modern split: store raw chunks in statement/session state, validate the final
  type only at execute time, and keep `send_long_data` off the execution path.

### What we reject
- We do not add a separate piece-cache subsystem like OceanBase because
  AxiomDB already has `PreparedStatement` in `session.rs` and does not need a
  second ownership layer.
- We do not decode long text on arrival because that would break chunk
  boundaries for multibyte client charsets.
- We do not implement long data by rewriting SQL strings in `handler.rs`; that
  would duplicate the cached-AST path and diverge from the current prepared
  statement architecture.

## What to build (not how)

Implement MySQL command `COM_STMT_SEND_LONG_DATA` (`0x18`) so large prepared
statement parameters can be transmitted in multiple chunks before
`COM_STMT_EXECUTE`.

The wire contract for `COM_STMT_SEND_LONG_DATA` is:
- command byte `0x18`
- payload body `[stmt_id: u32 LE][param_id: u16 LE][chunk_bytes...]`

Behavior:
- Each `COM_STMT_SEND_LONG_DATA` appends `chunk_bytes` to a statement-local
  pending buffer for `param_id`.
- The command sends no response packet on success.
- The command does not touch the SQL engine or the `Database` mutex.
- Pending long-data state belongs to the prepared statement, not to the whole
  connection and not to the engine.

Pending long-data buffers are consumed by the next `COM_STMT_EXECUTE` of that
statement and are cleared afterward, regardless of whether execute succeeds or
fails.

`COM_STMT_RESET` becomes statement-scoped and real:
- it parses `stmt_id`
- clears that statement's pending long-data buffers
- clears that statement's deferred long-data error
- keeps the prepared statement itself, its cached plan, and its remembered
  type metadata
- returns `ERR 1243` if the statement id is unknown

`COM_STMT_CLOSE` continues to deallocate the statement and therefore also drops
all pending long-data state for that statement.

Long-data precedence:
- if a parameter has pending long-data bytes, that value wins over the inline
  value bytes in `COM_STMT_EXECUTE`
- if a parameter has pending long-data bytes, that value also wins over the
  null bitmap for that parameter

This precedence is required so AxiomDB matches the `has_long_data_value()`
behavior seen in `research/mariadb-server/sql/sql_prepare.cc`.

Type handling:
- pending long-data bytes are stored as raw bytes until `COM_STMT_EXECUTE`
- at execute time, text-like parameter types are decoded with the negotiated
  client charset and become `Value::Text`
- binary-like parameter types become `Value::Bytes` without charset decoding
- unsupported parameter types with pending long data return ERR on execute

Text-like types for this subphase:
- `MYSQL_TYPE_VARCHAR` (`0x0f`)
- `MYSQL_TYPE_VAR_STRING` (`0xfd`)
- `MYSQL_TYPE_STRING` (`0xfe`)

Binary-like types for this subphase:
- `MYSQL_TYPE_TINY_BLOB` (`0xf9`)
- `MYSQL_TYPE_MEDIUM_BLOB` (`0xfa`)
- `MYSQL_TYPE_LONG_BLOB` (`0xfb`)
- `MYSQL_TYPE_BLOB` (`0xfc`)

To avoid divergent semantics between inline prepared params and long-data
prepared params, AxiomDB must also correct the existing inline decoder so the
binary-like type codes above decode to `Value::Bytes` instead of `Value::Text`.

Empty long data is meaningful:
- the first `COM_STMT_SEND_LONG_DATA` for a parameter may carry zero bytes
- that still marks the parameter as having long data
- at execute time it must become empty text (`""`) or empty bytes (`[]`)
  depending on the type code

`max_allowed_packet` enforcement:
- each individual `COM_STMT_SEND_LONG_DATA` packet is already bounded by the
  decoder from subphase `5.4a`
- additionally, the accumulated long-data value for a single parameter must not
  exceed the current session `max_allowed_packet`
- overflow is deferred: `COM_STMT_SEND_LONG_DATA` still sends no response, but
  the statement stores a deferred error and the next `COM_STMT_EXECUTE` returns
  ERR and clears the pending state

Deferred long-data errors also cover:
- parameter index out of range for the statement
- missing parameter type information at execute time for a parameter that has
  pending long data

Command observability:
- each `COM_STMT_SEND_LONG_DATA` chunk increments a new
  `Com_stmt_send_long_data` counter in both global and session `SHOW STATUS`
  output
- the counter is per chunk, not per execute

## Inputs / Outputs

- Input:
  - `COM_STMT_SEND_LONG_DATA` body: `&[u8]` with
    `[stmt_id:4][param_id:2][chunk_bytes...]`
  - `ConnectionState` with prepared statements and session variables
  - `COM_STMT_EXECUTE` body for the same statement
  - negotiated client charset from `ConnectionState`
- Output:
  - `COM_STMT_SEND_LONG_DATA`: no response packet on success
  - `COM_STMT_EXECUTE`: existing prepared-statement success packets or ERR
  - `COM_STMT_RESET`: OK packet or ERR packet
- Errors:
  - accumulated long data over `max_allowed_packet` returns ERR on the next
    `COM_STMT_EXECUTE`
  - pending long data on a non-string/non-binary parameter type returns ERR on
    `COM_STMT_EXECUTE`
  - `COM_STMT_RESET` for an unknown `stmt_id` returns `ERR 1243`

## Use cases

1. A client prepares `INSERT INTO t(id, txt) VALUES(?, ?)` and sends the text
   value in two `COM_STMT_SEND_LONG_DATA` chunks before `COM_STMT_EXECUTE`.
   AxiomDB concatenates the raw bytes and inserts the final string.

2. A client prepares `INSERT INTO t(id, blb) VALUES(?, ?)` and sends binary
   chunks containing `0x00` bytes. AxiomDB stores the exact bytes and does not
   route them through UTF-8 decoding.

3. A client sends zero-length long data for a text parameter. The next execute
   stores `""`, not the previous bound value and not `NULL`.

4. A client executes the same prepared statement again without sending new long
   data. The previous long-data value is not reused.

5. A client sends long data, then `COM_STMT_RESET`, then executes with inline
   params. The reset clears the pending long-data state.

6. A client sends chunks whose accumulated size for one parameter exceeds
   `@@max_allowed_packet`. `COM_STMT_SEND_LONG_DATA` sends no reply, but the
   next `COM_STMT_EXECUTE` returns ERR and the connection remains usable.

## Acceptance criteria

- [ ] `COM_STMT_SEND_LONG_DATA` appends chunk bytes to a statement-local pending buffer keyed by `stmt_id` and `param_id`
- [ ] `COM_STMT_SEND_LONG_DATA` sends no OK packet on success
- [ ] `COM_STMT_SEND_LONG_DATA` does not require taking the `Database` mutex
- [ ] Pending long data is stored as raw bytes and decoded only at `COM_STMT_EXECUTE`, not on chunk arrival
- [ ] Pending long data for text-like types is decoded with the negotiated client charset at execute time
- [ ] Pending long data for binary-like types becomes `Value::Bytes` and never goes through text decoding
- [ ] Empty long data (`chunk_bytes.len() == 0` on the first chunk for a param) is preserved as an explicitly set empty value, not treated as “no long data”
- [ ] If a parameter has pending long data, that value wins over both the inline execute payload and the null bitmap for that parameter
- [ ] After any `COM_STMT_EXECUTE` attempt, pending long data and deferred long-data errors for that statement are cleared
- [ ] `COM_STMT_RESET` clears pending long data for the addressed statement and returns `ERR 1243` for an unknown statement id
- [ ] Accumulated long data larger than `@@max_allowed_packet` is rejected on execute and does not close the connection
- [ ] Parameter-index-out-of-range during `COM_STMT_SEND_LONG_DATA` is deferred to execute, not returned as an immediate packet reply
- [ ] Inline prepared-statement decoding for `MYSQL_TYPE_TINY_BLOB|MEDIUM_BLOB|LONG_BLOB|BLOB` now produces `Value::Bytes`, so inline and long-data binary params share the same semantics
- [ ] `SHOW STATUS LIKE 'Com_stmt_send_long_data'` reflects the number of `COM_STMT_SEND_LONG_DATA` chunks seen by the session and by the server globally
- [ ] Wire tests prove a multibyte text character split across two long-data chunks is reconstructed correctly on the live server
- [ ] Wire tests prove a binary parameter containing `0x00` survives long-data insertion unchanged on the live server

## Out of scope

- `COM_STMT_FETCH` / cursor mode
- Client-library helpers or a Rust embedded prepared API
- Streaming long data directly into storage without buffering in statement state
- New SQL data types beyond the text and bytes types AxiomDB already exposes
- SQL-level `PREPARE ... EXECUTE` syntax (`phase 31`)

## Dependencies

- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/status.rs`
- `crates/axiomdb-network/src/mysql/codec.rs`
- `crates/axiomdb-network/src/mysql/charset.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `crates/axiomdb-types/src/coerce.rs`
- `crates/axiomdb-sql/src/table.rs`
- `tools/wire-test.py`
