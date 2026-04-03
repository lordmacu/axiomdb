# Catalog and Schema Introspection

AxiomDB maintains an internal catalog that records logical databases, tables,
columns, and indexes. The catalog is persisted in system heaps rooted from the
meta page and is exposed through convenience commands plus catalog-backed SQL
resolution.

---

## Databases

Fresh databases always bootstrap a default logical database named `axiomdb`.
Existing databases created before multi-database support are upgraded lazily on
open and their legacy tables remain owned by `axiomdb`.

```sql
SHOW DATABASES;
```

Example output:

| Database |
|----------|
| axiomdb  |
| analytics |

```sql
CREATE DATABASE analytics;
USE analytics;
SELECT DATABASE();
```

Expected result:

| DATABASE() |
|------------|
| analytics  |

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Legacy Compatibility</span>
Old tables created before <code>CREATE DATABASE</code> existed remain visible under
the default database <code>axiomdb</code>. You do not need to rewrite old table names
just to adopt <code>SHOW DATABASES</code> and <code>USE</code>.
</div>
</div>

## System Tables

The catalog exposes six system tables in the `axiom` schema. They are always readable
without any special privileges.

| Table | Purpose |
|-------|---------|
| `axiom_tables` | One row per user table |
| `axiom_columns` | One row per column |
| `axiom_indexes` | One row per index (logical metadata; clustered PK rows may reuse the table root) |
| `axiom_constraints` | Named CHECK constraints |
| `axiom_foreign_keys` | FK constraint definitions |
| `axiom_stats` | Per-column NDV and row_count for the query planner |

### axiom_tables

Contains one row per user-visible table.

Phase `39.13` adds physical-layout metadata to these rows even though the
introspection surface is still being expanded. The important rule today is:

- explicit `PRIMARY KEY` table → clustered table root
- no explicit `PRIMARY KEY` → heap table root

The catalog now keeps that distinction even before clustered DML is exposed.

| Column         | Type   | Description                                |
|----------------|--------|--------------------------------------------|
| `id`           | BIGINT | Internal table identifier (table_id)       |
| `schema_name`  | TEXT   | Schema name (`public` by default)          |
| `table_name`   | TEXT   | Name of the table                          |
| `column_count` | INT    | Number of columns                          |
| `created_at`   | BIGINT | LSN at which the table was created         |

```sql
-- List all user tables
SELECT schema_name, table_name, column_count
FROM axiom_tables
ORDER BY schema_name, table_name;
```

### axiom_columns

Contains one row per column, in declaration order.

| Column          | Type    | Description                                              |
|-----------------|---------|----------------------------------------------------------|
| `table_id`      | BIGINT  | Foreign key → `axiom_tables.id`                         |
| `table_name`    | TEXT    | Denormalized table name for convenience                  |
| `col_index`     | INT     | Zero-based position within the table                    |
| `col_name`      | TEXT    | Column name                                              |
| `data_type`     | TEXT    | SQL type name (e.g., `TEXT`, `BIGINT`, `DECIMAL`)       |
| `not_null`      | BOOL    | TRUE if declared NOT NULL                               |
| `default_value` | TEXT    | DEFAULT expression as a string, or NULL if none         |

```sql
-- All columns of the orders table
SELECT col_index, col_name, data_type, not_null, default_value
FROM axiom_columns
WHERE table_name = 'orders'
ORDER BY col_index;
```

### axiom_indexes

Contains one row per index (including automatically generated PK and UNIQUE indexes).

| Column          | Type   | Description                                              |
|-----------------|--------|----------------------------------------------------------|
| `id`            | BIGINT | Internal index identifier                               |
| `table_id`      | BIGINT | Foreign key → `axiom_tables.id`                         |
| `table_name`    | TEXT   | Denormalized table name                                 |
| `index_name`    | TEXT   | Index name                                              |
| `is_unique`     | BOOL   | TRUE for UNIQUE and PRIMARY KEY indexes                 |
| `is_primary`    | BOOL   | TRUE for the PRIMARY KEY index                          |
| `columns`       | TEXT   | Comma-separated list of indexed column names            |
| `root_page_id`  | BIGINT | Page ID of the index root; clustered PRIMARY KEY metadata reuses the table root |

```sql
-- All indexes on the products table
SELECT index_name, is_unique, is_primary, columns
FROM axiom_indexes
WHERE table_name = 'products'
ORDER BY is_primary DESC, index_name;
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Clustered PK Metadata</span>
On clustered tables, the PRIMARY KEY row in `axiom_indexes` is logical metadata, not a second heap-era tree. Its `root_page_id` matches the table root and may point to clustered pages instead of classic B+ Tree pages.
</div>
</div>

---

## Convenience Commands

### SHOW DATABASES

Lists all logical databases persisted in the catalog.

```sql
SHOW DATABASES;
```

### USE

Changes the selected database for the current connection. Unqualified table
names are resolved inside that database.

```sql
USE analytics;
SHOW TABLES;
```

If the database does not exist, AxiomDB returns MySQL error `1049`:

```sql
USE missing_db;
-- ERROR 1049 (42000): Unknown database 'missing_db'
```

### SHOW TABLES

Lists all tables in the current schema.

```sql
SHOW TABLES;
```

Example output:

| Table name  |
|-------------|
| accounts    |
| order_items |
| orders      |
| products    |
| users       |

### SHOW TABLES LIKE

Filters by a LIKE pattern.

```sql
SHOW TABLES LIKE 'order%';
```

| Table name  |
|-------------|
| order_items |
| orders      |

### DESCRIBE (or DESC)

Shows the column structure of a table.

```sql
DESCRIBE users;
-- or:
DESC products;
```

Example output:

| Column     | Type    | Null | Key | Default           |
|------------|---------|------|-----|-------------------|
| id         | BIGINT  | NO   | PRI | AUTO_INCREMENT    |
| email      | TEXT    | NO   | UNI |                   |
| name       | TEXT    | NO   |     |                   |
| age        | INT     | YES  |     |                   |
| created_at | TIMESTAMP | NO |     | CURRENT_TIMESTAMP |

---

## Introspection Queries

Because the catalog is exposed as regular tables, you can write arbitrary SQL against
it.

### Find all NOT NULL columns across all tables

```sql
SELECT table_name, col_name, data_type
FROM axiom_columns
WHERE not_null = TRUE
ORDER BY table_name, col_index;
```

### Find tables with no indexes

```sql
SELECT t.table_name
FROM axiom_tables t
LEFT JOIN axiom_indexes i ON i.table_id = t.id
WHERE i.id IS NULL
ORDER BY t.table_name;
```

### Find foreign key columns that lack an index

```sql
-- Assumes FK columns follow the naming convention: <table>_id
SELECT c.table_name, c.col_name
FROM axiom_columns c
LEFT JOIN axiom_indexes i
    ON i.table_id = c.table_id
   AND i.columns LIKE c.col_name || '%'
WHERE c.col_name LIKE '%_id'
  AND c.col_name <> 'id'
  AND i.id IS NULL
ORDER BY c.table_name, c.col_name;
```

### Column count per table

```sql
SELECT table_name, column_count
FROM axiom_tables
ORDER BY column_count DESC;
```

---

## Catalog Bootstrap

The catalog is bootstrapped on the very first `open()` call. AxiomDB allocates the
catalog roots, inserts the default database `axiomdb`, and makes the catalog durable
before the database accepts traffic. Subsequent opens detect the initialized roots
and skip the bootstrap path.

The bootstrap is idempotent: if AxiomDB crashes during bootstrap, the incomplete
transaction has no COMMIT record in the WAL, so crash recovery discards it and
the next `open()` re-runs the bootstrap from scratch.

---

## Schema Visibility Rules

The default schema is `public`. All tables created without an explicit schema prefix
belong to `public`. System tables live in the `axiom` schema and are always visible.

```sql
-- These are equivalent if the default schema is 'public'
CREATE TABLE users (...);
CREATE TABLE public.users (...);

-- System tables require the axiom. prefix or are accessible without schema
SELECT * FROM axiom_tables;          -- works
SELECT * FROM axiom.axiom_tables;   -- also works
```
