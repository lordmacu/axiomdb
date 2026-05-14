# Schema Migrations

AxiomDB includes a built-in schema migration tool as a subcommand of
`axiomdb-server`. It tracks applied migrations in a `axiomdb_migrations` table
inside the target database — no separate process or external tool required.

## Quick start

```bash
# 1. Create your first migration
axiomdb-server migrate create create_users --dir ./migrations

# 2. Edit the generated file (migrations/0001_create_users.sql):
#    CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
#    -- +migrate Down
#    DROP TABLE users;

# 3. Apply all pending migrations
axiomdb-server migrate up --data-dir ./data --dir ./migrations

# 4. Check status
axiomdb-server migrate status --data-dir ./data --dir ./migrations
```

## Commands

### `migrate status`

Lists all migration files and whether each has been applied.

```bash
axiomdb-server migrate status [OPTIONS]
```

Output:

```
Version  Name                                     Status
------------------------------------------------------------
1        create_users                             applied
2        add_email_column                         pending

2 migrations total, 1 applied, 1 pending
```

### `migrate up`

Applies all pending migrations in version order.

```bash
axiomdb-server migrate up [OPTIONS]
```

```
  Applying 0001_create_users ... ok
  Applying 0002_add_email_column ... ok
Done.
```

Idempotent: re-running `up` when all migrations are applied prints
"Nothing to migrate." and exits cleanly.

### `migrate down`

Reverts the **last applied** migration using its DOWN section.

```bash
axiomdb-server migrate down [OPTIONS]
```

```
  Reverting 0002_add_email_column ... ok
Done.
```

Requires a `-- +migrate Down` section in the migration file. If absent,
`down` exits with an error.

### `migrate create NAME`

Creates a new numbered migration file in the migrations directory.

```bash
axiomdb-server migrate create add_email_column [--dir ./migrations]
```

```
Created migration file: migrations/0002_add_email_column.sql
```

The version number is automatically one higher than the largest existing
migration. Spaces in `NAME` are replaced with underscores.

### Options

| Flag          | Default       | Description                      |
|---------------|---------------|----------------------------------|
| `--data-dir`  | `./data`      | AxiomDB data directory           |
| `--db`        | `axiomdb`     | Target database name             |
| `--dir`       | `./migrations`| Directory containing `.sql` files|

## Migration file format

Files must be named `{VERSION}_{description}.sql` where `VERSION` is a
positive integer (e.g., `0001_create_users.sql`).

```sql
-- Write your UP migration here:
CREATE TABLE orders (
    id     INT  PRIMARY KEY,
    total  INT  NOT NULL,
    status TEXT
);

-- +migrate Down
DROP TABLE orders;
```

- Everything before `-- +migrate Down` is the **UP** migration.
- Everything after is the **DOWN** migration (optional).
- Multiple SQL statements are supported; separate them with `;`.
- Semicolons inside string literals are handled correctly.

## State tracking

Applied migrations are recorded in `axiomdb_migrations` inside the target
database. AxiomDB creates this table automatically on first use.

```sql
SELECT version, name, applied_at FROM axiomdb_migrations ORDER BY version;
```

| Column       | Type   | Description                      |
|--------------|--------|----------------------------------|
| `version`    | INT    | Migration version number         |
| `name`       | TEXT   | Migration name (from filename)   |
| `applied_at` | BIGINT | Unix timestamp in milliseconds   |

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Running before the server starts</span>
<code>axiomdb-server migrate</code> opens the data directory directly — the
wire-protocol server does not need to be running. This makes it safe to run
migrations as part of a container startup script before the server accepts
connections.
</div>
</div>
