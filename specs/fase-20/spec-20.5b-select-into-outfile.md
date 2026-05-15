# Spec: 20.5b SELECT INTO OUTFILE

Phase: 20 — Types + import/export
Task: MySQL-compatible `SELECT … INTO OUTFILE` for exporting query results
Status: approved

## Context

Phase 20.5 added `COPY TO 'path' FORMAT CSV` (PostgreSQL syntax). Subphase 20.5b extends
export to MySQL's `SELECT … INTO OUTFILE 'path'` syntax, which differs in two ways:
the file path is specified *inside* the SELECT statement, and a MySQL-specific
`FIELDS TERMINATED BY / ENCLOSED BY / LINES TERMINATED BY` clause controls formatting.
The existing `copy_to.rs` serialization helpers are reused for value formatting.

## Goal

Allow `SELECT col, ... FROM table [WHERE ...] [ORDER BY ...] INTO OUTFILE 'path'
[FIELDS TERMINATED BY 'x' [OPTIONALLY ENCLOSED BY 'y']] [LINES TERMINATED BY 'z']`
to write the query result to a file in CSV-like format.

## Non-goals

- `FIELDS ESCAPED BY` — deferred; MySQL default backslash escaping is not implemented
- `LOAD DATA INFILE` (the inverse operation) — separate subphase
- SELECT INTO OUTFILE inside a subquery or CTE body — SQL error
- Network / streaming output — only local filesystem paths
- Header row — MySQL INTO OUTFILE never writes headers (consistent with MySQL)
- `DUMPFILE` variant — single-row, no formatting; deferred

## Behavior

### SQL surface syntax

```sql
-- Minimal (MySQL defaults: TAB separator, no enclosure, \n line terminator)
SELECT id, name FROM users INTO OUTFILE '/tmp/users.tsv';

-- Custom field separator
SELECT id, name FROM users INTO OUTFILE '/tmp/users.csv'
FIELDS TERMINATED BY ',';

-- With quoting
SELECT id, name FROM users INTO OUTFILE '/tmp/users.csv'
FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '"';

-- Full options
SELECT id, name FROM users INTO OUTFILE '/tmp/out.csv'
FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '"'
LINES TERMINATED BY '\n';

-- With WHERE and ORDER BY
SELECT id, score FROM results WHERE score > 90 ORDER BY score DESC
INTO OUTFILE '/tmp/top.csv' FIELDS TERMINATED BY ',';
```

### MySQL defaults (when options are absent)

| Option | Default |
|--------|---------|
| FIELDS TERMINATED BY | `\t` (TAB) |
| ENCLOSED BY / OPTIONALLY ENCLOSED BY | none (no quoting) |
| LINES TERMINATED BY | `\n` |

### AST change

```rust
/// Options for `SELECT … INTO OUTFILE`.
#[derive(Debug, Clone, PartialEq)]
pub struct IntoOutfile {
    /// Filesystem path for the output file.
    pub path: String,
    /// Field separator character. Default: `\t`.
    pub field_sep: char,
    /// Enclosure character (None = no quoting). Both ENCLOSED BY and
    /// OPTIONALLY ENCLOSED BY set this field; behaviour is always-enclose.
    pub enclosure: Option<char>,
    /// Line terminator string. Default: `\n`.
    pub line_term: String,
}

pub struct SelectStmt {
    // ... existing fields ...
    /// Phase 20.5b — `INTO OUTFILE 'path' [FIELDS ...] [LINES ...]`.
    /// `None` for ordinary SELECT statements (the common case).
    pub into_outfile: Option<IntoOutfile>,
}
```

### Parser grammar

`INTO OUTFILE` is parsed after the optional `LIMIT`/`OFFSET`/`LOCK` clause, before
the final `;`. Token sequence:

```
INTO OUTFILE StringLit
  [ FIELDS TERMINATED BY StringLit
    [ OPTIONALLY ENCLOSED BY StringLit | ENCLOSED BY StringLit ] ]
  [ LINES TERMINATED BY StringLit ]
```

`FIELDS` keyword is optional — `TERMINATED BY` may appear without `FIELDS`.
`LINES` is optional. Options may appear in any order between FIELDS and LINES groups.

Parsing rules:
- `StringLit` for single-char options (field_sep, enclosure) must be exactly one
  character after unescaping; otherwise `DbError::InvalidValue`.
- `LINES TERMINATED BY` accepts `'\n'`, `'\r\n'`, `'\r'` and their literal forms.
- INTO OUTFILE inside a subquery (parser detects nesting depth > 0): return
  `DbError::NotSupported { feature: "INTO OUTFILE inside a subquery".into() }`.

### Execution

The executor runs the SELECT query normally to obtain `(column_names, rows)`.
If `into_outfile.is_some()`, instead of returning rows to the client:

1. Open the file at `path` for writing (create or truncate, 0o644 permissions).
   On error → `DbError::Io(...)`.
2. Write all rows using `write_into_outfile(w, &col_names, &rows, &into_outfile)`:
   - For each row, for each field:
     - Convert value to string using `outfile_field_str(value)` (see below).
     - If `enclosure.is_some()`: wrap the string in the enclosure char; any
       occurrence of the enclosure char in the value is doubled (e.g., `"` → `""`).
     - Write the field string.
     - If not the last field: write `field_sep`.
   - After all fields: write `line_term`.
3. Return `QueryResult::Affected { count: rows.len() as u64, last_insert_id: None }`.
   The client sees "Query OK, N rows affected" (MySQL compatible).

### Value serialization (`outfile_field_str`)

| Value type | Serialization |
|------------|---------------|
| `Null` | `\N` (two chars: backslash + N) |
| `Bool` | `1` / `0` (MySQL convention) |
| `Int`, `BigInt` | decimal string |
| `Real` | `Display` format |
| `Decimal(m, s)` | decimal with s fractional digits |
| `Text` | raw string value |
| `Json`, `Jsonb` | JSON text |
| `Bytes` | hex string |
| `Timestamp` | `seconds.microseconds` |
| `Date` | `YYYY-MM-DD` |
| `Uuid` | `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` |
| `Array` | JSON array text |

### Error cases

| Input | Expected error |
|-------|----------------|
| INTO OUTFILE inside subquery | `DbError::NotSupported { feature: "INTO OUTFILE inside a subquery" }` |
| File path not writable | `DbError::Io(...)` |
| `FIELDS TERMINATED BY 'ab'` (>1 char) | `DbError::InvalidValue { reason: "FIELDS TERMINATED BY requires a single character" }` |
| `ENCLOSED BY 'ab'` (>1 char) | `DbError::InvalidValue { reason: "ENCLOSED BY requires a single character" }` |

## Edge cases

- [x] Empty result set → file is created empty (0 bytes), returns "0 rows affected"
- [x] NULL values → written as `\N` (two characters)
- [x] Enclosure char appears in a Text value → doubled inside the enclosure
- [x] Path already exists → file is truncated and overwritten (MySQL behavior)
- [x] Trailing slash path (directory) → `DbError::Io` from OS
- [x] LINES TERMINATED BY '\r\n' → Windows line endings
- [x] INTO OUTFILE with ORDER BY → ORDER BY applies before write (normal execution order)
- [x] INTO OUTFILE with LIMIT → LIMIT applies before write
- [x] Column alias in SELECT list → alias becomes field header? No — INTO OUTFILE
     never writes headers (field names unused)

## Performance budget

No throughput target. I/O is bounded by disk speed. The serialization loop is
O(rows × cols). Same cost as `COPY TO CSV`.

## Dependencies

- Depends on: Phase 20.5 (COPY FROM/TO) — complete; `copy_to.rs` value helpers available
- Depends on: SelectStmt (stable since Phase 4)
- Blocks: nothing

## Open questions

All resolved:
- Header row: no (MySQL INTO OUTFILE never writes headers)
- `OPTIONALLY ENCLOSED BY` vs `ENCLOSED BY`: same implementation (always-enclose)
- Enclosure char escaping: doubling (not backslash) — standard CSV convention
- `FIELDS ESCAPED BY`: deferred to a later subphase

## Done criteria

- [ ] `SELECT id, name FROM users INTO OUTFILE '/tmp/out.csv' FIELDS TERMINATED BY ','` writes correct CSV
- [ ] No ENCLOSED BY → no quoting (TAB/newline defaults)
- [ ] `OPTIONALLY ENCLOSED BY '"'` → all fields quoted, inner `"` doubled
- [ ] NULL → `\N` in output file
- [ ] Empty result → empty file, 0 rows affected
- [ ] INTO OUTFILE inside subquery → `DbError::NotSupported`
- [ ] Parser test: INTO OUTFILE parses `IntoOutfile` struct with correct defaults
- [ ] 12+ integration tests in `tests/integration_select_into_outfile.rs` pass
- [ ] `cargo nextest run -p axiomdb-sql` passes (Lima VM)
- [ ] `cargo nextest run --workspace` passes (Lima VM)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke: 3 new assertions pass (546+3 = 549/549)
- [ ] `docs-site/src/user-guide/sql-reference/dml.md` updated with SELECT INTO OUTFILE section
- [ ] `docs-site/src/internals/sql-parser.md` updated

## References

- MySQL docs: "SELECT … INTO Statement" — https://dev.mysql.com/doc/refman/8.0/en/select-into.html
- MySQL docs: "FIELDS and LINES handling" — https://dev.mysql.com/doc/refman/8.0/en/load-data.html#load-data-field-line-handling
- Existing COPY TO executor: `crates/axiomdb-sql/src/executor/copy_to.rs`
- Phase 20.5 spec: `specs/fase-20/spec-20.5-copy-from-to.md`
- SelectStmt AST: `crates/axiomdb-sql/src/ast.rs:723`
