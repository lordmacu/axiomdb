# Spec: 5.11c — Explicit connection state machine

## What to build
Add an explicit transport-level connection state machine for the MySQL wire server so every connection moves through the states:

`CONNECTED -> AUTH -> IDLE -> EXECUTING -> CLOSING`

This subphase makes the connection lifecycle explicit and enforces timeout behavior per state. It does not change SQL semantics, transaction semantics, prepared statement semantics, or result encoding.

The new behavior must:
- keep transport lifecycle separate from `ConnectionState`, which remains the owner of session SQL variables and prepared statements;
- enforce a fixed authentication timeout during handshake/authentication;
- enforce `wait_timeout` or `interactive_timeout` while the connection is idle, depending on the client handshake capabilities;
- enforce `net_read_timeout` for transport reads that occur after the server has entered an in-flight protocol step;
- enforce `net_write_timeout` for packet writes;
- enable TCP keepalive on accepted sockets so abruptly dead peers are eventually detected by the OS;
- preserve all current command behavior, including `COM_RESET_CONNECTION`, prepared statements, `SHOW STATUS`, warnings, and group commit.

Timeouts in this subphase are transport events. They close the connection; they do not introduce SQL warnings, statement cancellation, or a new SQL-visible error contract.

## Research synthesis
### AxiomDB files that constrain the design
These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/packets.rs`
- `crates/axiomdb-network/src/mysql/codec.rs`
- `crates/axiomdb-network/src/mysql/mod.rs`
- `crates/axiomdb-network/Cargo.toml`
- `crates/axiomdb-server/src/main.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`

### Research sources and how they inform this subphase
- `research/mariadb-server/sql/sys_vars.cc`
  Borrow: `connect_timeout`, `wait_timeout`, `interactive_timeout`, `net_read_timeout`, and `net_write_timeout` are distinct server/session concepts with positive-integer validation.
  Reject: MariaDB's THD/sys_var machinery and global server variable stack.
  Adapt: AxiomDB will validate the four existing session timeout vars in `ConnectionState` and use a fixed 10-second auth timeout that mirrors MariaDB's `connect_timeout` role without exposing a new variable yet.

- `research/mariadb-server/sql/sql_connect.cc`
  Borrow: connection establishment and authentication are transport-lifecycle concerns, not SQL-session concerns.
  Reject: MariaDB's full user-resource/accounting subsystem.
  Adapt: AxiomDB will keep auth timeout/state transitions in the wire handler lifecycle layer only.

- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
  Borrow: track interactive-vs-non-interactive behavior and last-activity/session-state separately from generic session variables.
  Reject: OceanBase's tenant/session machinery and distributed session control.
  Adapt: AxiomDB will add a lightweight lifecycle runtime that carries client capability flags and phase, while `ConnectionState` keeps SQL session data.

- `research/oceanbase/deps/oblib/src/rpc/obmysql/ob_sql_nio.h`
  Borrow: TCP keepalive belongs in the transport runtime, not in SQL execution state.
  Reject: the full custom NIO stack and thread scheduler.
  Adapt: AxiomDB will configure keepalive directly on accepted `TcpStream`s with a small socket helper.

- `research/postgres/src/backend/tcop/postgres.c`
  Borrow: explicit command-read vs execution phases, with timeout logic tied to the current transport phase rather than sprinkled through command handlers.
  Reject: PostgreSQL's backend-global flags, statement timeout framework, and query cancel machinery.
  Adapt: AxiomDB will use explicit phase transitions in the MySQL handler and will defer statement timeout/query cancellation to later subphases.

## Inputs / Outputs
- Input:
  - `tokio::net::TcpStream`
  - `Arc<Mutex<Database>>`
  - parsed `HandshakeResponse.capability_flags`
  - per-session timeout variables from `ConnectionState`:
    - `net_read_timeout`
    - `net_write_timeout`
    - `wait_timeout`
    - `interactive_timeout`
- Output:
  - same externally visible command results as today for successful traffic
  - deterministic connection close on auth timeout, idle timeout, read timeout, write timeout, EOF, or explicit quit
- Errors:
  - malformed handshake still returns ERR as today when possible
  - timeout events close the connection and do not create SQL warnings/results
  - invalid `SET wait_timeout|interactive_timeout|net_read_timeout|net_write_timeout` must return `DbError::InvalidValue`

## Use cases
1. Normal connection lifecycle
   A client connects, receives the greeting, authenticates, runs several commands, sends `COM_QUIT`, and the server transitions through `CONNECTED -> AUTH -> IDLE -> EXECUTING -> IDLE -> CLOSING`.

2. Idle timeout on a non-interactive connection
   A client authenticates and then sends nothing for longer than `wait_timeout`. The server closes the socket from the `IDLE` state.

3. Idle timeout on an interactive connection
   A client sets the `CLIENT_INTERACTIVE` capability in the handshake. After auth, the `IDLE` timeout uses `interactive_timeout` instead of `wait_timeout`.

4. `COM_RESET_CONNECTION`
   The server resets `ConnectionState` to defaults, but the transport lifecycle remains on the same connection. The phase returns to `IDLE`; the interactive/non-interactive nature of the connection does not change.

5. Long-running query execution
   A query that spends a long time executing on the server is not killed by `wait_timeout`. `wait_timeout` applies only while the connection is idle between commands. Statement timeout remains out of scope for this subphase.

## Acceptance criteria
- [ ] A typed lifecycle state exists for `CONNECTED`, `AUTH`, `IDLE`, `EXECUTING`, and `CLOSING`.
- [ ] The transport lifecycle is stored separately from `ConnectionState`; `ConnectionState` remains focused on session SQL state.
- [ ] Handshake/authentication reads and writes are bounded by a fixed 10-second auth timeout.
- [ ] `IDLE` reads use `interactive_timeout` when the client set `CLIENT_INTERACTIVE`; otherwise they use `wait_timeout`.
- [ ] Packet writes during command handling are bounded by `net_write_timeout`.
- [ ] Reads performed after entering an in-flight protocol step use `net_read_timeout`.
- [ ] `wait_timeout`, `interactive_timeout`, `net_read_timeout`, and `net_write_timeout` are validated as positive integers when changed via `SET`.
- [ ] `COM_RESET_CONNECTION` resets the timeout variable values back to defaults via `ConnectionState::new()`, but does not erase the connection's interactive/non-interactive classification.
- [ ] `COM_QUIT`, EOF, timeout expiry, and socket read/write failure all transition through `CLOSING` exactly once before returning from `handle_connection`.
- [ ] Accepted sockets have TCP keepalive enabled before the command loop starts.
- [ ] No SQL statement semantics change: prepared statements, warnings, `ON_ERROR`, charset/collation, and result encoding continue to behave the same as before.

## Out of scope
- SQL-level `statement_timeout` or query cancellation
- `KILL QUERY` / `KILL CONNECTION`
- TLS state machine changes
- Exposing `connect_timeout` as a user-settable variable
- Per-command streaming read timeouts beyond the transport helpers introduced here
- Bench-level performance optimization of the command loop

## Dependencies
- `5.1` TCP listener with Tokio
- `5.2` handshake/auth foundation
- `5.2a` typed charset negotiation in `ConnectionState`
- `5.9` session variables infrastructure
- `5.11b` prepared statement long-data handling

## ⚠️ DEFERRED
- SQL-visible `connect_timeout` variable and full MySQL-compatible timeout surface → pending in a future wire/session subphase
- Statement timeout / query cancellation for long-running execution → pending in a future execution-control subphase
- Client connection check during CPU-bound execution (`client_connection_check_interval`-style behavior) → pending in a future execution-control subphase
