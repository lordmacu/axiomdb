# DDL — Schema Definition Language

DDL statements define and modify the structure of the database: tables, columns,
constraints, and indexes. All DDL operations are transactional in AxiomDB — a failed
DDL statement is automatically rolled back.

---

## CREATE DATABASE

Creates a new logical database in the persisted catalog.

### Syntax

```sql
CREATE DATABASE database_name;
```

### Example

```sql
CREATE DATABASE analytics;
SHOW DATABASES;
```

Expected output includes:

| Database |
|----------|
| analytics |
| axiomdb |

`CREATE DATABASE` fails if the name already exists:

```sql
CREATE DATABASE analytics;
-- ERROR 1007 (HY000): Can't create database 'analytics'; database exists
```

## DROP DATABASE

Removes a logical database from the catalog.

### Syntax

```sql
DROP DATABASE database_name;
DROP DATABASE IF EXISTS database_name;
```

### Behavior

- Removing a database also removes the tables it owns from SQL/catalog lookup.
- `IF EXISTS` suppresses the error for a missing database.
- The current connection cannot drop the database it has selected with `USE`.

```sql
DROP DATABASE analytics;
```

```sql
DROP DATABASE IF EXISTS scratch;
```

```sql
USE analytics;
DROP DATABASE analytics;
-- ERROR 1105 (HY000): Can't drop database 'analytics'; database is currently selected
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Current Scope</span>
<code>CREATE DATABASE</code> and <code>DROP DATABASE</code> are catalog-backed today, but
cross-database queries such as <code>other_db.public.users</code> are still deferred to the
next multi-database subphase.
</div>
</div>

## CREATE SCHEMA

Creates a new schema (namespace) within the current database.

### Syntax

```sql
CREATE SCHEMA schema_name;
CREATE SCHEMA IF NOT EXISTS schema_name;
```

### Behavior

- Schemas organize tables, views, and sequences into named namespaces.
- `IF NOT EXISTS` is idempotent — no error if the schema already exists.
- The `public` schema is always implicitly available; no `CREATE SCHEMA public` is required.

```sql
CREATE SCHEMA app;
CREATE TABLE app.users (id INT, email TEXT);
INSERT INTO app.users VALUES (1, 'alice@example.com');
SELECT * FROM app.users;
```

---

## DROP SCHEMA

Removes a schema from the current database.

### Syntax

```sql
DROP SCHEMA schema_name;
DROP SCHEMA IF EXISTS schema_name;
DROP SCHEMA schema_name RESTRICT;    -- default: error if schema has tables
DROP SCHEMA schema_name CASCADE;     -- drop all tables in the schema first
```

### Behavior

- **RESTRICT** (default): fails with `SchemaNotEmpty` if the schema still contains tables.
- **CASCADE**: drops all tables in the schema, then drops the schema itself.
- `IF EXISTS` suppresses the error for a missing schema.
- Dropping `information_schema` is always rejected.

```sql
DROP SCHEMA IF EXISTS temp_ns;

DROP SCHEMA analytics CASCADE;

DROP SCHEMA ns_r RESTRICT;
-- ERROR 2BP01: can't drop schema 'ns_r'; schema is not empty
```

---

## SHOW SCHEMAS

Lists the schemas in the current database.

### Syntax

```sql
SHOW SCHEMAS;
SHOW SCHEMAS LIKE 'pattern%';
```

### Example

```sql
CREATE SCHEMA app_main;
CREATE SCHEMA app_test;

SHOW SCHEMAS LIKE 'app%';
-- Schema
-- -------
-- app_main
-- app_test
```

The `public` schema is always included.

---

## SET search_path

Sets the schema search path for the current session so that unqualified table names
resolve without an explicit `schema.table` prefix.

```sql
SET search_path = 'app';
-- After this, `SELECT * FROM orders` is equivalent to `SELECT * FROM app.orders`
```

Only one schema at a time is supported in the current release.

---

## CREATE TABLE

### Basic Syntax

```sql
CREATE TABLE [IF NOT EXISTS] table_name (
    column_name  data_type  [column_constraints...],
    ...
    [table_constraints...]
);
```

### Column Constraints

#### NOT NULL

Rejects any attempt to insert or update a row with a NULL value in this column.

```sql
CREATE TABLE employees (
    id    BIGINT NOT NULL,
    name  TEXT   NOT NULL,
    dept  TEXT            -- nullable: dept may be unassigned
);
```

#### DEFAULT

Provides a value when the column is omitted from INSERT.

```sql
CREATE TABLE orders (
    id         BIGINT   PRIMARY KEY AUTO_INCREMENT,
    status     TEXT     NOT NULL DEFAULT 'pending',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    priority   INT      NOT NULL DEFAULT 0
);

-- Default values are used automatically
INSERT INTO orders (status) VALUES ('shipped');
-- Row: id=<auto>, status='shipped', created_at=<now>, priority=0
```

#### GENERATED ALWAYS AS (... ) STORED

Stored generated columns compute their value from other columns in the same row
on every `INSERT` and `UPDATE`, then persist that value like any other physical
column.

```sql
CREATE TABLE line_items (
    price INT NOT NULL,
    qty   INT NOT NULL,
    total INT GENERATED ALWAYS AS (price * qty) STORED
);

INSERT INTO line_items (price, qty) VALUES (10, 3);
SELECT price, qty, total FROM line_items;
-- 10 | 3 | 30
```

Rules:

- The expression may reference only existing non-generated columns from the
  same table.
- `DEFAULT`, `ON UPDATE`, and `AUTO_INCREMENT` are not allowed on a generated
  column.
- `STORED` is implemented now. `VIRTUAL` is parsed but returns
  `NotImplemented`.
- `ALTER TABLE ... GENERATED` is deferred to a later rewrite/backfill subphase.

#### Statement-level triggers

AxiomDB supports a bounded validation-trigger MVP for base tables:

```sql
CREATE TRIGGER journal_balanced
AFTER INSERT ON journal
FOR EACH STATEMENT
AS
SELECT 'journal not balanced'
FROM journal
GROUP BY 1
HAVING SUM(debit) <> SUM(credit);
```

Supported DDL:

```sql
CREATE TRIGGER trg_name
AFTER INSERT|UPDATE|DELETE
ON table_name
FOR EACH STATEMENT
AS SELECT ...;

DROP TRIGGER trg_name ON table_name;
SHOW CREATE TRIGGER trg_name ON table_name;
```

Behavior:

- The trigger fires once after the whole DML statement, not once per row.
- The trigger body must be a single read-only `SELECT`.
- If that `SELECT` returns any row, the outer statement fails and is rolled
  back under normal statement-rollback semantics.
- Trigger bodies may use `@@trigger_name`, `@@trigger_table`,
  `@@trigger_event`, and `@@trigger_row_count`.

Current limits:

- Only `AFTER ... FOR EACH STATEMENT` is implemented.
- `BEFORE`, `FOR EACH ROW`, `WHEN`, `SIGNAL`, transition tables, recursive
  triggers, and procedural bodies are deferred.
- Triggers are supported only on base tables, not views or materialized views.

#### PRIMARY KEY

Declares a column (or set of columns) as the primary key. A primary key:
- Implies `NOT NULL`
- Creates a unique B+ Tree index automatically
- Is used for `REFERENCES` in foreign keys

```sql
-- Single-column primary key
CREATE TABLE users (
    id   BIGINT PRIMARY KEY AUTO_INCREMENT,
    name TEXT   NOT NULL
);

-- Composite primary key (declared as table constraint)
CREATE TABLE order_items (
    order_id   BIGINT NOT NULL,
    product_id BIGINT NOT NULL,
    quantity   INT    NOT NULL,
    PRIMARY KEY (order_id, product_id)
);
```

#### UNIQUE

Guarantees no two rows share the same value in this column (or set of columns).
NULL values are excluded from uniqueness checks — multiple NULLs are allowed.

```sql
CREATE TABLE accounts (
    id       BIGINT PRIMARY KEY AUTO_INCREMENT,
    email    TEXT   NOT NULL UNIQUE,
    username TEXT   NOT NULL UNIQUE
);
```

#### AUTO_INCREMENT / SERIAL

Automatically generates a monotonically increasing integer for each new row.
The counter starts at 1 and increments by 1 for each inserted row. The following
forms are all equivalent:

```sql
-- MySQL-style
id BIGINT PRIMARY KEY AUTO_INCREMENT

-- PostgreSQL-style shorthand (SERIAL = INT AUTO_INCREMENT, BIGSERIAL = BIGINT AUTO_INCREMENT)
id SERIAL    PRIMARY KEY
id BIGSERIAL PRIMARY KEY
```

**Behavior:**

```sql
CREATE TABLE users (
    id   BIGINT PRIMARY KEY AUTO_INCREMENT,
    name TEXT   NOT NULL
);

-- Omit the AUTO_INCREMENT column — the engine generates the value
INSERT INTO users (name) VALUES ('Alice');   -- id = 1
INSERT INTO users (name) VALUES ('Bob');     -- id = 2

-- Retrieve the last generated ID (current session only)
SELECT LAST_INSERT_ID();   -- returns 2
SELECT lastval();          -- PostgreSQL alias — same result

-- Multi-row INSERT: LAST_INSERT_ID() returns the ID of the FIRST row in the batch
INSERT INTO users (name) VALUES ('Carol'), ('Dave');  -- ids: 3, 4
SELECT LAST_INSERT_ID();   -- returns 3

-- Explicit non-NULL value bypasses the sequence and does NOT advance it
INSERT INTO users (id, name) VALUES (100, 'Eve');
-- id=100; sequence remains at 4; next auto id will be 5
```

`LAST_INSERT_ID()` returns `0` if no auto-increment INSERT has been performed
in the current session. See [LAST_INSERT_ID() in expressions](expressions.md#session-functions)
for the full function reference.

**TRUNCATE resets the counter:**

```sql
TRUNCATE TABLE users;
INSERT INTO users (name) VALUES ('Frank');  -- id = 1 (reset by TRUNCATE)
```

#### REFERENCES — Foreign Keys

Declares a foreign key relationship to another table's primary key.

```sql
CREATE TABLE orders (
    id         BIGINT PRIMARY KEY AUTO_INCREMENT,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    product_id BIGINT NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    placed_at  TIMESTAMP NOT NULL
);
```

**ON DELETE actions:**

| Action     | Behavior when the referenced row is deleted             |
|------------|---------------------------------------------------------|
| `RESTRICT` | Reject the DELETE if any referencing row exists (default) |
| `CASCADE`  | Delete all referencing rows automatically               |
| `SET NULL` | Set the foreign key column to NULL                      |
| `SET DEFAULT` | Set the foreign key column to its DEFAULT value      |
| `NO ACTION`| Same as RESTRICT but deferred to end of statement       |

**ON UPDATE actions:** Same options as ON DELETE — apply when the referenced primary
key is updated.

> **Current limitation:** Only `ON UPDATE RESTRICT` (the default) is enforced.
> `ON UPDATE CASCADE` and `ON UPDATE SET NULL` return `NotImplemented` and are
> planned for Phase 6.10. Write `ON UPDATE RESTRICT` or omit the clause entirely
> for correct behaviour today.

```sql
CREATE TABLE order_items (
    id         BIGINT PRIMARY KEY AUTO_INCREMENT,
    order_id   BIGINT NOT NULL
        REFERENCES orders(id)
        ON DELETE CASCADE
        ON UPDATE CASCADE,
    product_id BIGINT NOT NULL
        REFERENCES products(id)
        ON DELETE RESTRICT
        ON UPDATE RESTRICT,
    quantity   INT    NOT NULL,
    unit_price DECIMAL NOT NULL
);
```

#### CHECK

Validates that a condition is TRUE for every row. A row where the CHECK condition
evaluates to FALSE or NULL is rejected.

```sql
CREATE TABLE products (
    id     BIGINT  PRIMARY KEY AUTO_INCREMENT,
    name   TEXT    NOT NULL,
    price  DECIMAL NOT NULL CHECK (price > 0),
    stock  INT     NOT NULL CHECK (stock >= 0),
    rating REAL    CHECK (rating IS NULL OR (rating >= 1.0 AND rating <= 5.0))
);
```

### Table-Level Constraints

Table constraints apply to multiple columns and are declared after all column definitions.

```sql
CREATE TABLE shipments (
    id           BIGINT    PRIMARY KEY AUTO_INCREMENT,
    order_id     BIGINT    NOT NULL,
    warehouse_id INT       NOT NULL,
    shipped_at   TIMESTAMP,
    delivered_at TIMESTAMP,

    -- Named constraints (recommended for meaningful error messages)
    CONSTRAINT fk_shipment_order
        FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE,

    CONSTRAINT chk_delivery_after_shipment
        CHECK (delivered_at IS NULL OR delivered_at >= shipped_at),

    CONSTRAINT uq_one_active_shipment
        UNIQUE (order_id, warehouse_id)
);
```

### IF NOT EXISTS

Suppresses the error when the table already exists. Useful in migration scripts.

```sql
CREATE TABLE IF NOT EXISTS config (
    key   TEXT NOT NULL UNIQUE,
    value TEXT NOT NULL
);
```

### Full Example — E-commerce Schema

```sql
CREATE TABLE users (
    id         BIGINT      PRIMARY KEY AUTO_INCREMENT,
    email      TEXT        NOT NULL UNIQUE,
    name       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMPTZ
);

CREATE TABLE categories (
    id   INT  PRIMARY KEY AUTO_INCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE products (
    id          BIGINT      PRIMARY KEY AUTO_INCREMENT,
    category_id INT         NOT NULL REFERENCES categories(id),
    name        TEXT        NOT NULL,
    description TEXT,
    price       DECIMAL     NOT NULL CHECK (price > 0),
    stock       INT         NOT NULL DEFAULT 0 CHECK (stock >= 0),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE orders (
    id          BIGINT      PRIMARY KEY AUTO_INCREMENT,
    user_id     BIGINT      NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    total       DECIMAL     NOT NULL CHECK (total >= 0),
    status      TEXT        NOT NULL DEFAULT 'pending',
    placed_at   TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    shipped_at  TIMESTAMPTZ,
    CONSTRAINT chk_order_status CHECK (
        status IN ('pending', 'paid', 'shipped', 'delivered', 'cancelled')
    )
);

CREATE TABLE order_items (
    order_id   BIGINT  NOT NULL REFERENCES orders(id)   ON DELETE CASCADE,
    product_id BIGINT  NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    quantity   INT     NOT NULL CHECK (quantity > 0),
    unit_price DECIMAL NOT NULL CHECK (unit_price > 0),
    PRIMARY KEY (order_id, product_id)
);
```

### CREATE TABLE LIKE

Copies the schema of an existing table into a new empty table. Column definitions,
constraints, and indexes are all replicated — but no rows are copied.

```sql
CREATE TABLE staging LIKE production_orders;

CREATE TABLE IF NOT EXISTS archive_users LIKE users;
```

The new table is independent of the source. Changes to the source schema (via
`ALTER TABLE`) do not affect the copy, and vice versa.

**What is copied:**
- All columns (name, type, nullability, default, auto_increment, generated metadata)
- All constraints (PRIMARY KEY, UNIQUE, CHECK, FOREIGN KEY)
- All secondary indexes (with fresh empty B-tree roots)

**What is not copied:** rows, AUTO_INCREMENT counter state, table-level comments.

### CREATE TABLE AS SELECT (CTAS)

Creates a new heap table and populates it from a `SELECT` query in one statement.

```sql
CREATE TABLE cheap_products AS
SELECT id, name, price
FROM products
WHERE price < 50;

-- With AS keyword (optional)
CREATE TABLE report_2024 AS
SELECT user_id, SUM(total) AS revenue
FROM orders
WHERE YEAR(placed_at) = 2024
GROUP BY user_id;
```

**Column types** are inferred from the first non-NULL value in each column of the
result set. If the first row is all-NULL for a column, AxiomDB falls back to `TEXT`.
Column names come from the SELECT projection aliases (or the raw expression text if
no alias is given).

The resulting table is always a **heap table** regardless of whether the source
table is clustered. To create a clustered copy, add a `PRIMARY KEY` and use
`ALTER TABLE ... REBUILD` or declare the PK inline:

```sql
-- Heap CTAS first, then promote to clustered
CREATE TABLE copy AS SELECT * FROM src;
ALTER TABLE copy ADD CONSTRAINT PRIMARY KEY (id);
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — CTAS always produces heap tables</span>
CTAS infers column types from result values, not from column metadata — so the
output schema is not known until the SELECT runs. Deciding whether to build a
clustered tree would require a two-pass approach (run SELECT, build schema, then
rebuild as clustered). AxiomDB uses a single-pass approach: heap first, clustered
on explicit request. This matches SQLite's behavior and avoids a hidden O(N log N)
sort on every CTAS.
</div>
</div>

---

## CREATE INDEX

Indexes accelerate lookups and range scans. AxiomDB automatically creates a unique B+
Tree index for every PRIMARY KEY and UNIQUE constraint. Additional indexes are created
explicitly. `CREATE INDEX` works on both heap tables and clustered (PRIMARY KEY) tables.

### Basic Syntax

```sql
CREATE [UNIQUE] INDEX [IF NOT EXISTS] index_name
ON table_name (column [ASC|DESC], ...)
[WITH (fillfactor = N)]
[WHERE condition];
```

`fillfactor` controls how full a B-Tree leaf page gets before splitting (10–100,
default 90). Lower values leave room for future inserts without triggering splits.
See [Fill Factor](../features/indexes.md#fill-factor) for details.

### Examples

```sql
-- Standard index
CREATE INDEX idx_users_email ON users (email);

-- Composite index: queries filtering by (user_id, placed_at) benefit
CREATE INDEX idx_orders_user_date ON orders (user_id, placed_at DESC);

-- Unique index (equivalent to UNIQUE column constraint)
CREATE UNIQUE INDEX uq_products_sku ON products (sku);

-- Partial index: index only active products (reduces index size)
CREATE INDEX idx_active_products ON products (category_id)
WHERE deleted_at IS NULL;

-- Fill factor: append-heavy time-series table (leaves 30% free for inserts)
CREATE INDEX idx_ts ON events(created_at) WITH (fillfactor = 70);

-- Fill factor + partial index combined
CREATE UNIQUE INDEX uq_active_email ON users(email)
WHERE deleted_at IS NULL
-- WITH clause can appear before or after WHERE (both are accepted)
```

### When to Add an Index

- Columns appearing in `WHERE`, `JOIN ON`, or `ORDER BY` clauses on large tables
- Foreign key columns (AxiomDB does not auto-index FK columns — add them explicitly)
- Columns used in range queries (`BETWEEN`, `>`, `<`)

See [Indexes](../features/indexes.md) for the query planner interaction and composite
index column ordering rules.

---

## DROP TABLE

Removes a table and all its data permanently.

```sql
DROP TABLE [IF EXISTS] table_name [CASCADE | RESTRICT];
```

| Option     | Behavior                                                        |
|------------|-----------------------------------------------------------------|
| `RESTRICT` | Fail if any other table has a foreign key referencing this table (default) |
| `CASCADE`  | Also drop all foreign key constraints that reference this table  |

```sql
-- Safe drop: fails if referenced by other tables
DROP TABLE products;

-- Drop without error if already gone
DROP TABLE IF EXISTS temp_import;

-- Drop even if referenced (removes FK constraints first)
DROP TABLE categories CASCADE;
```

> Dropping a table is immediate and permanent. There is no RECYCLE BIN. Make sure
> you have a backup or are inside a transaction if you need to recover.

---

## DROP INDEX

Removes an index. The table and its data are not affected.

```sql
DROP INDEX [IF EXISTS] index_name;
```

```sql
DROP INDEX idx_users_email;
DROP INDEX IF EXISTS idx_old_lookup;
```

---

## ALTER TABLE

Modifies the structure of an existing table. All four forms are blocking
operations — no concurrent DDL is allowed while an ALTER TABLE is in progress.

### Add Column

Adds a new column at the end of the column list. If existing rows are present,
they are rewritten to include the default value for the new column. If no
`DEFAULT` clause is given, existing rows receive `NULL` for that column.

```sql
ALTER TABLE table_name ADD COLUMN column_name data_type [NOT NULL] [DEFAULT expr];
```

```sql
-- Add a nullable column (existing rows get NULL)
ALTER TABLE users ADD COLUMN phone TEXT;

-- Add a NOT NULL column with a default (existing rows get 0)
ALTER TABLE orders ADD COLUMN priority INT NOT NULL DEFAULT 0;

-- Add a column with a string default
ALTER TABLE products ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
```

> A column with `NOT NULL` and no `DEFAULT` cannot be added to a non-empty
> table — existing rows would have no value to fill in and would violate the
> constraint. Provide a `DEFAULT` value, or add the column as nullable first
> and back-fill the data before adding the constraint.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Row Rewriting on Schema Change</span>
AxiomDB rows are stored positionally: each row is a packed binary blob where
values are addressed by column index, not by name. The null bitmap and value
offsets are fixed at write time according to the schema that was active when
the row was inserted. When a column is added or dropped, the column count
changes and all existing rows must be rewritten to match the new layout.
This is the same approach used by SQLite for its "full table rewrite" DDL path.
Rename operations (RENAME COLUMN, RENAME TO) touch only the catalog — no rows
are rewritten because column positions do not change.
</div>
</div>

### Drop Column

Removes a column from the table. All existing rows are rewritten without the
dropped column's value. The column name must exist unless `IF EXISTS` is used.

```sql
ALTER TABLE table_name DROP COLUMN column_name [IF EXISTS];
```

```sql
-- Remove a column (fails if the column does not exist)
ALTER TABLE users DROP COLUMN phone;

-- Remove a column only if it exists (idempotent, safe in migrations)
ALTER TABLE users DROP COLUMN phone IF EXISTS;
```

> Dropping a column is permanent. The data stored in that column is discarded
> when rows are rewritten and cannot be recovered without a backup.

Secondary-index behavior:

- secondary indexes that reference the dropped column through key columns,
  `INCLUDE` columns, or a partial-index predicate are dropped automatically
- on heap tables, surviving secondary indexes are rebuilt automatically because
  the row rewrite assigns new physical `RecordId`s
- dropping a column still fails if it participates in the PRIMARY KEY, a
  FOREIGN KEY definition, or a CHECK constraint

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Rebuild Heap Survivors</span>
Heap secondary indexes bookmark physical <code>RecordId</code>s, so any heap row
rewrite invalidates even unrelated secondary entries. Clustered tables keep
secondary bookmarks on PRIMARY KEY bytes instead, so AxiomDB only pays the full
survivor rebuild cost on heap layouts.
</div>
</div>

### Modify Column

Changes the data type or nullability of an existing column. All existing rows
are rewritten, coercing their stored values to the new type.

```sql
ALTER TABLE table_name MODIFY COLUMN column_name new_type [NOT NULL];
```

```sql
-- Widen an integer column to 64 bits (existing values preserved)
ALTER TABLE events MODIFY COLUMN count BIGINT;

-- Convert integers to text (always safe, values become their decimal string)
ALTER TABLE codes MODIFY COLUMN code TEXT;

-- Add a NOT NULL constraint (fails if any row has NULL in that column)
ALTER TABLE orders MODIFY COLUMN status TEXT NOT NULL;
```

**Rules and restrictions:**

- Narrowing casts (e.g. `BIGINT → INT`, `TEXT → INT`) are applied with strict
  coercion. If any existing value cannot be represented in the new type the
  statement fails and no rows are changed.
- secondary indexes are repaired automatically after the rewrite:
  - heap tables rebuild all secondary indexes so their bookmarks stay valid
  - clustered tables rebuild only indexes whose definition depends on the
    modified column
- modifying a PRIMARY KEY, FOREIGN KEY, or CHECK-dependent column is still
  rejected explicitly
- Changing nullability from nullable to `NOT NULL` is allowed only when every
  existing row has a non-NULL value for that column.

### Add Primary Key

Promotes an existing heap table to clustered storage by installing a new
primary key.

```sql
ALTER TABLE table_name ADD [CONSTRAINT name] PRIMARY KEY (column_name, ...);
```

```sql
CREATE TABLE users (id INT, email TEXT);
INSERT INTO users VALUES (1, 'alice@example.com'), (2, 'bob@example.com');

ALTER TABLE users ADD PRIMARY KEY (id);
```

Behavior:

- validates that every referenced column exists
- rejects the ALTER if any existing row has `NULL` in a key column
- rejects the ALTER if existing rows contain duplicate key tuples
- marks the primary-key columns as `NOT NULL`
- rewrites the heap table into clustered storage ordered by the new key
- rebuilds existing secondary indexes so they store clustered PK bookmarks

This form is supported for heap tables that do not already have a primary key.
`DROP PRIMARY KEY` and replacing an existing clustered primary key remain
deferred.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Migration Advantage</span>
Unlike SQLite-style migration flows that require <code>CREATE TABLE + INSERT + RENAME</code>,
AxiomDB can promote a populated heap table to clustered storage in one
<code>ALTER TABLE ... ADD PRIMARY KEY</code> statement while preserving existing
secondary-index behavior.
</div>
</div>

### Rename Column

Renames an existing column. This is a catalog-only operation — no rows are
rewritten because the positional encoding is not affected by column names.

```sql
ALTER TABLE table_name RENAME COLUMN old_name TO new_name;
```

```sql
-- Rename a column
ALTER TABLE users RENAME COLUMN full_name TO display_name;

-- Rename to fix a typo
ALTER TABLE orders RENAME COLUMN shiped_at TO shipped_at;
```

### Rename Table

Renames the table itself. This is a catalog-only operation.

```sql
ALTER TABLE old_name RENAME TO new_name;
```

```sql
-- Rename during a refactoring
ALTER TABLE user_profiles RENAME TO profiles;

-- Rename a staging table after a migration
ALTER TABLE orders_import RENAME TO orders;
```

### Rebuild To Clustered

Migrates a **legacy heap table that already has PRIMARY KEY metadata** into
clustered storage.

```sql
ALTER TABLE table_name REBUILD;
```

Example:

```sql
-- After opening an older AxiomDB database where `users` is still heap-backed
ALTER TABLE users REBUILD;
```

Behavior:

- walks the existing PRIMARY KEY index in logical key order
- rebuilds the table into a clustered PRIMARY KEY tree
- rebuilds every non-primary index so it stores clustered PK bookmarks instead
  of heap `RecordId`s
- swaps the catalog metadata atomically at the end of the statement

Common errors:

```sql
ALTER TABLE logs REBUILD;
-- ERROR 1105 (HY000): ALTER TABLE REBUILD requires a PRIMARY KEY on 'logs'
```

```sql
ALTER TABLE users REBUILD;
-- ERROR 1105 (HY000): table 'users' is already clustered
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
The rebuild path follows PostgreSQL <code>CLUSTER</code> and InnoDB sorted-rebuild ideas: build the new clustered roots first, then swap catalog metadata. AxiomDB adds deferred free of the old heap/index pages so the metadata swap never races with page reclamation.
</div>
</div>

### Not Yet Supported

The following ALTER TABLE forms are planned for later phases:

- `ADD CONSTRAINT` / `DROP CONSTRAINT` for multi-column foreign keys
- `DROP PRIMARY KEY` / replacing one primary key with another
- dropping or modifying a PRIMARY KEY / FOREIGN KEY / CHECK-dependent column
- Non-blocking `ALTER TABLE` (zero-downtime schema migration via shadow table + WAL delta)

---

## TRUNCATE TABLE

Removes all rows from a table without dropping its structure, and resets the
`AUTO_INCREMENT` counter to 1. The table schema, indexes, and constraints are
preserved.

```sql
TRUNCATE TABLE table_name;
```

```sql
-- Wipe a staging table before re-importing
TRUNCATE TABLE import_staging;

-- AUTO_INCREMENT is always reset after TRUNCATE
CREATE TABLE log_events (id INT AUTO_INCREMENT PRIMARY KEY, msg TEXT);
INSERT INTO log_events (msg) VALUES ('start'), ('end');  -- ids: 1, 2
TRUNCATE TABLE log_events;
INSERT INTO log_events (msg) VALUES ('restart');          -- id: 1
```

Returns `Affected { count: 0 }` (MySQL convention). See also
[TRUNCATE TABLE in the DML reference](dml.md#truncate-table) for a comparison
with `DELETE FROM table`.


---

## ANALYZE

Refreshes per-column statistics used by the query planner to choose between
an index scan and a full table scan.

```sql
ANALYZE;                          -- all tables in the current schema
ANALYZE TABLE table_name;         -- specific table, all indexed columns
ANALYZE TABLE table_name (col);   -- specific table, one column only
```

`ANALYZE` computes exact `row_count` and NDV (number of distinct non-NULL
values) for each target column by scanning the full table. Results are stored
in the `axiom_stats` system catalog and are immediately available to the planner.

```sql
-- After a bulk import, refresh stats so the planner uses correct selectivity:
INSERT INTO products SELECT * FROM products_staging;
ANALYZE TABLE products;

-- Check a single column after targeted inserts:
ANALYZE TABLE orders (status);
```

See [Index Statistics](../features/indexes.md#index-statistics-and-query-planner)
for how NDV and row_count affect query planning decisions.

---

## SHOW INDEX

Lists all indexes defined on a table. `SHOW INDEXES` and `SHOW KEYS` are
synonyms recognized by MySQL clients and ORMs.

```sql
SHOW INDEX   FROM table_name;
SHOW INDEXES FROM table_name;
SHOW KEYS    FROM table_name;
```

The result set matches MySQL's `SHOW INDEX` column layout:

| Column | Type | Description |
|--------|------|-------------|
| `Table` | TEXT | Table name |
| `Non_unique` | INT | `0` for PRIMARY / UNIQUE, `1` otherwise |
| `Key_name` | TEXT | `"PRIMARY"` for the PK; index name otherwise |
| `Seq_in_index` | INT | 1-based column position within the index |
| `Column_name` | TEXT | Name of the indexed column |
| `Collation` | TEXT | `"A"` (ascending) |
| `Cardinality` | INT | Estimated distinct values (0 until `ANALYZE` is run) |
| `Sub_part` | TEXT | `NULL` (prefix indexes not yet supported) |
| `Packed` | TEXT | `NULL` |
| `Null` | TEXT | `"YES"` if the column is nullable, `""` otherwise |
| `Index_type` | TEXT | Always `"BTREE"` |
| `Comment` | TEXT | `""` |
| `Index_comment` | TEXT | `""` |
| `Visible` | TEXT | Always `"YES"` |

```sql
CREATE TABLE orders (
    id       INT  PRIMARY KEY,
    email    TEXT UNIQUE,
    status   TEXT
);
CREATE INDEX idx_status ON orders (status);

SHOW INDEX FROM orders;
```

Example output:

| Table  | Non_unique | Key_name   | Seq_in_index | Column_name | … |
|--------|-----------|------------|--------------|-------------|---|
| orders | 0         | PRIMARY    | 1            | id          | … |
| orders | 0         | idx_email  | 1            | email       | … |
| orders | 1         | idx_status | 1            | status      | … |

---

## SHOW TABLES / SHOW FULL TABLES

Lists tables in the current (or specified) schema.

```sql
SHOW TABLES [FROM schema] [LIKE 'pattern'];
SHOW FULL TABLES [FROM schema] [LIKE 'pattern'];
```

`SHOW FULL TABLES` adds a `Table_type` column, required by Sequelize, ActiveRecord,
and other ORMs that probe the schema on startup.

| Column | SHOW TABLES | SHOW FULL TABLES |
|--------|:-----------:|:----------------:|
| `Tables_in_<schema>` | ✓ | ✓ |
| `Table_type` | — | ✓ (always `"BASE TABLE"`) |

```sql
CREATE TABLE products (id INT, name TEXT);
CREATE TABLE orders   (id INT, total REAL);

SHOW FULL TABLES;
-- Tables_in_public  Table_type
-- products          BASE TABLE
-- orders            BASE TABLE
```

---

## SHOW COLUMNS / SHOW FULL COLUMNS

Lists column metadata for a table. `DESCRIBE` and `DESC` are synonyms.

```sql
SHOW COLUMNS      FROM table_name;
SHOW FULL COLUMNS FROM table_name;
DESCRIBE table_name;
```

`SHOW FULL COLUMNS` adds three extra columns required by Prisma, TypeORM, and
MySQL Workbench:

| Column | SHOW COLUMNS | SHOW FULL COLUMNS |
|--------|:------------:|:-----------------:|
| `Field` | ✓ | ✓ |
| `Type` | ✓ | ✓ |
| `Null` | ✓ | ✓ |
| `Key` | ✓ | ✓ |
| `Default` | ✓ | ✓ |
| `Extra` | ✓ | ✓ |
| `Collation` | — | ✓ (`utf8mb4_general_ci` for text, `NULL` for numeric) |
| `Privileges` | — | ✓ (`select,insert,update,references`) |
| `Comment` | — | ✓ (always `""`) |

---

## SHOW TABLE STATUS

Returns one row per table with storage metadata, compatible with MySQL's
`SHOW TABLE STATUS` output.

```sql
SHOW TABLE STATUS [FROM schema] [LIKE 'pattern'];
```

The result set has 18 columns:

| Column | Description |
|--------|-------------|
| `Name` | Table name |
| `Engine` | Always `"InnoDB"` |
| `Version` | Always `10` |
| `Row_format` | Always `"Dynamic"` |
| `Rows` | Approximate row count from last `ANALYZE` |
| `Avg_row_length` | `0` (not tracked) |
| `Data_length` | `0` (not tracked) |
| `Max_data_length` | `0` |
| `Index_length` | `0` |
| `Data_free` | `0` |
| `Auto_increment` | `NULL` |
| `Create_time` | `NULL` |
| `Update_time` | `NULL` |
| `Check_time` | `NULL` |
| `Collation` | Always `"utf8mb4_general_ci"` |
| `Checksum` | `NULL` |
| `Create_options` | `""` |
| `Comment` | `""` |

```sql
SHOW TABLE STATUS LIKE 'order%';
-- Returns rows for all tables whose names start with "order".
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
Row counts come from the stats catalog populated by <code>ANALYZE TABLE</code>,
not from live heap scans, keeping <code>SHOW TABLE STATUS</code> O(1) regardless
of table size — the same trade-off MySQL InnoDB makes with its own approximate
row counts.
</div>
</div>

---

## SHOW ENGINES / SHOW CHARSET / SHOW COLLATION

Informational commands used by DB management tools (MySQL Workbench, DBeaver,
TablePlus) and JDBC drivers on connect.

```sql
SHOW ENGINES;
SHOW CHARSET;           -- or SHOW CHARACTER SET
SHOW COLLATION;
```

**SHOW ENGINES** returns a single row:

| Engine | Support | Transactions | XA | Savepoints |
|--------|---------|:------------:|:--:|:----------:|
| InnoDB | DEFAULT | YES | YES | YES |

**SHOW CHARSET** returns four rows: `utf8mb4`, `utf8`, `latin1`, `binary`.

**SHOW COLLATION** returns five rows: `utf8mb4_general_ci`, `utf8mb4_bin`,
`utf8_general_ci`, `latin1_swedish_ci`, `binary`.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip</span>
These commands are read-only. AxiomDB still stores strings as UTF-8 internally,
but collation metadata is no longer purely decorative: session / database /
table / column / query overrides normalize onto the engine's current runtime
collations (`binary` / `es`) and do affect text comparison behavior.
</div>
</div>

## SHOW WARNINGS / SHOW ERRORS

Returns the warning or error list from the current session.

```sql
SHOW WARNINGS [LIMIT N];
SHOW ERRORS  [LIMIT N];
```

MySQL connectors (`JDBC`, `MySQL Connector/Python`, `mysqlclient`) issue
`SHOW WARNINGS` automatically after every DML statement. AxiomDB returns a
three-column result set that these clients expect:

| Column  | Type | Description                                   |
|---------|------|-----------------------------------------------|
| Level   | TEXT | `Note`, `Warning`, or `Error`                 |
| Code    | INT  | MySQL error number                            |
| Message | TEXT | Human-readable description                    |

`SHOW ERRORS` is identical but only returns rows whose `Level` is `Error`.

`LIMIT N` restricts the result to the first N rows.

An empty result set (zero rows) is valid and means there are no outstanding
warnings or errors in the current session.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Connector Compatibility</span>
MySQL Connector/Python and JDBC issue <code>SHOW WARNINGS</code> after every
statement by default. Returning the correct three-column result set (not an
error or an OK packet) is required for these connectors to work without
warnings or connection drops.
</div>
</div>

---

## CREATE AGGREGATE

Define a custom aggregate function backed by a registered internal helper.

```sql
CREATE AGGREGATE name ( arg_type [, ...] ) (
  SFUNC  = transition_function,
  STYPE  = state_type,
  FINALFUNC = final_function
);
```

| Parameter | Description |
|---|---|
| `name` | Name of the new aggregate |
| `arg_type` | Input column type (e.g. `FLOAT`, `INT`) |
| `SFUNC` | Transition function called once per row |
| `STYPE` | Internal state type |
| `FINALFUNC` | Optional finalization function called once per group |

**Example — median:**

```sql
CREATE AGGREGATE median(FLOAT) (
  SFUNC     = median_state,
  STYPE     = FLOAT[],
  FINALFUNC = median_final
);

CREATE TABLE latency (service TEXT, ms FLOAT);
INSERT INTO latency VALUES ('api', 10.0), ('api', 50.0), ('api', 30.0);

SELECT service, median(ms) AS p50 FROM latency GROUP BY service;
-- service | p50
-- api     | 30.0
```

**Supported helpers (current registry):**

| SFUNC | STYPE | FINALFUNC | Result |
|---|---|---|---|
| `median_state` | `FLOAT[]` | `median_final` | exact median via sort |

<div class="callout callout-design">
<span class="callout-icon">🔧</span>
<div class="callout-body">
<span class="callout-label">Bounded registry</span>
<code>SFUNC</code> and <code>FINALFUNC</code> names are not arbitrary SQL functions —
they are validated against an internal Rust registry at <code>CREATE AGGREGATE</code>
time. This avoids the need for a generic <code>CREATE FUNCTION</code> runtime while
still giving real catalog-backed DDL semantics. Future registry entries can be
added without changing the DDL syntax.
</div>
</div>

**Error cases:**

| Situation | Error |
|---|---|
| Unknown SFUNC/FINALFUNC combination | `InvalidValue` |
| Duplicate aggregate signature | `InvalidValue` — already exists |
| Wrong invocation arity | signature mismatch error |

---

## CREATE SEQUENCE

Create a standalone BIGINT sequence object.

```sql
CREATE SEQUENCE name;
CREATE SEQUENCE IF NOT EXISTS name;
CREATE SEQUENCE name START WITH 10 INCREMENT BY 5 MINVALUE 1 MAXVALUE 100 NO CYCLE CACHE 1;
DROP SEQUENCE name;
DROP SEQUENCE IF EXISTS name;
```

`NEXTVAL('name')` returns the next value and advances the sequence. `CURRVAL('name')`
returns the last value produced by `NEXTVAL` in the current session.

Defaults:

| Option | Default |
|---|---|
| `START WITH` | `1` |
| `INCREMENT BY` | `1` |
| `MINVALUE` | `1` |
| `MAXVALUE` | `9223372036854775807` |
| `CYCLE` | `NO CYCLE` |
| `CACHE` | `1` |

Sequence advancement is not rolled back. If a transaction calls `NEXTVAL` and
then rolls back, that value remains consumed and the next call returns the next
number.

| Situation | Error |
|---|---|
| Duplicate sequence | `InvalidValue` — already exists |
| `CURRVAL` before this session calls `NEXTVAL` | `InvalidValue` |
| Exhausted `NO CYCLE` sequence | `InvalidValue` |
| `INCREMENT BY 0` or invalid bounds | `InvalidValue` |

---

## DROP AGGREGATE

Remove a custom aggregate definition.

```sql
DROP AGGREGATE name ( arg_type [, ...] );
```

Removes the aggregate from the catalog. Built-in aggregates (`SUM`, `COUNT`, etc.) cannot be dropped.

```sql
DROP AGGREGATE median(FLOAT);
```

---

## CREATE VIEW

Creates a named view — a stored query that can be referenced like a table.

### Syntax

```sql
CREATE [OR REPLACE] VIEW view_name AS select_statement;
```

### Examples

```sql
CREATE TABLE orders (id INT, user_id INT, amount INT, status TEXT);
INSERT INTO orders VALUES (1, 10, 100, 'active'), (2, 10, 200, 'active'), (3, 11, 50, 'cancelled');

-- Simple filtered view
CREATE VIEW active_orders AS
    SELECT id, user_id, amount FROM orders WHERE status = 'active';

-- Query through the view
SELECT id, amount FROM active_orders ORDER BY id;
-- 1  100
-- 2  200

-- Aggregating view
CREATE VIEW user_totals AS
    SELECT user_id, SUM(amount) AS total FROM orders GROUP BY user_id;

SELECT user_id, total FROM user_totals ORDER BY user_id;
-- 10  300
-- 11  50
```

Views expand transparently at query time — no physical rows are stored.

### OR REPLACE

```sql
CREATE OR REPLACE VIEW active_orders AS
    SELECT id, user_id, amount FROM orders WHERE status = 'active' ORDER BY amount;
```

Replaces the existing view definition without changing its catalog identity. Fails if the name belongs to a base table rather than a view.

### Views in JOIN

Views can be used in `JOIN` clauses just like base tables:

```sql
CREATE TABLE users (id INT, name TEXT);
CREATE VIEW alice_orders AS SELECT id, amount FROM orders WHERE user_id = 10;

SELECT u.name, o.amount
FROM alice_orders o
JOIN users u ON u.id = 10
ORDER BY o.amount;
```

### Nested views

Views can reference other views. Circular references are detected and return an error.

```sql
CREATE VIEW base_v AS SELECT id FROM orders WHERE status = 'active';
CREATE VIEW nested_v AS SELECT id FROM base_v WHERE id > 1;

SELECT id FROM nested_v;
```

### Limitations (Phase 20.1)

- **Read-only** — views are not updatable (INSERT/UPDATE/DELETE through a view is not yet supported).
- `WITH CHECK OPTION` is accepted syntactically but not enforced.
- Column-alias list `CREATE VIEW v (a, b) AS SELECT ...` is stored but column renaming is not applied at query time yet.

---

## DROP VIEW

Removes one or more views from the catalog.

### Syntax

```sql
DROP VIEW [IF EXISTS] view_name [, view_name, ...];
```

### Examples

```sql
DROP VIEW active_orders;

-- Multiple views at once
DROP VIEW active_orders, user_totals;

-- No error if the view does not exist
DROP VIEW IF EXISTS unknown_view;
```

`DROP VIEW` fails if the name refers to a base table rather than a view.

---

## SHOW CREATE VIEW

Returns the DDL statement that recreates a view.

```sql
SHOW CREATE VIEW active_orders;
```

| View | Create View |
|------|-------------|
| active_orders | CREATE VIEW \`active_orders\` AS SELECT id, user_id, amount FROM orders WHERE status = 'active' |

`SHOW CREATE VIEW` fails if the name refers to a base table rather than a view.

---

## CREATE SERVER

Registers an external data source as a foreign server. Required before creating
any `FOREIGN TABLE` that uses that source.

```sql
CREATE SERVER name
  FOREIGN DATA WRAPPER fdw_type
  OPTIONS (key 'value' [, key 'value'] ...);

CREATE SERVER IF NOT EXISTS name
  FOREIGN DATA WRAPPER http
  OPTIONS (url 'http://api.example.com', timeout_ms '5000');
```

### Parameters

| Parameter | Description |
|-----------|-------------|
| `name` | Unique server name in the catalog |
| `FOREIGN DATA WRAPPER fdw_type` | FDW type; currently only `http` is supported |
| `OPTIONS` | Key-value pairs specific to the FDW type |

### HTTP server OPTIONS

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `url` | Yes | — | Base URL of the remote service. Must start with `http://`. |
| `timeout_ms` | No | `10000` | Request timeout in milliseconds. |

---

## DROP SERVER

Removes a registered foreign server.

```sql
DROP SERVER name;
DROP SERVER IF EXISTS name;
```

`DROP SERVER` does not automatically drop foreign tables that reference the server.
Drop those first, or the behavior of any subsequent queries on them is undefined.

---

## CREATE FOREIGN TABLE

Maps a remote data source endpoint to a local table-like schema that can be
queried with `SELECT`.

```sql
CREATE FOREIGN TABLE [IF NOT EXISTS] [schema.]name (
    col_name data_type [NOT NULL],
    ...
)
SERVER server_name
OPTIONS (key 'value' [, key 'value'] ...);
```

### Example

```sql
CREATE SERVER analytics_api FOREIGN DATA WRAPPER http
  OPTIONS (url 'http://analytics.internal', timeout_ms '3000');

CREATE FOREIGN TABLE ft_daily_events (
    event_date  TEXT,
    event_name  TEXT,
    user_id     INT,
    properties  TEXT
)
SERVER analytics_api
OPTIONS (endpoint '/events/daily');

SELECT event_name, COUNT(*) AS cnt
FROM ft_daily_events
GROUP BY event_name
ORDER BY cnt DESC;
```

### HTTP table OPTIONS

| Key | Default | Description |
|-----|---------|-------------|
| `endpoint` | `/` | Path appended to the server URL for GET requests. |

### JSON mapping

The remote endpoint must return a JSON **array of objects**. Each object becomes
one row. Field names are matched case-insensitively to column names. Missing
fields and JSON `null` both produce SQL `NULL`.

Type coercions:

| SQL type | JSON source | Coercion |
|----------|-------------|----------|
| `INT` | number / string / bool | `as_i64` → `i32`; string `parse`; bool → 0/1 |
| `BIGINT` | number / string / bool | `as_i64`; string `parse`; bool → 0/1 |
| `FLOAT` | number / string / bool | `as_f64`; string `parse`; bool → 0.0/1.0 |
| `BOOLEAN` | bool / string / number | `true`/`"true"`/`"1"`/non-zero → true |
| `TEXT` / others | any | `to_string()` (JSON serialized for non-strings) |

---

## DROP FOREIGN TABLE

Removes a foreign table definition from the catalog. The remote data source is
unaffected.

```sql
DROP FOREIGN TABLE name;
DROP FOREIGN TABLE IF EXISTS name;
```

---

## BACKUP DATABASE

Creates a physical backup of the database in AxiomDB's `.axbk` binary format.
The engine checkpoints the WAL before writing, so the backup always reflects a
consistent, crash-safe state.

```sql
-- Full backup
BACKUP DATABASE TO '/path/to/backup.axbk';

-- Incremental backup — only pages that changed since the base backup
BACKUP DATABASE TO '/path/to/inc.axbk'
    INCREMENTAL FROM '/path/to/full.axbk';
```

**Notes:**
- The destination path must not exist; the command fails if it does.
- An incremental backup requires a **full** backup as its base; chaining
  incremental → incremental is not supported.
- Incremental diff is based on page-level CRC32c checksums: only pages whose
  checksum differs from the base are written.
- The base path stored in an incremental file must be ≤ 71 bytes. Use a symlink
  if your path is longer.

**Result column:** `status TEXT` — a human-readable progress message.

---

## RESTORE DATABASE

Reconstructs a database file on disk from a `.axbk` backup file.

```sql
RESTORE DATABASE FROM '/path/to/backup.axbk'
    TO '/path/to/restored.db';
```

- The destination path must not exist.
- If `backup.axbk` is an incremental backup, the engine automatically locates
  and applies the base full backup first, then overlays the delta pages.
- The restored file is a raw page store identical to the original; point
  `axiomdb-server` at it to bring the database online.

**Result column:** `status TEXT` — page count and path information.
