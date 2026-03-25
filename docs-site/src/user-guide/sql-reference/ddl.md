# DDL — Schema Definition Language

DDL statements define and modify the structure of the database: tables, columns,
constraints, and indexes. All DDL operations are transactional in AxiomDB — a failed
DDL statement is automatically rolled back.

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

> **Phase 6.5 limitation:** Only `ON UPDATE RESTRICT` (the default) is enforced.
> `ON UPDATE CASCADE` and `ON UPDATE SET NULL` return `NotImplemented` and are
> planned for Phase 6.9. Write `ON UPDATE RESTRICT` or omit the clause entirely
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

---

## CREATE INDEX

Indexes accelerate lookups and range scans. AxiomDB automatically creates a unique B+
Tree index for every PRIMARY KEY and UNIQUE constraint. Additional indexes are created
explicitly.

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

> **Not yet supported:** dropping a column that is part of a PRIMARY KEY,
> UNIQUE constraint, or FOREIGN KEY. These require constraint-aware DDL
> (Phase 4.22b).

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

### Not Yet Supported

The following ALTER TABLE forms are planned for Phase 4.22b and later:

- `MODIFY COLUMN` / `ALTER COLUMN` — changing a column's data type
- `ADD CONSTRAINT` — adding a CHECK, UNIQUE, or FOREIGN KEY after table creation
- `DROP CONSTRAINT` — removing a named constraint
- Dropping columns that participate in a constraint

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
