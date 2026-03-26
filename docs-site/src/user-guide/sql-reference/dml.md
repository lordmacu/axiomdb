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

### FULL OUTER JOIN

Returns **all rows from both tables**. Matched rows are joined normally.
Unmatched rows from either side are padded with `NULL` on the missing side.

> **AxiomDB extension over the MySQL wire protocol.** MySQL does not support
> `FULL OUTER JOIN`. AxiomDB clients connecting via the MySQL wire protocol can
> use it, but standard MySQL clients may not send it.

```sql
-- Audit: find users with no orders AND orders with no valid user
SELECT
    u.id   AS user_id,
    u.name AS user_name,
    o.id   AS order_id,
    o.total
FROM users u
FULL OUTER JOIN orders o ON u.id = o.user_id
ORDER BY u.id, o.id;
```

| user_id | user_name | order_id | total |
|---------|-----------|----------|-------|
| 1 | Alice | 10 | 100 |
| 1 | Alice | 11 | 200 |
| 2 | Bob | 12 | 50 |
| 3 | Carol | NULL | NULL | ← user with no orders |
| NULL | NULL | 13 | 300 | ← order with no valid user |

Both `FULL JOIN` and `FULL OUTER JOIN` are accepted.

**`ON` vs `WHERE` semantics:**

- `ON` predicates are evaluated *before* null-extension.
  Rows that do not satisfy `ON` are treated as unmatched and receive NULLs.
- `WHERE` predicates run *after* the full join is materialized.
  Adding `WHERE u.id IS NOT NULL` removes unmatched right rows from the result.

```sql
-- ON vs WHERE: only keep rows where the user side is not NULL
SELECT u.id, o.id
FROM users u
FULL OUTER JOIN orders o ON u.id = o.user_id
WHERE u.id IS NOT NULL;     -- removes the (NULL, 13) row
```

**Nullability:** In `SELECT *` over a `FULL OUTER JOIN`, all columns from both
tables are marked nullable even if the catalog defines them as `NOT NULL`,
because either side can be null-extended.

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

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Bulk-empty fast path</span>
<code>DELETE FROM t</code> without a <code>WHERE</code> clause uses a <strong>root-rotation fast path</strong>
instead of per-row B-Tree deletes. New empty heap and index roots are allocated,
the catalog is updated atomically inside the transaction, and old pages are freed
only after WAL fsync confirms commit durability. This eliminates the 10,000× slowdown
that previously occurred when a table had any index (PK, UNIQUE, or secondary).
The operation is fully transactional: <code>ROLLBACK</code> restores original roots.
</div>
</div>

> **When parent FK references exist**, `DELETE FROM t` keeps the row-by-row path so
> `RESTRICT`/`CASCADE`/`SET NULL` FK enforcement still fires correctly.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Indexed DELETE WHERE</span>
<code>DELETE ... WHERE col = value</code> or <code>WHERE col &gt; lo</code> uses the available index
to discover candidate rows instead of scanning the full heap. The planner always
prefers the index for DELETE (unlike SELECT, which may reject an index when selectivity
is too low) because avoiding a heap scan is always beneficial even when many rows match.
The full <code>WHERE</code> predicate is rechecked on fetched rows before deletion.
</div>
</div>

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
| FK parent table | Row-by-row (enforces FK) | Fails if child FKs exist |
| Typical use | Conditional deletes | Full table wipe |

`TRUNCATE TABLE` fails with an error if any FK constraint references the table as
the parent. Delete or truncate child tables first, then truncate the parent.

Both `DELETE FROM t` (no WHERE) and `TRUNCATE TABLE t` use the same bulk-empty
root-rotation machinery internally and are fully transactional.

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
| `@@axiom_compat` | `'standard'` | Compatibility mode — controls default session collation (see [AXIOM_COMPAT](#axiom_compat)) |
| `@@collation` | `'binary'` | Executor-visible text semantics — `binary` or `es` (see [AXIOM_COMPAT](#axiom_compat)) |
| `@@in_transaction` | `0` | `1` when inside an active transaction, `0` otherwise |
| `@@on_error` | `'rollback_statement'` | How statement errors affect the transaction (see [ON_ERROR](#on_error)) |
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

### AXIOM_COMPAT and collation

`@@axiom_compat` controls the high-level compatibility behavior of the session.
`@@collation` controls how text values are compared, sorted, and grouped.

```sql
SET AXIOM_COMPAT = 'mysql';          -- CI+AI text semantics (default collation = 'es')
SET AXIOM_COMPAT = 'postgresql';     -- exact binary text semantics
SET AXIOM_COMPAT = 'standard';       -- default AxiomDB behavior (binary)
SET AXIOM_COMPAT = DEFAULT;          -- reset to 'standard'

SET collation = 'es';                -- explicit CI+AI fold for this session
SET collation = 'binary';            -- explicit exact byte order
SET collation = DEFAULT;             -- restore compat-derived default
```

#### `binary` collation (default)

Exact byte-order string comparison — current AxiomDB default:

- `'a' != 'A'`, `'a' != 'á'`
- `LIKE` is case-sensitive and accent-sensitive
- `GROUP BY`, `DISTINCT`, `ORDER BY`, `MIN/MAX(TEXT)` all use raw byte order

#### `es` collation — CI+AI fold

A lightweight session-level CI+AI fold: NFC normalize → lowercase → strip combining
accent marks. No ICU / CLDR dependency.

- `'Jose' = 'JOSE' = 'José'` compare equal
- `LIKE 'jos%'` matches `José`
- `GROUP BY`, `DISTINCT`, `COUNT(DISTINCT ...)` collapse accent/case variants into one group
- `ORDER BY` sorts by folded text first, raw text as a tie-break for determinism
- `MIN/MAX(TEXT)` and `GROUP_CONCAT(DISTINCT/ORDER BY ...)` respect the fold

```sql
-- Binary (default): José and jose are different rows
SELECT name FROM users GROUP BY name;
-- → 'José', 'jose', 'JOSE'

-- Es: all three fold to "jose" — one group
SET AXIOM_COMPAT = 'mysql';
SELECT name FROM users GROUP BY name;
-- → 'José'   (or whichever variant appears first)

-- Explicit collation independent of compat mode:
SET collation = 'es';
SELECT * FROM products WHERE name = 'widget';  -- matches Widget, WIDGET, wídget
```

**Index safety:** When `@@collation = 'es'`, AxiomDB automatically falls back from text
index lookups to full table scans for correctness. Binary-ordered B-Tree keys do not match
`es`-folded predicates, so using the index would silently miss rows. Non-text indexes
(INT, BIGINT, DATE, etc.) are unaffected.

> **Note:** `@@collation` and `@@collation_connection` are separate variables.
> `@@collation_connection` is the **transport charset** (set during handshake or via `SET NAMES`).
> `@@collation` is the **executor-visible text-comparison behavior** added by `AXIOM_COMPAT`.

Full layered collation (per-database, per-column, ICU locale) is planned for Phase 13.13.

### ON_ERROR

`@@on_error` controls what happens to the current transaction when a statement
fails. It applies to all pipeline stages: parse errors, semantic errors, and
executor errors.

```sql
SET on_error = 'rollback_statement';    -- default
SET on_error = 'rollback_transaction';
SET on_error = 'savepoint';
SET on_error = 'ignore';
SET on_error = DEFAULT;                  -- reset to rollback_statement
```

Both quoted strings and bare identifiers are accepted:

```sql
SET on_error = rollback_statement;      -- same as 'rollback_statement'
```

#### Modes

**`rollback_statement`** (default) — When a statement fails inside an active
transaction, only that statement's writes are rolled back. The transaction stays
open. This matches MySQL's statement-level rollback behavior.

```sql
BEGIN;
INSERT INTO t VALUES (1);          -- ok
INSERT INTO t VALUES (1);          -- ERROR: duplicate key
-- transaction still active, id=1 is the only write that will commit
INSERT INTO t VALUES (2);          -- ok
COMMIT;                            -- commits id=1 and id=2
```

**`rollback_transaction`** — When any statement fails inside an active transaction,
the entire transaction is rolled back immediately. `@@in_transaction` becomes 0.

```sql
SET on_error = 'rollback_transaction';

BEGIN;
INSERT INTO t VALUES (1);          -- ok
INSERT INTO t VALUES (1);          -- ERROR: duplicate key → whole txn rolled back
SELECT @@in_transaction;           -- 0 — transaction is gone
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Eager Rollback vs PostgreSQL Abort Latch</span>
PostgreSQL keeps the transaction open after an error in a "aborted" state (SQLSTATE 25P02) where every subsequent statement returns <code>ERROR: current transaction is aborted</code> until the client sends ROLLBACK. AxiomDB's <code>rollback_transaction</code> uses eager rollback instead: the transaction is closed immediately on error, so the client starts fresh without needing an explicit ROLLBACK.
</div>
</div>

**`savepoint`** — Same as `rollback_statement` when a transaction is already
active. When `autocommit = 0`, the key difference appears on the **first** DML
in an implicit transaction: `savepoint` preserves the implicit transaction after
a failing first DML, while `rollback_statement` closes it.

```sql
SET autocommit = 0;
SET on_error = 'savepoint';

INSERT INTO t VALUES (999);        -- fails (dup key)
SELECT @@in_transaction;           -- 1 — implicit txn stays open
INSERT INTO t VALUES (1);          -- ok, continues in the same txn
COMMIT;
```

**`ignore`** — Ignorable SQL errors (parse errors, semantic errors, constraint
violations, type mismatches) are converted to session warnings and the statement
is reported as success. Non-ignorable errors (I/O failures, WAL errors, storage
corruption) still return ERR; if one happens inside an active transaction,
AxiomDB eagerly rolls that transaction back before returning the error.

```sql
SET on_error = 'ignore';

INSERT INTO t VALUES (1);          -- ok
INSERT INTO t VALUES (1);          -- duplicate key → silently ignored
SHOW WARNINGS;                     -- shows code 1062 + original message
INSERT INTO t VALUES (2);          -- ok, continues
COMMIT;                            -- commits id=1 and id=2
```

In a multi-statement `COM_QUERY`, `ignore` continues executing later statements
after an ignored error.

```sql
-- Single COM_QUERY with three statements:
INSERT INTO t VALUES (1); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);
-- First succeeds, second is ignored (dup), third succeeds.
-- Only the ignored statement's OK packet carries warning_count > 0.
```

#### Inspecting the current mode

```sql
SELECT @@on_error;                  -- 'rollback_statement'
SELECT @@session.on_error;          -- same
SHOW VARIABLES LIKE 'on_error';     -- on_error | rollback_statement
```

`COM_RESET_CONNECTION` resets `@@on_error` to `rollback_statement`.

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

---

## Prepared Statements — Binary Protocol

AxiomDB supports the full MySQL binary prepared statement protocol, including
large parameter transmission via `COM_STMT_SEND_LONG_DATA`.

### Large parameters (BLOB / TEXT)

When a parameter value is too large to send in a single `COM_STMT_EXECUTE`
packet, client libraries split it into multiple `COM_STMT_SEND_LONG_DATA`
chunks before execute. AxiomDB buffers all chunks and assembles the final value
at execute time.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Chunk Boundaries Are Safe</span>
AxiomDB buffers long-data chunks as raw bytes and decodes text only at execute time. A UTF-8 character may be split across packets without corrupting the final value.
</div>
</div>

**Python (PyMySQL):**

```python
import pymysql, os

conn = pymysql.connect(host="127.0.0.1", port=3306, user="root", db="test")
cur = conn.cursor()
cur.execute("CREATE TABLE IF NOT EXISTS files (id INT, data LONGBLOB)")

# PyMySQL automatically uses COM_STMT_SEND_LONG_DATA for values > 8 KB
large_blob = os.urandom(64 * 1024)   # 64 KB binary data
cur.execute("INSERT INTO files VALUES (%s, %s)", (1, large_blob))
conn.commit()
```

**Binary parameters** (`BLOB`, `LONGBLOB`, `MEDIUMBLOB`, `TINYBLOB`) are stored
as raw bytes — `0x00` bytes and non-UTF-8 sequences are preserved exactly.

**Text parameters** (`VARCHAR`, `TEXT`, `LONGTEXT`) are decoded with the
connection's `character_set_client` after all chunks are assembled, so multibyte
characters split across chunk boundaries are reconstructed correctly.

### Parameter type mapping

| MySQL type | AxiomDB type | Notes |
|---|---|---|
| `MYSQL_TYPE_STRING` / `VAR_STRING` / `VARCHAR` | `TEXT` | UTF-8 decoded |
| `MYSQL_TYPE_BLOB` / `TINY_BLOB` / `MEDIUM_BLOB` / `LONG_BLOB` | `BYTES` | Raw bytes, no charset |
| `MYSQL_TYPE_LONG` / `LONGLONG` | `INT` / `BIGINT` | |
| `MYSQL_TYPE_FLOAT` / `DOUBLE` | `REAL` | |
| `MYSQL_TYPE_DATE` | `DATE` | |
| `MYSQL_TYPE_DATETIME` | `TIMESTAMP` | |

### COM_STMT_RESET

Calling `mysql_stmt_reset()` (or the equivalent in any MySQL driver) clears any
pending long-data buffers for that statement without deallocating the prepared
statement itself. The statement can then be re-executed with fresh parameters.

### SHOW STATUS counter

`SHOW STATUS LIKE 'Com_stmt_send_long_data'` reports how many long-data chunks
have been received by the current session (session scope) or by the server since
startup (global scope).

```sql
SHOW STATUS LIKE 'Com_stmt_send_long_data';
-- Variable_name                | Value
-- Com_stmt_send_long_data      | 3
```

