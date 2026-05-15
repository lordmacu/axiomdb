# Phase 22b — Platform Features

## Subphases completed: 22b.1, 22b.2, 22b.3a, 22b.3b, 22b.4, 22b.5

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

### 22b.1 — Scheduled jobs (pg_cron-style)

AxiomDB now supports persistent background cron jobs, compatible with the
`pg_cron` SQL API and backed by a catalog heap.

**SQL surface:**

```sql
-- Register a job (schedule, name, SQL command)
SELECT cron_schedule('nightly_vacuum', '@daily', 'DELETE FROM logs WHERE ts < NOW() - INTERVAL 30 DAY');
-- Returns: 'nightly_vacuum'

-- Register with 5-field cron expression
SELECT cron_schedule('top_of_hour', '0 * * * *', 'SELECT refresh_stats()');

-- Pause / resume
SELECT cron_disable('nightly_vacuum');  -- returns 1
SELECT cron_enable('nightly_vacuum');   -- returns 1

-- Remove
SELECT cron_unschedule('nightly_vacuum');  -- returns 1 (0 if not found)

-- Inspect
SELECT JOB_NAME, SCHEDULE, COMMAND, DATABASE_NAME, ENABLED, NEXT_RUN, LAST_RUN, LAST_STATUS
FROM information_schema.scheduled_jobs;
```

**Catalog:** `axiom_cron_jobs` heap at offset 160 in the meta page. Each row is
a binary-encoded `CronJobDef` (name, schedule, command, database, enabled flag,
`next_run_ms`, `last_run_ms`, `last_status`).

**Scheduler:** A background tokio task (`axiomdb-network/src/scheduler.rs`)
launched at server startup. Every minute it:
1. Snapshots the catalog and lists enabled jobs.
2. Fires any job whose `next_run_ms ≤ now` (or `next_run_ms == 0` = first run).
3. Executes the job's SQL in a `SessionContext` scoped to the job's target database.
4. Persists `last_run_ms`, `next_run_ms`, and `last_status` in a mini-transaction.
5. Sleeps to the next minute boundary.

**Cron expressions:** Custom parser (no external cron dependency in the executor).
Supports `*`, `*/N` (step), `N-M` (range), `N,M,...` (list), and exact values
for all 5 fields (min hour dom month dow). Aliases: `@hourly`, `@daily`,
`@midnight`, `@weekly`, `@monthly`, `@yearly`, `@annually`.

**Tests:**

- `crates/axiomdb-sql/tests/integration_scheduled_jobs.rs` — 11 unit tests
- `tools/wire-test.py` section `[22b.1 cron]` — 9 wire scenarios

### 22b.2 — HTTP Foreign Data Wrappers

AxiomDB now supports querying external HTTP JSON APIs as if they were local tables,
using a PostgreSQL-compatible Foreign Data Wrapper (FDW) SQL interface.

**SQL surface:**

```sql
-- Register an external HTTP service as a server
CREATE SERVER myapi FOREIGN DATA WRAPPER http
  OPTIONS (url 'http://api.example.com', timeout_ms '5000');

-- Map a remote endpoint to a local schema
CREATE FOREIGN TABLE ft_users (
    id     INT,
    name   TEXT,
    active BOOLEAN
) SERVER myapi OPTIONS (endpoint '/users');

-- Query transparently — AxiomDB fetches JSON and maps rows
SELECT id, name FROM ft_users WHERE active = TRUE;
SELECT COUNT(*) FROM ft_users;

-- Lifecycle management
DROP FOREIGN TABLE ft_users;
DROP SERVER myapi;

-- Idempotent creation
CREATE SERVER IF NOT EXISTS myapi FOREIGN DATA WRAPPER http OPTIONS (url 'http://...');
CREATE FOREIGN TABLE IF NOT EXISTS ft_users (...) SERVER myapi OPTIONS (...);
DROP SERVER IF EXISTS myapi;
DROP FOREIGN TABLE IF EXISTS ft_users;

-- Catalog introspection
SELECT * FROM information_schema.foreign_servers;
SELECT * FROM information_schema.foreign_tables;
```

**HTTP connector:** Implemented in `fdw_http.rs` using `std::net::TcpStream` — no
`ureq` or `reqwest` dependency. The connector:

1. Parses the URL via the `url` crate (already in the workspace).
2. Opens a plain TCP connection (`http://` only; HTTPS deferred to a later phase).
3. Sends a minimal HTTP/1.1 GET request with `Accept: application/json`.
4. Reads the response and strips headers to extract the JSON body.
5. Parses the JSON body as an array of objects using `serde_json`.
6. Maps each JSON object to an AxiomDB row, coercing values to declared column types.
7. Missing fields and JSON `null` both map to `Value::Null`.

**Catalog:** Two new system heaps:

| Heap | Meta page offset | Content |
|---|---|---|
| `axiom_foreign_servers` | 168 | `ForeignServerDef` binary rows |
| `axiom_foreign_tables` | 176 | `ForeignTableDef` binary rows with inline columns |

Foreign table IDs are allocated from the range `>= 0x8000_0000` (`FOREIGN_TABLE_ID_BASE`)
so they never collide with physical table IDs.

**Execution model:** Both SELECT paths (with and without `SessionContext`) detect
`table_id >= FOREIGN_TABLE_ID_BASE` and route to the FDW connector instead of the
storage engine. The analyzer binder also resolves foreign tables via the catalog,
allowing `SELECT col FROM ft_users` to bind column indices correctly.

**Information schema:**

- `information_schema.foreign_servers` — `SERVER_NAME`, `FDW_NAME`, `OPTIONS`
- `information_schema.foreign_tables` — `TABLE_SCHEMA`, `TABLE_NAME`, `SERVER_NAME`,
  `COLUMN_COUNT`, `OPTIONS`

**Tests:**

- `crates/axiomdb-sql/tests/integration_fdw.rs` — 26 unit tests covering DDL lifecycle,
  IS introspection, live HTTP scan (mock TcpListener), WHERE filtering, COUNT(*),
  null/missing field handling, and connection-refused error propagation.
- `tools/wire-test.py` section `[22b.2 fdw]` — 11 wire scenarios.

**Closure gates passed:**

- `cargo nextest run --workspace` (`3805/3805` passed)
- `cargo clippy --workspace -- -D warnings` — clean
- `cargo fmt --check` — clean
- `python3 tools/wire-test.py` (`521/521` passed)

### 22b.5 — Schema migrations CLI

AxiomDB ships a built-in migrations CLI as a subcommand of `axiomdb-server`.

**Commands:**

```bash
# Show migration status
axiomdb-server migrate status --data-dir ./data --db myapp --dir ./migrations

# Apply all pending migrations
axiomdb-server migrate up --data-dir ./data --db myapp --dir ./migrations

# Revert the last applied migration
axiomdb-server migrate down --data-dir ./data --db myapp --dir ./migrations

# Create a new migration file
axiomdb-server migrate create add_users_table --dir ./migrations
```

**Migration file format** (`N_description.sql`):

```sql
-- Write your UP migration here:
CREATE TABLE users (
    id   INT  PRIMARY KEY,
    name TEXT NOT NULL
);

-- +migrate Down
DROP TABLE users;
```

Files are named `{version}_{description}.sql` (e.g., `0001_create_users.sql`).
The `-- +migrate Down` marker separates UP from DOWN. DOWN is optional —
if absent, `migrate down` reports an error for that migration.

**State tracking:** Applied migrations are recorded in `axiomdb_migrations`
table inside the target database. The table is created automatically on first
use. Schema:

```sql
CREATE TABLE IF NOT EXISTS axiomdb_migrations (
    version    INT  NOT NULL,
    name       TEXT NOT NULL,
    applied_at BIGINT NOT NULL,
    PRIMARY KEY (version)
)
```

**Tests:** 18 unit tests in `crates/axiomdb-server/src/migrate.rs` covering
file parsing, sorting, duplicate detection, up/down lifecycle, idempotency,
and `create` generation.

## Deferred

- 22b.6 — FDW pushdown (push SQL predicates to remote origin; depends on 22b.2)
- HTTPS support — deferred; current FDW only supports `http://` URLs
- later platform phases — per-database COMPAT, encryption, quotas, ownership
