# Spec: 5.9c — SHOW STATUS

## What to build (not how)

Implement MySQL-compatible status result sets in the MySQL wire handler for:

- `SHOW STATUS`
- `SHOW GLOBAL STATUS`
- `SHOW SESSION STATUS`
- `SHOW LOCAL STATUS`
- each of the forms above with `LIKE 'pattern'`

This subphase is about compatibility for clients that probe the server on
connect or during health checks. AxiomDB already answers bootstrap statements
such as `SHOW VARIABLES`, `SELECT @@...`, and `SELECT version()` directly in the
wire handler. `SHOW STATUS` must stop returning an empty stub and instead
expose the minimum counter surface defined in
[`docs/progreso.md`](/Users/cristian/nexusdb/docs/progreso.md).

### Supported variables

- `Threads_connected`
- `Threads_running`
- `Questions`
- `Uptime`
- `Bytes_received`
- `Bytes_sent`
- `Com_select`
- `Com_insert`
- `Innodb_buffer_pool_read_requests`
- `Innodb_buffer_pool_reads`

### Scope semantics

- `SHOW STATUS` behaves as `SHOW SESSION STATUS`
- `SHOW LOCAL STATUS` is an alias of `SHOW SESSION STATUS`
- `SHOW GLOBAL STATUS` is the explicit global form

Session scope returns:

- session-local values for:
  - `Questions`
  - `Bytes_received`
  - `Bytes_sent`
  - `Com_select`
  - `Com_insert`
- current-command/session semantics for:
  - `Threads_running` = `1` while serving the statement
- server-wide values exposed in session scope for compatibility:
  - `Threads_connected`
  - `Uptime`
  - `Innodb_buffer_pool_read_requests`
  - `Innodb_buffer_pool_reads`

Global scope returns server-wide values for all supported variables.

### Counter semantics

- `Threads_connected`
  - number of authenticated connections currently inside the command loop
- `Threads_running`
  - global scope: number of connections actively executing a command
  - session scope: `1` for the current connection while the statement is being
    processed
- `Questions`
  - number of processed SQL statements after authentication
  - each statement in a multi-statement `COM_QUERY` counts separately
  - each `COM_STMT_EXECUTE` counts once
  - intercepted statements answered in the wire layer still count as processed
    statements
- `Uptime`
  - whole seconds since the server status subsystem was initialized
- `Bytes_received`
  - count full MySQL packet sizes after authentication, including the 4-byte
    packet header
- `Bytes_sent`
  - count full MySQL packet sizes after authentication, including the 4-byte
    packet header
- `Com_select`
  - number of processed `SELECT` statements
  - includes intercepted `SELECT @@...`, `SELECT version()`, and
    `SELECT DATABASE()` forms that never enter the executor
- `Com_insert`
  - number of processed `INSERT` statements
- `Innodb_buffer_pool_read_requests`
  - AxiomDB compatibility counter for logical page-read requests
- `Innodb_buffer_pool_reads`
  - AxiomDB compatibility counter for physical page reads

### LIKE semantics

The optional `LIKE 'pattern'` filter is part of this subphase.

- `%` matches zero or more characters
- `_` matches exactly one character
- matching is case-insensitive against the variable name
- returned variable names keep canonical MySQL-style casing
- unknown patterns return the correct headers and zero rows

This subphase supports `LIKE`, not `WHERE`.

### Result shape

Every supported statement returns a two-column rowset:

- `Variable_name`
- `Value`

Rows must be deterministic and sorted ascending by `Variable_name`.
All `Value` cells are serialized as text.

## Inputs / Outputs

- Input:
  - a SQL string intercepted by
    [`intercept_special_query`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/handler.rs)
  - the current
    [`ConnectionState`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/session.rs)
  - shared server status state already available to the connection without
    taking a fresh execution lock for every `SHOW STATUS`
- Output:
  - `QueryResult::Rows { columns, rows }`
  - `columns` is always:
    - `Variable_name TEXT NOT NULL`
    - `Value TEXT NOT NULL`
  - `rows` is zero or more status rows
- Errors:
  - no error for an unknown `LIKE` pattern
  - no error for `LOCAL` vs `SESSION`

## Use cases

1. `SHOW STATUS` returns a non-empty session-scoped rowset with the two
   required columns.
2. `SHOW GLOBAL STATUS LIKE 'Threads_connected'` returns the current global
   connection count.
3. `SHOW SESSION STATUS LIKE 'Com_select'` reflects only the current
   connection’s processed `SELECT` statements.
4. `SHOW LOCAL STATUS LIKE 'Bytes_received'` behaves exactly like
   `SHOW SESSION STATUS LIKE 'Bytes_received'`.
5. `SHOW STATUS LIKE 'Com_%'` returns exactly `Com_insert` and `Com_select`.
6. `SHOW STATUS LIKE 'Com_inser_'` returns exactly `Com_insert`.
7. `SHOW STATUS LIKE 'threads%'` matches both `Threads_connected` and
   `Threads_running` regardless of case in the pattern.
8. After `SELECT 1; SELECT 2;`, the same connection’s
   `SHOW STATUS LIKE 'Questions'` reflects two processed statements.
9. After `SELECT @@version`, the same connection’s
   `SHOW STATUS LIKE 'Com_select'` has increased, even though the query was
   answered in the wire layer.
10. After `COM_RESET_CONNECTION`, session counters such as `Questions` and
    `Com_select` reset for that connection while global counters do not.
11. A fresh second connection sees
    `SHOW SESSION STATUS LIKE 'Com_select' = 0` even if another connection has
    already executed `SELECT`.
12. `SHOW STATUS` remains queryable while another connection is holding the
    main `Database` mutex to execute SQL.

## Acceptance criteria

- [ ] `SHOW STATUS` is accepted and behaves as session scope
- [ ] `SHOW SESSION STATUS` and `SHOW LOCAL STATUS` behave identically
- [ ] `SHOW GLOBAL STATUS` returns global counters
- [ ] every result has exactly the columns `Variable_name` and `Value`
- [ ] row order is deterministic and ascending by `Variable_name`
- [ ] `SHOW STATUS LIKE 'x'` returns zero rows, not ERR
- [ ] `SHOW STATUS LIKE 'Com_%'` returns `Com_insert` and `Com_select`
- [ ] `SHOW STATUS LIKE 'Com_inser_'` returns only `Com_insert`
- [ ] variable-name matching is case-insensitive
- [ ] `%` and `_` behave as SQL-LIKE wildcards, not as a substring filter
- [ ] `Questions` increments once per statement in a multi-statement query
- [ ] `Questions` increments for `COM_STMT_EXECUTE`
- [ ] intercepted `SELECT @@...` / `SELECT version()` / `SELECT DATABASE()`
      contribute to `Com_select`
- [ ] `Com_select` increments only for `SELECT`
- [ ] `Com_insert` increments only for `INSERT`
- [ ] `Bytes_received` and `Bytes_sent` are derived from real wire packets, not
      SQL string length
- [ ] session `Threads_running` returns `1` while the statement is served
- [ ] `Threads_connected` reflects authenticated live sessions
- [ ] `Uptime` is monotonic during the process lifetime
- [ ] `COM_RESET_CONNECTION` resets session counters without resetting global
      counters
- [ ] once a connection has entered the command loop, building its `SHOW STATUS`
      rowset does not require taking a fresh `Database` execution lock
- [ ] wire smoke coverage includes session/global/local forms and `LIKE`

## Out of scope

- `SHOW STATUS WHERE ...`
- `FLUSH STATUS`
- explicit escape handling beyond plain `%` and `_` wildcard semantics
- full MariaDB/MySQL status-variable coverage beyond the 10 variables above
- exact InnoDB implementation parity for the two buffer-pool counters
- exposing every `Com_*` variable; this subphase only requires `Com_select`
  and `Com_insert`
- Performance Schema / Information Schema status tables

## Dependencies

These files were reviewed before writing this spec:

- [`db.md`](/Users/cristian/nexusdb/db.md)
- [`docs/progreso.md`](/Users/cristian/nexusdb/docs/progreso.md)
- [`handler.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/handler.rs)
- [`session.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/session.rs)
- [`database.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/database.rs)
- [`result.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/result.rs)
- [`eval.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/eval.rs)
- [`lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/lib.rs)
- [`integration_protocol.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/tests/integration_protocol.rs)
- [`wire-test.py`](/Users/cristian/nexusdb/tools/wire-test.py)

Existing codebase facts this spec depends on:

- `SHOW STATUS` currently lives in the wire intercept path, not in the SQL
  executor
- `COM_RESET_CONNECTION` already recreates `ConnectionState`
- `Database` already exposes the `schema_version` clone-on-connect pattern
- the current `SHOW VARIABLES` implementation already proves the two-column
  result shape, but its `LIKE` filter is simplified and must not define
  `5.9c` semantics by accident
- AxiomDB already has a tested SQL `LIKE` matcher in `axiomdb-sql`

## ⚠️ DEFERRED

- exact storage-level semantics for
  `Innodb_buffer_pool_read_requests` and `Innodb_buffer_pool_reads`
  → pending a dedicated storage metrics hook
- `SHOW STATUS WHERE VARIABLE_NAME LIKE ...`
  → pending a later introspection follow-up
- `FLUSH STATUS`
  → pending later observability work
- full MySQL/MariaDB status-variable coverage
  → pending future observability phases
