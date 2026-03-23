# Catalog and Schema Introspection

AxiomDB maintains an internal catalog that records all tables, columns, and indexes.
The catalog is persisted on the first page of the database file (the meta page) and is
accessible through system tables, convenience commands, and direct SQL queries.

---

## System Tables

The catalog exposes three system tables in the `nexus` schema. They are always readable
without any special privileges.

### nexus_tables

Contains one row per user-visible table.

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
FROM nexus_tables
ORDER BY schema_name, table_name;
```

### nexus_columns

Contains one row per column, in declaration order.

| Column          | Type    | Description                                              |
|-----------------|---------|----------------------------------------------------------|
| `table_id`      | BIGINT  | Foreign key → `nexus_tables.id`                         |
| `table_name`    | TEXT    | Denormalized table name for convenience                  |
| `col_index`     | INT     | Zero-based position within the table                    |
| `col_name`      | TEXT    | Column name                                              |
| `data_type`     | TEXT    | SQL type name (e.g., `TEXT`, `BIGINT`, `DECIMAL`)       |
| `not_null`      | BOOL    | TRUE if declared NOT NULL                               |
| `default_value` | TEXT    | DEFAULT expression as a string, or NULL if none         |

```sql
-- All columns of the orders table
SELECT col_index, col_name, data_type, not_null, default_value
FROM nexus_columns
WHERE table_name = 'orders'
ORDER BY col_index;
```

### nexus_indexes

Contains one row per index (including automatically generated PK and UNIQUE indexes).

| Column          | Type   | Description                                              |
|-----------------|--------|----------------------------------------------------------|
| `id`            | BIGINT | Internal index identifier                               |
| `table_id`      | BIGINT | Foreign key → `nexus_tables.id`                         |
| `table_name`    | TEXT   | Denormalized table name                                 |
| `index_name`    | TEXT   | Index name                                              |
| `is_unique`     | BOOL   | TRUE for UNIQUE and PRIMARY KEY indexes                 |
| `is_primary`    | BOOL   | TRUE for the PRIMARY KEY index                          |
| `columns`       | TEXT   | Comma-separated list of indexed column names            |
| `root_page_id`  | BIGINT | Page ID of the B+ Tree root for this index              |

```sql
-- All indexes on the products table
SELECT index_name, is_unique, is_primary, columns
FROM nexus_indexes
WHERE table_name = 'products'
ORDER BY is_primary DESC, index_name;
```

---

## Convenience Commands

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
FROM nexus_columns
WHERE not_null = TRUE
ORDER BY table_name, col_index;
```

### Find tables with no indexes

```sql
SELECT t.table_name
FROM nexus_tables t
LEFT JOIN nexus_indexes i ON i.table_id = t.id
WHERE i.id IS NULL
ORDER BY t.table_name;
```

### Find foreign key columns that lack an index

```sql
-- Assumes FK columns follow the naming convention: <table>_id
SELECT c.table_name, c.col_name
FROM nexus_columns c
LEFT JOIN nexus_indexes i
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
FROM nexus_tables
ORDER BY column_count DESC;
```

---

## Catalog Bootstrap

The catalog is bootstrapped on the very first `open()` call. AxiomDB writes the three
system tables (`nexus_tables`, `nexus_columns`, `nexus_indexes`) into the meta page
using a special bootstrap transaction with LSN 0. Subsequent opens detect the
bootstrapped meta page and skip the initialization step.

The bootstrap is idempotent: if AxiomDB crashes during bootstrap, the incomplete
transaction has no COMMIT record in the WAL, so crash recovery discards it and
the next `open()` re-runs the bootstrap from scratch.

---

## Schema Visibility Rules

The default schema is `public`. All tables created without an explicit schema prefix
belong to `public`. System tables live in the `nexus` schema and are always visible.

```sql
-- These are equivalent if the default schema is 'public'
CREATE TABLE users (...);
CREATE TABLE public.users (...);

-- System tables require the nexus. prefix or are accessible without schema
SELECT * FROM nexus_tables;          -- works
SELECT * FROM nexus.nexus_tables;   -- also works
```
