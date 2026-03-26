# DML — Queries and Mutations

DML statements read and modify table data: `SELECT`, `INSERT`, `UPDATE`, and `DELETE`.
All DML operations participate in the current transaction and are subject to MVCC
isolation.

---

## SELECT

### Full Syntax

```sql
SELECT [DISTINCT] select_list
FROM table_ref [AS alias]
     [JOIN ...]
[WHERE condition]
[GROUP BY column_list]
[HAVING condition]
[ORDER BY column_list [ASC|DESC] [NULLS FIRST|LAST]]
[LIMIT n [OFFSET m]];
```

### Basic Projections

```sql
-- All columns
SELECT * FROM users;

-- Specific columns with aliases
SELECT id, email AS user_email, name AS full_name
FROM users;

-- Computed columns
SELECT
    name,
    price * 1.19 AS price_with_tax,
    UPPER(name)  AS name_upper
FROM products;
```

### DISTINCT

Removes duplicate rows from the result. Two rows are duplicates if every selected
column has the same value (NULL = NULL for this purpose only).

```sql
-- All distinct status values in the orders table
SELECT DISTINCT status FROM orders;

-- All distinct (category_id, status) pairs
SELECT DISTINCT category_id, status FROM products ORDER BY category_id;
```

---

## FROM and JOIN

### Simple FROM

```sql
SELECT * FROM products;
SELECT p.* FROM products AS p WHERE p.price > 50;
```

### INNER JOIN

Returns only rows where the join condition matches in both tables.

```sql
SELECT
    u.name,
    o.id   AS order_id,
    o.total,
    o.status
FROM users u
INNER JOIN orders o ON o.user_id = u.id
WHERE o.status = 'shipped'
ORDER BY o.placed_at DESC;
```

### LEFT JOIN

Returns all rows from the left table; columns from the right table are NULL when
there is no matching row.

```sql
-- All users, including those with no orders
SELECT
    u.id,
    u.name,
    COUNT(o.id) AS total_orders
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
GROUP BY u.id, u.name
ORDER BY total_orders DESC;
```

### RIGHT JOIN

Returns all rows from the right table; left table columns are NULL on no match.
Less common — most RIGHT JOINs can be rewritten as LEFT JOINs by swapping tables.

```sql
SELECT p.name, SUM(oi.quantity) AS total_sold
FROM order_items oi
RIGHT JOIN products p ON p.id = oi.product_id
GROUP BY p.id, p.name;
```

### CROSS JOIN

Cartesian product — every row from the left table combined with every row from
the right table. Use with care: `m × n` rows.

```sql
-- Generate all combinations of size and color for a product grid
SELECT sizes.label AS size, colors.label AS color
FROM sizes
CROSS JOIN colors
ORDER BY sizes.sort_order, colors.sort_order;
```

### Multi-Table JOIN

```sql
SELECT
    u.name       AS customer,
    p.name       AS product,
    oi.quantity,
    oi.unit_price,
    oi.quantity * oi.unit_price AS line_total
FROM orders o
JOIN users       u  ON u.id  = o.user_id
JOIN order_items oi ON oi.order_id  = o.id
JOIN products    p  ON p.id  = oi.product_id
WHERE o.status = 'delivered'
ORDER BY o.placed_at DESC, p.name;
```

---

## WHERE

Filters rows before aggregation. Accepts any boolean expression.

```sql
-- Equality and comparison
SELECT * FROM products WHERE price > 100 AND stock > 0;

-- NULL check
SELECT * FROM users WHERE deleted_at IS NULL;
SELECT * FROM orders WHERE shipped_at IS NOT NULL;

-- BETWEEN (inclusive on both ends)
SELECT * FROM orders
WHERE placed_at BETWEEN '2026-01-01' AND '2026-03-31';

-- IN list
SELECT * FROM orders WHERE status IN ('pending', 'paid', 'shipped');

-- LIKE pattern matching (% = any sequence, _ = exactly one character)
SELECT * FROM users WHERE email LIKE '%@example.com';
SELECT * FROM products WHERE name LIKE 'USB-_';

-- NOT variants
SELECT * FROM orders WHERE status NOT IN ('cancelled', 'refunded');
SELECT * FROM products WHERE name NOT LIKE 'Test%';
```

---

## Subqueries

A subquery is a `SELECT` statement nested inside another statement. AxiomDB supports
five subquery forms, each with full NULL semantics identical to PostgreSQL and MySQL.

### Scalar Subqueries

A scalar subquery appears anywhere an expression is valid (SELECT list, WHERE, HAVING,
ORDER BY). It must return exactly one column. If it returns zero rows, the result is
`NULL`. If it returns more than one row, AxiomDB raises `CardinalityViolation`
(SQLSTATE 21000).

```sql
-- Compare each product price against the overall average
SELECT
    name,
    price,
    price - (SELECT AVG(price) FROM products) AS diff_from_avg
FROM products
ORDER BY diff_from_avg DESC;

-- Find the most recently placed order date
SELECT * FROM orders
WHERE placed_at = (SELECT MAX(placed_at) FROM orders);

-- Use a scalar subquery in HAVING
SELECT user_id, COUNT(*) AS order_count
FROM orders
GROUP BY user_id
HAVING COUNT(*) > (SELECT AVG(cnt) FROM (SELECT COUNT(*) AS cnt FROM orders GROUP BY user_id) AS sub);
```

If the subquery returns more than one row, AxiomDB raises:

```
ERROR 21000: subquery must return exactly one row, but returned 3 rows
```

Use `LIMIT 1` or a unique `WHERE` predicate to guarantee a single row.

### IN Subquery

`expr [NOT] IN (SELECT col FROM ...)` tests whether a value appears in the set of
values produced by the subquery.

```sql
-- Orders for users who have placed more than 5 orders total
SELECT * FROM orders
WHERE user_id IN (
    SELECT user_id FROM orders GROUP BY user_id HAVING COUNT(*) > 5
);

-- Products never sold
SELECT * FROM products
WHERE id NOT IN (
    SELECT DISTINCT product_id FROM order_items
);
```

**NULL semantics** — fully consistent with the SQL standard:

| Value in outer expr | Subquery result | Result |
|---|---|---|
| `'Alice'` | contains `'Alice'` | `TRUE` |
| `'Alice'` | does not contain `'Alice'`, no NULLs | `FALSE` |
| `'Alice'` | does not contain `'Alice'`, contains `NULL` | `NULL` |
| `NULL` | any non-empty set | `NULL` |
| `NULL` | empty set | `NULL` |

The third row is the subtle case: `x NOT IN (subquery with NULLs)` returns `NULL`,
not `FALSE`. This means `NOT IN` combined with a subquery that may produce NULLs
can silently exclude rows. A safe alternative is `NOT EXISTS`.

### EXISTS / NOT EXISTS

`[NOT] EXISTS (SELECT ...)` tests whether the subquery produces at least one row.
The result is always `TRUE` or `FALSE` — never `NULL`.

```sql
-- Users who have at least one paid order
SELECT * FROM users u
WHERE EXISTS (
    SELECT 1 FROM orders o
    WHERE o.user_id = u.id AND o.status = 'paid'
);

-- Products with no associated order items
SELECT * FROM products p
WHERE NOT EXISTS (
    SELECT 1 FROM order_items oi WHERE oi.product_id = p.id
);
```

The select list inside an `EXISTS` subquery does not matter — `SELECT 1`, `SELECT *`,
and `SELECT id` all behave identically. The engine only checks for row existence.

### Correlated Subqueries

A correlated subquery references columns from the outer query. AxiomDB re-executes
the subquery for each outer row, substituting the current outer column values.

```sql
-- For each order, fetch the user's name (correlated scalar subquery in SELECT list)
SELECT
    o.id,
    o.total,
    (SELECT u.name FROM users u WHERE u.id = o.user_id) AS customer_name
FROM orders o;

-- Orders whose total exceeds the average total for that user (correlated in WHERE)
SELECT * FROM orders o
WHERE o.total > (
    SELECT AVG(total) FROM orders WHERE user_id = o.user_id
);

-- Active products with above-average stock in their category
SELECT * FROM products p
WHERE p.stock > (
    SELECT AVG(stock) FROM products WHERE category_id = p.category_id
);
```

Correlated subqueries with large outer result sets can be slow (O(n) re-executions).
For performance-critical paths, rewrite them as JOINs with aggregation.

### Derived Tables (FROM Subquery)

A subquery in the `FROM` clause is called a derived table. It must have an alias.
AxiomDB materializes the derived table result in memory before executing the outer query.

```sql
-- Top spenders, computed as a subquery and then filtered
SELECT customer_name, total_spent
FROM (
    SELECT u.name AS customer_name, SUM(o.total) AS total_spent
    FROM users u
    JOIN orders o ON o.user_id = u.id
    WHERE o.status = 'delivered'
    GROUP BY u.id, u.name
) AS spending
WHERE total_spent > 500
ORDER BY total_spent DESC;

-- Percentile bucketing: compute rank in a subquery, filter in outer
SELECT *
FROM (
    SELECT
        id,
        name,
        price,
        RANK() OVER (ORDER BY price DESC) AS price_rank
    FROM products
) AS ranked
WHERE price_rank <= 10;
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Full SQL Standard NULL Semantics</span>
AxiomDB implements the same three-valued logic for <code>IN (subquery)</code> as PostgreSQL and MySQL: a non-matching lookup against a set that contains NULL returns NULL, not FALSE. This matches ISO SQL:2016 and avoids the "missing row" trap that catches developers when <code>NOT IN</code> is used against a nullable foreign key column. Every subquery form (scalar, IN, EXISTS, correlated, derived table) follows the same rules as PostgreSQL 15.
</div>
</div>

---

## GROUP BY and HAVING

`GROUP BY` collapses rows with the same values in the specified columns into a single
output row. Aggregate functions operate over each group.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Automatic Sorted Grouping</span>
When the query uses an indexed column as the GROUP BY key and the chosen B-Tree access method already delivers rows in key order, AxiomDB automatically switches to a streaming sorted executor — no hash table, <code>O(1)</code> memory per group. Unlike PostgreSQL, which requires a separate <code>GroupAggregate</code> plan node, AxiomDB selects the strategy transparently at execution time.
</div>
</div>

```sql
-- Orders per user
SELECT user_id, COUNT(*) AS order_count, SUM(total) AS revenue
FROM orders
GROUP BY user_id
ORDER BY revenue DESC;

-- Monthly revenue
SELECT
    DATE_TRUNC('month', placed_at) AS month,
    COUNT(*)   AS orders,
    SUM(total) AS revenue,
    AVG(total) AS avg_order_value
FROM orders
WHERE status != 'cancelled'
GROUP BY DATE_TRUNC('month', placed_at)
ORDER BY month;
```

`HAVING` filters groups after aggregation (analogous to `WHERE` for rows).

```sql
-- Only users with more than 5 orders
SELECT user_id, COUNT(*) AS order_count
FROM orders
GROUP BY user_id
HAVING COUNT(*) > 5
ORDER BY order_count DESC;

-- Only categories with average price above 50
SELECT category_id, AVG(price) AS avg_price
FROM products
WHERE deleted_at IS NULL
GROUP BY category_id
HAVING AVG(price) > 50;
```

---

## ORDER BY

Sorts the result. Multiple columns are sorted left to right.

```sql
-- Descending by total, then ascending by name as tiebreaker
SELECT user_id, SUM(total) AS revenue
FROM orders
GROUP BY user_id
ORDER BY revenue DESC, user_id ASC;
```

### NULLS FIRST / NULLS LAST

Controls where NULL values appear in the sort order.

```sql
-- Show NULL shipped_at rows at the bottom (unshipped orders last)
SELECT id, total, shipped_at
FROM orders
ORDER BY shipped_at ASC NULLS LAST;

-- Show most recent shipments first; unshipped at top
SELECT id, total, shipped_at
FROM orders
ORDER BY shipped_at DESC NULLS FIRST;
```

Default behavior: `ASC` sorts NULL last; `DESC` sorts NULL first (same as PostgreSQL).

---

## LIMIT and OFFSET

```sql
-- First 10 rows
SELECT * FROM products ORDER BY name LIMIT 10;

-- Rows 11-20 (page 2 with page size 10)
SELECT * FROM products ORDER BY name LIMIT 10 OFFSET 10;

-- Common pagination pattern
SELECT * FROM products
ORDER BY created_at DESC
LIMIT 20 OFFSET 40;   -- page 3 (0-indexed) of 20 items per page
```

> For large offsets (> 10,000), consider keyset pagination instead:
> `WHERE id > :last_seen_id ORDER BY id LIMIT 20`

---

## INSERT

### INSERT ... VALUES

When a table has an `AUTO_INCREMENT` column, omit it from the column list and
AxiomDB generates the next sequential ID automatically. Use `LAST_INSERT_ID()`
(or the PostgreSQL alias `lastval()`) immediately after the INSERT to retrieve
the generated value.

```sql
CREATE TABLE users (
    id   BIGINT PRIMARY KEY AUTO_INCREMENT,
    name TEXT   NOT NULL
);

-- Single row — id is generated automatically
INSERT INTO users (name) VALUES ('Alice');
-- id=1

SELECT LAST_INSERT_ID();   -- returns 1
```

For multi-row INSERT, `LAST_INSERT_ID()` returns the ID generated for the
**first** row of the batch (MySQL semantics). Subsequent rows receive
consecutive IDs.

```sql
INSERT INTO users (name) VALUES ('Bob'), ('Carol'), ('Dave');
-- ids: 2, 3, 4
SELECT LAST_INSERT_ID();   -- returns 2 (first of the batch)
```

Supplying an explicit non-NULL value in the AUTO_INCREMENT column bypasses the
sequence and does not advance it.

```sql
INSERT INTO users (id, name) VALUES (100, 'Eve');
-- id=100; sequence not advanced; next LAST_INSERT_ID() still returns 2
```

See [Expressions — Session Functions](expressions.md#session-functions) for
full `LAST_INSERT_ID()` / `lastval()` semantics.

```sql
-- Single row
INSERT INTO users (name, email, age)
VALUES ('Alice', 'alice@example.com', 30);

-- Multiple rows in one statement (more efficient than individual INSERTs)
INSERT INTO products (name, price, stock) VALUES
    ('Keyboard', 49.99, 100),
    ('Mouse',    29.99, 200),
    ('Monitor', 299.99,  50);
```

### INSERT ... DEFAULT VALUES

Inserts a single row using all column defaults. Useful when every column has a default.

```sql
CREATE TABLE audit_events (
    id         BIGINT      PRIMARY KEY AUTO_INCREMENT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    event_type TEXT        NOT NULL DEFAULT 'unknown'
);

INSERT INTO audit_events DEFAULT VALUES;
-- Row: id=1, created_at=<now>, event_type='unknown'
```

### INSERT ... SELECT

Inserts rows generated by a SELECT statement. Useful for bulk copies and migrations.

```sql
-- Copy all active users to an archive table
INSERT INTO users_archive (id, email, name, created_at)
SELECT id, email, name, created_at
FROM users
WHERE deleted_at IS NOT NULL;

-- Compute and store aggregates
INSERT INTO monthly_revenue (month, total)
SELECT
    DATE_TRUNC('month', placed_at),
    SUM(total)
FROM orders
WHERE status = 'delivered'
GROUP BY 1;
```

---

## UPDATE

Modifies existing rows. All matching rows are updated in a single statement.

```sql
UPDATE table_name
SET column = expression [, column = expression ...]
[WHERE condition];
```

```sql
-- Mark a specific order as shipped
UPDATE orders
SET status = 'shipped', shipped_at = CURRENT_TIMESTAMP
WHERE id = 42;

-- Apply a 10% discount to all products in a category
UPDATE products
SET price = price * 0.90
WHERE category_id = 5 AND deleted_at IS NULL;

-- Reset all pending orders older than 7 days to cancelled
UPDATE orders
SET status = 'cancelled'
WHERE status = 'pending'
  AND placed_at < CURRENT_TIMESTAMP - INTERVAL '7 days';
```

> An UPDATE without a WHERE clause updates **every row** in the table. This is
> rarely what you want. Always double-check before running unbounded updates in
> production.

---

## DELETE

Removes rows from a table.

```sql
DELETE FROM table_name [WHERE condition];
```

```sql
-- Delete a specific row
DELETE FROM sessions WHERE id = 'abc123';

-- Delete all expired sessions
DELETE FROM sessions WHERE expires_at < CURRENT_TIMESTAMP;

-- Soft delete pattern (prefer UPDATE to mark rows inactive)
UPDATE users SET deleted_at = CURRENT_TIMESTAMP WHERE id = 7;
-- Then filter: SELECT * FROM users WHERE deleted_at IS NULL;
```

> A DELETE without a WHERE clause removes **all rows**. For this use case,
> `TRUNCATE TABLE` is faster and should be preferred when you intend to empty
> a table entirely.

---

## TRUNCATE TABLE

Removes all rows from a table and resets its `AUTO_INCREMENT` counter to 1.
The table structure, indexes, and constraints are preserved.

```sql
TRUNCATE TABLE table_name;
```

```sql
-- Empty a staging table before a fresh import
TRUNCATE TABLE import_staging;

-- After truncate, AUTO_INCREMENT restarts from 1
CREATE TABLE counters (id INT AUTO_INCREMENT PRIMARY KEY, label TEXT);
INSERT INTO counters (label) VALUES ('a'), ('b');  -- ids: 1, 2
TRUNCATE TABLE counters;
INSERT INTO counters (label) VALUES ('c');          -- id: 1 (reset)
```

`TRUNCATE TABLE` returns `Affected { count: 0 }`, matching MySQL convention.

**TRUNCATE vs DELETE — when to use each:**

| | `DELETE FROM t` | `TRUNCATE TABLE t` |
|---|---|---|
| Rows removed | All (without WHERE) | All |
| WHERE clause | Supported | Not supported |
| AUTO_INCREMENT | Not reset | Reset to 1 |
| Rows affected | Returns actual count | Returns 0 |
| Typical use | Conditional deletes | Full table wipe |

---

## Session Variables

Session variables hold connection-scoped state. Read them with `SELECT @@name` and
change them with `SET name = value`.

### Reading session variables

```sql
SELECT @@autocommit;        -- 1 (autocommit on) or 0 (autocommit off)
SELECT @@in_transaction;    -- 1 inside an active transaction, 0 otherwise
SELECT @@version;           -- '8.0.36-AxiomDB-0.1.0'
SELECT @@character_set_client;   -- 'utf8mb4'
SELECT @@transaction_isolation;  -- 'REPEATABLE-READ'
```

### Supported variables

| Variable | Default | Description |
|---|---|---|
| `@@autocommit` | `1` | `1` = each statement auto-committed; `0` = explicit COMMIT required |
| `@@in_transaction` | `0` | `1` when inside an active transaction, `0` otherwise |
| `@@version` | `'8.0.36-AxiomDB-0.1.0'` | Server version (MySQL 8 compatible format) |
| `@@version_comment` | `'AxiomDB'` | Server variant |
| `@@character_set_client` | `'utf8mb4'` | Client character set |
| `@@character_set_results` | `'utf8mb4'` | Result character set |
| `@@collation_connection` | `'utf8mb4_general_ci'` | Connection collation |
| `@@max_allowed_packet` | `67108864` | Maximum packet size (64 MB) |
| `@@sql_mode` | `'STRICT_TRANS_TABLES'` | Active SQL mode (see [Strict Mode](#strict-mode)) |
| `@@strict_mode` | `'ON'` | AxiomDB strict coercion flag (alias for `STRICT_TRANS_TABLES` in sql_mode) |
| `@@transaction_isolation` | `'REPEATABLE-READ'` | Isolation level |

### Changing session variables

```sql
-- Switch to manual transaction mode (used by SQLAlchemy, Django ORM, etc.)
SET autocommit = 0;
SET autocommit = 1;   -- restore

-- Character set (accepted for ORM compatibility, utf8mb4 is always used internally)
SET NAMES 'utf8mb4';
SET character_set_client = 'utf8mb4';

-- Control coercion strictness (see Strict Mode below)
SET strict_mode = OFF;
SET sql_mode = '';
```

### @@in_transaction — transaction state check

```sql
SELECT @@in_transaction;    -- 0 — no transaction active

INSERT INTO t VALUES (1);   -- starts implicit txn when autocommit=0
SELECT @@in_transaction;    -- 1 — inside transaction

COMMIT;
SELECT @@in_transaction;    -- 0 — transaction closed
```

Use `@@in_transaction` to verify transaction state before issuing a `COMMIT` or
`ROLLBACK`. This avoids the warning generated when `COMMIT` is called with no
active transaction.

### Strict Mode

AxiomDB operates in **strict mode** by default. In strict mode, an `INSERT` or
`UPDATE` that cannot coerce a value to the column's declared type returns an error
immediately (SQLSTATE 22018). This prevents silent data corruption.

```sql
CREATE TABLE products (name TEXT, stock INT);

-- Strict mode (default): error on bad coercion
INSERT INTO products VALUES ('Widget', 'abc');
-- ERROR 22018: cannot coerce 'abc' (Text) to INT
```

To enable **permissive mode**, disable strict mode for the session:

```sql
SET strict_mode = OFF;
-- or equivalently:
SET sql_mode = '';
```

In permissive mode, AxiomDB first tries the strict coercion. If it fails, it
falls back to a best-effort conversion (e.g. `'42abc'` → `42`, `'abc'` → `0`),
stores the result, and emits warning **1265** instead of returning an error:

```sql
SET strict_mode = OFF;

CREATE TABLE products (name TEXT, stock INT);
INSERT INTO products VALUES ('Widget', '99abc');
-- Succeeds — stock stored as 99; warning emitted

SHOW WARNINGS;
-- Level    Code   Message
-- ─────────────────────────────────────────────────────────────────────
-- Warning  1265   Data truncated for column 'stock' at row 1
```

For multi-row INSERT, the row number in warning 1265 is 1-based and identifies
the specific row that triggered the fallback:

```sql
INSERT INTO products VALUES ('A', '10'), ('B', '99x'), ('C', '30');
SHOW WARNINGS;
-- Warning  1265   Data truncated for column 'stock' at row 2
```

Re-enable strict mode at any time:

```sql
SET strict_mode = ON;
-- or equivalently:
SET sql_mode = 'STRICT_TRANS_TABLES';
```

`SET strict_mode = DEFAULT` also restores the server default (ON).

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip — ORM Compatibility</span>
Some ORMs (e.g. older SQLAlchemy versions, legacy Rails) set <code>sql_mode = ''</code>
at connection time to get MySQL 5 permissive behavior. AxiomDB supports this pattern:
<code>SET sql_mode = ''</code> disables strict mode for that connection. Use
<code>SHOW WARNINGS</code> after bulk loads to audit truncated values.
</div>
</div>

### SHOW WARNINGS

After any statement that completes with warnings, query the warning list:

```sql
-- Warning from no-op COMMIT
COMMIT;               -- no active transaction — emits warning 1592
SHOW WARNINGS;
-- Level    Code   Message
-- ───────────────────────────────────────────────
-- Warning  1592   There is no active transaction

-- Warning from permissive coercion (strict_mode = OFF)
SET strict_mode = OFF;
INSERT INTO products VALUES ('Widget', '99abc');
SHOW WARNINGS;
-- Level    Code   Message
-- ─────────────────────────────────────────────────────────────────────
-- Warning  1265   Data truncated for column 'stock' at row 1
```

`SHOW WARNINGS` returns the warnings from the **most recent statement only**. The
list is cleared before each new statement executes.

| Warning Code | Condition |
|---|---|
| `1265` | Permissive coercion fallback: value was truncated/converted to fit the column type |
| `1592` | COMMIT or ROLLBACK issued with no active transaction |

---

## SHOW TABLES

Lists all tables in the current schema (or a named schema).

```sql
SHOW TABLES;
SHOW TABLES FROM schema_name;
```

The result set has a single column named `Tables_in_<schema>`:

```sql
SHOW TABLES;
-- Tables_in_public
-- ────────────────
-- users
-- orders
-- products
-- order_items
```

---

## SHOW COLUMNS / DESCRIBE

Returns the column definitions of a table.

```sql
SHOW COLUMNS FROM table_name;
DESCRIBE table_name;
DESC table_name;            -- shorthand
```

All three forms are equivalent. The result has six columns:

| Column    | Description                                           |
|-----------|-------------------------------------------------------|
| `Field`   | Column name                                           |
| `Type`    | Data type as declared in `CREATE TABLE`               |
| `Null`    | `YES` if the column accepts NULL, `NO` otherwise      |
| `Key`     | `PRI` for primary key columns; empty otherwise (stub) |
| `Default` | Default expression, or `NULL` if none (stub)          |
| `Extra`   | `auto_increment` for AUTO_INCREMENT columns; empty otherwise |

```sql
CREATE TABLE users (
    id   BIGINT PRIMARY KEY AUTO_INCREMENT,
    name TEXT   NOT NULL,
    bio  TEXT
);

DESCRIBE users;
-- Field  Type    Null  Key  Default  Extra
-- ─────────────────────────────────────────────────
-- id     BIGINT  NO    PRI  NULL     auto_increment
-- name   TEXT    NO         NULL
-- bio    TEXT    YES        NULL
```

> The `Key` and `Default` columns are stubs in the current release and do not
> yet reflect all constraints or computed defaults. Full metadata is tracked
> internally in the catalog and will be exposed in a future release.

---

## Practical Examples — E-commerce Queries

### Checkout: Atomic Order Placement

```sql
BEGIN;

-- Verify stock before committing
SELECT stock FROM products WHERE id = 1 AND stock >= 2;
-- If no row returned, rollback

INSERT INTO orders (user_id, total, status)
VALUES (99, 99.98, 'paid');

INSERT INTO order_items (order_id, product_id, quantity, unit_price)
VALUES (LAST_INSERT_ID(), 1, 2, 49.99);

UPDATE products SET stock = stock - 2 WHERE id = 1;

COMMIT;
```

### Revenue Report — Last 30 Days

```sql
SELECT
    p.name                          AS product,
    SUM(oi.quantity)                AS units_sold,
    SUM(oi.quantity * oi.unit_price) AS revenue
FROM order_items oi
JOIN orders  o ON o.id = oi.order_id
JOIN products p ON p.id = oi.product_id
WHERE o.placed_at >= CURRENT_TIMESTAMP - INTERVAL '30 days'
  AND o.status IN ('paid', 'shipped', 'delivered')
GROUP BY p.id, p.name
ORDER BY revenue DESC
LIMIT 10;
```

### User Activity Summary

```sql
SELECT
    u.id,
    u.name,
    u.email,
    COUNT(o.id)   AS total_orders,
    SUM(o.total)  AS lifetime_value,
    MAX(o.placed_at) AS last_order
FROM users u
LEFT JOIN orders o ON o.user_id = u.id AND o.status != 'cancelled'
WHERE u.deleted_at IS NULL
GROUP BY u.id, u.name, u.email
ORDER BY lifetime_value DESC NULLS LAST;
```

---

## Multi-Statement Queries

AxiomDB accepts multiple SQL statements separated by `;` in a single `COM_QUERY`
call. Each statement executes sequentially, and the client receives one result set
per statement.

```sql
-- Three statements in one call
CREATE TABLE IF NOT EXISTS sessions (
    id         UUID NOT NULL,
    user_id    INT  NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO sessions (id, user_id) VALUES (gen_random_uuid(), 42);
SELECT COUNT(*) FROM sessions WHERE user_id = 42;
```

**How it works (protocol):**

Each intermediate result set is sent with the `SERVER_MORE_RESULTS_EXISTS` flag
(`0x0008`) set in the EOF/OK status bytes, telling the client to read the next
result set. The final result set has the flag cleared.

**Behavior on error:**

If any statement fails, execution stops at that point and an error packet is sent.
Statements after the failing one are not executed.

```sql
-- If INSERT fails (e.g. UNIQUE violation), SELECT is not executed
INSERT INTO users (email) VALUES ('duplicate@example.com');
SELECT * FROM users WHERE email = 'duplicate@example.com';
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip — SQL scripts and migrations</span>
Multi-statement support makes it easy to run SQL migration scripts directly via the
MySQL wire protocol. The <code>mysql</code> CLI, pymysql, and most ORMs handle
multi-statement results automatically when the client flag
<code>CLIENT_MULTI_STATEMENTS</code> is set (default in most clients).
</div>
</div>

---

## ALTER TABLE — Constraints

### ADD CONSTRAINT UNIQUE

```sql
-- Named unique constraint (recommended for DROP CONSTRAINT later)
ALTER TABLE users ADD CONSTRAINT uq_users_email UNIQUE (email);

-- Anonymous unique constraint (auto-named)
ALTER TABLE users ADD UNIQUE (username);
```

`ADD CONSTRAINT UNIQUE` creates a unique index internally. Fails with
`IndexAlreadyExists` if a constraint/index with that name already exists on the table,
or `UniqueViolation` if the column already has duplicate values.

### ADD CONSTRAINT CHECK

```sql
ALTER TABLE orders ADD CONSTRAINT chk_positive_amount CHECK (amount > 0);
ALTER TABLE products ADD CONSTRAINT chk_stock CHECK (stock >= 0);
```

The CHECK expression is validated against all existing rows at the time of the
`ALTER TABLE`. If any row fails the check, the statement returns `CheckViolation`.
After the constraint is added, every subsequent `INSERT` and `UPDATE` on the table
evaluates the expression.

### DROP CONSTRAINT

```sql
-- Drop by name (works for both UNIQUE and CHECK constraints)
ALTER TABLE users DROP CONSTRAINT uq_users_email;

-- Silent no-op if the constraint does not exist
ALTER TABLE users DROP CONSTRAINT IF EXISTS uq_users_old;
```

`DROP CONSTRAINT` searches first in indexes (for UNIQUE constraints), then in the
named constraint catalog (for CHECK constraints).

### ADD CONSTRAINT FOREIGN KEY (Phase 6.5)

Adds a foreign key constraint after the table is created. Validates all existing
rows before persisting — fails if any existing value violates the new constraint.

```sql
ALTER TABLE orders
  ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;
```

Fails if any existing `user_id` value has no matching row in `users`.

### Limitations

```sql
-- Not yet supported:
ALTER TABLE users ADD CONSTRAINT pk_users PRIMARY KEY (id);
-- → NotImplemented: ADD CONSTRAINT PRIMARY KEY — requires full table rewrite
```
