# Spec: 20.5 — COPY FROM / TO

Phase: 20 — Types + import/export
Task: Bulk data import and export via COPY
Status: approved

## Context

Phase 20 has delivered views, sequences, ENUMs, and arrays. Subphase 20.5
adds `COPY FROM` and `COPY TO` — the standard SQL bulk-load and bulk-export
mechanism. This is a pure-SQL, server-side operation (no wire-level STDIN/STDOUT),
supporting three formats: CSV, JSON (array-of-objects), and JSONL (one object
per line). Parquet is deferred to 20.6.

## Goal

Allow users to import data from a server-side file (`COPY t FROM '/path'`) and
export data to a server-side file (`COPY t TO '/path'`), supporting CSV, JSON,
and JSONL formats.

## Non-goals

- Wire-level `COPY ... FROM STDIN` / `COPY ... TO STDOUT` — deferred; requires MySQL
  wire protocol changes
- Parquet import/export — Phase 20.6 (`READ_PARQUET()`)
- `COPY (SELECT ...) TO` (subquery source) — deferred; only table copies in 20.5
- Partial exports with WHERE predicates — deferred
- Remote URLs (`http://`, `s3://`) — deferred to Phase 22
- Binary format — not planned
- Progress reporting / cancellation — not planned

## Behavior

### SQL syntax

```sql
COPY table_name FROM 'path'
    [WITH (
        FORMAT { CSV | JSON | JSONL },
        HEADER { TRUE | FALSE },           -- default TRUE for CSV, ignored for JSON/JSONL
        DELIMITER 'char',                  -- default ',' (CSV only)
        NULL 'string'                      -- default '\N' (CSV only)
    )]

COPY table_name TO 'path'
    [WITH (
        FORMAT { CSV | JSON | JSONL },
        HEADER { TRUE | FALSE },           -- default TRUE for CSV, ignored for JSON/JSONL
        DELIMITER 'char'                   -- default ',' (CSV only)
    )]
```

Format auto-detection from file extension when `FORMAT` is omitted:
- `.csv`  → `Csv`
- `.json` → `Json`
- `.jsonl` or `.ndjson` → `Jsonl`
- Any other extension or no extension → `Csv` (default)

### Public AST additions

```rust
/// COPY table FROM 'path' [WITH (...)]
pub struct CopyFromStmt {
    pub table: String,
    pub path: String,
    pub options: CopyOptions,
}

/// COPY table TO 'path' [WITH (...)]
pub struct CopyToStmt {
    pub table: String,
    pub path: String,
    pub options: CopyOptions,
}

pub struct CopyOptions {
    pub format: Option<CopyFormat>,   // None = auto-detect from extension
    pub header: Option<bool>,         // None = default (true for CSV, ignored for JSON/JSONL)
    pub delimiter: Option<char>,      // None = ',' (CSV only)
    pub null_str: Option<String>,     // None = "\\N" (CSV only)
}

pub enum CopyFormat {
    Csv,
    Json,
    Jsonl,
}
```

New `Stmt` variants:
```rust
Stmt::CopyFrom(CopyFromStmt),
Stmt::CopyTo(CopyToStmt),
```

### Return value

Both `COPY FROM` and `COPY TO` return:
```
QueryResult::Affected { count: u64 }
```
where `count` is the number of rows imported or exported.

### COPY FROM semantics

1. Resolve table by name; error if not found.
2. Open the file at `path`; error if not readable.
3. Detect format (from option or extension).
4. Parse rows according to format (see below).
5. For each parsed row: coerce each field string to the column's declared type.
6. Insert rows using the existing batch-insert path (same as `INSERT INTO ... VALUES`).
7. Return `QueryResult::Affected { count }`.

All rows in a single `COPY FROM` run in the caller's transaction context
(explicit or autocommit). On any error the transaction is aborted and no rows
are committed (standard SQL error semantics).

### COPY TO semantics

1. Resolve table by name; error if not found.
2. Open (create or overwrite) the file at `path`; error if not writable.
3. Full table scan (all rows, no filtering).
4. Serialize each row according to format (see below).
5. Flush and close the file.
6. Return `QueryResult::Affected { count }`.

### Format details

#### CSV (read and write)

- Uses the `csv` crate (to be added to workspace dependencies).
- Field delimiter: `delimiter` option, default `,`.
- Quote character: `"` (double-quote, not configurable in 20.5).
- NULL representation: `null_str` option, default `\N` (PostgreSQL convention).
  A field matching `null_str` exactly (unquoted) maps to SQL NULL on read;
  NULL values are written as `null_str` on write.
- `HEADER TRUE` (default): first row is column names on read (order may differ
  from table column order); column header is written on export.
- `HEADER FALSE`: rows are positional; must match table column count exactly.
- Column order on read with `HEADER TRUE`: matched by name (case-insensitive).
- Column order on read with `HEADER FALSE`: positional left-to-right.
- Quoted fields containing the delimiter, `"`, or newlines are handled by the
  `csv` crate per RFC 4180.
- `\r\n` and `\n` line endings both accepted on read.

#### JSON (read and write)

- The file must contain a single JSON array at the top level: `[{...}, {...}, ...]`.
- Each element must be a JSON object; keys are column names (case-insensitive).
- Columns not present in an object are set to NULL.
- On write: outputs a JSON array with one object per row; keys are column names.
- `HEADER` option is ignored.

#### JSONL / NDJSON (read and write)

- One JSON object per line; blank lines are skipped.
- Keys are column names (case-insensitive); missing keys → NULL.
- On write: one JSON object per line, no trailing comma, terminated by `\n`.
- `HEADER` option is ignored.

### Type coercion (COPY FROM)

String values from CSV/JSON/JSONL are coerced to the column's declared type:

| Target type | Accepted input |
|---|---|
| INT / BIGINT / SMALLINT | Decimal integer string, e.g. `"42"` |
| FLOAT / DOUBLE | Decimal float string, e.g. `"3.14"` or `"1e10"` |
| BOOLEAN | `"true"`, `"false"`, `"1"`, `"0"`, `"t"`, `"f"`, `"yes"`, `"no"` (case-insensitive) |
| DATE | `"YYYY-MM-DD"` |
| DATETIME / TIMESTAMP | `"YYYY-MM-DD HH:MM:SS"` |
| TEXT / VARCHAR | Any string |
| ENUM | Enum variant label string |
| ARRAY | JSON array literal, e.g. `"[1,2,3]"` |
| NULL (any type) | CSV: field matches `null_str`; JSON/JSONL: JSON `null` |

### Error cases

| Situation | Error | Message |
|---|---|---|
| Table not found | `DbError::TableNotFound` | `"table 'name' not found"` |
| File not found (FROM) | `DbError::Io` | `"COPY FROM: cannot open 'path': No such file or directory"` |
| File not writable (TO) | `DbError::Io` | `"COPY TO: cannot create 'path': ..."` |
| Column count mismatch (CSV HEADER FALSE) | `DbError::InvalidInput` | `"COPY FROM: line N: expected M columns, got K"` |
| Unknown column name (HEADER TRUE) | `DbError::InvalidInput` | `"COPY FROM: unknown column 'name'"` |
| Type coercion failure | `DbError::InvalidInput` | `"COPY FROM: line N, column 'col': cannot cast '...' to TYPE"` |
| Malformed CSV | `DbError::InvalidInput` | `"COPY FROM: line N: CSV parse error: ..."` |
| Malformed JSON | `DbError::InvalidInput` | `"COPY FROM: JSON parse error: ..."` |
| Malformed JSONL | `DbError::InvalidInput` | `"COPY FROM: line N: JSON parse error: ..."` |
| Unknown FORMAT option | `DbError::ParseError` | `"unknown COPY FORMAT 'x'; expected CSV, JSON, or JSONL"` |
| Invalid DELIMITER (not single char) | `DbError::ParseError` | `"COPY DELIMITER must be a single character"` |

## Edge cases

- [ ] Empty file → 0 rows imported, no error
- [ ] CSV with HEADER only, no data rows → 0 rows, no error
- [ ] COPY TO on empty table → empty file (with header if CSV), 0 rows
- [ ] JSONL with blank lines → blank lines skipped silently
- [ ] CSV field containing delimiter, quotes, newlines → handled by `csv` crate
- [ ] NULL: `\N` in CSV (unquoted) → NULL; `""` (empty quoted) → empty string
- [ ] COPY TO: file already exists → overwrite silently
- [ ] COPY TO: destination directory does not exist → `DbError::Io`
- [ ] COPY FROM with HEADER TRUE, column order differs from table → matched by name
- [ ] COPY FROM with HEADER TRUE, extra columns in file not in table → error (unknown column)
- [ ] COPY FROM with HEADER TRUE, missing columns in file → NULL for missing columns
- [ ] Unicode in string fields → pass through as-is
- [ ] Table with auto-increment PK: CSV has a column for PK → use provided value;
  CSV does NOT have PK column → engine assigns next auto-increment (same as INSERT)
- [ ] ARRAY column: value must be a valid JSON array string, e.g. `"[1,2,3]"`

## Performance budget

| Operation | Target |
|---|---|
| COPY FROM 100 K rows CSV (no index) | ≤ 2 s |
| COPY TO 100 K rows CSV | ≤ 1 s |
| COPY FROM 100 K rows JSONL | ≤ 3 s |

## Dependencies

- Depends on: 20.1 (views), 20.4 (arrays) — both implemented ✅
- Adds `csv = "1"` to workspace `[dependencies]`
- Blocks: nothing — independent subphase

## Open questions

None — all resolved in brainstorm.

## Done criteria

- [ ] `Stmt::CopyFrom` and `Stmt::CopyTo` in AST
- [ ] `CopyFormat`, `CopyOptions`, `CopyFromStmt`, `CopyToStmt` types in AST
- [ ] Parser: `COPY t FROM 'path'` and `COPY t TO 'path'` with full WITH clause
- [ ] Format auto-detected from extension when FORMAT omitted
- [ ] COPY FROM CSV (`csv` crate): header-mapped and positional modes
- [ ] COPY FROM JSON: top-level array of objects
- [ ] COPY FROM JSONL: one object per line, blank lines skipped
- [ ] COPY TO CSV: header + rows, NULL as `\N`
- [ ] COPY TO JSON: single JSON array
- [ ] COPY TO JSONL: one object per line
- [ ] Type coercion for all scalar types listed above
- [ ] NULL handling: `\N` in CSV, JSON `null` in JSON/JSONL
- [ ] `QueryResult::Affected { count }` returned for both directions
- [ ] Error messages include file path / line number / column name where applicable
- [ ] Round-trip test: COPY TO + COPY FROM gives identical row set
- [ ] `cargo nextest run -p axiomdb-sql` passes (including new integration tests)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire test: 4 new assertions (538+ total)
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` — COPY section added
- [ ] `docs-site/src/user-guide/features/data-import-export.md` — new page with examples

## References

- Phase 20 in `db.md`: "COPY FROM/TO: CSV, JSON, JSONL"
- PostgreSQL: `src/backend/commands/copy.c`, `copyfrom.c`, `copyto.c`
- `csv` crate: https://docs.rs/csv (RFC 4180 compliant)
- Implemented predecessors: `specs/fase-20/spec-20.4-arrays.md`
