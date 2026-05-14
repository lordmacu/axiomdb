# Phase 22b — Platform Features

## Subphases completed: 22b.3a, 22b.3b, 22b.4

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

### 22b.3b — Cross-database queries

AxiomDB supports fully-qualified `database.schema.table` references in all DML
and DDL statements.

**Resolution model:** Each `TableRef` in a query carries an optional `database`
field. `resolve_table_cached` reads it and delegates to a `SchemaResolver`
scoped to the target database. The session's effective database is used as the
fallback when the field is absent.

**SQL surface:**

```sql
SELECT * FROM analytics.public.events;
INSERT INTO axiomdb.public.log SELECT * FROM local_copy;
UPDATE other_db.public.items SET score = 99 WHERE id = 1;
DELETE FROM old_db.public.logs WHERE id > 100;
CREATE TABLE analytics.public.scores (id INT, val INT);

-- Cross-db JOIN (two databases in one query)
SELECT c.name, o.total
FROM crm.public.customers AS c
JOIN axiomdb.public.orders AS o ON o.user_id = c.id;
```

**Error handling:** Unknown database in a 3-part name returns
`DatabaseNotFound (ER_BAD_DB_ERROR)` immediately, before table resolution.

**Tests:**

- `crates/axiomdb-sql/tests/integration_namespacing_cross_db.rs` — 9 unit tests
- `tools/wire-test.py` section `[22b.3b]` — 8 wire scenarios including cross-db JOIN

## Deferred

- later schema phases — database-local schemas beyond `public`
- later platform phases — per-database COMPAT, encryption, quotas, ownership
