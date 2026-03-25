# Plan: 5.9c — SHOW STATUS

## Files to create/modify

- `crates/axiomdb-network/src/mysql/status.rs`
  - new typed status subsystem: global registry, session snapshot helpers,
    scope parsing, canonical variable order, row builder
- `crates/axiomdb-network/src/mysql/mod.rs`
  - expose the new internal status module
- `crates/axiomdb-network/src/mysql/database.rs`
  - store a shared `Arc<StatusRegistry>` on `Database`
- `crates/axiomdb-network/src/mysql/handler.rs`
  - clone the shared status registry once per connection, replace the empty
    `SHOW STATUS` stub, instrument counters in the existing command loop
- `crates/axiomdb-network/src/mysql/session.rs`
  - add session-local status counters to `ConnectionState`
- `crates/axiomdb-sql/src/eval.rs`
  - expose or extract the already-tested `LIKE` matcher so wire-layer `SHOW`
    filtering can reuse the same wildcard semantics
- `crates/axiomdb-sql/src/lib.rs`
  - re-export the `LIKE` helper if needed by `axiomdb-network`
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - unit coverage for scope parsing, wildcard semantics, and session/global
    snapshot behavior
- `tools/wire-test.py`
  - live MySQL-client assertions for `SHOW STATUS`
- `docs-site/src/user-guide/getting-started.md`
  - note that MySQL tooling can probe `SHOW STATUS`
- `docs-site/src/internals/architecture.md`
  - explain where status counters live in AxiomDB and why they are not rebuilt
    from the engine on every call

## Algorithm / Data structure

### Chosen approach

Anchor the implementation to the AxiomDB codebase exactly as it exists today:

- `Database` remains the server-owned object behind `Arc<Mutex<Database>>`
- `Database` gains `Arc<StatusRegistry>`
- [`handle_connection`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/handler.rs)
  clones that registry once per connection, exactly like it already clones
  `schema_version`
- `ConnectionState` owns only session-local counters
- `SHOW STATUS` remains in the existing intercept layer, alongside
  `SHOW VARIABLES`, `SELECT @@...`, and `SELECT version()`

This is the best fit for AxiomDB today because it:

- avoids the architectural contradiction of "status inside `Database` but
  somehow no mutex"
- avoids unnecessary churn from inventing a brand-new runtime object
- keeps the read path modern and cheap: atomics for global counters, plain
  `u64` for session counters, and deterministic typed snapshots

### Research applied after reading the codebase

- MariaDB
  - defines the external semantics:
    - `SHOW STATUS` defaults to session scope
    - `LOCAL` aliases `SESSION`
    - `Threads_running` has special session behavior
    - `LIKE` is real wildcard matching, not substring matching
- OceanBase
  - validates the global/session split as a first-class design
- PostgreSQL
  - reinforces the distinction between current-state counters
    (`Threads_connected`, `Threads_running`) and cumulative counters
    (`Questions`, bytes, `Com_*`)
- SQLite
  - shows status should be its own subsystem, not reconstructed on demand
- DuckDB / DataFusion
  - justify typed, cheap, shared metric structures rather than mutable string
    maps or expensive derivation

### Alternatives considered

1. Derive everything under `db.lock().await`
   - Pros: minimal code motion
   - Cons: directly fights the current intercept architecture and makes
     monitoring queries contend with SQL execution

2. Introduce a new top-level runtime object and thread it through every server
   entry point
   - Pros: clean in the abstract
   - Cons: more churn than necessary for this subphase; current AxiomDB already
     has the exact clone-on-connect pattern we need

3. Put `Arc<StatusRegistry>` on `Database`, clone it once per connection, keep
   session counters in `ConnectionState`, and reuse the tested SQL `LIKE`
   matcher for `SHOW` wildcard filtering
   - Pros: aligned with the current codebase, avoids repeated locks, avoids
     re-implementing wildcard semantics badly
   - Cons: buffer-pool counters remain best-effort until storage metrics hooks
     exist

Chosen: option 3.

### Data structures

```rust
pub struct StatusRegistry {
    started_at: std::time::Instant,
    threads_connected: AtomicU64,
    threads_running: AtomicU64,
    questions: AtomicU64,
    bytes_received: AtomicU64,
    bytes_sent: AtomicU64,
    com_select: AtomicU64,
    com_insert: AtomicU64,
    innodb_buffer_pool_read_requests: AtomicU64,
    innodb_buffer_pool_reads: AtomicU64,
}

#[derive(Debug, Default, Clone)]
pub struct SessionStatus {
    questions: u64,
    bytes_received: u64,
    bytes_sent: u64,
    com_select: u64,
    com_insert: u64,
}

pub enum StatusScope {
    Global,
    Session,
}

pub struct ShowStatusQuery {
    scope: StatusScope,
    like_pattern: Option<String>,
}

pub enum SqlCommandClass {
    Select,
    Insert,
    Other,
}
```

Notes:

- atomics are sufficient for global counters; they do not guard correctness
  invariants, so relaxed ordering is enough
- `SessionStatus` stays embedded in `ConnectionState` because one task owns it
- canonical variable names should live in one typed list to keep ordering and
  filtering deterministic

### AxiomDB-specific wiring

At connection setup, reuse the already-existing pattern in `handler.rs`:

```text
lock db once
  clone schema_version
  clone status_registry
unlock db
```

This is the key architectural decision. `SHOW STATUS` cannot require looking
back into `Database` on each query because the intercept path only has access to
`&mut ConnectionState` unless we explicitly clone shared state ahead of time.

### LIKE handling

Do not inherit the current `SHOW VARIABLES` filter logic:

- today [`show_variables_result()`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/handler.rs)
  lowercases the SQL, strips `%`, and uses `contains()`
- that is not sufficient for `5.9c`, because `%` and `_` are part of the
  promised semantics

Plan:

- reuse the already-tested `like_match` algorithm from
  [`axiomdb-sql/src/eval.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/eval.rs)
- normalize both variable name and pattern to ASCII lowercase before matching,
  since all supported status variables are ASCII names and the spec requires
  case-insensitive matching
- reuse the matcher only for wildcard semantics, not the old
  `SHOW VARIABLES` substring filter

### Counter update rules

```text
on auth success entering the command loop:
  global.threads_connected += 1

on authenticated connection close:
  global.threads_connected -= 1

while serving COM_QUERY / COM_STMT_PREPARE / COM_STMT_EXECUTE:
  global.threads_running += 1
  ...
  global.threads_running -= 1

on each command packet received after auth:
  global.bytes_received += payload_len + 4
  session.bytes_received += payload_len + 4

for each processed SQL statement:
  global.questions += 1
  session.questions += 1

  if class == Select:
    global.com_select += 1
    session.com_select += 1

  if class == Insert:
    global.com_insert += 1
    session.com_insert += 1

on each response packet sent after auth:
  global.bytes_sent += payload_len + 4
  session.bytes_sent += payload_len + 4
```

Statement classification must follow the code paths AxiomDB already has:

- `COM_QUERY`
  - use the existing multi-statement split already in `handler.rs`
  - classify by leading keyword because only `SELECT` and `INSERT` matter here
- intercepted statements
  - classify intercepted `SELECT @@...`, `SELECT version()`, and
    `SELECT DATABASE()` as `Select`
  - do not classify `SHOW STATUS` / `SHOW VARIABLES` as `Select`
- `COM_STMT_EXECUTE`
  - use prepared statement metadata already stored in `ConnectionState`
  - when available, prefer cached analyzed statement kind
  - otherwise fall back to the original SQL template prefix

### Snapshot building

```text
parse_show_status(sql) -> ShowStatusQuery

build_rows(query, registry, conn_state):
  rows = canonical variable order
  for each variable:
    if query.scope == Session:
      Questions / Bytes_* / Com_*  -> conn_state.session_status
      Threads_running              -> "1"
      everything else              -> registry snapshot
    else:
      all variables                -> registry snapshot

  apply LIKE if present
  build QueryResult::Rows
  serialize through existing text protocol path
```

The row builder belongs in `status.rs`, not inline in `handler.rs`.

### Storage-related counters

`Innodb_buffer_pool_read_requests` and `Innodb_buffer_pool_reads` do not map
perfectly to the current AxiomDB storage architecture:

- AxiomDB uses `MmapStorage`, not an InnoDB buffer pool
- `axiomdb-storage` should not depend on `axiomdb-network`

For 5.9c the plan is:

- expose the two variables now for compatibility
- back them with explicit best-effort AxiomDB counters in `StatusRegistry`
- increment them only where current server-side code has a clear hook
- isolate them behind named methods so a future storage metrics hook can change
  the implementation without changing wire behavior

## Implementation phases

1. Read and lock the scope of `5.9c` against the already-existing handler
   architecture, the current `SHOW VARIABLES` code, and the existing SQL
   `LIKE` matcher.
2. Add `status.rs` with:
   - `StatusRegistry`
   - `SessionStatus`
   - `StatusScope`
   - `ShowStatusQuery`
   - canonical variable list
   - rowset builder
3. Extend `Database` with `status: Arc<StatusRegistry>`.
4. Clone the shared status registry in `handle_connection()` alongside
   `schema_version`.
5. Extend `ConnectionState` with session counters and helper methods for
   incrementing them.
   Reuse the existing `ConnectionState::new()` reset path on
   `COM_RESET_CONNECTION`.
6. Expose the tested SQL `LIKE` matcher from `axiomdb-sql` so `SHOW STATUS`
   wildcard filtering has real `%` / `_` semantics.
7. Replace the current `SHOW STATUS` stub in `intercept_special_query()` with:
   - exact scope parsing
   - exact `LIKE` parsing
   - status rowset building
8. Add counted send helpers so both single-packet responses and multi-packet
   result sets update `Bytes_sent`.
9. Instrument command receive paths for `Bytes_received`.
10. Instrument statement dispatch for `Questions`, `Com_select`, and
    `Com_insert` across:
    - intercepted statements
    - normal `COM_QUERY`
    - `COM_STMT_EXECUTE`
11. Instrument connection and execution lifecycle for `Threads_connected` and
    `Threads_running`, preferably with small guard types to avoid early-return
    drift.
12. Add best-effort hooks for the buffer-pool compatibility counters.
13. Add unit tests, wire-test coverage, and docs updates.

## Tests to write

- unit: `SHOW STATUS` defaults to session scope
- unit: `SHOW LOCAL STATUS` equals `SHOW SESSION STATUS`
- unit: `SHOW GLOBAL STATUS` uses global scope
- unit: rows are returned in deterministic canonical order
- unit: `LIKE 'x'` returns zero rows with the correct two columns
- unit: `LIKE 'Com_%'` returns exactly `Com_insert` and `Com_select`
- unit: `LIKE 'Com_inser_'` returns exactly `Com_insert`
- unit: wildcard filtering is case-insensitive
- unit: session snapshot uses `ConnectionState` counters for `Questions`,
  `Bytes_*`, and `Com_*`
- unit: session snapshot returns `Threads_running = 1`
- unit: global snapshot uses shared registry values for every variable
- unit: fresh `ConnectionState::new()` starts with zero session counters
- integration: `SELECT @@version` increments session `Com_select`
- integration: one connection executes `SELECT`; a fresh second connection
  still sees `SHOW SESSION STATUS LIKE 'Com_select' = 0`
- integration: `SELECT 1; SELECT 2;` increments `Questions` by 2
- integration: `COM_STMT_EXECUTE` increments `Questions` and `Com_select` or
  `Com_insert`
- integration: `COM_RESET_CONNECTION` resets session counters without touching
  global counters
- integration: two simultaneous authenticated connections make
  `SHOW GLOBAL STATUS LIKE 'Threads_connected' >= 2`
- wire: `SHOW STATUS`
- wire: `SHOW GLOBAL STATUS LIKE 'Threads_connected'`
- wire: `SHOW SESSION STATUS LIKE 'Com_select'`
- wire: `SHOW LOCAL STATUS LIKE 'Bytes_received'`
- wire: `SHOW STATUS LIKE 'Com_%'`
- wire: `SHOW STATUS LIKE 'Com_inser_'`

## Anti-patterns to avoid

- Do not write the spec or plan assuming a runtime abstraction that does not
  exist in AxiomDB today.
- Do not lock `Database` on every `SHOW STATUS` call after the connection has
  already cloned shared state.
- Do not copy the current `SHOW VARIABLES` `LIKE` filter logic into
  `SHOW STATUS`.
- Do not keep status counters in `HashMap<String, String>`.
- Do not add a second full SQL parse just to classify `SELECT` vs `INSERT`
  where existing statement prefixes or prepared metadata already suffice.
- Do not count bytes from SQL string length; count encoded/decoded wire packets.
- Do not promise exact InnoDB buffer-pool parity while AxiomDB still lacks a
  general storage metrics hook.

## Risks

- Default scope accidentally implemented as global
  - Mitigation: explicit parser tests for `SHOW STATUS`, `SHOW SESSION STATUS`,
    `SHOW LOCAL STATUS`, and `SHOW GLOBAL STATUS`
- `%` / `_` wildcard semantics regress to substring matching
  - Mitigation: reuse the already-tested SQL `LIKE` matcher and add dedicated
    `SHOW STATUS` wildcard tests
- Intercepted `SELECT` statements omitted from `Com_select`
  - Mitigation: classify intercepted `SELECT @@...`, `SELECT version()`, and
    `SELECT DATABASE()` explicitly in the intercept path
- `COM_STMT_EXECUTE` omitted from counters
  - Mitigation: use existing prepared statement metadata in `ConnectionState`
- Counter drift on early returns or broken sockets
  - Mitigation: use small RAII guards for running/connected lifecycle
- Buffer-pool counters remain approximate
  - Mitigation: document that explicitly and isolate the hook behind registry
    methods
