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

### String Concatenation — `||`

The `||` operator concatenates two string values. It is the SQL-standard alternative
to `CONCAT()` and works in any expression context.

```sql
-- Build a full name from two columns
SELECT first_name || ' ' || last_name AS full_name FROM users;

-- Append a suffix
SELECT sku || '-v2' AS new_sku FROM products;

-- NULL propagates: if either operand is NULL the result is NULL
SELECT 'hello' || NULL;   -- NULL
```

Use `COALESCE` to guard against NULL operands:

```sql
SELECT COALESCE(first_name, '') || ' ' || COALESCE(last_name, '') AS full_name
FROM users;
```

### CAST — Explicit Type Conversion

`CAST(expr AS type)` converts a value to the specified type. Use it when an implicit
coercion would be rejected in strict mode (the default).

```sql
-- Text-to-number: always works when the text is a valid number
SELECT CAST('42' AS INT);        -- 42
SELECT CAST('3.14' AS REAL);     -- 3.14
SELECT CAST('100' AS BIGINT);    -- 100

-- Use CAST to store a text literal in a numeric column
INSERT INTO users (age) VALUES (CAST('30' AS INT));
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Current Limitation</span>
<code>CAST(numeric AS TEXT)</code> — converting an integer or real value to text — is not
supported in the current release and raises <code>22018 invalid_character_value_for_cast</code>.
Use application-side formatting or wait for Phase 5 (full coercion matrix). The supported
direction is <strong>text → number</strong>, not number → text.
</div>
</div>

**Supported CAST pairs (Phase 4.16):**

| From    | To                        | Notes                                 |
|---------|---------------------------|---------------------------------------|
| `TEXT`  | `INT`, `BIGINT`           | Entire string must be a valid integer |
| `TEXT`  | `REAL`                    | Entire string must be a valid float   |
| `TEXT`  | `DECIMAL`                 | Entire string must be a valid decimal |
| `INT`   | `BIGINT`, `REAL`, `DECIMAL` | Widening — always succeeds          |
| `BIGINT`| `REAL`, `DECIMAL`         | Widening — always succeeds            |
| `NULL`  | any                       | Always returns NULL                   |

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

#### Current date / time

| Function            | Return type | Description                        |
|---------------------|-------------|------------------------------------|
| `NOW()`             | TIMESTAMP   | Current timestamp (UTC)            |
| `CURRENT_DATE`      | DATE        | Current date (no time)             |
| `CURRENT_TIME`      | TIMESTAMP   | Current time (no date)             |
| `CURRENT_TIMESTAMP` | TIMESTAMP   | Alias for `NOW()`                  |
| `UNIX_TIMESTAMP()`  | BIGINT      | Current time as Unix seconds       |

#### Date component extractors

| Function         | Returns | Description                                       |
|------------------|---------|---------------------------------------------------|
| `year(val)`      | INT     | Year (e.g. `2025`)                                |
| `month(val)`     | INT     | Month `1–12`                                      |
| `day(val)`       | INT     | Day of month `1–31`                               |
| `hour(val)`      | INT     | Hour `0–23`                                       |
| `minute(val)`    | INT     | Minute `0–59`                                     |
| `second(val)`    | INT     | Second `0–59`                                     |
| `DATEDIFF(a, b)` | INT     | Days between two dates (`a - b`)                  |

`val` accepts `DATE`, `TIMESTAMP`, or a text string coercible to a date.
Returns `NULL` if the input is `NULL` or not a valid date type.

```sql
SELECT year(NOW()),  month(NOW()),  day(NOW());   -- e.g. 2025, 3, 25
SELECT hour(NOW()),  minute(NOW()), second(NOW()); -- e.g. 14, 30, 45
```

#### DATE_FORMAT — format a date as text

```sql
DATE_FORMAT(ts, format_string) → TEXT
```

Formats a `DATE` or `TIMESTAMP` value using MySQL-compatible format specifiers.
Returns `NULL` if either argument is `NULL` or the format string is empty.

| Specifier | Description              | Example   |
|-----------|--------------------------|-----------|
| `%Y`      | 4-digit year             | `2025`    |
| `%y`      | 2-digit year             | `25`      |
| `%m`      | Month `01–12`            | `03`      |
| `%c`      | Month `1–12` (no pad)    | `3`       |
| `%M`      | Full month name          | `March`   |
| `%b`      | Abbreviated month name   | `Mar`     |
| `%d`      | Day `01–31`              | `05`      |
| `%e`      | Day `1–31` (no pad)      | `5`       |
| `%H`      | Hour `00–23`             | `14`      |
| `%h`      | Hour `01–12` (12-hour)   | `02`      |
| `%i`      | Minute `00–59`           | `30`      |
| `%s`/`%S` | Second `00–59`           | `45`      |
| `%p`      | AM / PM                  | `PM`      |
| `%W`      | Full weekday name        | `Tuesday` |
| `%a`      | Abbreviated weekday      | `Tue`     |
| `%j`      | Day of year `001–366`    | `084`     |
| `%w`      | Weekday `0=Sun…6=Sat`    | `2`       |
| `%T`      | Time `HH:MM:SS` (24h)    | `14:30:45`|
| `%r`      | Time `HH:MM:SS AM/PM`    | `02:30:45 PM` |
| `%%`      | Literal `%`              | `%`       |

Unknown specifiers are passed through literally (`%X` → `%X`).

```sql
-- Format a stored timestamp as ISO date
SELECT DATE_FORMAT(created_at, '%Y-%m-%d') FROM orders;
-- '2025-03-25'

-- European date format
SELECT DATE_FORMAT(NOW(), '%d/%m/%Y');
-- '25/03/2025'

-- Full datetime
SELECT DATE_FORMAT(NOW(), '%Y-%m-%d %H:%i:%s');
-- '2025-03-25 14:30:45'

-- NULL input → NULL output
SELECT DATE_FORMAT(NULL, '%Y-%m-%d');  -- NULL
```

#### STR_TO_DATE — parse a date string

```sql
STR_TO_DATE(str, format_string) → DATE | TIMESTAMP | NULL
```

Parses a text string into a date or timestamp using MySQL-compatible format
specifiers (same table as `DATE_FORMAT` above).

- Returns `DATE` if the format contains only date components.
- Returns `TIMESTAMP` if the format contains any time components (`%H`, `%i`, `%s`).
- Returns `NULL` on any parse failure — **never raises an error** (MySQL behavior).
- Returns `NULL` if either argument is `NULL`.

**2-digit year rule (`%y`):** `00–69` → `2000–2069`; `70–99` → `1970–1999`.

```sql
-- Parse ISO date → Value::Date
SELECT STR_TO_DATE('2025-03-25', '%Y-%m-%d');

-- Parse European date → Value::Date
SELECT STR_TO_DATE('25/03/2025', '%d/%m/%Y');

-- Parse datetime → Value::Timestamp
SELECT STR_TO_DATE('2025-03-25 14:30:00', '%Y-%m-%d %H:%i:%s');

-- Extract components from a parsed date
SELECT year(STR_TO_DATE('2025-03-25', '%Y-%m-%d'));  -- 2025

-- Round-trip: parse then format
SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%d/%m/%Y');
-- '25/03/2025'

-- Invalid date → NULL (Feb 30 does not exist)
SELECT STR_TO_DATE('2025-02-30', '%Y-%m-%d');  -- NULL

-- Bad format → NULL (never an error)
SELECT STR_TO_DATE('not-a-date', '%Y-%m-%d');  -- NULL
```

#### FIND_IN_SET — search a comma-separated list

```sql
FIND_IN_SET(needle, csv_list) → INT
```

Returns the **1-indexed** position of `needle` in the comma-separated string
`csv_list`. Returns `0` if not found. Comparison is **case-insensitive**.
Returns `NULL` if either argument is `NULL`.

```sql
SELECT FIND_IN_SET('b', 'a,b,c');   -- 2
SELECT FIND_IN_SET('B', 'a,b,c');   -- 2  (case-insensitive)
SELECT FIND_IN_SET('z', 'a,b,c');   -- 0  (not found)
SELECT FIND_IN_SET('a', '');        -- 0  (empty list)
SELECT FIND_IN_SET(NULL, 'a,b,c'); -- NULL
```

Useful for querying rows where a column holds a comma-separated tag list:

```sql
SELECT * FROM articles WHERE FIND_IN_SET('rust', tags) > 0;
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
DATE_FORMAT and STR_TO_DATE map MySQL format specifiers manually rather than
delegating to chrono's own format strings. This is intentional: MySQL's
<code>%m</code> means zero-padded month but chrono uses <code>%m</code> differently.
Manual mapping guarantees exact MySQL semantics for all 18 specifiers including
<code>%T</code>, <code>%r</code>, and 2-digit year rules, without risking divergence
from the underlying library's format grammar.
</div>
</div>

```sql
-- DATE_TRUNC and DATE_PART (PostgreSQL-compatible aliases)
SELECT DATE_TRUNC('month', placed_at) AS month, COUNT(*) FROM orders GROUP BY 1;
SELECT DATE_PART('year', created_at) AS signup_year FROM users;
```

### Session Functions {#session-functions}

Session functions return state that is specific to the current connection and is
not visible to other sessions.

| Function                | Return type | Description                                      |
|-------------------------|-------------|--------------------------------------------------|
| `LAST_INSERT_ID()`      | `BIGINT`    | ID generated by the most recent AUTO_INCREMENT INSERT in this session |
| `lastval()`             | `BIGINT`    | PostgreSQL-compatible alias for `LAST_INSERT_ID()` |
| `version()`             | `TEXT`      | Server version string, e.g. `'8.0.36-AxiomDB-0.1.0'` |
| `current_user()`        | `TEXT`      | Authenticated username of the current connection |
| `session_user()`        | `TEXT`      | Alias for `current_user()` |
| `current_database()`    | `TEXT`      | Name of the current database (`'axiomdb'`) |
| `database()`            | `TEXT`      | MySQL-compatible alias for `current_database()` |

```sql
-- Commonly called by ORMs on connect to verify server identity
SELECT version();             -- '8.0.36-AxiomDB-0.1.0'
SELECT current_user();        -- 'root'
SELECT current_database();    -- 'axiomdb'
```

**Semantics:**

- Returns `0` if no `AUTO_INCREMENT` INSERT has occurred in the current session.
- For a single-row INSERT, returns the generated ID.
- For a multi-row INSERT (`INSERT INTO t VALUES (...), (...), ...`), returns the
  ID generated for the **first** row of the batch (MySQL semantics). Subsequent
  rows receive consecutive IDs.
- Inserting an explicit non-NULL value into an `AUTO_INCREMENT` column does **not**
  advance the sequence and does **not** update `LAST_INSERT_ID()`.
- `TRUNCATE TABLE` resets the sequence to 1 but does **not** change the session's
  `LAST_INSERT_ID()` value.

```sql
CREATE TABLE items (id BIGINT PRIMARY KEY AUTO_INCREMENT, name TEXT);

-- Single-row INSERT
INSERT INTO items (name) VALUES ('Widget');
SELECT LAST_INSERT_ID();      -- 1
SELECT lastval();             -- 1

-- Multi-row INSERT
INSERT INTO items (name) VALUES ('Gadget'), ('Gizmo'), ('Doohickey');
SELECT LAST_INSERT_ID();      -- 2 (first generated ID in the batch)

-- Explicit value — does not change LAST_INSERT_ID()
INSERT INTO items (id, name) VALUES (99, 'Special');
SELECT LAST_INSERT_ID();      -- still 2

-- Use inside the same statement (e.g., insert a child row)
INSERT INTO orders (user_id, item_id) VALUES (42, LAST_INSERT_ID());
```

---

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

### GROUP_CONCAT — String Aggregation

`GROUP_CONCAT` concatenates non-NULL values across the rows of a group into a single
string. It is MySQL's most widely-used aggregate function for collecting tags, roles,
categories, and comma-separated lists without a client-side join.

`string_agg(expr, separator)` is the PostgreSQL-compatible alias.

#### Syntax

```sql
GROUP_CONCAT([DISTINCT] expr [ORDER BY col [ASC|DESC], ...] [SEPARATOR 'str'])

string_agg(expr, separator)
```

| Clause      | Default | Description |
|-------------|---------|-------------|
| `DISTINCT`  | off     | Deduplicate values before concatenating |
| `ORDER BY`  | none    | Sort values within the group before joining |
| `SEPARATOR` | `','`   | String inserted between values |

#### Behavior

- NULL values are **skipped** — they do not appear in the result and do not add a separator.
- An empty group (no rows) or a group where every value is NULL returns `NULL`.
- A single value returns that value with no separator added.
- Result is truncated to **1 MB** (1,048,576 bytes) maximum.

```sql
-- Basic: comma-separated tags per post
SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC)
FROM post_tags
GROUP BY post_id;
-- post 1 → 'async,db,rust'
-- post 2 → 'rust,web'
-- post 3 (all NULL tags) → NULL

-- Custom separator
SELECT GROUP_CONCAT(tag ORDER BY tag ASC SEPARATOR ' | ')
FROM post_tags
WHERE post_id = 1;
-- → 'async | db | rust'

-- DISTINCT: deduplicate before joining
SELECT GROUP_CONCAT(DISTINCT tag ORDER BY tag ASC)
FROM tags;
-- Duplicate 'rust' rows → 'async,db,rust' (appears once)

-- string_agg PostgreSQL alias
SELECT string_agg(tag, ', ')
FROM post_tags
WHERE post_id = 2;
-- → 'rust, web' (or 'web, rust' — insertion order)

-- HAVING on a GROUP_CONCAT result
SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC) AS tags
FROM post_tags
GROUP BY post_id
HAVING GROUP_CONCAT(tag ORDER BY tag ASC) LIKE '%rust%';
-- Only posts that have the 'rust' tag

-- Collect integers as text
SELECT GROUP_CONCAT(n ORDER BY n ASC) FROM nums;
-- 1, 2, 3 → '1,2,3'
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip — MySQL compatibility</span>
AxiomDB supports the full MySQL <code>GROUP_CONCAT</code> syntax including <code>DISTINCT</code>,
multi-column <code>ORDER BY</code>, and the <code>SEPARATOR</code> keyword. MySQL codebases
that use <code>GROUP_CONCAT</code> for tags or role lists migrate without modification.
</div>
</div>

---

## BLOB / Binary Functions

AxiomDB stores binary data as the `BLOB` / `BYTES` type and provides functions for
encoding, decoding, and measuring binary values.

| Function | Returns | Description |
|---|---|---|
| `FROM_BASE64(text)` | `BLOB` | Decode standard base64 → raw bytes. Returns `NULL` on invalid input. |
| `TO_BASE64(blob)` | `TEXT` | Encode raw bytes → base64 string. Also accepts `TEXT` and `UUID`. |
| `OCTET_LENGTH(value)` | `INT` | Byte length of a `BLOB`, `TEXT` (UTF-8 bytes), or `UUID` (always 16). |
| `ENCODE(blob, fmt)` | `TEXT` | Encode bytes as `'base64'` or `'hex'`. |
| `DECODE(text, fmt)` | `BLOB` | Decode `'base64'` or `'hex'` text → raw bytes. |

### Usage examples

```sql
-- Store binary data encoded as base64
INSERT INTO files (name, data)
VALUES ('logo.png', FROM_BASE64('iVBORw0KGgoAAAANSUhEUgAA...'));

-- Retrieve as base64 for transport
SELECT name, TO_BASE64(data) AS data_b64 FROM files;

-- Check byte size of a blob
SELECT name, OCTET_LENGTH(data) AS size_bytes FROM files;

-- Hex encoding (PostgreSQL / MySQL ENCODE style)
SELECT ENCODE(data, 'hex') FROM files;          -- → 'deadbeef...'
SELECT DECODE('deadbeef', 'hex');               -- → binary bytes

-- OCTET_LENGTH vs LENGTH for text
SELECT LENGTH('héllo');       -- 5 (characters)
SELECT OCTET_LENGTH('héllo'); -- 6 (UTF-8 bytes: é = 2 bytes)
```

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Tip — Base64 for JSON APIs</span>
When returning binary data through a JSON API, wrap the column with
<code>TO_BASE64(data)</code> to get a transport-safe string. The client reverses it
with <code>FROM_BASE64()</code> on INSERT. This pattern avoids binary encoding
issues in MySQL wire protocol text mode.
</div>
</div>

---

## UUID Functions

AxiomDB generates and validates UUIDs server-side. No application-level library
needed — the DB handles UUID primary keys directly.

| Function | Returns | Description |
|---|---|---|
| `gen_random_uuid()` | `UUID` | UUID v4 — 122 random bits. Aliases: `uuid_generate_v4()`, `random_uuid()`, `newid()` |
| `uuid_generate_v7()` | `UUID` | UUID v7 — 48-bit unix timestamp + random bits. Alias: `uuid7()` |
| `is_valid_uuid(text)` | `BOOL` | `TRUE` if text is a valid UUID string (hyphenated or compact). Alias: `is_uuid()`. Returns `NULL` if arg is `NULL`. |

### Usage

```sql
-- Auto-generate a UUID primary key at insert time
CREATE TABLE events (
    id   UUID NOT NULL,
    name TEXT NOT NULL
);

INSERT INTO events (id, name)
VALUES (gen_random_uuid(), 'page_view');

-- Use UUID v7 for tables that benefit from time-ordered inserts
INSERT INTO events (id, name)
VALUES (uuid_generate_v7(), 'checkout');

-- Validate an incoming UUID string before inserting
SELECT is_valid_uuid('550e8400-e29b-41d4-a716-446655440000');  -- TRUE
SELECT is_valid_uuid('not-a-uuid');                             -- FALSE
SELECT is_valid_uuid(NULL);                                     -- NULL
```

### UUID v4 vs UUID v7 — which to use?

```sql
-- UUID v4: fully random, best for security-sensitive IDs
-- Format: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx (122 random bits)
SELECT gen_random_uuid();
-- → 'f47ac10b-58cc-4372-a567-0e02b2c3d479'

-- UUID v7: time-ordered prefix, best for primary keys on B+ Tree indexes
-- Format: [48-bit ms timestamp]-[12-bit rand]-[62-bit rand]
SELECT uuid_generate_v7();
-- → '018e2e3a-1234-7abc-8def-0123456789ab'
--    ^^^^^^^^^^^ always increasing
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — UUID v7 for Primary Keys</span>
UUID v4 generates random 122-bit keys. When used as a B+ Tree primary key, each insert lands at a random leaf position, causing frequent page splits and poor cache locality. UUID v7 embeds a 48-bit millisecond timestamp as a prefix — inserts are nearly always at the rightmost leaf, eliminating most splits and matching the sequential-insert performance of <code>AUTO_INCREMENT</code>. For tables receiving hundreds of inserts per second, UUID v7 can be 2-5× faster than v4 for write throughput.
</div>
</div>
