# Error Reference

AxiomDB returns structured errors with a SQLSTATE code, a human-readable message,
and optional detail fields. Understanding these codes allows applications to handle
specific failure scenarios correctly (for example: catching a uniqueness violation
to show a "email already taken" message rather than a generic crash page).

---

## Error Format

Every error from AxiomDB is represented as an `ErrorResponse` struct with these fields:

| Field | Type | Always present? | Description |
|---|---|---|---|
| `sqlstate` | string (5 chars) | **Yes** | SQLSTATE code for programmatic handling (e.g. `"23505"`) |
| `severity` | string | **Yes** | `"ERROR"`, `"WARNING"`, or `"NOTICE"` |
| `message` | string | **Yes** | Short human-readable description. Do not parse this — use `sqlstate` |
| `detail` | string | Sometimes | Extended context about the failure (offending value, referenced row) |
| `hint` | string | Sometimes | Actionable suggestion for how to fix the error |
| `position` | integer | Future | Byte offset of the error in the SQL query (Phase 4.25b) |

```json
{
  "sqlstate": "23505",
  "severity": "ERROR",
  "message": "unique key violation on users.email",
  "hint": "A row with the same email already exists in users. Use INSERT ... ON CONFLICT to handle duplicates."
}
```

```json
{
  "sqlstate": "23503",
  "severity": "ERROR",
  "message": "foreign key violation: orders.user_id = 999",
  "detail": "Key (user_id)=(999) is not present in table users.",
  "hint": "Insert the referenced row first, or use ON DELETE CASCADE."
}
```

**Always use `sqlstate` for programmatic handling.** The `message` text may change between versions; SQLSTATE codes are stable.

When using the MySQL wire protocol, the error is delivered as a MySQL error packet
with the SQLSTATE code in the `sql_state` field (5 bytes following the `#` marker).

---

## Integrity Constraint Violations (Class 23)

These errors indicate that an INSERT, UPDATE, or DELETE violated a declared
constraint. The application should handle them and return a user-facing message.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Constraint Enforcement Status (Phase 4.16)</span>
The following constraints are <strong>parsed and stored in the schema</strong> but are
<strong>not yet enforced at INSERT/UPDATE time</strong>:
<ul>
<li><strong>NOT NULL</strong> — declared columns accept NULL without error</li>
<li><strong>UNIQUE</strong> — duplicate values are allowed</li>
<li><strong>CHECK</strong> — expressions are not evaluated at write time</li>
</ul>
As a result, <code>23502</code>, <code>23505</code>, and <code>23514</code> are not raised
by DML in the current release. Enforcement will be added in a future phase.
<strong>PRIMARY KEY uniqueness is enforced</strong> via the B+ tree index.
</div>
</div>

### 23505 — unique_violation

A row with the same value already exists in a column or set of columns declared
UNIQUE or PRIMARY KEY.

```sql
CREATE TABLE users (email TEXT NOT NULL UNIQUE);
INSERT INTO users VALUES ('alice@example.com');
INSERT INTO users VALUES ('alice@example.com');  -- ERROR 23505
```

**Typical application response:** Show "An account with this email already exists."

```python
try:
    db.execute("INSERT INTO users (email) VALUES (?)", [email])
except AxiomDbError as e:
    if e.sqlstate == '23505':
        return {"error": "Email already taken"}
    raise
```

### 23503 — foreign_key_violation

#### Child insert / update — parent key does not exist

An INSERT or UPDATE references a value in the FK column that has no matching row
in the parent table.

```sql
INSERT INTO orders (user_id, total) VALUES (99999, 100);
-- ERROR 23503: Foreign key constraint fails: 'orders.user_id' = '99999'
```

**Typical response:** Validate that the referenced entity exists before inserting,
or surface "Referenced record not found."

#### Parent delete — children still reference it (RESTRICT / NO ACTION)

A DELETE on the parent table was blocked because child rows reference the row
being deleted and the FK action is `RESTRICT` or `NO ACTION` (the default).

```sql
-- orders.user_id REFERENCES users(id) ON DELETE RESTRICT
DELETE FROM users WHERE id = 1;
-- ERROR 23503: foreign key constraint "fk_orders_user": orders.user_id references this row
```

**Typical response:** Either delete child rows first, use `ON DELETE CASCADE`, or
prevent parent deletion in the application layer.

#### Cascade depth exceeded

A chain of `ON DELETE CASCADE` constraints exceeded the maximum depth of 10 levels.

```sql
-- If table chain A→B→C→...→K (11 levels all with CASCADE) and you delete from A:
DELETE FROM a WHERE id = 1;
-- ERROR 23503: foreign key cascade depth exceeded limit of 10
```

**Typical response:** Restructure the schema to reduce cascade depth, or perform
the deletes manually level-by-level.

#### SET NULL on a NOT NULL column

`ON DELETE SET NULL` is defined on a foreign key column that was declared `NOT NULL`.

```sql
-- orders.user_id is NOT NULL, but ON DELETE SET NULL is declared
DELETE FROM users WHERE id = 1;
-- ERROR 23503: cannot set FK column orders.user_id to NULL: column is NOT NULL
```

**Typical response:** Either remove the `NOT NULL` constraint from the FK column,
or change the action to `ON DELETE RESTRICT` or `ON DELETE CASCADE`.

### 23502 — not_null_violation

An INSERT or UPDATE attempted to store NULL in a NOT NULL column.

```sql
INSERT INTO users (name, email) VALUES (NULL, 'bob@example.com');
-- ERROR 23502: null value in column "name" violates not-null constraint
```

**Typical application response:** Validate required fields on the client before
submitting.

### 23514 — check_violation

A row failed a CHECK constraint.

```sql
INSERT INTO products (name, price) VALUES ('Widget', -5.00);
-- ERROR 23514: new row for relation "products" violates check constraint "chk_price_positive"
```

---

## Cardinality Errors (Class 21)

### 21000 — cardinality_violation

A scalar subquery returned more than one row. Scalar subqueries (a `SELECT` used
where a single value is expected) must return exactly one row. Zero rows yield
`NULL`; more than one row is an error.

```sql
-- Suppose users contains Alice and Bob
SELECT (SELECT name FROM users) AS single_name FROM orders;
-- ERROR 21000: subquery must return exactly one row, but returned 2 rows
```

**Fix:** add a `WHERE` condition that makes the result unique, or use `LIMIT 1`
if you intentionally want only the first row:

```sql
-- Safe: guaranteed single row via primary key
SELECT (SELECT name FROM users WHERE id = o.user_id) AS customer_name
FROM orders o;

-- Safe: explicit LIMIT 1 when you want "any one" result
SELECT (SELECT name FROM users ORDER BY created_at LIMIT 1) AS oldest_user
FROM config;
```

```python
try:
    db.execute("SELECT (SELECT name FROM users) FROM orders")
except AxiomDbError as e:
    if e.sqlstate == '21000':
        # The subquery returned multiple rows — add a WHERE clause
        ...
```

---

## Undefined Object Errors (Class 42)

These errors indicate a reference to an object (table, column, index) that does
not exist in the catalog. They are typically programming errors caught in development.

### 42P01 — undefined_table

A statement referenced a table or view that does not exist.

```sql
SELECT * FROM nonexistent_table;
-- ERROR 42P01: relation "nonexistent_table" does not exist
```

### 42703 — undefined_column

A statement referenced a column that does not exist in the specified table.

```sql
SELECT typo_column FROM users;
-- ERROR 42703: column "typo_column" does not exist in table "users"
```

### 42P07 — duplicate_table

`CREATE TABLE` was called for a table that already exists (without `IF NOT EXISTS`).

```sql
CREATE TABLE users (...);
CREATE TABLE users (...);
-- ERROR 42P07: relation "users" already exists
```

### 42701 — duplicate_column

`ALTER TABLE ... ADD COLUMN` was called for a column that already exists in
the table.

```sql
CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT NOT NULL);
ALTER TABLE users ADD COLUMN email TEXT;
-- ERROR 42701: column "email" already exists in table "users"
```

**Fix:** Use a different column name, or check the current schema with
`DESCRIBE users` before adding the column.

### 42702 — ambiguous_column

An unqualified column name appears in multiple tables in the FROM clause.

```sql
-- Both users and orders have a column named "id"
SELECT id FROM users JOIN orders ON orders.user_id = users.id;
-- ERROR 42702: column reference "id" is ambiguous

-- Fix: qualify the column
SELECT users.id FROM users JOIN orders ON orders.user_id = users.id;
```

---

## Transaction Errors (Class 40)

### 40001 — serialization_failure

A concurrent write conflict was detected. The transaction must be retried.

```sql
-- Two transactions try to update the same row simultaneously.
-- The second one receives:
-- ERROR 40001: could not serialize access due to concurrent update
```

**The application must catch this and retry the transaction.** This is normal and
expected behavior under high concurrency, not a bug.

### 40P01 — deadlock_detected

Two transactions are each waiting for a lock held by the other.

```sql
-- Txn A holds lock on row 1, waiting for row 2
-- Txn B holds lock on row 2, waiting for row 1
-- → AxiomDB detects the cycle and aborts one transaction with 40P01
-- ERROR 40P01: deadlock detected
```

**Prevention:** Access rows in a consistent order across all transactions. If you
always acquire locks on (accounts with lower id) before (accounts with higher id),
deadlocks cannot form between two such transactions.

---

## I/O and System Errors (Class 58)

### 58030 — io_error

The storage engine encountered an operating system I/O error.

```
ERROR 58030: could not write to file "axiomdb.db": No space left on device
```

**Possible causes:**
- Disk full — free space or expand the volume
- File permissions — ensure the AxiomDB process can write to the data directory
- Hardware error — check dmesg / system logs for disk errors

---

## Syntax and Parse Errors (Class 42)

### 42601 — syntax_error

The SQL statement is not syntactically valid.

```sql
SELECT FORM users;  -- 'FORM' is not a keyword
-- ERROR 42601: syntax error at or near "FORM"
-- Position: 8
```

### 42883 — undefined_function

A function name was called that does not exist.

```sql
SELECT unknown_function(1);
-- ERROR 42883: function "unknown_function" does not exist
```

---

## Data Errors (Class 22)

### 22001 — string_data_right_truncation

A TEXT or VARCHAR value exceeds the column's declared length.

```sql
CREATE TABLE codes (code CHAR(3));
INSERT INTO codes VALUES ('TOOLONG');
-- ERROR 22001: value too long for type CHAR(3)
```

### 22003 — numeric_value_out_of_range

A numeric value exceeds the range of its declared type.

```sql
INSERT INTO users (age) VALUES (99999);  -- age is SMALLINT
-- ERROR 22003: integer out of range for type SMALLINT
```

### 22012 — division_by_zero

Division by zero in an arithmetic expression.

```sql
SELECT 10 / 0;
-- ERROR 22012: division by zero
```

### 22018 — invalid_character_value_for_cast

A value cannot be implicitly coerced to the target type. This error is raised
when AxiomDB is in **strict mode** (the default) and a conversion is attempted
that would discard data or is not defined.

```sql
-- Text with non-numeric characters inserted into an INT column (strict mode):
INSERT INTO users (age) VALUES ('42abc');
-- ERROR 22018: cannot coerce '42abc' (Text) to INT: '42abc' is not a valid integer

-- A type pair with no implicit conversion:
SELECT 3.14 + DATE '2026-01-01';
-- ERROR 22018: cannot coerce 3.14 (Real) to Date: no implicit numeric promotion between these types
```

**Hint:** Use explicit `CAST` for conversions that AxiomDB does not apply
automatically:

```sql
INSERT INTO users (age) VALUES (CAST('42' AS INT));   -- explicit — always works
SELECT CAST(3 AS REAL) + 1.5;                         -- explicit widening
```

**MySQL compat mode** (permissive): if your application requires MySQL-style
lenient coercion (`'42abc'` silently converted to `42`), set the session to
permissive mode. This will be available via `SET AXIOM_COMPAT = 'mysql'` in
Phase 5 (MySQL wire protocol). Until then, strict mode is always active.

#### Implicit coercions that always succeed (no error)

The following conversions happen automatically without raising 22018:

| From | To | Example |
|---|---|---|
| `INT` | `BIGINT` | `1 + 9999999999` → `BIGINT` |
| `INT` | `REAL` | `5 + 1.5` → `Real(6.5)` |
| `INT` | `DECIMAL` | `2 + 3.14` → `Decimal(5.14)` |
| `BIGINT` | `REAL` | `100 + 1.5` → `Real(101.5)` |
| `BIGINT` | `DECIMAL` | `100 + 3.14` → `Decimal(103.14)` |
| `BIGINT` | `INT` | only if value fits in INT range |
| `TEXT` | `INT` / `BIGINT` | `'42'` → `42` (strict: entire string must be a number) |
| `TEXT` | `REAL` | `'3.14'` → `3.14` |
| `TEXT` | `DECIMAL` | `'3.14'` → `Decimal(314, 2)` |
| `DATE` | `TIMESTAMP` | midnight UTC of the given date |
| `NULL` | any | always passes through as `NULL` |

---

## Complete SQLSTATE Reference

| SQLSTATE | Name                          | Common Cause                              |
|----------|-------------------------------|-------------------------------------------|
| `21000`  | cardinality_violation         | Scalar subquery returned more than 1 row  |
| `23505`  | unique_violation              | Duplicate value in UNIQUE / PK column     |
| `23503`  | foreign_key_violation         | Referencing non-existent FK target        |
| `23502`  | not_null_violation            | NULL inserted into NOT NULL column        |
| `23514`  | check_violation               | Row failed a CHECK constraint             |
| `40001`  | serialization_failure         | Write-write conflict; retry the txn       |
| `40P01`  | deadlock_detected             | Circular lock dependency                  |
| `42P01`  | undefined_table               | Table does not exist                      |
| `42703`  | undefined_column              | Column does not exist                     |
| `42702`  | ambiguous_column              | Unqualified column name is ambiguous      |
| `42P07`  | duplicate_table               | Table already exists                      |
| `42701`  | duplicate_column              | Column already exists in table            |
| `42601`  | syntax_error                  | Malformed SQL                             |
| `42883`  | undefined_function            | Unknown function name                     |
| `22001`  | string_data_right_truncation  | Value too long for column type            |
| `22003`  | numeric_value_out_of_range    | Number exceeds type bounds                |
| `22012`  | division_by_zero              | Division by zero in expression            |
| `22018`  | invalid_character_value_for_cast | Implicit type coercion failed          |
| `22P02`  | invalid_text_representation   | Invalid literal value                     |
| `42501`  | insufficient_privilege        | Permission denied on object               |
| `42702`  | ambiguous_column              | Unqualified column matches in 2+ tables   |
| `42804`  | datatype_mismatch             | Type mismatch in expression               |
| `25001`  | active_sql_transaction        | BEGIN inside an active transaction        |
| `25P01`  | no_active_sql_transaction     | COMMIT/ROLLBACK with no active transaction|
| `25006`  | read_only_sql_transaction     | Transaction expired                       |
| `0A000`  | feature_not_supported         | SQL feature not yet implemented           |
| `53100`  | disk_full                     | Storage volume is full                    |
| `58030`  | io_error                      | OS-level I/O failure (disk, permissions)  |
