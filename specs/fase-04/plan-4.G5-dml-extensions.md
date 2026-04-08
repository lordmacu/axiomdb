# Plan: 4.G5 — DML Extensions (MySQL Compatibility)

## Files to create/modify

### AST
- `crates/axiomdb-sql/src/ast.rs` — add fields to InsertStmt, UpdateStmt, DeleteStmt;
  add SelectStmt.lock_mode; add Stmt::Call, Stmt::Do, Stmt::CreateTableLike,
  Stmt::CreateTableAsSelect; add LockMode enum

### Lexer
- `crates/axiomdb-sql/src/lexer.rs` — add Token::Call, Token::Do, Token::For,
  Token::Share, Token::Mode, Token::Ignore, Token::Like (if not present)

### Parser
- `crates/axiomdb-sql/src/parser/mod.rs` — dispatch CALL and DO at top level
- `crates/axiomdb-sql/src/parser/dml.rs` — parse INSERT IGNORE flag; parse
  DELETE/UPDATE ORDER BY + LIMIT; parse SELECT ... FOR UPDATE / LOCK IN SHARE MODE
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse CREATE TABLE ... LIKE and
  CREATE TABLE ... AS SELECT after table name

### Analyzer
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — handle new Stmt variants
  (Call, Do → noop analysis; CreateTableLike → resolve source; CreateTableAsSelect →
  analyze inner SELECT)

### Executor
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — dispatch new Stmt variants
- `crates/axiomdb-sql/src/executor/insert_heap.rs` — INSERT IGNORE: wrap insert
  attempt in constraint-catching loop
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` — same for clustered path
- `crates/axiomdb-sql/src/executor/delete.rs` — DELETE ORDER BY + LIMIT: sort
  candidates before deleting
- `crates/axiomdb-sql/src/executor/update_entry.rs` — UPDATE ORDER BY + LIMIT:
  sort candidates before updating
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` — CREATE TABLE LIKE:
  copy TableDef from catalog; CREATE TABLE AS SELECT: run inner SELECT + create table

### Plan traversal (for new Stmt variants in existing traversal code)
- `crates/axiomdb-sql/src/plan_deps.rs` — add arms for new Stmt variants
- `crates/axiomdb-sql/src/eval/core.rs` or `eval/` — ensure no breakage

---

## Algorithm / Data structures

### G5.1 — CALL / DO

```
// Lexer: add Token::Call ("CALL"), Token::Do ("DO")
// Parser/mod.rs dispatch:
Token::Call => parse_call(p)   // consume CALL, parse ident, consume (args), return Stmt::Call
Token::Do   => parse_do(p)     // consume DO, parse_expr, return Stmt::Do

// Stmt::Call { name: String, args: Vec<Expr> }
// Stmt::Do   { expr: Expr }

// Analyzer: both are noops — no column resolution needed
// Executor: both return QueryResult::Empty
```

### G5.2 — SELECT FOR UPDATE / LOCK IN SHARE MODE

```
// AST: SelectStmt gains:
//   pub lock_mode: Option<LockMode>
// New enum:
//   pub enum LockMode { ForUpdate, ShareMode }

// Parser/dml.rs — after parsing LIMIT/OFFSET, peek:
//   Token::For → advance, expect UPDATE → lock_mode = ForUpdate
//   Token::Lock → advance, expect IN SHARE MODE → lock_mode = ShareMode

// Analyzer: pass through (lock_mode is not analyzed)
// Executor: ignore lock_mode entirely (Phase 13.7 will implement it)
```

### G5.3 — INSERT IGNORE

```
// AST: InsertStmt gains:
//   pub ignore: bool

// Parser: after INSERT keyword, if Token::Ignore → advance, set ignore=true

// Executor (heap path, insert_heap.rs):
//   for each row in source:
//     match insert_one_row(row) {
//       Ok(_)  => count += 1,
//       Err(e) if stmt.ignore && is_ignorable(e) => continue,  // skip row
//       Err(e) => return Err(e),                               // propagate
//     }
//
// fn is_ignorable(e: &DbError) -> bool {
//     matches!(e,
//       DbError::UniqueViolation { .. }
//     | DbError::DuplicateKey
//     | DbError::NotNullViolation { .. }
//     | DbError::ForeignKeyViolation { .. }
//     )
// }

// Executor (clustered path, insert_clustered.rs): same pattern
// Note: SQLite uses pre-write check; MariaDB uses post-write handler flag.
// We use post-attempt catch (simpler, same semantics for our use case).
```

### G5.4 — DELETE / UPDATE ORDER BY + LIMIT

```
// AST additions:
// DeleteStmt gains:
//   pub order_by: Vec<OrderByItem>
//   pub limit:    Option<Expr>
// UpdateStmt gains:
//   pub order_by: Vec<OrderByItem>
//   pub limit:    Option<Expr>

// Parser/dml.rs — after WHERE clause, peek:
//   Token::Order → parse order_by (reuse existing parse_order_by)
//   Token::Limit → parse limit expr

// Executor — DELETE with order_by + limit (heap path):
//   1. Collect all matching RecordIds (existing candidate collection)
//   2. If order_by: fetch rows for all candidates, sort by order_by exprs
//   3. If limit: truncate candidate list to N
//   4. Delete remaining candidates as normal
//
// For clustered DELETE: same — sort ClusteredDeleteCandidates before deletion
// For UPDATE: same pattern — collect → sort → limit → apply updates
//
// Sorting uses the same eval+compare logic as ORDER BY in SELECT
// (reuse eval_order_by_key or similar from executor/select_helpers.rs)
```

### G5.5 — CREATE TABLE ... LIKE

```
// AST: new Stmt::CreateTableLike:
//   pub struct CreateTableLikeStmt {
//     pub if_not_exists: bool,
//     pub new_table:    TableRef,
//     pub source_table: TableRef,
//   }

// Parser/ddl.rs — after parsing new table name in parse_create_table:
//   if peek == Token::Like:
//     advance, parse source_table_name → return CreateTableLike stmt

// Analyzer: resolve source_table name (check it exists in catalog)

// Executor (in ddl_create_table.rs or new fn):
//   1. Read source TableDef from catalog
//   2. Build new TableDef:
//      - same columns (clone Vec<ColumnDef>)
//      - same table_constraints (clone Vec<TableConstraint>)
//      - reset auto_increment sequence to 0
//      - new table name = new_table.name
//   3. Create the table via catalog writer (same path as regular CREATE TABLE)
//   4. Create indexes via the same index-creation path as CREATE TABLE
//   5. Return QueryResult::Empty

// Key insight from MariaDB research: mysql_prepare_alter_table() copies both
// columns AND index definitions. We do the same — copy the full TableDef.
```

### G5.6 — CREATE TABLE ... AS SELECT (CTAS)

```
// AST: new Stmt::CreateTableAsSelect:
//   pub struct CreateTableAsSelectStmt {
//     pub new_table: TableRef,
//     pub select:    SelectStmt,
//   }

// Parser/ddl.rs — after parsing new table name:
//   if peek == Token::As and next == Token::Select:
//     advance past AS (optional), parse inner SELECT → return CreateTableAsSelect
//   also handle: CREATE TABLE t SELECT ... (without AS keyword — MySQL allows it)

// Analyzer: analyze the inner SelectStmt fully (resolve columns, types)

// Executor (in ddl_create_table.rs):
//   1. Execute inner SELECT → collect all rows into Vec<Vec<Value>>
//   2. Derive column names from SelectStmt.columns (aliases or expr names)
//   3. Derive column types from first non-null value in each column
//      (or TEXT fallback for all-null columns) — DataFusion approach adapted:
//      since our SELECT is already evaluated (not a plan), we infer from values
//   4. Build ColumnDef for each column (name + type, no constraints)
//   5. Create the table (catalog writer)
//   6. Insert all rows using normal heap INSERT path
//   7. Return QueryResult::Affected { count: rows_inserted }

// Type inference table:
//   Value::Int(_)       → DataType::Int
//   Value::BigInt(_)    → DataType::BigInt
//   Value::Real(_)      → DataType::Real
//   Value::Text(_)      → DataType::Text
//   Value::Bool(_)      → DataType::Bool
//   Value::Date(_)      → DataType::Date
//   Value::Timestamp(_) → DataType::Timestamp
//   Value::Null         → skip (use next non-null row), fallback DataType::Text

fn infer_column_type(rows: &[Vec<Value>], col_idx: usize) -> DataType {
    for row in rows {
        match &row[col_idx] {
            Value::Null => continue,
            v => return data_type_of(v),
        }
    }
    DataType::Text  // all-null column
}
```

---

## Implementation order

1. **Lexer** — add Token::Call, Token::Do, Token::Ignore, Token::For, Token::Share, Token::Mode
   (some may already exist; verify first)
2. **AST** — add all new fields and structs
3. **Parser** — G5.1, G5.2, G5.3, G5.4 (DML changes), G5.5, G5.6 (DDL changes)
4. **Analyzer** — add arms for new Stmt variants (mostly noops)
5. **plan_deps.rs** — add arms for new Stmt variants
6. **Executor G5.1 + G5.2** — trivial (noops + ignored field)
7. **Executor G5.3** — INSERT IGNORE catch loop
8. **Executor G5.4** — sort+limit in delete/update candidate collection
9. **Executor G5.5** — CREATE TABLE LIKE (catalog copy)
10. **Executor G5.6** — CTAS (SELECT → type inference → create → insert)

---

## Tests to write

**Unit/parser tests** (`integration_mysql_compat.rs` or new `integration_g5.rs`):
- `CALL proc()` → `Stmt::Call`
- `DO 1+1` → `Stmt::Do`
- `SELECT * FROM t FOR UPDATE` → `SelectStmt { lock_mode: Some(ForUpdate) }`
- `SELECT * FROM t LOCK IN SHARE MODE` → `SelectStmt { lock_mode: Some(ShareMode) }`
- `INSERT IGNORE INTO t VALUES (1)` → `InsertStmt { ignore: true }`
- `DELETE FROM t ORDER BY id LIMIT 10` → `DeleteStmt { order_by: [..], limit: Some(..) }`
- `UPDATE t SET x=1 ORDER BY id DESC LIMIT 5` → `UpdateStmt { order_by: [..], limit: Some(..) }`
- `CREATE TABLE t2 LIKE t1` → `Stmt::CreateTableLike`
- `CREATE TABLE t2 AS SELECT * FROM t1` → `Stmt::CreateTableAsSelect`

**Integration/executor tests** (new `integration_g5.rs`):
- INSERT IGNORE: dup PK → row skipped, affected=1 not 2
- INSERT IGNORE: multi-row, some dup → correct affected count
- DELETE ORDER BY + LIMIT: deletes exactly N rows in correct order
- UPDATE ORDER BY + LIMIT: updates exactly N rows in correct order
- CREATE TABLE LIKE: new table has same schema, no rows
- CTAS: new table has correct columns, all rows from SELECT

---

## Anti-patterns to avoid

- **DO NOT** reuse/copy the OrderByItem sort logic — import it from `executor/select_helpers.rs`
- **DO NOT** let INSERT IGNORE catch panics or non-constraint errors (only the 4 listed error variants)
- **DO NOT** copy rows before collecting for CTAS — execute SELECT once, collect into Vec, then create table

## Risks

- `Token::For` / `Token::Share` / `Token::Mode` may conflict with existing tokens (check lexer)
- DELETE/UPDATE ORDER BY candidate sort: for clustered tables, candidates are
  already key-ordered — verify sort doesn't break MVCC visibility
- CTAS with all-NULL first row: type inference falls to TEXT, which is correct
  but may surprise users expecting INT — acceptable for MVP
