# Plan: 5.11c — Explicit connection state machine

## Files to create/modify
- `Cargo.toml` — add workspace dependency for `socket2`
- `crates/axiomdb-network/Cargo.toml` — consume `socket2` from workspace
- `crates/axiomdb-network/src/mysql/mod.rs` — register the new lifecycle module
- `crates/axiomdb-network/src/mysql/lifecycle.rs` — typed connection phases, timeout policy, socket keepalive helper, and timeout-wrapped read/write helpers
- `crates/axiomdb-network/src/mysql/packets.rs` — add `CLIENT_INTERACTIVE` capability constant
- `crates/axiomdb-network/src/mysql/session.rs` — validate timeout session vars and expose typed getters
- `crates/axiomdb-network/src/mysql/handler.rs` — drive explicit phase transitions and replace raw reads/writes with lifecycle helpers
- `crates/axiomdb-server/src/main.rs` — replace direct `set_nodelay(true)` call with a socket-config helper that sets both `TCP_NODELAY` and keepalive
- `crates/axiomdb-network/tests/integration_protocol.rs` — add protocol/unit assertions for timeout var validation and interactive capability parsing
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — live-socket tests for auth timeout, idle timeout, `COM_RESET_CONNECTION`, and graceful close semantics
- `tools/wire-test.py` — wire smoke assertions for timeout variables and lifecycle-visible behavior once the subphase is implemented

## Algorithm / Data structure
### 1. New transport runtime, separate from `ConnectionState`
Create a new internal module:

```text
enum ConnectionPhase {
    Connected,
    Auth,
    Idle,
    Executing,
    Closing,
}

struct LifecycleTimeouts {
    auth_timeout: Duration,   // fixed 10s
}

struct ConnectionLifecycle {
    phase: ConnectionPhase,
    client_capability_flags: u32,
    timeouts: LifecycleTimeouts,
}
```

Rules:
- `ConnectionLifecycle` owns transport/lifecycle state only.
- `ConnectionState` remains the owner of SQL session state and timeout variable values.
- `client_capability_flags` is initialized to `0` on accept and updated after parsing `HandshakeResponse41`.
- `CLIENT_INTERACTIVE` is derived from `HandshakeResponse.capability_flags` and must survive `COM_RESET_CONNECTION`.

### 2. Timeout getters live in `ConnectionState`
Add typed getters in `session.rs`:

```text
fn net_read_timeout_secs(&self) -> Result<u64, DbError>
fn net_write_timeout_secs(&self) -> Result<u64, DbError>
fn wait_timeout_secs(&self) -> Result<u64, DbError>
fn interactive_timeout_secs(&self) -> Result<u64, DbError>
```

Update `apply_set()` so these variables are validated exactly like `max_allowed_packet`:
- decimal integer only
- `> 0`
- stored back as normalized decimal strings

This prevents the lifecycle layer from doing ad hoc string parsing during I/O.

### 3. Socket configuration helper
Add a helper in `lifecycle.rs`:

```text
fn configure_client_socket(stream: &TcpStream) -> io::Result<()>
```

Behavior:
- always enable `TCP_NODELAY`
- always enable `SO_KEEPALIVE`
- use `socket2::SockRef` so configuration happens without taking ownership away from Tokio
- attempt best-effort keepalive tuning:
  - idle = 60s
  - interval = 30s
  - retries = 3
- unsupported tuning knobs must not fail the connection; log at `debug`/`warn` and keep `SO_KEEPALIVE` enabled

`main.rs` should call this helper instead of directly calling `set_nodelay(true)`.

### 4. Timeout-wrapped I/O helpers
Add lifecycle helpers that wrap the existing framed codec I/O with `tokio::time::timeout`:

```text
async fn read_auth_packet(...)
async fn read_idle_packet(...)
async fn read_execute_packet(...)
async fn send_auth_packet(...)
async fn send_execute_packet(...)
async fn send_packet_batch(...)
```

Timeout sources:
- auth read/write: fixed `auth_timeout = 10s`
- idle read:
  - `interactive_timeout` if `CLIENT_INTERACTIVE` is set
  - else `wait_timeout`
- execute read: `net_read_timeout`
- execute write: `net_write_timeout`

These helpers return a small internal error enum, for example:

```text
enum ConnectionIoError {
    Timeout(ConnectionPhase),
    Read(MySqlCodecError),
    Write(std::io::Error),
    Closed,
}
```

Timeouts are terminal. The handler transitions to `CLOSING` and returns.

### 5. `handle_connection()` becomes explicitly phase-driven
Refactor `handler.rs` so the top-level flow is:

```text
configure socket
lifecycle = ConnectionLifecycle::new()

enter Connected
send greeting with auth timeout
enter Auth
read HandshakeResponse with auth timeout
populate lifecycle.client_capability_flags
perform auth exchange with auth timeout
enter Idle

loop:
    read next packet with idle timeout
    if EOF / timeout / read error:
        enter Closing
        break

    enter Executing
    dispatch command
    all packet writes use execute write timeout
    any extra in-flight reads use execute read timeout

    on COM_QUIT:
        enter Closing
        break

    on successful command completion:
        enter Idle
```

Important invariants:
- `RunningGuard` remains metrics/status only; it is not the lifecycle state machine.
- `ConnectedGuard` remains connection metrics only; it is not the lifecycle state machine.
- `COM_RESET_CONNECTION` recreates `ConnectionState`, but does not recreate `ConnectionLifecycle`.

### 6. `COM_RESET_CONNECTION` semantics
Keep current behavior:
- reset `session = SessionContext::new()`
- reset `conn_state = ConnectionState::new()`
- reset codec `max_allowed_packet`

Additional requirement:
- preserve `lifecycle.client_capability_flags`
- preserve keepalive/NODELAY socket settings
- after reset, phase becomes `Idle`

### 7. Testability design
Do not hardcode untestable wall-clock sleeps into `handle_connection`.

Introduce an internal constructor or helper such as:

```text
ConnectionLifecycle::with_timeouts(LifecycleTimeouts)
handle_connection_with_lifecycle(...)
```

Production `handle_connection()` uses the default 10s auth timeout.
Integration tests can inject millisecond-scale timeouts.

## Implementation phases
1. Add `CLIENT_INTERACTIVE` constant in `packets.rs`.
2. Add validated timeout getters and `SET` validation in `session.rs`.
3. Add `socket2` dependency and implement `configure_client_socket()` plus lifecycle types/helpers in `lifecycle.rs`.
4. Register the lifecycle module in `mysql/mod.rs`.
5. Switch `main.rs` to call the socket configuration helper on accept.
6. Refactor handshake/auth in `handler.rs` to use explicit `Connected`/`Auth`/`Closing` transitions and auth-timeout-wrapped I/O.
7. Refactor the command loop to use `Idle`/`Executing` transitions and timeout-wrapped I/O for reads/writes.
8. Ensure `COM_RESET_CONNECTION` preserves lifecycle metadata while resetting session state.
9. Add unit/integration/wire tests.

## Tests to write
- unit:
  - `CLIENT_INTERACTIVE` selects `interactive_timeout` instead of `wait_timeout`
  - non-interactive connections use `wait_timeout`
  - `SET wait_timeout|interactive_timeout|net_read_timeout|net_write_timeout` rejects non-numeric and zero values
  - lifecycle transitions `Connected -> Auth -> Idle -> Executing -> Closing` are deterministic
- integration:
  - auth timeout closes an unauthenticated connection when the client never sends `HandshakeResponse41`
  - idle timeout closes an authenticated connection after the configured timeout
  - `COM_RESET_CONNECTION` resets timeout vars to defaults but preserves interactive/non-interactive classification
  - `COM_QUIT` reaches `Closing` cleanly and returns without extra packets
  - malformed handshake still returns ERR before close when possible
- wire:
  - `SHOW VARIABLES LIKE 'wait_timeout'`, `interactive_timeout`, `net_read_timeout`, and `net_write_timeout` reflect live session values
  - `SET` of those variables is visible through `SELECT @@...`
  - regression coverage that existing handshake/auth/query/prepared statement flows still work after the lifecycle refactor

## Anti-patterns to avoid
- Do not store lifecycle phase or client capability flags inside `ConnectionState`.
- Do not parse timeout variables ad hoc in `handler.rs`; validate them in `session.rs`.
- Do not use `wait_timeout` while a command is actively executing.
- Do not overload `RunningGuard` or `ConnectedGuard` as lifecycle state.
- Do not make keepalive a SQL session variable in this subphase.
- Do not silently ignore `CLIENT_INTERACTIVE`; it is the compatibility hook that makes `interactive_timeout` meaningful.
- Do not introduce statement timeout/query cancellation under the name of connection lifecycle.

## Risks
- Risk: `COM_RESET_CONNECTION` accidentally erases transport metadata.
  Mitigation: keep lifecycle in a separate struct that survives session reset.

- Risk: timeout var parsing failures happen during I/O instead of at `SET` time.
  Mitigation: validate and normalize timeout vars in `ConnectionState::apply_set()`.

- Risk: socket keepalive tuning is platform-dependent.
  Mitigation: require `SO_KEEPALIVE` always, make detailed tuning best-effort, and keep tests focused on helper behavior rather than OS timers.

- Risk: a long-running query is incorrectly aborted by idle timeout.
  Mitigation: apply `wait_timeout`/`interactive_timeout` only around idle command reads.

- Risk: refactoring `send_packets()` breaks multi-packet result sets.
  Mitigation: keep the batched encoding logic, only wrap the final batch future in the lifecycle write timeout helper.

## Assumptions
- `connect_timeout` is represented in this subphase as a fixed 10-second auth timeout; exposing it as a variable is deferred.
- After auth, current AxiomDB commands rarely require additional client reads while executing; the execute-read timeout helper is added now for correctness and future protocol growth.
- TCP keepalive only improves eventual dead-peer detection; it is not a substitute for the explicit state timeouts above.
