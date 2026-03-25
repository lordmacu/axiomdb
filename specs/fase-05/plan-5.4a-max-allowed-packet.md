# Plan: 5.4a — max_allowed_packet enforcement

## Files to create/modify

- `crates/axiomdb-network/src/mysql/codec.rs`
  - turn `MySqlCodec` into a stateful decoder with:
    - configurable `max_payload_len`
    - logical multi-packet reassembly for incoming MySQL commands
    - explicit oversize decoder error before unbounded `reserve()`
- `crates/axiomdb-network/src/mysql/handler.rs`
  - construct the codec with the default limit
  - handle oversized-packet decoder errors during handshake and command loop
  - change both `intercept_special_query()` call sites to an explicit
    `match InterceptResult`
  - sync the live decoder limit after auth, successful
    `SET max_allowed_packet`, and `COM_RESET_CONNECTION`
- `crates/axiomdb-network/src/mysql/session.rs`
  - expose the default `max_allowed_packet` constant once
  - add a helper that parses the session value into an effective byte limit
  - change `apply_set()` from `bool` to `Result<bool, DbError>`
  - validate `SET max_allowed_packet` instead of silently accepting garbage
- `crates/axiomdb-network/src/mysql/packets.rs`
  - add `build_net_packet_too_large_err_packet()` so the wire oversized-packet
    path has one canonical constructor for `1153 / 08S01`
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - add decoder-level tests for reassembly, exact-boundary acceptance, and
    overflow rejection
- `tools/wire-test.py`
  - add live wire assertions for:
    - `SET max_allowed_packet`
    - oversize `COM_QUERY`
    - `COM_RESET_CONNECTION` restoring the default
- `docs-site/src/user-guide/sql-reference/dml.md`
  - document `SET/SELECT @@max_allowed_packet` behavior and the oversize error
- `docs-site/src/user-guide/errors.md`
  - document MySQL error `1153 / 08S01`
- `docs-site/src/internals/architecture.md`
  - explain why packet-size enforcement lives in the codec boundary and how the
    live per-connection limit is synchronized

## Algorithm / Data structure

### Chosen approach

Use a **stateful codec with a live per-connection limit**, and keep that limit
in sync with `ConnectionState`.

This is the best fit for the AxiomDB codebase as it exists today because:

- the current handler already owns the `FramedRead<MySqlCodec>` for the entire
  connection lifetime
- `tokio-util 0.7.18` already gives us `reader.decoder_mut()`
- `ConnectionState` already owns the session variable that should drive the
  limit
- `COM_RESET_CONNECTION` already resets session state by recreating
  `ConnectionState`

That means we can solve the problem where it belongs:

- protocol-size checks in `codec.rs`
- session-driven limit changes in `handler.rs`
- parsing/validation of the session value in `session.rs`

without inventing a new runtime object or lying about OOM protection.

### Alternatives considered

1. Enforce only in `handler.rs` after `reader.next().await`
   - Pros: minimal diff
   - Cons: the full packet has already been buffered; does not really satisfy
     the roadmap goal of preventing oversized inputs from consuming memory first

2. Enforce a fixed global ceiling in `codec.rs`, then enforce the session value
   later in `handler.rs`
   - Pros: some early protection
   - Cons: duplicated semantics and ambiguity about which limit is authoritative

3. Make `MySqlCodec` stateful, reassemble logical packets, and update its
   limit from `ConnectionState`
   - Pros: codebase-aligned, session-correct, modern, efficient, and actually
     protects the framing layer
   - Cons: requires a custom decoder error and a more capable decode loop

Chosen: option 3.

### Research applied after reading the codebase

- MariaDB
  - gives the public contract:
    - error `1153`
    - SQLSTATE `08S01`
    - canonical message `Got a packet bigger than 'max_allowed_packet' bytes`
  - also shows that clients can observe either the ERR packet or a disconnect,
    which justifies closing the connection after the error
- OceanBase
  - confirms a modern architecture where packet-size checks are done per MySQL
    message type against session state, not in the SQL parser
  - reinforces that the effective limit belongs to the connection/session layer
- PostgreSQL / SQLite / DuckDB / DataFusion
  - not sources of MySQL packet semantics here
  - useful only for the general design principle: size guards belong at the
    transport boundary, not deep in SQL parsing/execution

### Data structures

```rust
pub struct MySqlCodec {
    max_payload_len: usize,
}

pub enum MySqlCodecError {
    Io(std::io::Error),
    PacketTooLarge {
        actual: usize,
        max: usize,
    },
}

impl MySqlCodec {
    pub fn new(max_payload_len: usize) -> Self;
    pub fn max_payload_len(&self) -> usize;
    pub fn set_max_payload_len(&mut self, max_payload_len: usize);
}

pub enum InterceptResult {
    NotIntercepted,
    Packets(Vec<(u8, Vec<u8>)>),
    Error(DbError),
}
```

`MySqlCodecError` must implement `From<std::io::Error>` because
`FramedRead` surfaces underlying socket read errors through the decoder error
type.

Session-side helpers:

```rust
impl ConnectionState {
    pub const DEFAULT_MAX_ALLOWED_PACKET: usize = 67_108_864;

    pub fn max_allowed_packet_bytes(&self) -> Result<usize, DbError>;
}
```

The session helper is the single source of truth for:

- parsing the user-visible session value
- rejecting invalid `SET max_allowed_packet`
- feeding the decoder with a normalized byte limit

### Decoder algorithm

Decode one **logical** incoming MySQL command at a time.

```text
decode(src):
  if src has fewer than 4 bytes:
    return None

  scan physical packets from the front of src:
    read payload_len (u24) and seq_id
    if the full physical packet is not buffered yet:
      reserve only the missing bytes for that fragment
      return None

    total_payload += payload_len
    if total_payload > max_payload_len:
      return PacketTooLarge { actual: total_payload, max: max_payload_len }

    if payload_len < 0xFF_FFFF:
      final fragment reached
      break

    continue scanning the next physical packet

  if there was only one physical packet:
    fast path: return its payload without extra copy

  else:
    allocate one contiguous buffer with capacity = total_payload
    append each fragment payload in order
    return the reassembled logical payload
```

Important notes:

- the limit applies to `total_payload`, not to each physical fragment
- the 4-byte physical headers are not included in `total_payload`
- the sequence id returned to the handler should remain the first fragment's
  sequence id, matching the current handler expectations
- if `decode()` returns `PacketTooLarge`, the buffered stream is considered
  unsalvageable for this connection and the handler must close it

### Handler wiring

At connection start:

```text
reader = FramedRead::new(reader_half, MySqlCodec::new(DEFAULT_MAX_ALLOWED_PACKET))
```

Handshake path:

- if the handshake response decode fails with `PacketTooLarge`, send
  `ER_NET_PACKET_TOO_LARGE` and close
- do not try to authenticate or parse the handshake packet after that

Post-auth path:

```text
conn_state = ConnectionState::new()
reader.decoder_mut().set_max_payload_len(conn_state.max_allowed_packet_bytes()?)
```

After intercepted `SET`:

```text
old_limit = reader.decoder().max_payload_len()
apply SET to conn_state
new_limit = conn_state.max_allowed_packet_bytes()?
if new_limit != old_limit:
  reader.decoder_mut().set_max_payload_len(new_limit)
```

After `COM_RESET_CONNECTION`:

```text
conn_state = ConnectionState::new()
reader.decoder_mut().set_max_payload_len(DEFAULT_MAX_ALLOWED_PACKET)
```

`intercept_special_query()` will change from:

```rust
fn intercept_special_query(...) -> Option<Vec<(u8, Vec<u8>)>>
```

to:

```rust
fn intercept_special_query(...) -> InterceptResult
```

Chosen reason:

- the current handler has exactly three real outcomes to model:
  - not intercepted
  - intercepted with packets
  - intercepted with validation error
- a dedicated enum keeps the two existing call sites in `handler.rs`
  straightforward and avoids nested wrappers

Concrete call-site behavior:

```text
match intercept_special_query(...) {
  InterceptResult::NotIntercepted => continue into normal SQL path
  InterceptResult::Packets(packets) => {
    sync session.autocommit
    if current statement lowercases to start with "set ":
      sync reader.decoder_mut() from conn_state.max_allowed_packet_bytes()
    send packets
  }
  InterceptResult::Error(e) => {
    build ERR packet from e
    send ERR
    single-statement COM_QUERY: continue command loop
    multi-statement COM_QUERY: stop processing remaining statements
  }
}
```

The key requirement is fixed here: invalid `SET max_allowed_packet` produces
ERR, never unconditional OK.

### `SET max_allowed_packet` validation

Do not keep the current "generic string map with silent acceptance" behavior
for this specific variable.

Plan:

- change `ConnectionState::apply_set()` from a pure boolean helper into one
  that can return `Result<bool, DbError>`
- for `max_allowed_packet`:
  - accept only decimal byte values after trimming quotes
  - require a positive integer
  - normalize the stored value to plain decimal string form
  - reject invalid values with `DbError::InvalidValue`
- for other existing supported variables:
  - preserve current behavior

Exact call sites to update:

- `crates/axiomdb-network/src/mysql/handler.rs`
  - `if let Some(packets) = intercept_special_query(sql, ...)` in the direct
    `COM_QUERY` intercept path
  - `if let Some(packets) = intercept_special_query(stmt_sql, ...)` inside the
    multi-statement loop
  - `intercept_special_query()` itself, where `SET` currently calls
    `conn_state.apply_set(sql);` and always returns `OK`
- `crates/axiomdb-network/src/mysql/session.rs`
  - existing unit tests that currently assert bare `bool`

This keeps `@@max_allowed_packet` honest: the visible session value and the
live decoder limit cannot drift apart.

For the invalid-value error returned by `SET max_allowed_packet`:

- use `DbError::InvalidValue`
- do **not** extend `dberror_to_mysql()` in `5.4a`
- the invalid-SET path therefore reuses the current generic SQL ERR mapping
  already used by the wire layer

That keeps this subphase focused: exact MySQL-family parity is required for the
oversized wire packet itself, not for session-variable validation errors.

### Oversize error packet

Use a dedicated helper in `packets.rs`:

```text
ER_NET_PACKET_TOO_LARGE:
  code     = 1153
  sqlstate = "08S01"
  message  = "Got a packet bigger than 'max_allowed_packet' bytes"
```

This is a protocol-layer error, not a SQL executor error, so it does not need
to be routed through the full `DbError -> dberror_to_mysql()` path in `5.4a`.

### Why not reuse the parser's `max_bytes` guard?

Because it is the wrong layer for this problem.

Current facts from the codebase:

- `handle_connection()` currently calls `parse(sql, None)`
- the parser limit only applies after a payload has already been accepted as
  UTF-8 SQL text
- it does nothing for:
  - handshake packets
  - `COM_INIT_DB`
  - prepared-statement binary payloads
  - oversized fragmented logical packets before SQL decoding

So `5.4a` must be solved in the codec/handler boundary, not in `axiomdb-sql`.

## Implementation phases

1. Refactor `MySqlCodec` into a configurable stateful decoder and add the
   logical multi-packet reassembly path.
2. Introduce the explicit oversized-packet decoder error and keep encoder
   behavior unchanged.
3. Add `ConnectionState` helpers/constants for the default and parsed effective
   `max_allowed_packet`.
4. Change `apply_set()` so `max_allowed_packet` is validated and stored in a
   normalized form.
5. Update `handle_connection()` to:
   - construct the codec with the default limit
   - map oversized decode errors to ERR `1153 / 08S01`
   - sync the decoder limit after auth, after valid `SET max_allowed_packet`,
     and after `COM_RESET_CONNECTION`
6. Add unit tests for codec reassembly and limit boundaries.
7. Add live wire coverage for `SET`, overflow, and reset behavior.
8. Update user and internals docs for the new visible wire behavior.

## Tests to write

- unit:
  - single physical packet under limit decodes successfully
  - single physical packet exactly at limit decodes successfully
  - single physical packet one byte above limit returns `PacketTooLarge`
  - multi-packet logical command reassembles successfully when total is within
    limit
  - fragmented logical command returns `PacketTooLarge` when total exceeds the
    limit even if each fragment individually does not
  - `ConnectionState::max_allowed_packet_bytes()` accepts decimal and quoted
    decimal values
  - invalid `max_allowed_packet` values are rejected and do not overwrite the
    previous session value
- integration:
  - live server: `SET max_allowed_packet = 1024`, then oversized `COM_QUERY`
    returns `1153 / 08S01`
  - live server: `COM_RESET_CONNECTION` restores the default limit
  - live server: oversized packet during handshake/auth path is rejected
  - live server: binary protocol path (`COM_STMT_PREPARE` / `COM_STMT_EXECUTE`)
    obeys the same limit
  - live server or raw-socket protocol test: fragmented logical command larger
    than the limit is rejected based on reassembled size, not fragment size
  - live server: invalid `SET max_allowed_packet = 'abc'` returns ERR and does
    not change the effective limit
- bench:
  - none required for subphase close
  - optional micro-bench later if codec reassembly becomes hot

## Anti-patterns to avoid

- Do not enforce the limit only in `handler.rs` after `reader.next().await`.
- Do not store a second hidden packet limit disconnected from
  `ConnectionState`.
- Do not silently accept invalid `SET max_allowed_packet` values and fall back
  to some hidden default.
- Do not count the 4-byte physical packet headers toward `max_allowed_packet`.
- Do not reject based only on the first fragment of a multi-packet logical
  command.
- Do not try to continue using the same connection after an oversized packet;
  close it deterministically.
- Do not route this through the SQL parser's `max_bytes` guard and call it done.

## Risks

- Decoder complexity around fragmented packets
  - Mitigation: keep a single-fragment fast path and cover fragmented exact/over
    boundary tests explicitly.
- `tokio_util::codec` custom error wiring
  - Mitigation: keep the decoder error local to `FramedRead`; encoder can stay
    on `std::io::Error`.
- Session/value drift between `SET max_allowed_packet` and the live decoder
  - Mitigation: make `ConnectionState` the single parser/source of truth and
    always sync `reader.decoder_mut()` from it.
- Existing clients may observe disconnect after the oversize ERR
  - Mitigation: document it explicitly; this matches real MySQL-family behavior
    better than attempting unsafe stream resynchronization.
