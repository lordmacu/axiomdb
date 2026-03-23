# Spec: 4.25 + 4.7 — Error Handling Framework

## What to build (not how)

Two complementary improvements to the error system:

1. **Complete SQLSTATE mapping** — every `DbError` variant maps to a precise
   5-character SQLSTATE code. Variants currently returning `"XX000"` that are
   reachable by SQL clients get meaningful codes.

2. **`ErrorResponse` type** — a structured presentation type in `axiomdb-core`
   that carries `{sqlstate, severity, message, detail, hint, position}`. The
   wire protocol server (Phase 5) and embedded API use this to produce
   actionable error messages. Inspired by the Rust compiler error format and
   PostgreSQL's `ErrorResponse` packet.

These two subfases (4.25 and 4.7) are implemented together because SQLSTATE
completion is the prerequisite for a meaningful `ErrorResponse`.

---

## Part 1 — Complete SQLSTATE mapping

### Current state

`DbError::sqlstate()` already maps 13 variants correctly. The remaining
variants return `"XX000"` (internal error), which is acceptable for internal
errors but wrong for errors that SQL clients can trigger and handle.

### Complete SQLSTATE table

| `DbError` variant | SQLSTATE | Class name | Reachable by SQL? |
|---|---|---|---|
| `UniqueViolation` | `23505` | unique_violation | Yes |
| `ForeignKeyViolation` | `23503` | foreign_key_violation | Yes |
| `NotNullViolation` | `23502` | not_null_violation | Yes |
| `CheckViolation` | `23514` | check_violation | Yes |
| `DeadlockDetected` | `40P01` | deadlock_detected | Yes |
| `ParseError` | `42601` | syntax_error | Yes |
| `TableNotFound` | `42P01` | undefined_table | Yes |
| `ColumnNotFound` | `42703` | undefined_column | Yes |
| `AmbiguousColumn` | `42702` | ambiguous_column | Yes ← **new** |
| `TableAlreadyExists` | `42P07` | duplicate_table | Yes ← **new** |
| `PermissionDenied` | `42501` | insufficient_privilege | Yes |
| `TypeMismatch` | `42804` | datatype_mismatch | Yes |
| `InvalidCoercion` | `22018` | invalid_character_value_for_cast | Yes |
| `DuplicateKey` | `23505` | unique_violation | Yes ← **new** |
| `KeyTooLong` | `22001` | string_data_right_truncation | Yes ← **new** |
| `ValueTooLarge` | `22001` | string_data_right_truncation | Yes ← **new** |
| `InvalidValue` | `22P02` | invalid_text_representation | Yes ← **new** |
| `DivisionByZero` | `22012` | division_by_zero | Yes ← **new** |
| `Overflow` | `22003` | numeric_value_out_of_range | Yes ← **new** |
| `TransactionAlreadyActive` | `25001` | active_sql_transaction | Yes ← **new** |
| `NoActiveTransaction` | `25P01` | no_active_sql_transaction | Yes ← **new** |
| `TransactionExpired` | `25006` | read_only_sql_transaction | Yes ← **new** |
| `NotImplemented` | `0A000` | feature_not_supported | Yes ← **new** |
| `StorageFull` | `53100` | disk_full | System |
| `Io` | `58030` | io_error | System |
| `FileLocked` | `55006` | object_in_use | System |
| `SequenceOverflow` | `2200H` | sequence_generator_limit_exceeded | Rare |
| `PageNotFound` | `XX000` | internal_error | Internal only |
| `ChecksumMismatch` | `XX000` | internal_error | Internal only |
| `WalChecksumMismatch` | `XX000` | internal_error | Internal only |
| `WalEntryTruncated` | `XX000` | internal_error | Internal only |
| `WalUnknownEntryType` | `XX000` | internal_error | Internal only |
| `WalInvalidHeader` | `XX000` | internal_error | Internal only |
| `HeapPageFull` | `XX000` | internal_error | Internal only |
| `InvalidSlot` | `XX000` | internal_error | Internal only |
| `AlreadyDeleted` | `XX000` | internal_error | Internal only |
| `BTreeCorrupted` | `XX000` | internal_error | Internal only |
| `CatalogNotInitialized` | `XX000` | internal_error | Internal only |
| `CatalogTableNotFound` | `XX000` | internal_error | Internal only |
| `CatalogIndexNotFound` | `XX000` | internal_error | Internal only |
| `ColumnIndexOutOfBounds` | `XX000` | internal_error | Internal only |
| `Other` | `XX000` | internal_error | Varies |

**Rule:** Internal errors (storage corruption, heap internals, catalog
bootstrap) remain `XX000` because they are not caused by user SQL and clients
cannot meaningfully handle them beyond "something is wrong with the database".

---

## Part 2 — `ErrorResponse` type

### Location

`axiomdb-core/src/error_response.rs`, re-exported as `axiomdb_core::ErrorResponse`.

### Structure

```rust
/// Structured error response for delivery to SQL clients.
///
/// Built from a [`DbError`] using [`ErrorResponse::from_error`].
/// Carries all information needed by:
/// - The MySQL wire protocol server (Phase 5): sqlstate + message fields
/// - The embedded API: full struct for typed error handling
/// - The CLI: human-readable display with hint
///
/// ## Fields
///
/// All fields match PostgreSQL's `ErrorResponse` message format (section F.2
/// of the PostgreSQL protocol specification), which is the de facto standard
/// for SQL error responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    /// SQLSTATE code — 5-character string (e.g. "42P01").
    pub sqlstate: String,

    /// Severity of the error.
    pub severity: Severity,

    /// Short human-readable error message. Same as `DbError::to_string()`.
    pub message: String,

    /// Optional extended detail about the error.
    ///
    /// Provides more context than `message`. For example:
    /// - `UniqueViolation` → "Key (email)=(alice@example.com) already exists."
    /// - `ForeignKeyViolation` → "Key (user_id)=(999) is not present in table users."
    ///
    /// `None` if no extra detail is available for this error type.
    pub detail: Option<String>,

    /// Optional hint for how to fix the error.
    ///
    /// Actionable suggestion. For example:
    /// - `TableNotFound` → "Did you mean to CREATE TABLE first?"
    /// - `AmbiguousColumn` → "Qualify the column name with a table alias."
    /// - `NotImplemented` → "This feature is planned for a future version."
    /// - `TransactionAlreadyActive` → "COMMIT or ROLLBACK the current transaction first."
    ///
    /// `None` for errors where no generic hint applies.
    pub hint: Option<String>,

    /// Byte offset of the error in the original SQL query (1-based).
    ///
    /// `None` in Phase 4.25 — requires parser position tracking (Phase 4.25b).
    pub position: Option<usize>,
}
```

### `Severity` enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A fatal error that terminated the statement.
    Error,
    /// A non-fatal warning (Phase 4.25c).
    Warning,
    /// An informational notice (Phase 4.25c).
    Notice,
}
```

### `ErrorResponse::from_error` constructor

```rust
impl ErrorResponse {
    /// Builds an `ErrorResponse` from a `DbError`.
    ///
    /// `detail` and `hint` are populated for variants where the error fields
    /// carry enough information to produce useful text. `position` is always
    /// `None` in Phase 4.25 (requires parser position tracking).
    pub fn from_error(err: &DbError) -> Self;
}
```

### `hint` and `detail` values per variant

| Variant | `detail` | `hint` |
|---|---|---|
| `UniqueViolation { table, column }` | `None` (offending value not available until constraints are enforced — Phase 4.25b) | `"A row with the same {column} already exists in {table}."` |
| `ForeignKeyViolation { table, column, value }` | `"Key ({column})=({value}) is not present in table {table}."` | `"Insert the referenced row first, or use ON DELETE CASCADE."` |
| `NotNullViolation { table, column }` | `None` | `"Provide a non-NULL value for column {column} in table {table}."` |
| `CheckViolation { table, constraint }` | `None` | `"The row violates CHECK constraint {constraint} on table {table}."` |
| `TableNotFound { name }` | `None` | `"Did you spell the table name correctly? Use SHOW TABLES to list available tables."` |
| `ColumnNotFound { name, table }` | `None` | `"Column '{name}' does not exist in table '{table}'. Use DESCRIBE {table} to list available columns."` |
| `AmbiguousColumn { name, tables }` | `"Column '{name}' appears in: {tables}."` | `"Qualify the column name with a table alias, e.g. t.{name}."` |
| `TableAlreadyExists { schema, name }` | `None` | `"Use CREATE TABLE IF NOT EXISTS to skip if the table already exists."` |
| `DuplicateKey` | `None` | `"A row with the same primary key already exists."` |
| `DivisionByZero` | `None` | `"Add a WHERE guard: WHERE divisor <> 0, or use NULLIF(divisor, 0)."` |
| `Overflow` | `None` | `"Use a wider numeric type (e.g. BIGINT instead of INT)."` |
| `InvalidCoercion { from, to, value, reason }` | `"Cannot convert {value} ({from}) to {to}: {reason}."` | `"Use an explicit CAST: CAST({value} AS {to})."` |
| `TransactionAlreadyActive` | `None` | `"COMMIT or ROLLBACK the current transaction before starting a new one."` |
| `NoActiveTransaction` | `None` | `"Start a transaction with BEGIN first."` |
| `NotImplemented { feature }` | `None` | `"This feature ({feature}) is planned for a future version of AxiomDB."` |
| `StorageFull` | `None` | `"Free up disk space or expand the storage volume."` |
| `ParseError { message }` | `None` | `None` |
| (all others) | `None` | `None` |

---

## `Display` for `ErrorResponse`

```rust
impl fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ERROR {}: {}", self.sqlstate, self.message)?;
        if let Some(ref d) = self.detail { write!(f, "\nDETAIL:  {d}")?; }
        if let Some(ref h) = self.hint   { write!(f, "\nHINT:    {h}")?; }
        if let Some(p) = self.position   { write!(f, "\nPOSITION: {p}")?; }
        Ok(())
    }
}
```

Example output:
```
ERROR 42P01: table 'orders' not found
HINT:    Did you spell the table name correctly? Use SHOW TABLES to list available tables.
```

```
ERROR 23503: foreign key violation: users.company_id = 999
DETAIL:  Key (company_id)=(999) is not present in table users.
HINT:    Insert the referenced row first, or use ON DELETE CASCADE.
```

---

## Inputs / Outputs

### `DbError::sqlstate() -> &'static str`
- Modified to return correct SQLSTATE for all variants listed in the table above.
- No new parameters.

### `ErrorResponse::from_error(err: &DbError) -> ErrorResponse`
- Input: any `DbError` reference
- Output: `ErrorResponse` with `severity: Severity::Error`, `position: None`
- Infallible — never returns `Result`

### `ErrorResponse::display_string() -> String`
- Convenience method returning the same as `Display`
- Useful for the CLI and embedded API

---

## Use cases

### 1. Client gets useful error for missing table

```rust
let err = DbError::TableNotFound { name: "users".into() };
let resp = ErrorResponse::from_error(&err);
// resp.sqlstate = "42P01"
// resp.message  = "table 'users' not found"
// resp.hint     = Some("Did you spell the table name correctly?...")
println!("{resp}");
// ERROR 42P01: table 'users' not found
// HINT:    Did you spell the table name correctly? Use SHOW TABLES to list available tables.
```

### 2. ORM catches SQLSTATE to retry on deadlock

```rust
let resp = ErrorResponse::from_error(&err);
if resp.sqlstate == "40P01" {
    // retry the transaction
}
```

### 3. Division by zero with hint

```sql
SELECT total / 0 FROM orders;
-- ERROR 22012: division by zero
-- HINT:    Add a WHERE guard: WHERE divisor <> 0, or use NULLIF(divisor, 0).
```

### 4. Ambiguous column with detail

```sql
SELECT id FROM users JOIN orders ON ...;
-- ERROR 42702: column reference 'id' is ambiguous — found in: users.id, orders.id
-- DETAIL:  Column 'id' appears in: users.id, orders.id.
-- HINT:    Qualify the column name with a table alias, e.g. t.id.
```

### 5. Type coercion with actionable hint

```sql
INSERT INTO t (age) VALUES ('abc');
-- ERROR 22018: cannot coerce 'abc' (Text) to INT: 'abc' is not a valid integer
-- DETAIL:  Cannot convert 'abc' (Text) to INT: 'abc' is not a valid integer.
-- HINT:    Use an explicit CAST: CAST('abc' AS INT).
```

---

## Acceptance criteria

- [ ] `DbError::sqlstate()` returns the correct code for every variant in the table
- [ ] No variant that is reachable by SQL returns `"XX000"` (see "Reachable by SQL?" column)
- [ ] `ErrorResponse` struct exists in `axiomdb-core`
- [ ] `ErrorResponse::from_error(&DbError) -> ErrorResponse` is implemented
- [ ] `detail` and `hint` are populated correctly for all variants in the hint table
- [ ] `Severity` enum has at least `Error` (Warning/Notice are stubs for 4.25c)
- [ ] `Display` for `ErrorResponse` shows SQLSTATE + message + optional DETAIL/HINT
- [ ] `From<&DbError> for ErrorResponse` trait impl exists
- [ ] Unit tests: SQLSTATE code for every variant
- [ ] Unit tests: `from_error` for variants with detail/hint — verify strings
- [ ] Unit tests: `Display` output format for a variant with and without detail/hint
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- `position` populated from parser (Phase 4.25b — requires parser token positions)
- JSON format for `ErrorResponse` (Phase 5 — `SET error_format = 'json'`)
- Warning/Notice severity (Phase 4.25c — requires session state)
- `SET warnings = ...` system (Phase 4.25c)
- Offending value in `UniqueViolation.detail` (requires constraint enforcement at executor level — Phase 4.25b)

---

## Dependencies

- `axiomdb-core` only — no new crate dependencies
- New file: `axiomdb-core/src/error_response.rs`
- Modified file: `axiomdb-core/src/error.rs` (sqlstate method)
- Modified file: `axiomdb-core/src/lib.rs` (re-export)
