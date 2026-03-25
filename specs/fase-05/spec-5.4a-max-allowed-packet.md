# Spec: 5.4a — max_allowed_packet enforcement

## What to build (not how)

Enforce `max_allowed_packet` on incoming MySQL wire packets in AxiomDB's
server path.

This subphase is not about adding a new session variable from scratch:
AxiomDB already exposes `@@max_allowed_packet` in
`ConnectionState` (`crates/axiomdb-network/src/mysql/session.rs`)
with a default value of `67108864` bytes. The missing piece is enforcement in
the real wire path so that oversized packets are rejected before they reach SQL
UTF-8 decoding, parsing, prepared-statement decoding, or executor logic.

`5.4a` must make that variable effective for the current codebase by enforcing
the limit in the MySQL framing layer itself, not only after the packet has
already been fully accepted by the server.

### Packet size semantics

- The limit applies to the **logical MySQL command payload**:
  - command byte + command body
  - across one or more physical MySQL packets
- The 4-byte physical packet header (`u24 payload_length + sequence_id`) does
  **not** count toward `max_allowed_packet`
- Physical MySQL packets with `payload_length = 0xFFFFFF` are continuation
  fragments of one logical command and must be reassembled for this limit
- The default per-connection limit is `67108864` bytes (64 MiB)

### Commands covered

The limit applies to all inbound client packets handled by the current MySQL
server path, including:

- `HandshakeResponse41`
- `COM_QUERY`
- `COM_INIT_DB`
- `COM_PING`
- `COM_RESET_CONNECTION`
- `COM_STMT_PREPARE`
- `COM_STMT_EXECUTE`
- `COM_STMT_CLOSE`
- `COM_STMT_RESET`
- unknown command packets that still enter the command loop

### Oversize behavior

When an incoming logical packet exceeds the current connection limit:

- the server returns MySQL ERR packet:
  - error code: `1153`
  - SQLSTATE: `08S01`
  - message: `Got a packet bigger than 'max_allowed_packet' bytes`
- the server closes the connection immediately after attempting to send that
  ERR packet

The connection is closed on purpose. Once the server has classified a command
as oversized, it must not try to continue reading the same stream and guess
where the next command begins.

### Session semantics

- before authentication, the connection uses the default `67108864`-byte limit
- after authentication, the live decoder limit follows the current session's
  `max_allowed_packet`
- `SET max_allowed_packet = <decimal bytes>` updates the effective limit for
  subsequent packets on that same connection
- quoted decimal values such as `SET max_allowed_packet = '2048'` are accepted
- invalid values for `SET max_allowed_packet` return ERR and leave the previous
  effective limit unchanged
- `COM_RESET_CONNECTION` restores the default `67108864`-byte limit
- `SELECT @@max_allowed_packet` must continue to reflect the session value

This subphase is about **incoming** packet enforcement. Outbound result-set
chunking is not part of `5.4a`.

## Inputs / Outputs

- Input:
  - raw bytes entering
    `MySqlCodec` (`crates/axiomdb-network/src/mysql/codec.rs`)
  - session updates performed through
    `ConnectionState` (`crates/axiomdb-network/src/mysql/session.rs`)
  - MySQL command payloads consumed by
    `handle_connection` (`crates/axiomdb-network/src/mysql/handler.rs`)
- Output:
  - normal decoded logical MySQL packet when the payload size is within limit
  - MySQL ERR packet `1153 / 08S01` plus connection close when the logical
    payload exceeds the current limit
- Errors:
  - oversized packet → ERR `1153 / 08S01`
  - malformed packets continue to use the existing malformed-packet behavior
  - invalid `SET max_allowed_packet` value → SQL ERR and no limit change

## Use cases

1. A normal `COM_QUERY` whose logical payload is below `67108864` bytes is
   decoded and executed normally.
2. A `HandshakeResponse41` below the default limit authenticates normally.
3. A single physical packet whose payload exceeds the current connection limit
   returns `1153 / 08S01` and the connection closes.
4. A logical command split across multiple `0xFFFFFF` continuation packets is
   accepted when the reassembled payload is within the limit.
5. A fragmented logical command where each physical fragment is individually
   below the limit but the reassembled total exceeds it is rejected with
   `1153 / 08S01`.
6. `SET max_allowed_packet = 2048` lowers the same connection's effective
   inbound limit before the next command is read.
7. `SET max_allowed_packet = 'abc'` returns ERR and the previous effective
   limit remains active.
8. `COM_RESET_CONNECTION` restores the effective inbound limit to `67108864`
   bytes.
9. An oversized `COM_QUERY` containing invalid UTF-8 still fails as
   `1153 / 08S01`, not as a UTF-8 or SQL parse error, because the size limit is
   enforced first.
10. A `COM_STMT_EXECUTE` packet is checked against the same limit as
    `COM_QUERY`; binary protocol is not exempt.
11. A payload whose logical size is exactly equal to the configured limit is
    accepted.
12. A payload whose logical size is one byte above the configured limit is
    rejected.

## Acceptance criteria

- [ ] incoming packet-size enforcement happens in the wire framing layer, not
      only after SQL decoding or parsing
- [ ] `max_allowed_packet` is enforced on the **logical** command payload, not
      only on the first physical fragment
- [ ] the 4-byte physical MySQL packet headers do not count toward the limit
- [ ] the default effective limit for a new connection is `67108864` bytes
- [ ] a logical packet with size exactly equal to the configured limit is
      accepted
- [ ] a logical packet with size `limit + 1` is rejected
- [ ] an oversized inbound packet returns MySQL error code `1153`
- [ ] an oversized inbound packet returns SQLSTATE `08S01`
- [ ] the ERR message for an oversized inbound packet is exactly
      `Got a packet bigger than 'max_allowed_packet' bytes`
- [ ] after attempting to send the oversized-packet ERR, the server closes the
      connection instead of trying to continue on the same stream
- [ ] oversized `HandshakeResponse41` is rejected using the default pre-auth
      limit
- [ ] oversized `COM_QUERY` is rejected before UTF-8 decoding and SQL parsing
- [ ] oversized `COM_STMT_PREPARE` is rejected before SQL parsing
- [ ] oversized `COM_STMT_EXECUTE` is rejected before binary-parameter decoding
- [ ] `SET max_allowed_packet = <decimal bytes>` updates the live effective
      limit for subsequent packets on the same connection
- [ ] quoted decimal values for `SET max_allowed_packet` are accepted
- [ ] invalid `SET max_allowed_packet` values return ERR and do not silently
      change the live limit
- [ ] `COM_RESET_CONNECTION` restores the live effective limit to the default
      `67108864` bytes
- [ ] regression coverage includes single-packet overflow, fragmented logical
      overflow, exact-boundary success, and post-`SET` / post-`COM_RESET_CONNECTION`
      behavior

## Out of scope

- outbound result-set packet splitting or outbound `max_allowed_packet`
  enforcement
- use of the client-advertised handshake `max_packet_size` to negotiate server
  response sizes
- `net_buffer_length` compatibility logic, warnings, or cross-validation
- unit suffix parsing such as `64K`, `16M`, or `1G` in `SET max_allowed_packet`
- `COM_STMT_SEND_LONG_DATA` chunk accumulation semantics
- sequence-id validation across multi-packet commands beyond what is required to
  reassemble the payload

## Dependencies

These files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/src/mysql/codec.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/packets.rs`
- `crates/axiomdb-network/src/mysql/error.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `tools/wire-test.py`
- `crates/axiomdb-sql/src/lexer.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`

Research reviewed before writing this spec:

- `research/mariadb-server/sql/share/errmsg-utf8.txt`
- `research/mariadb-server/mysql-test/main/variables-notembedded.test`
- `research/mariadb-server/mysql-test/main/variables-notembedded.result`
- `research/mariadb-server/sql/sql_client.cc`
- `research/oceanbase/src/observer/mysql/obmp_query.cpp`
- `research/oceanbase/src/observer/mysql/obmp_stmt_prepare.cpp`
- `research/oceanbase/src/observer/mysql/obmp_stmt_execute.cpp`
- `research/oceanbase/src/observer/mysql/obmp_stmt_send_long_data.cpp`
- `research/oceanbase/src/observer/mysql/obmp_stmt_fetch.cpp`
- `research/oceanbase/src/sql/session/ob_basic_session_info.cpp`
- `research/oceanbase/src/share/mysql_errno.h`
- `research/oceanbase/src/share/ob_errno.def`

Existing codebase facts this spec depends on:

- `ConnectionState` already stores `max_allowed_packet = 67108864`
- `parse_handshake_response()` currently reads the client's
  `max_packet_size` but does not retain or enforce it
- `COM_RESET_CONNECTION` already recreates `ConnectionState`
- the current `MySqlCodec` buffers one physical packet at a time and has no
  packet-size enforcement today
- the current parser `max_bytes` guard exists, but `handle_connection()` does
  not use it and it only applies after SQL text has already been decoded
- `tokio-util 0.7.18` in this workspace exposes `FramedRead::decoder_mut()`,
  which makes a live per-connection decoder limit implementable without
  inventing a new runtime layer

## ⚠️ DEFERRED

- outbound packet splitting for result sets larger than one physical MySQL
  packet → pending a later wire-protocol follow-up
- client `max_packet_size` negotiation for outbound responses
  → pending a later compatibility follow-up
- `net_buffer_length` variable support and MySQL-style warnings
  → pending a later session-variable follow-up
- unit suffix parsing (`K` / `M` / `G`) in `SET max_allowed_packet`
  → pending a later session-variable follow-up
