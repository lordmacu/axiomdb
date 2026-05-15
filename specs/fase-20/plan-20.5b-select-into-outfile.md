# Plan: 20.5b SELECT INTO OUTFILE

Phase: 20 — Types + import/export
Task: MySQL-compatible `SELECT … INTO OUTFILE`
Spec: specs/fase-20/spec-20.5b-select-into-outfile.md
Status: in-progress

## Summary

Five steps: (1) AST — add `IntoOutfile` struct + field on `SelectStmt`; (2) Parser — parse
`INTO OUTFILE 'path' [FIELDS ...] [LINES ...]` after limit/offset; (3) Executor — intercept
`into_outfile` at the two top-level dispatch points (`exec_entry.rs`, `exec_dispatch.rs`),
write file via new `select_into_outfile.rs`, return `Affected`; (4) Integration tests;
(5) Wire assertions + docs + close.

The parser guarantees `into_outfile` is always `None` for subquery `SelectStmt`s, so all
internal callers of `execute_select_ctx` (joins, CTEs, subqueries) are unaffected.

## Dependencies

Must be done first:
- [x] Phase 20.5 (COPY TO) — complete; `copy_to.rs` helpers available

Blocks:
- [ ] LOAD DATA INFILE (future subphase)

## Affected files

New files:
- `crates/axiomdb-sql/src/executor/select_into_outfile.rs` — write_into_outfile, handle_into_outfile
- `crates/axiomdb-sql/tests/integration_select_into_outfile.rs` — 12+ tests

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add `IntoOutfile` struct + `SelectStmt.into_outfile`
- `crates/axiomdb-sql/src/parser/dml.rs` — parse INTO OUTFILE clause
- `crates/axiomdb-sql/src/executor/exec_entry.rs` — extract + dispatch into_outfile
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — same
- `crates/axiomdb-sql/src/executor/mod.rs` — pub use select_into_outfile (if needed)
- `tools/wire-test.py` — 3 new assertions
- `docs-site/src/user-guide/sql-reference/dml.md` — SELECT INTO OUTFILE section
- `docs-site/src/internals/sql-parser.md` — parser internals section

---

## Step 1 — AST: IntoOutfile struct + SelectStmt field

**Goal:** Add `IntoOutfile` to the AST; all existing code compiles with `into_outfile: None`.

**Files:** `crates/axiomdb-sql/src/ast.rs`

### Changes

Add after the `CopyOptions` struct (around line 770):

```rust
/// Options for `SELECT … INTO OUTFILE 'path' [FIELDS ...] [LINES ...]`.
/// Phase 20.5b.
#[derive(Debug, Clone, PartialEq)]
pub struct IntoOutfile {
    pub path: String,
    /// Field separator. Default: `\t` (TAB — MySQL default).
    pub field_sep: char,
    /// Enclosure character; `None` = no quoting. Both ENCLOSED BY and
    /// OPTIONALLY ENCLOSED BY set this field.
    pub enclosure: Option<char>,
    /// Line terminator. Default: `"\n"`.
    pub line_term: String,
}
```

Add to `SelectStmt` (after `set_op_rest`):

```rust
/// Phase 20.5b — `INTO OUTFILE 'path' [FIELDS ...] [LINES ...]`.
/// `None` for ordinary SELECT statements.
pub into_outfile: Option<IntoOutfile>,
```

Fix all `SelectStmt { ... }` construction sites to add `into_outfile: None` (there are
several in `ast.rs` tests and one in `parser/dml.rs`). Also update any
`..Default::default()` spreads if SelectStmt derives Default (it doesn't currently).

### Test to add

```rust
// crates/axiomdb-sql/src/ast.rs (in existing test mod)
#[test]
fn select_stmt_has_into_outfile_field() {
    let s = SelectStmt {
        with_ctes: vec![],
        distinct: false,
        distinct_on: vec![],
        hints: vec![],
        calc_found_rows: false,
        columns: vec![],
        from: None,
        joins: vec![],
        where_clause: None,
        group_by: crate::ast::GroupByClause::None,
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        lock_clause: None,
        set_op_rest: vec![],
        into_outfile: None,
    };
    assert!(s.into_outfile.is_none());
}
```

### Verification

```bash
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): ast — add IntoOutfile struct + SelectStmt.into_outfile field (20.5b step 1)
```

---

## Step 2 — Parser: parse INTO OUTFILE clause

**Goal:** `SELECT ... INTO OUTFILE 'path' [FIELDS TERMINATED BY 'x' [OPTIONALLY ENCLOSED BY 'y']] [LINES TERMINATED BY 'z']` parses into `IntoOutfile`.

**Files:** `crates/axiomdb-sql/src/parser/dml.rs`

### Changes

Add a `parse_into_outfile(p: &mut Parser) -> Result<Option<IntoOutfile>, DbError>` function.

Call it inside `parse_select_body` (the fn that builds `SelectStmt`) after `parse_lock_clause` and before the final `Ok(SelectStmt { ... })`:

```rust
let into_outfile = parse_into_outfile(p)?;
```

Add `into_outfile` to the `SelectStmt { ... }` construction.

**Grammar implemented:**

```
INTO OUTFILE StringLit
  [ FIELDS TERMINATED BY StringLit
    [ OPTIONALLY ENCLOSED BY StringLit
    | ENCLOSED BY StringLit ] ]
  [ LINES TERMINATED BY StringLit ]
```

`FIELDS` keyword is optional before `TERMINATED BY`.
`LINES` is optional.
Options after `OUTFILE 'path'` are parsed in a loop consuming tokens while they match
`FIELDS`, `TERMINATED`, `LINES`, `ENCLOSED`, `OPTIONALLY`.

**Single-char validation:** if the parsed string for `field_sep` or `enclosure` is not
exactly one character, return:
```rust
DbError::InvalidValue { reason: "FIELDS TERMINATED BY requires a single character".into() }
DbError::InvalidValue { reason: "ENCLOSED BY requires a single character".into() }
```

**Subquery detection:** The parser tracks `p.subquery_depth: usize`. Increment it when
entering a parenthesized SELECT (subquery); decrement on exit. If `parse_into_outfile` is
called and `p.subquery_depth > 0`, return:
```rust
Err(DbError::NotSupported {
    feature: "INTO OUTFILE inside a subquery".into(),
})
```

*(Check whether subquery_depth already exists in the parser. If not, add an `into_outfile`
check in `execute_select_ctx` as the runtime guard instead — simpler.)*

### Test to add

```rust
// crates/axiomdb-sql/tests/parser_select_into_outfile.rs  (or in lib test mod)
#[test]
fn parse_into_outfile_minimal() {
    let stmt = parse_sql("SELECT id FROM t INTO OUTFILE '/tmp/out.tsv'").unwrap();
    let sel = sel_stmt(stmt);
    let outfile = sel.into_outfile.unwrap();
    assert_eq!(outfile.path, "/tmp/out.tsv");
    assert_eq!(outfile.field_sep, '\t');    // MySQL default
    assert_eq!(outfile.enclosure, None);
    assert_eq!(outfile.line_term, "\n");
}

#[test]
fn parse_into_outfile_full_options() {
    let stmt = parse_sql(
        "SELECT id FROM t INTO OUTFILE '/tmp/out.csv' \
         FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"' \
         LINES TERMINATED BY '\n'"
    ).unwrap();
    let sel = sel_stmt(stmt);
    let outfile = sel.into_outfile.unwrap();
    assert_eq!(outfile.field_sep, ',');
    assert_eq!(outfile.enclosure, Some('"'));
}

#[test]
fn parse_into_outfile_multi_char_sep_error() {
    let r = parse_sql("SELECT 1 INTO OUTFILE '/f' FIELDS TERMINATED BY 'ab'");
    assert!(r.is_err());
}

#[test]
fn parse_into_outfile_no_into_outfile() {
    let stmt = parse_sql("SELECT id FROM t").unwrap();
    let sel = sel_stmt(stmt);
    assert!(sel.into_outfile.is_none());
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test parser_select_into_outfile
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): parser — parse INTO OUTFILE clause on SelectStmt (20.5b step 2)
```

---

## Step 3 — Executor: write file on INTO OUTFILE

**Goal:** When `into_outfile.is_some()`, execute the SELECT normally then write rows to file.
Return `QueryResult::Affected` instead of `QueryResult::Rows`.

**Files:**
- `crates/axiomdb-sql/src/executor/select_into_outfile.rs` (NEW)
- `crates/axiomdb-sql/src/executor/exec_entry.rs` (MODIFIED)
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` (MODIFIED)

### New file: select_into_outfile.rs

```rust
use std::io::Write as _;
use crate::ast::IntoOutfile;
use crate::executor::QueryResult;
use axiomdb_core::{DbError, Value};

/// Serialize `rows` to a file according to MySQL INTO OUTFILE formatting rules.
pub(crate) fn write_into_outfile(
    outfile: &IntoOutfile,
    rows: &[Vec<Value>],
) -> Result<u64, DbError> {
    let mut f = std::fs::File::create(&outfile.path)
        .map_err(|e| DbError::Io(std::io::Error::new(e.kind(), format!("INTO OUTFILE: {e}"))))?;

    for row in rows {
        let mut first = true;
        for val in row {
            if !first {
                write!(f, "{}", outfile.field_sep).map_err(io_err)?;
            }
            first = false;
            let s = outfile_field_str(val);
            if let Some(enc) = outfile.enclosure {
                let escaped = s.replace(enc, &format!("{enc}{enc}"));
                write!(f, "{enc}{escaped}{enc}").map_err(io_err)?;
            } else {
                write!(f, "{s}").map_err(io_err)?;
            }
        }
        write!(f, "{}", outfile.line_term).map_err(io_err)?;
    }
    f.flush().map_err(io_err)?;
    Ok(rows.len() as u64)
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::Io(std::io::Error::new(e.kind(), format!("INTO OUTFILE write: {e}")))
}

fn outfile_field_str(v: &Value) -> String {
    match v {
        Value::Null          => r"\N".to_string(),
        Value::Bool(b)       => if *b { "1" } else { "0" }.to_string(),
        Value::Int(n)        => n.to_string(),
        Value::BigInt(n)     => n.to_string(),
        Value::Real(f)       => format!("{f}"),
        Value::Decimal(m, s) => {
            if *s == 0 {
                m.to_string()
            } else {
                let scale = *s as u32;
                let div = 10i128.pow(scale);
                let int_part = m / div;
                let frac_part = (m % div).unsigned_abs();
                format!("{int_part}.{frac_part:0>width$}", width = scale as usize)
            }
        }
        Value::Text(s)       => s.clone(),
        Value::Json(s)       => s.clone(),
        Value::Jsonb(b)      => String::from_utf8_lossy(b).into_owned(),
        Value::Bytes(b)      => b.iter().map(|byte| format!("{byte:02x}")).collect(),
        Value::Timestamp(ts) => {
            let secs = ts / 1_000_000;
            let us   = (ts % 1_000_000).unsigned_abs();
            format!("{secs}.{us:06}")
        }
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            if let Some(d) = epoch.checked_add_days(chrono::Days::new((*days).max(0) as u64)) {
                d.to_string()
            } else {
                days.to_string()
            }
        }
        Value::Uuid(bytes) => {
            let u = u128::from_be_bytes(*bytes);
            format!(
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                (u >> 96) as u32, (u >> 80) as u16,
                (u >> 64) as u16, (u >> 48) as u16,
                u & 0xffff_ffff_ffff
            )
        }
        Value::Array(elems) => {
            let arr: Vec<serde_json::Value> =
                elems.iter().map(val_to_json).collect();
            serde_json::to_string(&arr).unwrap_or_default()
        }
    }
}

fn val_to_json(v: &Value) -> serde_json::Value {
    // minimal — reuse logic from copy_to.rs if accessible, else inline
    match v {
        Value::Null      => serde_json::Value::Null,
        Value::Bool(b)   => serde_json::Value::Bool(*b),
        Value::Int(n)    => serde_json::Value::Number((*n).into()),
        Value::BigInt(n) => serde_json::Value::Number((*n).into()),
        Value::Text(s)   => serde_json::Value::String(s.clone()),
        other            => serde_json::Value::String(outfile_field_str(other)),
    }
}

/// Post-process a SELECT result: if `into_outfile` is set, write the file.
/// Returns Affected(N) on success; forwards the original QueryResult otherwise.
pub(crate) fn handle_into_outfile(
    result: Result<QueryResult, DbError>,
    into_outfile: Option<IntoOutfile>,
) -> Result<QueryResult, DbError> {
    let Some(outfile) = into_outfile else {
        return result;
    };
    match result? {
        QueryResult::Rows { rows, .. } => {
            let count = write_into_outfile(&outfile, &rows)?;
            Ok(QueryResult::Affected { count, last_insert_id: None })
        }
        other => Ok(other), // defensive; shouldn't occur for SELECT
    }
}
```

### Changes to exec_entry.rs

In the `Stmt::Select(s)` arm:
```rust
Stmt::Select(mut s) => {
    let into_outfile = s.into_outfile.take();
    let conn = ctx.conn_txn.take();
    let r = execute_select_ctx(s, &exec_ctx, conn.as_ref(), ctx);
    ctx.conn_txn = conn;
    handle_into_outfile(r, into_outfile)
}
```

### Changes to exec_dispatch.rs

Same pattern:
```rust
Stmt::Select(mut s) => {
    let into_outfile = s.into_outfile.take();
    let conn = ctx.conn_txn.take().expect("conn_txn set");
    let r = execute_select_ctx(s, exec_ctx, Some(&conn), ctx);
    ctx.conn_txn = Some(conn);
    handle_into_outfile(r, into_outfile)
}
```

Add `mod select_into_outfile;` to `executor/mod.rs` and the necessary `use` imports.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_unnest_select  # regression
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-20): executor — intercept INTO OUTFILE, write file, return Affected (20.5b step 3)
```

---

## Step 4 — Integration tests

**Goal:** 12+ tests covering the full spec behavior.

**Files:** `crates/axiomdb-sql/tests/integration_select_into_outfile.rs`

### Test cases

```rust
// helpers
fn run(sql: &str) -> QueryResult { /* execute via embedded */ }
fn file_contents(path: &str) -> String { std::fs::read_to_string(path).unwrap() }

// 1. basic CSV write
fn test_basic_csv_write() — SELECT id, name INTO OUTFILE '/tmp/axm_1.csv' FIELDS TERMINATED BY ','
// 2. TAB default
fn test_tab_default() — SELECT id, name INTO OUTFILE '/tmp/axm_2.tsv'  (no FIELDS option)
// 3. ENCLOSED BY quotes
fn test_enclosed_by() — check fields wrapped in quotes, inner quote doubled
// 4. OPTIONALLY ENCLOSED BY (same as ENCLOSED BY)
fn test_optionally_enclosed_by()
// 5. LINES TERMINATED BY '\r\n'
fn test_crlf_lines()
// 6. NULL value → \N
fn test_null_value()
// 7. empty result set → empty file, 0 rows affected
fn test_empty_result_empty_file()
// 8. multiple rows, multiple columns
fn test_multiple_rows()
// 9. WHERE filter applies before write
fn test_where_filter()
// 10. ORDER BY applies before write
fn test_order_by()
// 11. LIMIT applies before write
fn test_limit()
// 12. overwrite existing file
fn test_overwrite_existing()
// 13. bool value → 1/0
fn test_bool_serialization()
// 14. Affected result returns row count
fn test_returns_affected_count()
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_select_into_outfile
```

### Commit

```
feat(fase-20): integration tests for SELECT INTO OUTFILE — 12+ cases (20.5b step 4)
```

---

## Step 5 — Wire assertions + docs + close

**Goal:** Wire smoke test passes; docs updated; subphase closed.

### Wire assertions (tools/wire-test.py)

```python
# [20.5b into_outfile_basic] — write CSV, verify file contents
cur.execute("CREATE TABLE IF NOT EXISTS wire_outfile_t (id INT, name VARCHAR(20))")
cur.execute("INSERT INTO wire_outfile_t VALUES (1, 'alice'), (2, 'bob') ON DUPLICATE KEY UPDATE name = VALUES(name)")
cur.execute("SELECT id, name FROM wire_outfile_t ORDER BY id INTO OUTFILE '/tmp/axm_wire_outfile.csv' FIELDS TERMINATED BY ','")
import os; lines = open('/tmp/axm_wire_outfile.csv').read().strip().split('\n')
ok("[20.5b into_outfile_basic]...", len(lines) == 2 and lines[0] == '1,alice' and lines[1] == '2,bob', lines)

# [20.5b into_outfile_quoted] — ENCLOSED BY
cur.execute("SELECT name FROM wire_outfile_t ORDER BY id INTO OUTFILE '/tmp/axm_wire_outfile_q.csv' FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"'")
qlines = open('/tmp/axm_wire_outfile_q.csv').read().strip().split('\n')
ok("[20.5b into_outfile_quoted]...", qlines[0] == '"alice"', qlines)

# [20.5b into_outfile_null] — NULL → \N
cur.execute("SELECT NULL INTO OUTFILE '/tmp/axm_wire_null.csv'")
null_content = open('/tmp/axm_wire_null.csv').read().strip()
ok("[20.5b into_outfile_null]...", null_content == r'\N', repr(null_content))
```

### Docs

- `docs-site/src/user-guide/sql-reference/dml.md` — "SELECT INTO OUTFILE" section
- `docs-site/src/internals/sql-parser.md` — "Phase 20.5b — INTO OUTFILE parser pass"

### Verification against spec done criteria

- [ ] Basic CSV write with custom separator
- [ ] TAB default (no FIELDS option)
- [ ] ENCLOSED BY quotes all fields
- [ ] NULL → `\N`
- [ ] Empty result → empty file, 0 rows affected
- [ ] INTO OUTFILE inside subquery → error (parser or runtime)
- [ ] 12+ integration tests pass
- [ ] cargo nextest run --workspace clean
- [ ] cargo clippy --workspace -- -D warnings clean
- [ ] cargo fmt --check clean
- [ ] Wire: 549/549 assertions
- [ ] docs updated

### Commit

```
feat(fase-20): complete SELECT INTO OUTFILE (20.5b)

Implements specs/fase-20/spec-20.5b-select-into-outfile.md
- AST: IntoOutfile struct + SelectStmt.into_outfile field
- Parser: INTO OUTFILE 'path' [FIELDS ...] [LINES ...] after limit/lock clause
- Executor: intercept at exec_entry/exec_dispatch, write_into_outfile, return Affected
- 12+ integration tests, 3 wire assertions (549/549)
- Docs: dml.md + sql-parser.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `SelectStmt` construction sites miss `into_outfile: None` | Medium | Compiler will catch (struct fields are exhaustive) |
| `write_into_outfile` race with existing file | Low | `File::create` truncates — same as COPY TO |
| Parser doesn't track subquery depth | Low | Add runtime guard in handle_into_outfile: if result is from subquery context, `into_outfile` is always None (parser produces it only at top level) |
| Wire test file path not writable in VM | Low | Use `/tmp/` — always writable |

## Rollback plan

If abandoned mid-way:
1. `into_outfile` field is Option<> with default None — adding it doesn't break anything
2. Revert select_into_outfile.rs and exec_entry/exec_dispatch changes
3. Mark spec back to `draft`

## Estimated effort

Total: 3–4 hours
- Step 1 (AST): 20 min
- Step 2 (Parser): 45 min
- Step 3 (Executor): 45 min
- Step 4 (Tests): 60 min
- Step 5 (Wire + docs + close): 30 min
