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

## GROUP BY and HAVING

`GROUP BY` collapses rows with the same values in the specified columns into a single
output row. Aggregate functions operate over each group.

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
