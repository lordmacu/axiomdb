# Expressions and Operators

An expression is any construct that evaluates to a value. Expressions appear in SELECT
projections, WHERE conditions, ORDER BY clauses, CHECK constraints, and DEFAULT values.

---

## Operator Precedence

From highest to lowest binding (higher = evaluated first):

| Level | Operators                         | Associativity |
|-------|-----------------------------------|---------------|
| 1     | `()` parentheses                  | —             |
| 2     | Unary `-`, `NOT`                  | Right         |
| 3     | `*`, `/`, `%`                     | Left          |
| 4     | `+`, `-`                          | Left          |
| 5     | `=`, `<>`, `!=`, `<`, `<=`, `>`, `>=` | —         |
| 6     | `IS NULL`, `IS NOT NULL`, `BETWEEN`, `LIKE`, `IN` | — |
| 7     | `AND`                             | Left          |
| 8     | `OR`                              | Left          |

Use parentheses to make complex expressions explicit:

```sql
-- Without parens: AND binds tighter than OR
SELECT * FROM orders WHERE status = 'paid' OR status = 'shipped' AND total > 100;
-- Parsed as: status = 'paid' OR (status = 'shipped' AND total > 100)

-- Explicit grouping
SELECT * FROM orders WHERE (status = 'paid' OR status = 'shipped') AND total > 100;
```

---

## Arithmetic Operators

| Operator | Meaning          | Example                           | Result |
|----------|------------------|-----------------------------------|--------|
| `+`      | Addition         | `price + tax`                     | —      |
| `-`      | Subtraction      | `stock - sold`                    | —      |
| `*`      | Multiplication   | `quantity * unit_price`           | —      |
| `/`      | Division         | `total / 1.19`                    | —      |
| `%`      | Modulo           | `id % 10`                         | 0–9    |

Integer division truncates toward zero: `7 / 2 = 3`.

Division by zero raises a runtime error (`22012 division_by_zero`).

```sql
SELECT
    price,
    price * 0.19        AS tax,
    price * 1.19        AS price_with_tax,
    ROUND(price, 2)     AS rounded
FROM products;
```

---

## Comparison Operators

| Operator  | Meaning              | NULL behavior              |
|-----------|----------------------|----------------------------|
| `=`       | Equal                | Returns NULL if either operand is NULL |
| `<>`, `!=`| Not equal            | Returns NULL if either operand is NULL |
| `<`       | Less than            | Returns NULL if either operand is NULL |
| `<=`      | Less than or equal   | Returns NULL if either operand is NULL |
| `>`       | Greater than         | Returns NULL if either operand is NULL |
| `>=`      | Greater than or equal| Returns NULL if either operand is NULL |

```sql
SELECT * FROM products WHERE price = 49.99;
SELECT * FROM products WHERE stock <> 0;
SELECT * FROM orders   WHERE total >= 100;
```

---

## Boolean Operators

| Operator | Meaning                         |
|----------|---------------------------------|
| `AND`    | TRUE only if both operands are TRUE |
| `OR`     | TRUE if at least one operand is TRUE |
| `NOT`    | Negates a boolean value         |

---

## NULL Semantics — Three-Valued Logic

AxiomDB implements SQL three-valued logic: every boolean expression evaluates to
TRUE, FALSE, or **UNKNOWN** (which SQL represents as NULL in boolean context).
The rules below are critical for writing correct WHERE clauses.

### AND truth table

| AND     | TRUE    | FALSE | UNKNOWN |
|---------|---------|-------|---------|
| TRUE    | TRUE    | FALSE | UNKNOWN |
| FALSE   | FALSE   | FALSE | FALSE   |
| UNKNOWN | UNKNOWN | FALSE | UNKNOWN |

### OR truth table

| OR      | TRUE | FALSE   | UNKNOWN |
|---------|------|---------|---------|
| TRUE    | TRUE | TRUE    | TRUE    |
| FALSE   | TRUE | FALSE   | UNKNOWN |
| UNKNOWN | TRUE | UNKNOWN | UNKNOWN |

### NOT truth table

| NOT | Result  |
|-----|---------|
| TRUE | FALSE  |
| FALSE | TRUE  |
| UNKNOWN | UNKNOWN |

### Key consequences

```sql
-- NULL compared to anything is UNKNOWN, not TRUE or FALSE
SELECT NULL = NULL;      -- UNKNOWN (NULL, not TRUE)
SELECT NULL <> NULL;     -- UNKNOWN
SELECT NULL = 1;         -- UNKNOWN

-- WHERE filters only rows where condition is TRUE
-- Rows where the condition is UNKNOWN are excluded
SELECT * FROM users WHERE age = NULL;    -- always returns 0 rows!
SELECT * FROM users WHERE age IS NULL;   -- correct NULL check

-- UNKNOWN in AND
SELECT * FROM orders WHERE total > 100 AND NULL;  -- 0 rows (UNKNOWN is filtered)

-- UNKNOWN in OR
SELECT * FROM orders WHERE total > 100 OR NULL;   -- rows where total > 100
```

---

## IS NULL / IS NOT NULL

These predicates are the correct way to check for NULL. They always return TRUE
or FALSE, never UNKNOWN.

```sql
-- Find unshipped orders
SELECT * FROM orders WHERE shipped_at IS NULL;

-- Find orders that have been shipped
SELECT * FROM orders WHERE shipped_at IS NOT NULL;

-- Combine with other conditions
SELECT * FROM users WHERE deleted_at IS NULL AND age > 18;
```

---

## BETWEEN

`BETWEEN low AND high` is inclusive on both ends. Equivalent to `>= low AND <= high`.

```sql
-- Products priced between $10 and $50 inclusive
SELECT * FROM products WHERE price BETWEEN 10 AND 50;

-- Orders placed in Q1 2026
SELECT * FROM orders
WHERE placed_at BETWEEN '2026-01-01 00:00:00' AND '2026-03-31 23:59:59';

-- NOT BETWEEN
SELECT * FROM products WHERE price NOT BETWEEN 10 AND 50;
```

---

## LIKE — Pattern Matching

`LIKE` matches strings against a pattern.

| Wildcard | Meaning                          |
|----------|----------------------------------|
| `%`      | Any sequence of zero or more characters |
| `_`      | Exactly one character            |

Pattern matching is case-sensitive by default. Use `CITEXT` columns or `ILIKE`
for case-insensitive matching.

```sql
-- Emails from example.com
SELECT * FROM users WHERE email LIKE '%@example.com';

-- Names starting with 'Al'
SELECT * FROM users WHERE name LIKE 'Al%';

-- Exactly 5-character codes
SELECT * FROM products WHERE sku LIKE '_____';

-- NOT LIKE
SELECT * FROM users WHERE email NOT LIKE '%@test.%';

-- Escape a literal %
SELECT * FROM products WHERE description LIKE '50\% off' ESCAPE '\';
```

---

## IN — Membership Test

`IN` checks whether a value matches any element in a list.

```sql
-- Multiple status values
SELECT * FROM orders WHERE status IN ('pending', 'paid', 'shipped');

-- Numeric list
SELECT * FROM products WHERE category_id IN (1, 3, 7);

-- NOT IN
SELECT * FROM orders WHERE status NOT IN ('cancelled', 'refunded');
```

> `NOT IN (list)` returns UNKNOWN (no rows) if any element in the list is NULL.
> Use `NOT EXISTS` or explicit NULL checks when the list may contain NULLs.

```sql
-- Safe: explicit list with no NULLs
SELECT * FROM orders WHERE status NOT IN ('cancelled', 'refunded');

-- Dangerous if user_id can be NULL:
SELECT * FROM orders WHERE user_id NOT IN (SELECT id FROM banned_users);
-- If banned_users contains even one NULL user, this returns 0 rows!
-- Safe alternative:
SELECT * FROM orders o
WHERE NOT EXISTS (
    SELECT 1 FROM banned_users b WHERE b.id = o.user_id AND b.id IS NOT NULL
);
```

---

## Scalar Functions

### Numeric Functions

| Function          | Description                              | Example                |
|-------------------|------------------------------------------|------------------------|
| `ABS(x)`          | Absolute value                           | `ABS(-5)` → `5`       |
| `CEIL(x)`         | Ceiling (round up)                       | `CEIL(1.2)` → `2`     |
| `FLOOR(x)`        | Floor (round down)                       | `FLOOR(1.9)` → `1`    |
| `ROUND(x, d)`     | Round to `d` decimal places              | `ROUND(3.14159, 2)` → `3.14` |
| `MOD(x, y)`       | Modulo                                   | `MOD(10, 3)` → `1`    |
| `POWER(x, y)`     | x raised to the power y                  | `POWER(2, 8)` → `256` |
| `SQRT(x)`         | Square root                              | `SQRT(16)` → `4`      |

### String Functions

| Function              | Description                            | Example                         |
|-----------------------|----------------------------------------|---------------------------------|
| `LENGTH(s)`           | Number of bytes                        | `LENGTH('hello')` → `5`        |
| `CHAR_LENGTH(s)`      | Number of UTF-8 characters             | `CHAR_LENGTH('café')` → `4`    |
| `UPPER(s)`            | Convert to uppercase                   | `UPPER('hello')` → `'HELLO'`   |
| `LOWER(s)`            | Convert to lowercase                   | `LOWER('HELLO')` → `'hello'`   |
| `TRIM(s)`             | Remove leading and trailing spaces     | `TRIM('  hi  ')` → `'hi'`      |
| `LTRIM(s)`            | Remove leading spaces                  | —                               |
| `RTRIM(s)`            | Remove trailing spaces                 | —                               |
| `SUBSTR(s, pos, len)` | Substring from position (1-indexed)    | `SUBSTR('hello', 2, 3)` → `'ell'` |
| `CONCAT(a, b, ...)`   | Concatenate strings                    | `CONCAT('foo', 'bar')` → `'foobar'` |
| `REPLACE(s, from, to)`| Replace all occurrences               | `REPLACE('aabbcc', 'bb', 'X')` → `'aaXcc'` |
| `LPAD(s, n, pad)`     | Pad on the left to length n            | `LPAD('42', 5, '0')` → `'00042'` |
| `RPAD(s, n, pad)`     | Pad on the right to length n           | —                               |

### Conditional Functions

| Function                          | Description                                        |
|-----------------------------------|----------------------------------------------------|
| `COALESCE(a, b, ...)`             | Return first non-NULL argument                     |
| `NULLIF(a, b)`                    | Return NULL if a = b, otherwise return a           |
| `IIF(cond, then, else)`           | Inline if-then-else                               |
| `CASE WHEN ... THEN ... END`      | General conditional expression                     |

```sql
-- COALESCE: display a fallback when the column is NULL
SELECT name, COALESCE(phone, 'N/A') AS contact FROM users;

-- NULLIF: convert 'unknown' to NULL (for aggregate functions to ignore)
SELECT AVG(NULLIF(rating, 0)) AS avg_rating FROM products;

-- CASE: categorize order size
SELECT
    id,
    total,
    CASE
        WHEN total < 50   THEN 'small'
        WHEN total < 200  THEN 'medium'
        WHEN total < 1000 THEN 'large'
        ELSE                   'enterprise'
    END AS order_size
FROM orders;
```

---

## CASE WHEN — Conditional Expressions

`CASE WHEN` is a general-purpose conditional expression that can appear anywhere an
expression is valid: SELECT projections, WHERE clauses, ORDER BY, GROUP BY, HAVING,
and as arguments to aggregate functions.

AxiomDB supports two forms: **searched CASE** (any boolean condition per branch) and
**simple CASE** (equality comparison against a single value).

### Searched CASE

Evaluates each `WHEN` condition left to right and returns the `THEN` value of the
first condition that is TRUE. If no condition matches and an `ELSE` is present, the
`ELSE` value is returned. If no condition matches and there is no `ELSE`, the result
is NULL.

```sql
CASE
    WHEN condition1 THEN result1
    WHEN condition2 THEN result2
    ...
    [ELSE default_result]
END
```

```sql
-- Categorize orders by total amount
SELECT
    id,
    total,
    CASE
        WHEN total < 50    THEN 'small'
        WHEN total < 200   THEN 'medium'
        WHEN total < 1000  THEN 'large'
        ELSE                    'enterprise'
    END AS order_size
FROM orders;
```

```sql
-- Compute a human-readable status label, including NULL handling
SELECT
    id,
    CASE
        WHEN shipped_at IS NULL AND status = 'paid' THEN 'awaiting shipment'
        WHEN shipped_at IS NOT NULL                 THEN 'shipped'
        WHEN status = 'cancelled'                   THEN 'cancelled'
        ELSE                                             'unknown'
    END AS display_status
FROM orders;
```

### Simple CASE

Compares a single expression against a list of values. Equivalent to a searched CASE
using `=` for each `WHEN` comparison.

```sql
CASE expression
    WHEN value1 THEN result1
    WHEN value2 THEN result2
    ...
    [ELSE default_result]
END
```

```sql
-- Map status codes to display labels
SELECT
    id,
    CASE status
        WHEN 'pending'   THEN 'Pending Payment'
        WHEN 'paid'      THEN 'Paid'
        WHEN 'shipped'   THEN 'Shipped'
        WHEN 'delivered' THEN 'Delivered'
        WHEN 'cancelled' THEN 'Cancelled'
        ELSE                  'Unknown'
    END AS status_label
FROM orders;
```

### NULL Semantics in CASE

In a **searched CASE**, a `WHEN` condition that evaluates to UNKNOWN (NULL in boolean
context) is treated the same as FALSE — it does not match, and evaluation continues
to the next branch. This means a NULL condition never triggers a `THEN` clause.

In a **simple CASE**, the comparison `expression = value` uses standard SQL equality,
which returns UNKNOWN when either side is NULL. As a result, `WHEN NULL` never matches.
Use a searched CASE with `IS NULL` to handle NULL values explicitly.

```sql
-- Simple CASE: WHEN NULL never matches (NULL <> NULL in equality)
SELECT CASE NULL WHEN NULL THEN 'matched' ELSE 'no match' END;
-- Result: 'no match'

-- Correct way to handle NULL in a simple CASE: use searched form
SELECT
    CASE
        WHEN status IS NULL THEN 'no status'
        ELSE status
    END AS safe_status
FROM orders;
```

### CASE in ORDER BY — Controlled Sort Order

`CASE` can produce a sort key that cannot be expressed with a single column reference.

```sql
-- Sort orders: unshipped first (status='paid'), then by recency
SELECT id, status, placed_at
FROM orders
ORDER BY
    CASE WHEN status = 'paid' AND shipped_at IS NULL THEN 0 ELSE 1 END,
    placed_at DESC;
```

### CASE in GROUP BY — Dynamic Grouping

```sql
-- Group products by price tier and count items per tier
SELECT
    CASE
        WHEN price < 25   THEN 'budget'
        WHEN price < 100  THEN 'mid-range'
        ELSE                   'premium'
    END      AS tier,
    COUNT(*) AS product_count,
    AVG(price) AS avg_price
FROM products
WHERE deleted_at IS NULL
GROUP BY
    CASE
        WHEN price < 25   THEN 'budget'
        WHEN price < 100  THEN 'mid-range'
        ELSE                   'premium'
    END
ORDER BY avg_price;
```

> **Design note:** AxiomDB evaluates `CASE` expressions during row processing in the
> executor's expression evaluator. Short-circuit evaluation guarantees that branches
> after the first matching `WHEN` are never evaluated, which prevents side effects
> (e.g., division by zero in an unreachable branch).

---

### Date / Time Functions

| Function                        | Description                              |
|---------------------------------|------------------------------------------|
| `NOW()`                         | Current timestamp with timezone          |
| `CURRENT_DATE`                  | Current date (no time)                   |
| `CURRENT_TIME`                  | Current time (no date)                   |
| `CURRENT_TIMESTAMP`             | Alias for `NOW()`                        |
| `DATE_TRUNC('unit', ts)`        | Truncate to year/month/day/hour/...      |
| `DATE_PART('unit', ts)`         | Extract year, month, day, hour, ...      |
| `AGE(ts1, ts2)`                 | Interval between two timestamps          |

```sql
-- Current timestamp
SELECT NOW();

-- Truncate to month (for GROUP BY month)
SELECT DATE_TRUNC('month', placed_at) AS month, COUNT(*) FROM orders GROUP BY 1;

-- Extract year from a date
SELECT DATE_PART('year', created_at) AS signup_year FROM users;
```

### Aggregate Functions

| Function          | Description                       | NULL behavior           |
|-------------------|-----------------------------------|-------------------------|
| `COUNT(*)`        | Count all rows in the group       | Includes NULL rows      |
| `COUNT(col)`      | Count non-NULL values in col      | Excludes NULL values    |
| `SUM(col)`        | Sum of non-NULL values            | Returns NULL if all NULL |
| `AVG(col)`        | Arithmetic mean of non-NULL values| Returns NULL if all NULL |
| `MIN(col)`        | Minimum non-NULL value            | Returns NULL if all NULL |
| `MAX(col)`        | Maximum non-NULL value            | Returns NULL if all NULL |

```sql
SELECT
    COUNT(*)        AS total_rows,
    COUNT(email)    AS rows_with_email,   -- excludes NULL
    SUM(total)      AS gross_revenue,
    AVG(total)      AS avg_order_value,
    MIN(placed_at)  AS first_order,
    MAX(placed_at)  AS last_order
FROM orders
WHERE status != 'cancelled';
```
