# Plan: 4.25 + 4.7 — Error Handling Framework

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-core/src/error.rs` | modify | Complete `sqlstate()` method — 14 new mappings |
| `crates/axiomdb-core/src/error_response.rs` | **create** | `Severity`, `ErrorResponse`, `from_error`, `Display`, `From` impl |
| `crates/axiomdb-core/src/lib.rs` | modify | `pub mod error_response` + re-export `ErrorResponse`, `Severity` |

No new crate. No new dependency. Three files total.

---

## Algorithm / Data structures

### Step 1 — Complete `sqlstate()` in `error.rs`

Add 14 match arms:

```rust
DbError::AmbiguousColumn { .. }        => "42702",
DbError::TableAlreadyExists { .. }     => "42P07",
DbError::DuplicateKey                  => "23505",
DbError::KeyTooLong { .. }             => "22001",
DbError::ValueTooLarge { .. }          => "22001",
DbError::InvalidValue { .. }           => "22P02",
DbError::DivisionByZero                => "22012",
DbError::Overflow                      => "22003",
DbError::TransactionAlreadyActive { .. } => "25001",
DbError::NoActiveTransaction           => "25P01",
DbError::TransactionExpired { .. }     => "25006",
DbError::NotImplemented { .. }         => "0A000",
DbError::StorageFull                   => "53100",
DbError::SequenceOverflow              => "2200H",
```

Remove the catch-all `_ => "XX000"` from these variants; keep it for the
remaining internal variants (storage, WAL, B+Tree, catalog internals, heap).

### Step 2 — `Severity` enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,    // fatal — statement terminated
    Warning,  // non-fatal (Phase 4.25c)
    Notice,   // informational (Phase 4.25c)
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error   => write!(f, "ERROR"),
            Self::Warning => write!(f, "WARNING"),
            Self::Notice  => write!(f, "NOTICE"),
        }
    }
}
```

### Step 3 — `ErrorResponse` struct

```rust
pub struct ErrorResponse {
    pub sqlstate:  String,
    pub severity:  Severity,
    pub message:   String,
    pub detail:    Option<String>,
    pub hint:      Option<String>,
    pub position:  Option<usize>,  // always None in Phase 4.25; see 4.25b
}
```

### Step 4 — `ErrorResponse::from_error` constructor

```
fn from_error(err: &DbError) -> Self:
  let sqlstate  = err.sqlstate().to_string();
  let message   = err.to_string();
  let (detail, hint) = derive_detail_hint(err);
  ErrorResponse {
    sqlstate, severity: Severity::Error, message,
    detail, hint, position: None,
  }
```

### Step 5 — `derive_detail_hint` private helper

Returns `(Option<String>, Option<String>)`:

```
match err:
  ForeignKeyViolation { table, column, value } =>
    detail = Some(format!("Key ({column})=({value}) is not present in table {table}."))
    hint   = Some("Insert the referenced row first, or use ON DELETE CASCADE.")

  AmbiguousColumn { name, tables } =>
    detail = Some(format!("Column '{name}' appears in: {tables}."))
    hint   = Some(format!("Qualify the column name with a table alias, e.g. t.{name}."))

  InvalidCoercion { from, to, value, reason } =>
    detail = Some(format!("Cannot convert {value} ({from}) to {to}: {reason}."))
    hint   = Some(format!("Use an explicit CAST: CAST({value} AS {to})."))

  UniqueViolation { table, column } =>
    hint = Some(format!("A row with the same {column} already exists in {table}."))

  NotNullViolation { table, column } =>
    hint = Some(format!("Provide a non-NULL value for column {column} in table {table}."))

  CheckViolation { table, constraint } =>
    hint = Some(format!("The row violates CHECK constraint {constraint} on table {table}."))

  TableNotFound { .. } =>
    hint = Some("Did you spell the table name correctly? Use SHOW TABLES to list available tables.")

  ColumnNotFound { name, table } =>
    hint = Some(format!("Column '{name}' does not exist in table '{table}'. Use DESCRIBE {table} to list available columns."))

  TableAlreadyExists { .. } =>
    hint = Some("Use CREATE TABLE IF NOT EXISTS to skip if the table already exists.")

  DuplicateKey =>
    hint = Some("A row with the same primary key already exists.")

  DivisionByZero =>
    hint = Some("Add a WHERE guard: WHERE divisor <> 0, or use NULLIF(divisor, 0).")

  Overflow =>
    hint = Some("Use a wider numeric type (e.g. BIGINT instead of INT).")

  TransactionAlreadyActive { .. } =>
    hint = Some("COMMIT or ROLLBACK the current transaction before starting a new one.")

  NoActiveTransaction =>
    hint = Some("Start a transaction with BEGIN first.")

  NotImplemented { feature } =>
    hint = Some(format!("This feature ({feature}) is planned for a future version of AxiomDB."))

  StorageFull =>
    hint = Some("Free up disk space or expand the storage volume.")

  _ => (None, None)
```

### Step 6 — `Display` for `ErrorResponse`

```rust
impl fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.severity, self.sqlstate, self.message)?;
        if let Some(ref d) = self.detail   { write!(f, "\nDETAIL:   {d}")?; }
        if let Some(ref h) = self.hint     { write!(f, "\nHINT:     {h}")?; }
        if let Some(p)     = self.position { write!(f, "\nPOSITION: {p}")?; }
        Ok(())
    }
}
```

### Step 7 — `From<&DbError> for ErrorResponse` and `From<DbError>`

```rust
impl From<&DbError> for ErrorResponse {
    fn from(err: &DbError) -> Self { Self::from_error(err) }
}

impl From<DbError> for ErrorResponse {
    fn from(err: DbError) -> Self { Self::from_error(&err) }
}
```

### Step 8 — Re-export from `axiomdb-core/src/lib.rs`

```rust
pub mod error_response;
pub use error_response::{ErrorResponse, Severity};
```

---

## Implementation order

1. **`error.rs`** — add 14 sqlstate arms. `cargo check -p axiomdb-core`.

2. **`error_response.rs`** — create with `Severity`, `ErrorResponse`, `from_error`
   (all arms), `Display`, `From` impls. `cargo check -p axiomdb-core`.

3. **`lib.rs`** — add re-exports. `cargo check --workspace`.

4. **Unit tests** in `error_response.rs` — see test list below.

5. **Full check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

All tests live in `error_response.rs` — no I/O, no external deps.

### SQLSTATE completeness

```
test_sqlstate_all_sql_reachable_variants
  — verify each of the 14 newly-mapped variants returns the correct code
  — also verify the 13 already-correct ones still pass (regression)

test_sqlstate_internal_variants_return_xx000
  — PageNotFound, WalChecksumMismatch, BTreeCorrupted, HeapPageFull, etc.
    all return "XX000"
```

### `from_error` construction

```
test_from_error_table_not_found
  — sqlstate="42P01", hint contains "SHOW TABLES"

test_from_error_column_not_found
  — sqlstate="42703", hint contains the column and table names

test_from_error_ambiguous_column
  — sqlstate="42702", detail contains the column name + tables

test_from_error_unique_violation
  — sqlstate="23505", hint contains table and column

test_from_error_foreign_key_violation
  — sqlstate="23503", detail contains the offending value, hint present

test_from_error_not_null_violation
  — sqlstate="23502", hint contains table and column

test_from_error_invalid_coercion
  — sqlstate="22018", detail + hint both present and non-empty

test_from_error_division_by_zero
  — sqlstate="22012", hint contains "NULLIF"

test_from_error_overflow
  — sqlstate="22003", hint contains "BIGINT"

test_from_error_transaction_already_active
  — sqlstate="25001", hint contains "COMMIT or ROLLBACK"

test_from_error_no_active_transaction
  — sqlstate="25P01", hint contains "BEGIN"

test_from_error_not_implemented
  — sqlstate="0A000", hint contains the feature name

test_from_error_parse_error
  — sqlstate="42601", no hint (parse errors are self-explanatory)

test_from_error_internal_error
  — PageNotFound: sqlstate="XX000", detail=None, hint=None
```

### `Display` format

```
test_display_error_only
  — only message, no detail/hint:
    "ERROR 42P01: table 'users' not found"

test_display_with_hint
  — message + hint, no detail:
    "ERROR 42P01: table 'users' not found\nHINT:     Did you spell..."

test_display_with_detail_and_hint
  — message + detail + hint:
    "ERROR 23503: foreign key...\nDETAIL:   Key...\nHINT:     Insert..."

test_display_severity_shown
  — severity appears: "ERROR 42P01: ..." (not just the code)
```

### `From` trait

```
test_from_ref_dbeerror
  — ErrorResponse::from(&err) produces same as from_error(&err)

test_from_owned_dbeerror
  — ErrorResponse::from(err) produces same result
```

---

## Anti-patterns to avoid

- **DO NOT** allocate in `sqlstate()` — it must return `&'static str` as it
  does today. No change to the return type.
- **DO NOT** add `hint` or `detail` fields to `DbError` variants — that would
  mix presentation into the domain error type. Keep them in `error_response.rs`.
- **DO NOT** `unwrap()` in `from_error` — it is infallible and must never panic.
- **DO NOT** change `DbError`'s existing `Display` messages — those are the
  `message` field of `ErrorResponse`; changing them would break existing tests.

---

## Risks

| Risk | Mitigation |
|---|---|
| Adding 14 arms makes `sqlstate()` exhaustive — rustc may warn about unreachable `_ => "XX000"` | Rust exhaustiveness checker will flag. Keep the catch-all but move it to the end; rustc allows overlapping catch-alls as long as the named arms come first. |
| `derive_detail_hint` is a large match — easy to miss a variant | Use `#[deny(unreachable_patterns)]` to catch duplicate arms. Also, the test `test_from_error_internal_error` catches the default branch. |
| Hint strings contain table/column names — formatting must not panic | Use `format!()` (infallible) everywhere. No indexing, no `unwrap()`. |
