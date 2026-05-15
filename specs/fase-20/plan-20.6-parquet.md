# Plan: 20.6 Parquet — READ_PARQUET TVF + COPY TO FORMAT PARQUET

Phase: 20 — Types + import/export
Task: Parquet read (TVF) + write (COPY TO extension)
Spec: specs/fase-20/spec-20.6-parquet.md
Status: in-progress

## Summary

Implements two complementary Parquet surfaces that share the `parquet = "58"` crate.
Order: AST structs first (Step 1) so every downstream step compiles; then parser (Step 2);
then analyzer bind for schema discovery at bind time (Step 3); then the READ_PARQUET executor
(Step 4, mirroring the GenerateSeries pattern); then COPY TO Parquet write (Step 5, extending
copy_to.rs); then integration tests (Step 6); finally docs and close (Step 7).

## Dependencies

Must be done first:
- [x] Phase 20.5 COPY FROM/TO infrastructure — complete
- [x] Phase 20.10 GENERATE_SERIES TVF pattern — complete (executor pattern to follow)

Blocks:
- Phase 20.9 (Parquet write) — merged into this subphase

## Affected files

New files:
- `crates/axiomdb-sql/src/executor/parquet_read.rs` — READ_PARQUET row materializer
- `crates/axiomdb-sql/tests/integration_parquet.rs` — integration tests

Modified files:
- `crates/axiomdb-sql/Cargo.toml` — add `parquet = "58"` dependency
- `crates/axiomdb-sql/src/ast.rs` — ReadParquetClause, FromClause::ReadParquet, CopyFormat::Parquet, ParquetCompression, CopyOptions.compression
- `crates/axiomdb-sql/src/parser/dml.rs` — parse READ_PARQUET FROM, PARQUET format, COMPRESSION option
- `crates/axiomdb-sql/src/analyzer_bind.rs` — bind ReadParquet: open file, read schema, build BoundTable
- `crates/axiomdb-sql/src/executor/select_core.rs` — dispatch ReadParquet like GenerateSeries
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — handle ReadParquet guard
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — ReadParquet as JOIN right-side
- `crates/axiomdb-sql/src/executor/select_helpers.rs` — ReadParquet exhaustive arm
- `crates/axiomdb-sql/src/executor/dml_join.rs` — ReadParquet arm (NotImplemented)
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — ReadParquet arm (not yet supported)
- `crates/axiomdb-sql/src/executor/copy_to.rs` — CopyFormat::Parquet write path
- `crates/axiomdb-sql/src/executor/mod.rs` — include!("parquet_read.rs")
- `tools/wire-test.py` — new wire assertions (552/552+)
- `docs-site/src/user-guide/sql-reference/dml.md` — READ_PARQUET + COPY TO PARQUET sections
- `docs-site/src/internals/sql-parser.md` — new TVF + COPY extension

---

## Step 1 — AST structs + Cargo dependency

**Goal:** Add the parquet crate + all new AST types so the project still compiles.
**Files:** `Cargo.toml`, `ast.rs`

### Test to add (compile-only — no new behavior yet)

```rust
// crates/axiomdb-sql/tests/integration_parquet.rs
mod common;
// placeholder: ensures new AST types compile
#[test]
fn test_parquet_ast_placeholder() {
    let _ = axiomdb_sql::ast::CopyFormat::Parquet;
}
```

### Implementation outline

`Cargo.toml`:
```toml
parquet = { version = "54", default-features = false, features = ["snap"] }
```
(Note: check `cargo search parquet` for latest stable; default-features=false avoids arrow.)

`ast.rs` — after `FromClause::GenerateSeries`:
```rust
/// Phase 20.6 — `READ_PARQUET('path') [AS alias [(col1, col2, ...)]]`.
ReadParquet(Box<ReadParquetClause>),

pub struct ReadParquetClause {
    pub path: String,
    pub alias: Option<String>,
    pub column_aliases: Vec<String>,
}
```

`ast.rs` — extend `CopyFormat`:
```rust
pub enum CopyFormat { Csv, Json, Jsonl, Parquet }
```

`ast.rs` — new type + extend `CopyOptions`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParquetCompression { Snappy, Uncompressed }

pub struct CopyOptions {
    // existing fields ...
    pub compression: Option<ParquetCompression>,
}
```

Fix all exhaustive `CopyFormat` match arms that need a `Parquet` arm (copy_to.rs,
resolve_copy_format, etc.) — add `CopyFormat::Parquet =>` stubs returning
`DbError::NotImplemented` until Step 5 replaces them.

### Verification

```bash
./tools/vm.sh cargo build -p axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(fase-20): ast — ReadParquetClause, CopyFormat::Parquet, parquet dep (20.6 step 1)
```

---

## Step 2 — Parser: READ_PARQUET + COPY TO FORMAT PARQUET

**Goal:** Parse `SELECT * FROM READ_PARQUET('path') [AS alias [(cols)]]` and
`COPY t TO 'p' WITH (FORMAT PARQUET [, COMPRESSION SNAPPY])`.
**Files:** `parser/dml.rs`

### Test to add

```rust
// integration_parquet.rs
use axiomdb_sql::{parse, ast::*};

#[test]
fn test_parse_read_parquet_basic() {
    let stmt = parse("SELECT * FROM READ_PARQUET('/tmp/x.parquet')").unwrap();
    // verify it parses to a SelectStmt with FromClause::ReadParquet
    if let Stmt::Select(s) = stmt {
        assert!(matches!(s.from, Some(FromClause::ReadParquet(_))));
    } else { panic!("expected Select"); }
}

#[test]
fn test_parse_copy_to_parquet() {
    let stmt = parse("COPY users TO '/tmp/u.parquet' WITH (FORMAT PARQUET)").unwrap();
    if let Stmt::CopyTo(c) = stmt {
        assert_eq!(c.options.format, Some(CopyFormat::Parquet));
    } else { panic!("expected CopyTo"); }
}
```

### Implementation outline

In `parse_from_clause`, before the existing `Table` fallback:
```rust
// Phase 20.6 — READ_PARQUET('path') [AS alias [(col, ...)]]
if let Token::Ident(name) = p.peek().clone() {
    if name.eq_ignore_ascii_case("READ_PARQUET") {
        p.advance(); // consume ident
        p.expect(&Token::LParen)?;
        let path = match p.peek().clone() {
            Token::StringLit(s) => { p.advance(); s }
            _ => return Err(ParseError { ... "READ_PARQUET requires a string path" })
        };
        p.expect(&Token::RParen)?;
        let (alias, column_aliases) = parse_optional_as_alias_with_columns(p)?;
        return Ok(FromClause::ReadParquet(Box::new(ReadParquetClause { path, alias, column_aliases })));
    }
}
```

In `parse_copy_options`, add `PARQUET` to the FORMAT arm:
```rust
"PARQUET" => CopyFormat::Parquet,
```

Add `COMPRESSION` key:
```rust
"COMPRESSION" => {
    let comp = p.parse_identifier()?.to_ascii_uppercase();
    opts.compression = Some(match comp.as_str() {
        "SNAPPY" => ParquetCompression::Snappy,
        "UNCOMPRESSED" => ParquetCompression::Uncompressed,
        other => return Err(ParseError { ... "unknown COMPRESSION; expected SNAPPY or UNCOMPRESSED" }),
    });
}
```

Also update the error message in the `other =>` arm to include `COMPRESSION`.

### Verification

```bash
./tools/vm.sh cargo nextest run -p axiomdb-sql --test integration_parquet 2>&1 | tail -10
./tools/vm.sh cargo clippy -p axiomdb-sql -- -D warnings 2>&1 | head -10
```

### Commit

```
feat(fase-20): parser — READ_PARQUET FROM, FORMAT PARQUET, COMPRESSION option (20.6 step 2)
```

---

## Step 3 — Analyzer bind: schema discovery at bind time

**Goal:** When the analyzer sees `FromClause::ReadParquet`, open the Parquet file,
read its schema, and produce a `BoundTable` with real column names and types.
**Files:** `analyzer_bind.rs`

### Test to add

```rust
// integration_parquet.rs — uses a real .parquet file written inline

fn make_test_parquet(path: &str, rows: Vec<Vec<parquet::record::Field>>) { ... }
// (Alternatively, write parquet files via the COPY TO path once Step 5 is done;
//  for Step 3, test with a pre-made file or skip until Step 6 for end-to-end.)
```

Since the bind test requires a real file, Step 3 can be partially verified via the
unit compile test + Step 6 end-to-end. Full bind error tests (file not found, nested
column) are in the integration suite.

### Implementation outline

In `bound_from_clause`, after the `GenerateSeries` arm:
```rust
FromClause::ReadParquet(rp) => {
    let path = &rp.path;
    let file = std::fs::File::open(path).map_err(|e| DbError::Io(std::io::Error::new(
        e.kind(), format!("READ_PARQUET: cannot open '{}': {e}", path),
    )))?;
    let reader = parquet::file::reader::SerializedFileReader::new(file)
        .map_err(|e| DbError::InvalidValue { reason: format!("READ_PARQUET schema error: {e}") })?;
    let schema = reader.metadata().file_metadata().schema_descr().root_schema();
    let fields = schema.get_fields();
    let mut columns = Vec::with_capacity(fields.len());
    for field in fields {
        let data_type = parquet_type_to_axiomdb(field)?;
        columns.push(ColumnDef { name: field.name().to_string(), data_type, ... });
    }
    // apply column_aliases if provided
    if !rp.column_aliases.is_empty() {
        if rp.column_aliases.len() != columns.len() {
            return Err(DbError::InvalidValue { reason: "READ_PARQUET column alias count mismatch".into() });
        }
        for (col, alias) in columns.iter_mut().zip(&rp.column_aliases) {
            col.name = alias.clone();
        }
    }
    let n = columns.len();
    let alias = rp.alias.clone().unwrap_or_else(|| "read_parquet".into());
    let bound = BoundTable { alias: Some(alias.clone()), name: alias, columns, col_offset: *col_offset };
    *col_offset += n;
    Ok(vec![bound])
}
```

`parquet_type_to_axiomdb(field)` — private fn in analyzer_bind.rs mapping Parquet
`BasicTypeInfo`+`LogicalType` to `DataType`:
- INT32 + DATE → DataType::Date
- INT32 + DECIMAL(p,s) → DataType::Decimal(p,s)
- INT32 → DataType::Int
- INT64 + TIMESTAMP_MICROS/MILLIS → DataType::Timestamp
- INT64 + DECIMAL(p,s) → DataType::Decimal(p,s)
- INT64 → DataType::BigInt
- FLOAT/DOUBLE → DataType::Real
- BOOLEAN → DataType::Bool
- BYTE_ARRAY + UTF8/STRING → DataType::Text
- BYTE_ARRAY (none) → DataType::Text
- FIXED_LEN_BYTE_ARRAY + DECIMAL → DataType::Decimal(p,s)
- repeated/complex → DbError::NotSupported { feature: "Parquet nested types" }

### Verification

```bash
./tools/vm.sh cargo build -p axiomdb-sql 2>&1 | tail -5
./tools/vm.sh cargo clippy -p axiomdb-sql -- -D warnings 2>&1 | head -10
```

### Commit

```
feat(fase-20): analyzer — READ_PARQUET schema discovery at bind time (20.6 step 3)
```

---

## Step 4 — Executor: READ_PARQUET source

**Goal:** Execute `SELECT * FROM READ_PARQUET('path')` — load Parquet rows into
`Vec<Row>` and feed through the existing select pipeline.
**Files:** `executor/parquet_read.rs` (new), `executor/mod.rs`, `executor/select_core.rs`,
`executor/select_ctx.rs`, `executor/select_joins_ctx.rs`, `executor/select_helpers.rs`,
`executor/dml_join.rs`, `executor/exec_explain.rs`

### Test to add

```rust
// integration_parquet.rs — round-trip test deferred to Step 6 (needs write too).
// Parser + bind tests compile-verified here.
```

### Implementation outline

New `parquet_read.rs` (included into mod.rs):
```rust
// Phase 20.6 — READ_PARQUET executor
fn read_parquet_rows(path: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;

    let file = std::fs::File::open(path).map_err(|e| DbError::Io(...))?;
    let reader = SerializedFileReader::new(file)
        .map_err(|e| DbError::InvalidValue { reason: format!("READ_PARQUET read error: {e}") })?;
    let schema = reader.metadata().file_metadata().schema_descr().clone();
    let col_names: Vec<String> = schema.root_schema().get_fields()
        .iter().map(|f| f.name().to_string()).collect();

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let iter = reader.get_row_iter(None)
        .map_err(|e| DbError::InvalidValue { reason: format!("READ_PARQUET iter: {e}") })?;
    for row_result in iter {
        let row = row_result.map_err(|e| DbError::InvalidValue { reason: format!("READ_PARQUET row: {e}") })?;
        let values: Vec<Value> = (0..col_names.len())
            .map(|i| parquet_field_to_value(row.get_field(i)))
            .collect();
        rows.push(values);
    }
    Ok((col_names, rows))
}

fn parquet_field_to_value(field: &parquet::record::Field) -> Value {
    use parquet::record::Field;
    match field {
        Field::Null => Value::Null,
        Field::Bool(b) => Value::Bool(*b),
        Field::Int(n) => Value::Int(*n),
        Field::Long(n) => Value::BigInt(*n),
        Field::Float(f) => Value::Real(*f as f64),
        Field::Double(f) => Value::Real(*f),
        Field::Str(s) => Value::Text(s.clone()),
        Field::Bytes(b) => Value::Text(String::from_utf8_lossy(b.data()).into_owned()),
        Field::Date(days) => Value::Date(*days),
        Field::TimestampMicros(ts) | Field::TimestampMillis(ts) => Value::BigInt(*ts), // mapped to Timestamp
        Field::Decimal(d) => { /* convert mantissa+scale to Value::Decimal */ Value::Null }
        _ => Value::Null, // nested types not supported (caught at bind time)
    }
}

fn execute_select_read_parquet_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &dyn TxnCoordinator,
    conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let rp = match stmt.from.take() {
        Some(FromClause::ReadParquet(rp)) => *rp,
        _ => unreachable!(),
    };
    let (col_names, rows) = read_parquet_rows(&rp.path)?;
    // Apply column_aliases if provided
    let final_names = if rp.column_aliases.is_empty() {
        col_names
    } else {
        rp.column_aliases.clone()
    };
    // Build DerivedSource and feed through select pipeline
    let derived_cols = final_names.iter().enumerate()
        .map(|(i, name)| ColumnMeta { name: name.clone(), col_idx: i })
        .collect::<Vec<_>>();
    execute_select_derived_source(stmt, derived_cols, rows, storage, txn, conn_txn)
}
```

In `select_core.rs`, add before the `from_table_ref` extraction:
```rust
if matches!(stmt.from, Some(FromClause::ReadParquet(_))) {
    return execute_select_read_parquet_source(stmt, storage, txn, conn_txn);
}
```

Update `select_ctx.rs` guard, `select_joins_ctx.rs` JOIN arm (materialize via
`read_parquet_rows`), `select_helpers.rs` arm (`NotImplemented`), `dml_join.rs` arm
(`NotImplemented`), `exec_explain.rs` arm (`not yet supported`). Each follows the
exact same pattern as GenerateSeries.

Update `mod.rs`:
```rust
include!("parquet_read.rs");
```

### Verification

```bash
./tools/vm.sh cargo nextest run -p axiomdb-sql 2>&1 | tail -10
./tools/vm.sh cargo clippy -p axiomdb-sql -- -D warnings 2>&1 | head -10
```

### Commit

```
feat(fase-20): executor — READ_PARQUET TVF source (20.6 step 4)
```

---

## Step 5 — Executor: COPY TO FORMAT PARQUET

**Goal:** Write a Parquet file from a full table scan using the columnar writer.
**Files:** `executor/copy_to.rs`

### Test to add (partial — full round-trip in Step 6)

```rust
// integration_parquet.rs
#[test]
fn test_copy_to_parquet_creates_file() {
    let path = "/tmp/axm_parquet_basic.parquet";
    let r = run_multi(&[
        "CREATE TABLE axm_pq_basic (id INT, name VARCHAR(20))",
        "INSERT INTO axm_pq_basic VALUES (1, 'alice'), (2, 'bob')",
        &format!("COPY axm_pq_basic TO '{path}' WITH (FORMAT PARQUET)"),
    ]);
    assert!(matches!(r, QueryResult::Affected { .. }));
    assert!(std::path::Path::new(path).exists());
}
```

### Implementation outline

In `copy_to.rs`, replace the `CopyFormat::Parquet` stub with a real implementation:
```rust
CopyFormat::Parquet => {
    let compression = stmt.options.compression.as_ref()
        .map(|c| match c {
            ParquetCompression::Snappy => parquet::basic::Compression::SNAPPY,
            ParquetCompression::Uncompressed => parquet::basic::Compression::UNCOMPRESSED,
        })
        .unwrap_or(parquet::basic::Compression::UNCOMPRESSED);
    write_parquet(&stmt.path, &col_names, &rows, compression)?;
}
```

`write_parquet` function:
1. Infer schema from first non-null row (BYTE_ARRAY for null-only columns)
2. Build `parquet::schema::types::Type` tree (one field per column, all OPTIONAL)
3. Create `SerializedFileWriter` with schema + writer properties (compression)
4. For each row group (single group — all rows): write one column at a time
5. Use column writers: `BoolColumnWriter`, `Int32ColumnWriter`, `Int64ColumnWriter`,
   `FloatColumnWriter`, `DoubleColumnWriter`, `ByteArrayColumnWriter`
6. Flush + close

Value → Parquet column type mapping:
- `Value::Bool` → BOOLEAN / BoolColumnWriter
- `Value::Int` → INT32 / Int32ColumnWriter + LogicalType::Integer(32, true)
- `Value::BigInt` / `Value::Timestamp` → INT64 / Int64ColumnWriter
- `Value::Date` → INT32 / Int32ColumnWriter + LogicalType::Date
- `Value::Timestamp` → INT64 + LogicalType::Timestamp(MICROS, UTC=true)
- `Value::Real` → DOUBLE / DoubleColumnWriter
- `Value::Decimal(m, s)` → INT64 + LogicalType::Decimal(18, s)
- `Value::Text` / `Value::Json` / `Value::Jsonb` / `Value::Uuid` / `Value::Array` → BYTE_ARRAY + LogicalType::String
- `Value::Bytes` → BYTE_ARRAY (no logical type)
- `Value::Null` → written as null in the definition level

Schema inference: iterate first non-null value per column to determine physical+logical type.
Null-only column defaults to BYTE_ARRAY.

### Verification

```bash
./tools/vm.sh cargo nextest run -p axiomdb-sql 2>&1 | tail -10
./tools/vm.sh cargo clippy -p axiomdb-sql -- -D warnings 2>&1 | head -10
```

### Commit

```
feat(fase-20): executor — COPY TO FORMAT PARQUET write (20.6 step 5)
```

---

## Step 6 — Integration tests (14+ tests)

**Goal:** Full coverage: round-trip, type fidelity, edge cases, error cases.
**Files:** `tests/integration_parquet.rs`

### Tests to add

```rust
// --- COPY TO tests ---
test_copy_to_parquet_creates_file          // file exists after COPY
test_copy_to_parquet_empty_table           // 0-row table → 0-row parquet
test_copy_to_parquet_snappy_compression    // WITH (FORMAT PARQUET, COMPRESSION SNAPPY)
test_copy_to_parquet_all_types             // INT, BIGINT, REAL, BOOL, TEXT, DATE, TIMESTAMP columns

// --- READ_PARQUET tests ---
test_read_parquet_basic                    // SELECT * FROM READ_PARQUET('/tmp/...')
test_read_parquet_projection               // SELECT id, name FROM READ_PARQUET(...)
test_read_parquet_where_filter             // SELECT … WHERE score > 90
test_read_parquet_alias                    // FROM READ_PARQUET(...) AS p; SELECT p.id
test_read_parquet_column_aliases           // AS p(id, score) explicit rename
test_read_parquet_order_by                 // ORDER BY on parquet result
test_read_parquet_limit                    // LIMIT on parquet result
test_read_parquet_null_values              // nullable column → Value::Null in result
test_read_parquet_empty_file               // 0-row parquet → 0 rows SELECT

// --- JOIN + CTE ---
test_read_parquet_in_cte                   // WITH raw AS (SELECT * FROM READ_PARQUET(...))
test_read_parquet_join_real_table          // JOIN users ON t.id = p.user_id

// --- Round-trip ---
test_parquet_round_trip                    // COPY TO then READ_PARQUET → identical data

// --- Error cases ---
test_read_parquet_file_not_found           // DbError::Io at bind time
test_copy_to_parquet_bad_path              // DbError::Io at execute time
test_read_parquet_no_arg_parse_error       // READ_PARQUET() → ParseError
test_read_parquet_column_alias_mismatch    // alias count mismatch → InvalidValue
```

Total: 17+ tests (exceeds the 14 minimum from spec).

### Verification

```bash
./tools/vm.sh cargo nextest run -p axiomdb-sql --test integration_parquet 2>&1 | tail -20
./tools/vm.sh cargo nextest run --workspace 2>&1 | tail -10
./tools/vm.sh cargo clippy --workspace -- -D warnings 2>&1 | head -10
./tools/vm.sh fmt 2>&1 | head -5
```

### Wire test assertions (add to tools/wire-test.py)

```python
# [20.6 parquet round-trip] — write a parquet file then read it back
cur.execute("DROP TABLE IF EXISTS wire_pq_t")
cur.execute("CREATE TABLE wire_pq_t (id INT, val TEXT)")
cur.execute("INSERT INTO wire_pq_t VALUES (1, 'hello'), (2, 'world')")
conn.commit()
cur.execute("COPY wire_pq_t TO '/tmp/wire_pq.parquet' WITH (FORMAT PARQUET)")
conn.commit()
cur.execute("SELECT * FROM READ_PARQUET('/tmp/wire_pq.parquet') ORDER BY id")
rows = cur.fetchall()
assert rows[0][0] == 1 and rows[0][1] == 'hello', f"[20.6 parquet row 1] got {rows[0]}"
assert rows[1][0] == 2 and rows[1][1] == 'world', f"[20.6 parquet row 2] got {rows[1]}"

# [20.6 parquet column count] — schema discovery
cur.execute("SELECT id FROM READ_PARQUET('/tmp/wire_pq.parquet') WHERE id = 1")
r = cur.fetchone()
assert r[0] == 1, f"[20.6 parquet projection] got {r}"

# [20.6 parquet copy empty] — empty table → 0 rows
cur.execute("DROP TABLE IF EXISTS wire_pq_empty")
cur.execute("CREATE TABLE wire_pq_empty (x INT)")
cur.execute("COPY wire_pq_empty TO '/tmp/wire_pq_empty.parquet' WITH (FORMAT PARQUET)")
conn.commit()
cur.execute("SELECT COUNT(*) FROM READ_PARQUET('/tmp/wire_pq_empty.parquet')")
r = cur.fetchone()
assert r[0] == 0, f"[20.6 parquet empty count] got {r}"
```

### Commit

```
feat(fase-20): integration tests + wire smoke — 17 parquet tests, 3 wire assertions (20.6 step 6)
```

---

## Step 7 — Docs + close

**Goal:** Update user docs + internals doc, close subphase.
**Files:** `docs-site/src/user-guide/sql-reference/dml.md`,
`docs-site/src/internals/sql-parser.md`, `docs/progreso.md`,
`memory/project_state.md`

### Docs to add

**dml.md** — new "READ_PARQUET" section:
- Syntax: `SELECT … FROM READ_PARQUET('path') [AS alias [(col1, ...)]]`
- Column alias, WHERE, ORDER BY, JOIN, CTE examples
- Supported types table
- Error cases

**dml.md** — extend COPY TO section:
- `WITH (FORMAT PARQUET [, COMPRESSION SNAPPY|UNCOMPRESSED])`
- Schema inference rules (first non-null row)
- Null-only column defaults to BYTE_ARRAY

**sql-parser.md** — Phase 20.6:
- AST variants: `FromClause::ReadParquet`, `CopyFormat::Parquet`
- Parser recognition: `READ_PARQUET` as non-reserved ident in FROM position
- Bind-time schema discovery via `parquet::file::reader::SerializedFileReader`
- Executor pattern: mirrors GenerateSeries TVF

### progreso.md update

Mark `20.6 ✅ Parquet read (READ_PARQUET TVF) + write (COPY TO FORMAT PARQUET)`.
Count: 159/442 (36.0%).

### Commit

```
progress(20.6): complete Parquet read + write — READ_PARQUET TVF, COPY TO FORMAT PARQUET

- 17 integration tests, 3 wire assertions (552/552)
- parquet crate (row-level API, no arrow dependency)
- Schema discovery at bind time from Parquet file metadata
- Consolidates 20.9 (Parquet write) into 20.6
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `parquet` crate API differs from v54 docs | medium | Check docs.rs for exact version in Cargo.lock after first build |
| Columnar write API is complex (one column at a time) | medium | Follow parquet crate examples; wrap in helper functions |
| Timestamp field variant name mismatch in parquet::record::Field | low | Check Field enum at compile time |
| Decimal representation in parquet::record::Field | low | Use i128 mantissa from Decimal type; fall back to BigInt if unavailable |

## Rollback plan

If abandoned mid-way:
1. `git reset --hard <commit before Step 1>`
2. Leave partial work on current branch with note in progreso.md
3. Mark spec status back to `draft`

## Estimated effort

Total: ~5 hours
- Step 1 (AST + cargo): 20 min
- Step 2 (parser): 30 min
- Step 3 (analyzer bind): 45 min
- Step 4 (executor read): 60 min
- Step 5 (executor write): 75 min
- Step 6 (tests + wire): 45 min
- Step 7 (docs + close): 30 min
