# DDL — Schema Definition Language

DDL statements define and modify the structure of the database: tables, columns,
constraints, and indexes. All DDL operations are transactional in NexusDB — a failed
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

Automatically generates a monotonically increasing integer. These are equivalent:

```sql
-- MySQL-style (both accepted)
id BIGINT PRIMARY KEY AUTO_INCREMENT

-- PostgreSQL-style shorthand
id SERIAL PRIMARY KEY       -- equivalent to: id INT NOT NULL DEFAULT nextval()
id BIGSERIAL PRIMARY KEY    -- equivalent to: id BIGINT NOT NULL DEFAULT nextval()
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

**ON UPDATE actions:** Same options — apply when the referenced primary key is updated.

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

Indexes accelerate lookups and range scans. NexusDB automatically creates a unique B+
Tree index for every PRIMARY KEY and UNIQUE constraint. Additional indexes are created
explicitly.

### Basic Syntax

```sql
CREATE [UNIQUE] INDEX [IF NOT EXISTS] index_name
ON table_name (column [ASC|DESC], ...)
[WHERE condition];
```

### Examples

```sql
-- Accelerate lookups by email in a large users table
CREATE INDEX idx_users_email ON users (email);

-- Composite index: queries filtering by (user_id, placed_at) benefit
CREATE INDEX idx_orders_user_date ON orders (user_id, placed_at DESC);

-- Unique index (equivalent to UNIQUE column constraint)
CREATE UNIQUE INDEX uq_products_sku ON products (sku);

-- Partial index: index only active products (reduces index size)
CREATE INDEX idx_active_products ON products (category_id)
WHERE deleted_at IS NULL;
```

### When to Add an Index

- Columns appearing in `WHERE`, `JOIN ON`, or `ORDER BY` clauses on large tables
- Foreign key columns (NexusDB does not auto-index FK columns — add them explicitly)
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

Modifies the structure of an existing table.

### Add Column

```sql
ALTER TABLE users ADD COLUMN phone TEXT;

-- With a default value (applied to all existing rows immediately)
ALTER TABLE orders ADD COLUMN priority INT NOT NULL DEFAULT 0;
```

### Drop Column

```sql
ALTER TABLE users DROP COLUMN phone;
```

### Rename Table

```sql
ALTER TABLE old_name RENAME TO new_name;
```

### Add Constraint

```sql
ALTER TABLE orders ADD CONSTRAINT chk_positive_total CHECK (total >= 0);
ALTER TABLE users  ADD CONSTRAINT uq_users_phone UNIQUE (phone);
```

### Drop Constraint

```sql
ALTER TABLE orders DROP CONSTRAINT chk_positive_total;
```

---

## TRUNCATE TABLE

Removes all rows from a table without dropping its structure. Faster than
`DELETE FROM table` for large tables because it does not generate individual
WAL entries for each row.

```sql
TRUNCATE TABLE import_staging;
TRUNCATE TABLE import_staging RESTART IDENTITY;  -- also resets AUTO_INCREMENT counters
```
