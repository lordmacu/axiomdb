# Phase 22b — Platform Features

## Subphases completed in this session: 22b.3a

## What was built

### 22b.3a — Database catalog and SQL/wire support

AxiomDB now has a real persisted database catalog instead of transport-only
session state.

The catalog adds two logical system heaps:

- `axiom_databases` — one row per logical database
- `axiom_table_databases` — optional table ownership binding by `table_id`

Fresh databases bootstrap the default logical database `axiomdb`. Legacy
databases created before this subphase are upgraded lazily on open:

- missing database roots are allocated
- `axiomdb` is inserted as the default database
- legacy tables without an explicit binding resolve to `axiomdb`

### SQL surface

The SQL layer now supports:

- `CREATE DATABASE name`
- `DROP DATABASE name`
- `DROP DATABASE IF EXISTS name`
- `USE name`
- `SHOW DATABASES`

`SHOW DATABASES` is no longer hardcoded in the MySQL handler. It is now backed
by the catalog and survives restart.

`DROP DATABASE` is catalog-destructive:

- all owned tables are dropped
- columns, indexes, constraints, foreign keys, and stats become unreachable
- explicit table-to-database bindings are removed

The current connection cannot drop its own selected database; the server returns
an explicit error instead.

### Resolution model

`SessionContext` now distinguishes:

- `selected_database()` — what the session explicitly selected via `USE` or the handshake
- `effective_database()` — selected database, or legacy fallback `axiomdb`

This keeps MySQL-compatible `DATABASE()` semantics while preserving old
unqualified name resolution for databases that predate multi-database support.

### MySQL wire behavior

The server now validates database names in both places where MySQL clients can
select them:

- handshake `CLIENT_CONNECT_WITH_DB`
- `COM_INIT_DB`

Unknown databases now fail with `ER_BAD_DB_ERROR (1049)` immediately.

### Tests and validation

Targeted regressions were added in:

- `crates/axiomdb-catalog/tests/integration_schema_binding.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs`

Closure gates passed:

- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo fmt --check`
- `python3 tools/wire-test.py` (`251/251` passed)

## Deferred

- `22b.3b` — fully-qualified `database.schema.table`
- `22b.3b` — cross-database SELECT / JOIN / DML
- later schema phases — database-local schemas beyond `public`
- later platform phases — per-database COMPAT, encryption, quotas, ownership
