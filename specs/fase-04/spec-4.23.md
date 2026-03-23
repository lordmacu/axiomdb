# Spec: 4.23 — QueryResult type

## What to build (not how)

A unified return type for every SQL statement executed by the NexusDB executor.
Every path through the executor — SELECT, INSERT, UPDATE, DELETE, DDL — returns
exactly one `QueryResult`. Callers (embedded API, wire protocol server, CLI REPL)
pattern-match on the variant to produce the appropriate response.

This type is the contract between the executor and everything above it. It must
carry enough information that Phase 5 (MySQL wire protocol) can serialize it
without touching any internal engine state.

---

## Inputs / Outputs

### Types defined

#### `Row`

```rust
/// A single output row produced by a SELECT.
///
/// One `Value` per output column, in declaration order (same order as
/// `QueryResult::Rows::columns`). May contain `Value::Null` for nullable
/// columns or computed expressions.
pub type Row = Vec<Value>;
```

#### `ColumnMeta`

```rust
/// Metadata describing one column in a SELECT result.
///
/// Carries all information needed by:
/// - The embedded API: name + data_type for typed access
/// - The CLI REPL: name + data_type for display formatting
/// - The MySQL wire protocol (Phase 5): all four fields for column definition
///   packets (COM_QUERY response)
pub struct ColumnMeta {
    /// Output column name.
    ///
    /// For a bare column reference (`SELECT age`): the column name from the
    /// catalog (`"age"`). For an aliased column (`SELECT age AS years`): the
    /// alias (`"years"`). For a computed expression (`SELECT 1 + 1`): a
    /// generated name (`"?column?"` or `"1 + 1"` — executor's choice, as long
    /// as it is non-empty and unique within the result set).
    pub name: String,

    /// SQL data type of this column's values.
    ///
    /// Used by the wire protocol to serialize each cell correctly. Also used
    /// by the embedded API for typed result access.
    pub data_type: DataType,

    /// Whether this column can contain `Value::Null`.
    ///
    /// `false` means the column has a NOT NULL constraint and the executor
    /// guarantees no NULL will appear. `true` for nullable columns, computed
    /// expressions, outer-join outputs, and aggregate results where the group
    /// might be empty.
    pub nullable: bool,

    /// Name of the originating table, if known.
    ///
    /// `Some("users")` for `SELECT users.age FROM users`.
    /// `None` for computed columns (`SELECT 1 + 1`, `SELECT COUNT(*)`),
    /// columns from subqueries, or columns whose origin cannot be determined
    /// at analysis time.
    ///
    /// Required by the MySQL wire protocol's column definition packet
    /// (`table` field in `ColumnDefinition41`). Omitting it here would force
    /// Phase 5 to add it later, breaking all executor call sites.
    pub table_name: Option<String>,
}
```

#### `QueryResult`

```rust
/// Unified return value from the NexusDB executor for any SQL statement.
///
/// ## Variants
///
/// - `Rows`     — SELECT (and future RETURNING clauses)
/// - `Affected` — INSERT, UPDATE, DELETE
/// - `Empty`    — DDL: CREATE TABLE, DROP TABLE, CREATE INDEX, DROP INDEX,
///                ALTER TABLE, TRUNCATE
///
/// ## Invariants
///
/// - In `Rows`, `rows[i].len() == columns.len()` for every row `i`.
/// - In `Rows`, `rows[i][j]` is `Value::Null` only if `columns[j].nullable`.
///   (The executor enforces this; callers may assume it.)
/// - In `Affected`, `count` is the number of heap tuples written, updated,
///   or deleted. It does NOT include cascaded FK actions (those are Phase 13).
/// - `last_insert_id` is `Some(id)` only when the statement was an INSERT
///   into a table with an AUTO_INCREMENT / SERIAL column AND at least one row
///   was inserted. It holds the last generated ID, matching MySQL's
///   `LAST_INSERT_ID()` semantics.
pub enum QueryResult {
    Rows {
        columns: Vec<ColumnMeta>,
        rows:    Vec<Row>,
    },
    Affected {
        count:          u64,
        last_insert_id: Option<u64>,
    },
    Empty,
}
```

---

## Constructors (convenience — not required, but must exist)

```rust
impl QueryResult {
    /// Empty SELECT result — no rows, but columns are known.
    pub fn empty_rows(columns: Vec<ColumnMeta>) -> Self;

    /// DML with no auto-increment.
    pub fn affected(count: u64) -> Self;

    /// DML with auto-increment (INSERT into AUTO_INCREMENT table).
    pub fn affected_with_id(count: u64, last_insert_id: u64) -> Self;
}

impl ColumnMeta {
    /// Convenience constructor for a non-nullable column from a catalog ColumnDef.
    pub fn from_column_def(name: String, data_type: DataType, nullable: bool,
                           table_name: Option<String>) -> Self;
}
```

---

## Use cases

### 1. SELECT returning rows

```rust
// executor builds:
QueryResult::Rows {
    columns: vec![
        ColumnMeta { name: "id".into(),    data_type: DataType::BigInt, nullable: false, table_name: Some("users".into()) },
        ColumnMeta { name: "email".into(), data_type: DataType::Text,   nullable: false, table_name: Some("users".into()) },
    ],
    rows: vec![
        vec![Value::BigInt(1), Value::Text("alice@example.com".into())],
        vec![Value::BigInt(2), Value::Text("bob@example.com".into())],
    ],
}
```

### 2. SELECT with no matching rows

```rust
// Same columns, empty rows vec:
QueryResult::Rows { columns: vec![...], rows: vec![] }
// NOT Empty — Empty is only for DDL.
```

### 3. INSERT with AUTO_INCREMENT

```rust
QueryResult::Affected { count: 1, last_insert_id: Some(42) }
```

### 4. UPDATE / DELETE (no auto-increment)

```rust
QueryResult::Affected { count: 5, last_insert_id: None }
```

### 5. CREATE TABLE

```rust
QueryResult::Empty
```

### 6. Computed column (SELECT 1 + 1)

```rust
QueryResult::Rows {
    columns: vec![
        ColumnMeta { name: "1 + 1".into(), data_type: DataType::Int, nullable: true, table_name: None },
    ],
    rows: vec![vec![Value::Int(2)]],
}
// nullable: true because computed expressions have no NOT NULL guarantee.
// table_name: None because there is no originating table.
```

---

## Acceptance criteria

- [ ] `nexusdb-sql/src/result.rs` compiles with `cargo check`
- [ ] `Row` type alias is `Vec<Value>` — no newtype wrapper
- [ ] `ColumnMeta` has exactly four public fields: `name`, `data_type`, `nullable`, `table_name`
- [ ] `QueryResult` has exactly three variants: `Rows`, `Affected`, `Empty`
- [ ] All three types derive `Debug` and `Clone`
- [ ] `QueryResult` additionally derives `PartialEq` (needed for executor tests)
- [ ] `ColumnMeta` additionally derives `PartialEq`
- [ ] Convenience constructors on `QueryResult` and `ColumnMeta` exist
- [ ] Unit tests: construction of each variant, clone, debug print, invariant checks
- [ ] Re-exported from `nexusdb-sql/src/lib.rs`
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- Streaming / lazy row iteration (all results are materialized `Vec<Row>` for now)
- `Display` / ASCII table formatting (Phase 4.15 — the CLI will add this)
- `RETURNING` clause support (DML+rows result, Phase 4.5+)
- Affected count for cascaded FK actions (Phase 13)
- Result set metadata beyond the four `ColumnMeta` fields (charset, precision,
  scale — Phase 5 adds these when serializing MySQL packets)
- Error results — errors are `Err(DbError)`, never a `QueryResult` variant

---

## Dependencies

- `nexusdb-types` — for `Value` and `DataType` (already a dep of `nexusdb-sql`)
- No new crate dependencies required
- Lives in: `nexusdb-sql/src/result.rs`, re-exported from `nexusdb-sql/src/lib.rs`
