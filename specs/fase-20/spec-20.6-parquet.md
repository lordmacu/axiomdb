# Spec: 20.6 Parquet — READ_PARQUET + COPY TO FORMAT PARQUET

Phase: 20 — Types + import/export
Task: Parquet read (TVF) + write (COPY TO extension)
Status: approved

## Context

Phase 20.5 added `COPY FROM/TO` for CSV/JSON/JSONL. Phase 20.10 added `GENERATE_SERIES`
as a table-valued function (TVF) following the `FromClause::GenerateSeries` pattern.
This subphase adds two Parquet surfaces:
1. **READ_PARQUET('path')** — TVF in the FROM clause; schema discovered at bind time from
   the file's metadata; produces rows like any table.
2. **COPY table TO 'path.parquet' WITH (FORMAT PARQUET)** — extends `CopyFormat` with a
   Parquet variant; writes a Parquet file from a full table scan.

This subphase consolidates 20.9 (Parquet write) — both surfaces ship together.

## Goal

Allow reading and writing Parquet files using two complementary SQL surfaces.

## Non-goals

- Streaming / chunked reads for files larger than RAM — full load into memory for now
- Arrow-backed columnar execution — values pass through `axiomdb_types::Value`
- Parquet nested types (LIST/MAP/STRUCT) — `DbError::NotSupported` with clear message
- `CREATE FOREIGN TABLE … USING parquet` (FDW) — deferred
- Parquet compression options other than Snappy and Uncompressed — deferred
- `COPY FROM 'file.parquet' TO table` — separate subphase (20.6b or follow-up)
- Predicate pushdown into Parquet row groups — deferred

## Behavior

### Surface 1: READ_PARQUET('path') TVF

```sql
-- Basic: all columns
SELECT * FROM READ_PARQUET('/tmp/data.parquet');

-- Column projection
SELECT id, name FROM READ_PARQUET('/tmp/data.parquet');

-- With alias
SELECT p.id FROM READ_PARQUET('/tmp/data.parquet') AS p;

-- With alias and explicit column rename (optional, same as AS alias)
SELECT p.id, p.score FROM READ_PARQUET('/tmp/data.parquet') AS p(id, score);

-- With WHERE (applied post-read in executor)
SELECT id FROM READ_PARQUET('/tmp/data.parquet') WHERE score > 90;

-- In CTE
WITH raw AS (SELECT * FROM READ_PARQUET('/tmp/data.parquet'))
SELECT id, name FROM raw WHERE active = TRUE;

-- JOIN with a real table
SELECT t.name, p.score
FROM users t
JOIN READ_PARQUET('/tmp/scores.parquet') AS p ON t.id = p.user_id;
```

### AST

New `FromClause` variant:

```rust
/// Phase 20.6 — `READ_PARQUET('path') [AS alias [(col1, col2, ...)]]`.
ReadParquet(Box<ReadParquetClause>),

pub struct ReadParquetClause {
    pub path: String,
    /// Table alias (e.g., `AS p`). None = default alias "read_parquet".
    pub alias: Option<String>,
    /// Optional explicit column names to rename/reorder output.
    /// None = use column names from Parquet schema metadata.
    pub column_aliases: Vec<String>,
}
```

### Parser: READ_PARQUET recognition

In `parse_from_clause`, after existing FROM alternatives, recognize:
`Ident("READ_PARQUET")` followed by `'(' StringLit ')' [AS alias [(col, ...)]]`.
Returns `FromClause::ReadParquet(Box::new(ReadParquetClause { path, alias, column_aliases }))`.

### Analyzer: schema discovery at bind time

In `bound_from_clause` (analyzer_bind.rs), case `FromClause::ReadParquet(rp)`:
1. Open the Parquet file at `rp.path` using `parquet::file::reader::SerializedFileReader`.
2. Read `file_reader.metadata().file_metadata().schema_descr()` to get column descriptors.
3. Map each Parquet column to `ColumnDef { name, data_type }` using the type-mapping table below.
4. If `rp.column_aliases` is non-empty, rename columns in order (error if length mismatch).
5. Build `BoundTable { alias, name, columns, col_offset }`.
6. On file-not-found → `DbError::Io`; on schema error → `DbError::InvalidValue`.

### Parquet → AxiomDB type mapping (read)

| Parquet physical type | Logical/converted type | AxiomDB DataType | Value variant |
|---|---|---|---|
| `INT32` | none / INT32 | `DataType::Int` | `Value::Int` |
| `INT64` | none / INT64 | `DataType::BigInt` | `Value::BigInt` |
| `INT32` | `DATE` | `DataType::Date` | `Value::Date` (days since epoch) |
| `INT64` | `TIMESTAMP_MICROS` (UTC or not) | `DataType::Timestamp` | `Value::Timestamp` (µs since epoch) |
| `INT64` | `TIMESTAMP_MILLIS` | `DataType::Timestamp` | `Value::Timestamp` (×1000 µs) |
| `FLOAT` | none | `DataType::Real` | `Value::Real` |
| `DOUBLE` | none | `DataType::Real` | `Value::Real` |
| `BOOLEAN` | none | `DataType::Bool` | `Value::Bool` |
| `BYTE_ARRAY` | `STRING` / `UTF8` | `DataType::Text` | `Value::Text` |
| `BYTE_ARRAY` | none | `DataType::Text` | `Value::Text` (UTF-8 lossy) |
| `INT32` / `INT64` | `DECIMAL(p, s)` | `DataType::Decimal(p, s)` | `Value::Decimal` |
| `FIXED_LEN_BYTE_ARRAY` | `DECIMAL(p, s)` | `DataType::Decimal(p, s)` | `Value::Decimal` |
| repeated / complex (LIST, MAP, STRUCT) | any | — | `DbError::NotSupported { feature: "Parquet nested types" }` |

Null values in any column → `Value::Null`.

### Surface 2: COPY table TO FORMAT PARQUET

```sql
-- Uncompressed (default)
COPY users TO '/tmp/users.parquet' WITH (FORMAT PARQUET);

-- Snappy compression
COPY orders TO '/tmp/orders.parquet' WITH (FORMAT PARQUET, COMPRESSION SNAPPY);
```

### AST change for COPY

Add `Parquet` to `CopyFormat`:
```rust
pub enum CopyFormat {
    Csv,
    Json,
    Jsonl,
    Parquet,
}
```

Add `compression: Option<ParquetCompression>` to `CopyOptions`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParquetCompression {
    Snappy,
    Uncompressed,
}

pub struct CopyOptions {
    // ... existing fields ...
    pub compression: Option<ParquetCompression>,
}
```

### AxiomDB → Parquet type mapping (write)

| Value variant | Parquet physical type | Logical annotation |
|---|---|---|
| `Value::Null` | any (written as null) | — |
| `Value::Bool` | `BOOLEAN` | none |
| `Value::Int` | `INT32` | `INT32` |
| `Value::BigInt` | `INT64` | `INT64` |
| `Value::Real` | `DOUBLE` | none |
| `Value::Decimal(m, s)` | `INT64` | `DECIMAL(18, s)` |
| `Value::Text` | `BYTE_ARRAY` | `UTF8` |
| `Value::Json` / `Value::Jsonb` | `BYTE_ARRAY` | `UTF8` |
| `Value::Bytes` | `BYTE_ARRAY` | none |
| `Value::Date` | `INT32` | `DATE` |
| `Value::Timestamp` | `INT64` | `TIMESTAMP_MICROS(UTC=true)` |
| `Value::Uuid` | `BYTE_ARRAY` | `UTF8` (UUID string form) |
| `Value::Array` | `BYTE_ARRAY` | `UTF8` (JSON array text) |

Schema is inferred from the first non-null row; null-only columns use `BYTE_ARRAY`.
All columns are written as optional (nullable).

### Error cases

| Input | Expected error |
|---|---|
| `READ_PARQUET('/nonexistent.parquet')` | `DbError::Io(...)` at bind time |
| `READ_PARQUET('/file.parquet')` where file has nested LIST column | `DbError::NotSupported { feature: "Parquet nested types" }` |
| `COPY t TO '/path' WITH (FORMAT PARQUET)` bad path | `DbError::Io(...)` at execute time |
| `READ_PARQUET()` (no argument) | `DbError::ParseError` |
| Column alias count mismatch | `DbError::InvalidValue { reason: "..." }` |

## Edge cases

- [x] Empty Parquet file (0 rows) → 0 rows returned (schema still discovered)
- [x] Single column → works
- [x] All columns nullable → schema with optional fields
- [x] Null values in any column → `Value::Null`
- [x] READ_PARQUET in JOIN → works (same as any FROM source)
- [x] READ_PARQUET in CTE body → works
- [x] COPY TO FORMAT PARQUET on 0-row table → empty Parquet file
- [x] Round-trip: COPY TO FORMAT PARQUET → READ_PARQUET → identical data

## Performance budget

No throughput target. The `parquet` crate handles row-group decompression efficiently.
Acceptable overhead: O(rows × cols) type conversion after decompression.

## Dependencies

- New crate dependency: `parquet = "58"` (Apache Arrow Rust implementation)
  Added to `crates/axiomdb-sql/Cargo.toml` (not workspace — single crate needs it)
- Depends on: Phase 20.5 (COPY TO infrastructure) — complete
- Depends on: Phase 20.10 (GENERATE_SERIES TVF pattern) — complete
- Consolidates: Phase 20.9 (Parquet write) — merge into 20.6, mark 20.9 as merged

## Open questions

All resolved:
- Arrow vs. row-level API: row-level (`parquet::record::RowIter`) for reading; columnar writer for writing — avoids the `arrow` crate dependency
- Nested types: NotSupported (too complex for first iteration)
- Compression: Snappy (common default) + Uncompressed; Zstd/Gzip deferred
- Schema on null-only columns: BYTE_ARRAY as safe fallback
- File reading at bind time vs. execute time: bind time for schema, execute time for data

## Done criteria

- [ ] `SELECT * FROM READ_PARQUET('/tmp/data.parquet')` returns rows with correct column names and types
- [ ] Column projection, WHERE, ORDER BY, LIMIT work on READ_PARQUET result
- [ ] READ_PARQUET in JOIN with a real table works
- [ ] READ_PARQUET in CTE body works
- [ ] Null values in Parquet columns → `Value::Null`
- [ ] `COPY table TO 'path.parquet' WITH (FORMAT PARQUET)` writes a valid Parquet file
- [ ] `COPY table TO 'path.parquet' WITH (FORMAT PARQUET, COMPRESSION SNAPPY)` works
- [ ] Round-trip: COPY TO FORMAT PARQUET then READ_PARQUET returns identical data
- [ ] Nested Parquet column → `DbError::NotSupported` with clear message
- [ ] Empty table → empty Parquet file → READ_PARQUET returns 0 rows
- [ ] 14+ integration tests in `tests/integration_parquet.rs` pass
- [ ] `cargo nextest run -p axiomdb-sql` passes (Lima VM)
- [ ] `cargo nextest run --workspace` passes (Lima VM)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke: 3 new assertions pass (552/552+)
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated with READ_PARQUET + COPY TO PARQUET sections
- [ ] `docs-site/src/internals/sql-parser.md` updated

## References

- Apache Arrow Rust parquet crate: https://docs.rs/parquet/latest/parquet/
- parquet record reader (row-level API): `parquet::record::reader::RowIter`
- parquet file writer: `parquet::file::writer::SerializedFileWriter`
- GENERATE_SERIES TVF pattern: `specs/fase-20/spec-20.10-generate-series.md`
- COPY TO executor: `crates/axiomdb-sql/src/executor/copy_to.rs`
- Analyzer bind pattern: `crates/axiomdb-sql/src/analyzer_bind.rs` line 483 (GenerateSeries)
