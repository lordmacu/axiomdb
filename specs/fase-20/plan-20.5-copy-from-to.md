# Plan: 20.5 — COPY FROM / TO

Phase: 20 — Types + import/export
Task: Bulk data import and export via COPY
Spec: specs/fase-20/spec-20.5-copy-from-to.md
Status: done

## Summary

Four steps in dependency order. Step 1 adds `csv = "1"` to the workspace,
defines `CopyFromStmt`, `CopyToStmt`, `CopyFormat`, `CopyOptions` in the AST,
and parses `COPY t FROM/TO 'path' [WITH (...)]`. Step 2 implements `COPY FROM`
by parsing the source file and routing rows through the existing
`execute_insert_ctx` path. Step 3 implements `COPY TO` via a full table scan
followed by file serialization. Step 4 adds wire-test assertions and doc-site
pages. All steps follow TDD order.

## Dependencies

Must be done first:
- [x] spec-20.5-copy-from-to.md approved

Blocks (until this plan is done):
- [ ] Phase 20 closure

## Affected files

New files:
- `crates/axiomdb-sql/src/executor/copy_from.rs` — COPY FROM executor
- `crates/axiomdb-sql/src/executor/copy_to.rs` — COPY TO executor
- `crates/axiomdb-sql/tests/integration_copy.rs` — integration tests

Modified files:
- `Cargo.toml` — add `csv = "1"` to `[workspace.dependencies]`
- `crates/axiomdb-sql/Cargo.toml` — add `csv = { workspace = true }`
- `crates/axiomdb-sql/src/ast.rs` — new AST types + `Stmt::CopyFrom/CopyTo`
- `crates/axiomdb-sql/src/parser/dml.rs` — `parse_copy_stmt`
- `crates/axiomdb-sql/src/executor/mod.rs` — include new files
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — dispatch new Stmt variants
- `tools/wire-test.py` — 4 new assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — COPY section
- `docs-site/src/user-guide/features/data-import-export.md` — new page

---

## Step 1 — `csv` crate + AST + Parser

**Goal:** Parse `COPY t FROM/TO 'path' [WITH (...)]`; all four formats + options
**Files:** `Cargo.toml`, `crates/axiomdb-sql/Cargo.toml`, `ast.rs`, `parser/dml.rs`

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_ddl_parser.rs (append)

#[test]
fn parse_copy_from_csv_with_options() {
    let stmt = parse_one(
        "COPY t FROM '/tmp/data.csv' WITH (FORMAT CSV, HEADER TRUE, DELIMITER ',')"
    );
    let Stmt::CopyFrom(c) = stmt else { panic!("expected CopyFrom") };
    assert_eq!(c.table, "t");
    assert_eq!(c.path, "/tmp/data.csv");
    assert_eq!(c.options.format, Some(CopyFormat::Csv));
    assert_eq!(c.options.header, Some(true));
    assert_eq!(c.options.delimiter, Some(','));
}

#[test]
fn parse_copy_to_jsonl_no_options() {
    let stmt = parse_one("COPY orders TO '/tmp/out.jsonl'");
    let Stmt::CopyTo(c) = stmt else { panic!("expected CopyTo") };
    assert_eq!(c.table, "orders");
    assert_eq!(c.path, "/tmp/out.jsonl");
    assert_eq!(c.options.format, None); // auto-detect from extension
}

#[test]
fn parse_copy_from_json() {
    let stmt = parse_one("COPY items FROM '/tmp/in.json' WITH (FORMAT JSON)");
    let Stmt::CopyFrom(c) = stmt else { panic!("expected CopyFrom") };
    assert_eq!(c.options.format, Some(CopyFormat::Json));
}

#[test]
fn parse_copy_header_false() {
    let stmt = parse_one("COPY t FROM '/tmp/rows.csv' WITH (FORMAT CSV, HEADER FALSE)");
    let Stmt::CopyFrom(c) = stmt else { panic!("expected CopyFrom") };
    assert_eq!(c.options.header, Some(false));
}

#[test]
fn parse_copy_null_option() {
    let stmt = parse_one("COPY t FROM '/p.csv' WITH (FORMAT CSV, NULL 'NULL')");
    let Stmt::CopyFrom(c) = stmt else { panic!("expected CopyFrom") };
    assert_eq!(c.options.null_str, Some("NULL".into()));
}

#[test]
fn parse_copy_to_csv_with_header() {
    let stmt = parse_one("COPY products TO '/out.csv' WITH (FORMAT CSV, HEADER TRUE)");
    let Stmt::CopyTo(c) = stmt else { panic!("expected CopyTo") };
    assert_eq!(c.options.format, Some(CopyFormat::Csv));
    assert_eq!(c.options.header, Some(true));
}
```

### Implementation outline

```rust
// ast.rs — add before CopyFromStmt

#[derive(Debug, Clone, PartialEq)]
pub enum CopyFormat { Csv, Json, Jsonl }

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CopyOptions {
    pub format: Option<CopyFormat>,
    pub header: Option<bool>,
    pub delimiter: Option<char>,
    pub null_str: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopyFromStmt {
    pub table: String,
    pub path: String,
    pub options: CopyOptions,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopyToStmt {
    pub table: String,
    pub path: String,
    pub options: CopyOptions,
}

// In Stmt enum (after Delete):
CopyFrom(CopyFromStmt),
CopyTo(CopyToStmt),
```

```rust
// parser/dml.rs — add parse_copy_stmt

pub fn parse_copy_stmt(p: &mut Parser) -> Result<Stmt, DbError> {
    // COPY table FROM|TO 'path' [WITH ( options )]
    let table = parse_ident(p)?;
    let is_from = if eat_keyword(p, Keyword::From) {
        true
    } else if eat_ident_ci(p, "TO") {
        false
    } else {
        return Err(parse_error(p, "expected FROM or TO after COPY"));
    };
    let path = parse_string_literal(p)?;
    let options = if eat_keyword(p, Keyword::With) {
        expect_token(p, Token::LParen)?;
        parse_copy_options(p)?
    } else {
        CopyOptions::default()
    };
    if is_from {
        Ok(Stmt::CopyFrom(CopyFromStmt { table, path, options }))
    } else {
        Ok(Stmt::CopyTo(CopyToStmt { table, path, options }))
    }
}

fn parse_copy_options(p: &mut Parser) -> Result<CopyOptions, DbError> {
    // FORMAT CSV|JSON|JSONL, HEADER TRUE|FALSE, DELIMITER 'c', NULL 'str'
    // ... loop until ')'
}
```

Plug into the top-level parser dispatch on the `COPY` keyword token (add `Keyword::Copy` or
use `eat_ident_ci(p, "COPY")`).

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_ddl_parser
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): AST CopyFromStmt/CopyToStmt + parser COPY FROM/TO (20.5 step 1)

Adds CopyFormat, CopyOptions, CopyFromStmt, CopyToStmt to AST.
parse_copy_stmt recognises COPY t FROM/TO 'path' [WITH (...)].
csv = "1" added to workspace. 6 parser tests.
```

---

## Step 2 — COPY FROM executor

**Goal:** Import data from CSV / JSON / JSONL file into a table
**Files:** `executor/copy_from.rs`, `executor/mod.rs`, `executor/exec_dispatch.rs`,
          `crates/axiomdb-sql/tests/integration_copy.rs`

### Strategy

Parse the file into `Vec<(col_name, Value)>` rows, then build an `InsertStmt`
with `InsertSource::Values(...)` and call `execute_insert_ctx`. This reuses
auto-increment, FK enforcement, trigger firing, and type coercion — for free.

Column name → table column matching:
- `HEADER TRUE` (default): read header row, match by name (case-insensitive)
- `HEADER FALSE`: positional — order must match table schema

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_copy.rs

fn setup_copy_table(db: &Database) {
    db.exec("CREATE TABLE copy_test (id INT, name TEXT, score FLOAT)").unwrap();
}

// T1: basic CSV round-trip (HEADER TRUE)
#[test]
fn copy_from_csv_basic() {
    // write CSV file to tempdir, COPY FROM, SELECT * to verify
}

// T2: CSV HEADER FALSE — positional columns
#[test]
fn copy_from_csv_no_header_positional() { ... }

// T3: JSONL import
#[test]
fn copy_from_jsonl_basic() { ... }

// T4: JSON array import
#[test]
fn copy_from_json_array() { ... }

// T5: NULL via \N in CSV
#[test]
fn copy_from_csv_null_value() { ... }

// T6: empty file → 0 rows imported, no error
#[test]
fn copy_from_empty_file_returns_zero() { ... }

// T7: file not found → DbError::Io
#[test]
fn copy_from_missing_file_errors() { ... }

// T8: column count mismatch (HEADER FALSE) → error with line number
#[test]
fn copy_from_csv_column_count_mismatch_errors() { ... }
```

### Implementation outline

```rust
// executor/copy_from.rs

fn execute_copy_from(
    stmt: CopyFromStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let format = resolve_format(&stmt.options, &stmt.path);
    let header = stmt.options.header.unwrap_or(format == CopyFormat::Csv);
    let delimiter = stmt.options.delimiter.unwrap_or(',');
    let null_str = stmt.options.null_str.as_deref().unwrap_or(r"\N");

    let file = std::fs::File::open(&stmt.path).map_err(|e| DbError::Io {
        message: format!("COPY FROM: cannot open '{}': {}", stmt.path, e),
    })?;

    let (columns, value_rows): (Vec<String>, Vec<Vec<Value>>) = match format {
        CopyFormat::Csv => parse_csv(file, header, delimiter, null_str, exec_ctx, &stmt.table, conn_txn, ctx)?,
        CopyFormat::Json => parse_json_array(file)?,
        CopyFormat::Jsonl => parse_jsonl(file)?,
    };

    if value_rows.is_empty() {
        return Ok(QueryResult::Affected { count: 0 });
    }

    // Convert Vec<Vec<Value>> → InsertStmt → execute_insert_ctx
    let expr_rows: Vec<Vec<Expr>> = value_rows.into_iter()
        .map(|row| row.into_iter().map(Expr::Literal).collect())
        .collect();

    let table_ref = TableRef { schema: None, name: stmt.table.clone() };
    let insert_stmt = InsertStmt {
        table: table_ref,
        columns: Some(columns),
        source: InsertSource::Values(expr_rows),
        ignore: false,
        replace: false,
        returning: vec![],
        on_duplicate_update: None,
    };

    execute_insert_ctx(insert_stmt, exec_ctx, conn_txn, ctx)
}

fn resolve_format(opts: &CopyOptions, path: &str) -> CopyFormat {
    if let Some(ref f) = opts.format { return f.clone(); }
    match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("csv")   => CopyFormat::Csv,
        Some("json")  => CopyFormat::Json,
        Some("jsonl") | Some("ndjson") => CopyFormat::Jsonl,
        _ => CopyFormat::Csv,
    }
}
```

Wire into `exec_dispatch.rs`:
```rust
Stmt::CopyFrom(s) => {
    let mut conn = ctx.conn_txn.take().expect("conn_txn set");
    let r = execute_copy_from(s, exec_ctx, &mut conn, ctx);
    ctx.conn_txn = Some(conn);
    r
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_copy
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): COPY FROM executor — CSV, JSON, JSONL import (20.5 step 2)

parse_csv uses csv crate; parse_json_array and parse_jsonl use serde_json.
Rows routed through execute_insert_ctx (auto-increment, FK, triggers).
8 integration tests.
```

---

## Step 3 — COPY TO executor

**Goal:** Export table data to CSV / JSON / JSONL file
**Files:** `executor/copy_to.rs`, `executor/mod.rs`, `executor/exec_dispatch.rs`,
          `crates/axiomdb-sql/tests/integration_copy.rs` (add more tests)

### Tests to add (append to integration_copy.rs)

```rust
// T9: CSV export with header
#[test]
fn copy_to_csv_basic() { ... }

// T10: JSONL export
#[test]
fn copy_to_jsonl_basic() { ... }

// T11: JSON array export
#[test]
fn copy_to_json_array() { ... }

// T12: round-trip CSV (COPY TO then COPY FROM, compare rows)
#[test]
fn copy_roundtrip_csv() { ... }

// T13: round-trip JSONL
#[test]
fn copy_roundtrip_jsonl() { ... }

// T14: COPY TO empty table → 0 rows, file with header only
#[test]
fn copy_to_empty_table() { ... }

// T15: COPY TO: file overwritten if exists
#[test]
fn copy_to_overwrites_existing_file() { ... }
```

### Implementation outline

```rust
// executor/copy_to.rs

fn execute_copy_to(
    stmt: CopyToStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // 1. Resolve table
    let resolved = resolve_table_cached(exec_ctx.storage(), exec_ctx.coord(), ctx, conn_txn, &stmt.table)?;
    let col_names: Vec<String> = resolved.def.columns.iter().map(|c| c.name.clone()).collect();

    // 2. Full table scan
    let rows = full_heap_scan(exec_ctx, &resolved, conn_txn)?; // reuse existing scan

    // 3. Open output file (create or truncate)
    let file = std::fs::File::create(&stmt.path).map_err(|e| DbError::Io {
        message: format!("COPY TO: cannot create '{}': {}", stmt.path, e),
    })?;
    let mut writer = BufWriter::new(file);

    let format = resolve_format(&stmt.options, &stmt.path);
    let header = stmt.options.header.unwrap_or(format == CopyFormat::Csv);
    let count = rows.len() as u64;

    match format {
        CopyFormat::Csv => write_csv(&mut writer, &col_names, &rows, header, ',')?,
        CopyFormat::Json => write_json_array(&mut writer, &col_names, &rows)?,
        CopyFormat::Jsonl => write_jsonl(&mut writer, &col_names, &rows)?,
    }

    writer.flush().map_err(|e| DbError::Io { message: e.to_string() })?;
    Ok(QueryResult::Affected { count })
}
```

Wire into `exec_dispatch.rs`:
```rust
Stmt::CopyTo(s) => {
    let conn_ref = ctx.conn_txn.as_ref();
    execute_copy_to(s, exec_ctx, conn_ref, ctx)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_copy
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
./tools/vm.sh test --workspace
```

### Commit

```
feat(fase-20): COPY TO executor — CSV, JSON, JSONL export (20.5 step 3)

Full table scan + format serialization. Round-trip tests (CSV + JSONL).
7 more integration tests (15 total in integration_copy.rs).
```

---

## Step 4 — Wire test + docs

**Goal:** Verify COPY through the MySQL wire protocol; update docs-site
**Files:** `tools/wire-test.py`, `docs-site/…`

### Wire test additions (4 assertions)

```python
# [20.5] COPY FROM / TO
with tempfile.NamedTemporaryFile(suffix='.csv', delete=False, mode='w') as f:
    f.write("id,val\n1,alpha\n2,beta\n")
    csv_path = f.name

cur.execute("CREATE TABLE wire_copy (id INT, val TEXT)")
cur.execute(f"COPY wire_copy FROM '{csv_path}' WITH (FORMAT CSV, HEADER TRUE)")
ok("[20.5 copy_from_csv] COPY FROM CSV loads rows",
   cur.rowcount == 2, cur.rowcount)

cur.execute("SELECT id, val FROM wire_copy ORDER BY id")
rows = cur.fetchall()
ok("[20.5 copy_from_csv_content] COPY FROM CSV content correct",
   rows == [(1, 'alpha'), (2, 'beta')], rows)

with tempfile.NamedTemporaryFile(suffix='.jsonl', delete=False) as f:
    out_path = f.name
cur.execute(f"COPY wire_copy TO '{out_path}' WITH (FORMAT JSONL)")
ok("[20.5 copy_to_jsonl] COPY TO JSONL exports rows",
   cur.rowcount == 2, cur.rowcount)

# Verify exported content
with open(out_path) as f:
    lines = [l.strip() for l in f if l.strip()]
ok("[20.5 copy_to_jsonl_content] COPY TO JSONL content correct",
   len(lines) == 2, lines)
```

### Docs changes

`docs-site/src/user-guide/sql-reference/dml.md`:
- Add `## COPY` section after INSERT/UPDATE/DELETE with full syntax reference,
  format table, option table, NULL handling, and a CSV + JSONL example

`docs-site/src/user-guide/features/data-import-export.md` (new page):
- Overview: when to use COPY vs INSERT
- CSV quick-start example
- JSONL example (append-log use case)
- NULL handling
- Performance notes (bulk import path via execute_insert_ctx)
- Limitations (server-side paths only; no STDIN/STDOUT; Parquet → 20.6)

Add link to `data-import-export.md` from the sidebar / SUMMARY.md.

### Verification against spec

- [x] `Stmt::CopyFrom` and `Stmt::CopyTo` in AST
- [x] `CopyFormat`, `CopyOptions`, `CopyFromStmt`, `CopyToStmt` types
- [x] Parser: all syntax variants
- [x] Format auto-detected from extension
- [x] COPY FROM CSV (header-mapped + positional)
- [x] COPY FROM JSON + JSONL
- [x] COPY TO CSV + JSON + JSONL
- [x] Type coercion for scalar types
- [x] NULL: `\N` in CSV, JSON null in JSON/JSONL
- [x] `QueryResult::Affected { count }` returned
- [x] Error messages with path / line number / column name
- [x] Round-trip tests (CSV + JSONL)
- [x] `cargo nextest run --workspace` passes
- [x] `cargo clippy --workspace -- -D warnings` clean
- [x] Wire test 538+ assertions
- [x] docs-site COPY section + data-import-export page

### Commit

```
feat(fase-20): complete COPY FROM/TO — wire test + docs (20.5 step 4)

Implements specs/fase-20/spec-20.5-copy-from-to.md
Wire: 4 new assertions (538 total).
docs-site: dml.md COPY section + data-import-export.md (new).
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `execute_insert_ctx` rejects large `InsertSource::Values` batches | low | test with 10K rows in integration_copy.rs |
| CSV with embedded newlines in fields: line-number error reporting off | medium | use `csv` crate's `position().line()` for accurate line numbers |
| `InsertStmt` columns field: `None` = all columns, `Some(v)` = named subset | low | always pass `Some(columns)` from COPY to avoid positional mismatch |
| `full_heap_scan` not exported from shared.rs | low | can also synthesize a `SelectStmt` and call `execute_select_ctx` |

## Rollback plan

1. `git reset --hard <commit before step 1>`, or
2. Branch `abandoned/plan-20.5-copy-from-to-<date>`, spec back to `draft`.

## Estimated effort

Total: ~3 hours
- Step 1: 30 min
- Step 2: 70 min
- Step 3: 50 min
- Step 4: 30 min
