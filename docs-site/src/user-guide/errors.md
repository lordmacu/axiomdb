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

An INSERT or UPDATE references a row in another table that does not exist, or a
DELETE would leave referencing rows without a parent.

```sql
INSERT INTO orders (user_id, total) VALUES (99999, 100);  -- user 99999 does not exist
-- ERROR 23503: insert into "orders" violates foreign key constraint "fk_orders_user"
-- Detail: Key (user_id)=(99999) is not present in table "users".
```

**Typical application response:** Validate that the referenced entity exists before
inserting, or show "Referenced record not found."

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
