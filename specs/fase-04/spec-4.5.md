# Spec: 4.5 — Basic Executor

## What to build (not how)

The executor is the component that interprets an analyzed `Stmt` and produces a
`QueryResult`. It is the first component to exercise the full pipeline:
`parse → analyze → execute → WAL → result`.

All previous components (parser, analyzer, `TableEngine`, `CatalogWriter`,
`SchemaResolver`) are already complete. The executor connects them.

**Included in this subfase (also closes 4.5a):**
- `SELECT` — table scan with optional WHERE filter + column projection
- `SELECT` without `FROM` — evaluate expressions against an empty row
- `INSERT VALUES` — single and multi-row, with or without explicit column list
- `UPDATE` — full scan + WHERE filter + row replacement
- `DELETE` — full scan + WHERE filter + row removal
- `CREATE TABLE` — catalog entry + data page allocation
- `DROP TABLE [IF EXISTS]` — catalog deletion
- `CREATE [UNIQUE] INDEX` — catalog entry + B-Tree root page (no index build)
- `DROP INDEX [IF EXISTS]` — catalog deletion
- `BEGIN / COMMIT / ROLLBACK` — delegation to `TxnManager`
- `SET variable = value` — stub, returns `Empty`

---

## Entry point

```rust
/// Executes a single analyzed SQL statement.
///
/// If no transaction is currently active, the statement is automatically
/// wrapped in an implicit BEGIN / COMMIT (autocommit mode). If a transaction
/// is already active, the executor participates in it without committing.
///
/// Transaction control statements (`BEGIN`, `COMMIT`, `ROLLBACK`) are handled
/// specially: they operate directly on `txn` regardless of autocommit.
///
/// # Errors
/// Any `DbError` from storage, WAL, or semantic validation. On error in
/// autocommit mode, the implicit transaction is automatically rolled back.
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError>
```

### Autocommit semantics

```
if txn.active_txn_id().is_some():
    // Inside an explicit transaction — just execute, do not commit.
    dispatch(stmt, storage, txn)
else:
    match stmt {
        // Transaction control statements bypass autocommit.
        Stmt::Begin => { txn.begin()?; Ok(QueryResult::Empty) }
        Stmt::Commit => { txn.commit()?; Ok(QueryResult::Empty) }
        Stmt::Rollback => { txn.rollback(storage)?; Ok(QueryResult::Empty) }
        // All other statements use autocommit.
        other => txn.autocommit(storage, |txn| dispatch(other, storage, txn))
    }
```

Note: when `txn.active_txn_id().is_some()` and the user sends `BEGIN`, it is
an error (`DbError::TransactionAlreadyActive`). The executor propagates this
directly — no double-wrapping.

### Snapshot selection for reads

| Context | Snapshot to use | Why |
|---|---|---|
| Inside explicit txn | `txn.active_snapshot()?` | Sees own uncommitted writes |
| Autocommit (implicit txn inside `autocommit()`) | `txn.active_snapshot()?` | Same — `autocommit` has begun a txn |
| Outside any txn (e.g. for SHOW TABLES) | `txn.snapshot()` | Read-only view of committed data |

The executor always uses `txn.active_snapshot()?` inside `autocommit` or an
explicit txn. Since `autocommit` always calls `begin()` first, the snapshot is
always valid when `execute_select` is called.

---

## Statement specifications

### SELECT (with FROM)

```
Input:  SelectStmt { distinct, columns, from: Some(FromClause::Table(ref)),
                     joins: [], where_clause, group_by: [], having: None,
                     order_by: [], limit: None, offset: None }

Preconditions:
  - joins must be empty → else Err(NotImplemented { feature: "JOIN — Phase 4.8" })
  - group_by must be empty → else Err(NotImplemented { feature: "GROUP BY — Phase 4.9" })
  - order_by must be empty → else Err(NotImplemented { feature: "ORDER BY — Phase 4.10" })
  - limit must be None → else Err(NotImplemented { feature: "LIMIT — Phase 4.10" })
  - distinct must be false → else Err(NotImplemented { feature: "DISTINCT — Phase 4.12" })
  - from must be FromClause::Table (not Subquery) → else Err(NotImplemented { feature: "subquery — Phase 4.11" })

Algorithm:
  1. Resolve table: resolver.resolve_table(ref.schema.as_deref(), &ref.name)?
     → ResolvedTable { def, columns: Vec<ColumnDef>, ... }
  2. Get snapshot: txn.active_snapshot()?
  3. Scan: TableEngine::scan_table(storage, &def, &columns, snap)?
     → Vec<(RecordId, Vec<Value>)>
  4. Filter: for each row, if where_clause.is_some():
       result = eval(&where_clause, &row_values)?
       keep row only if is_truthy(&result)
  5. Project: for each kept row, build output row from columns:
       - SelectItem::Wildcard → all column values in schema order
       - SelectItem::QualifiedWildcard(t) → all columns from table t (same here, just t == table name)
       - SelectItem::Expr { expr, alias } → eval(&expr, &row_values)?, name = alias.unwrap_or(expr_text)
  6. Build ColumnMeta for each output column:
       - For Wildcard: one ColumnMeta per catalog ColumnDef
       - For Expr: ColumnMeta::computed(name, inferred_type)
         where inferred_type is DataType::from_catalog(col_type) if the expr is a plain column,
         otherwise DataType::Text as a safe fallback (proper type inference is Phase 6)
  7. Return QueryResult::Rows { columns: Vec<ColumnMeta>, rows: Vec<Row> }
```

**`expr_text` for computed column names:** use `"?column?"` as the fallback
name for any non-column expression (same as PostgreSQL behavior for unnamed
expressions). The caller can override with an alias.

### SELECT without FROM

```
Input: SelectStmt { from: None, joins: [], where_clause: None, ... }

Algorithm:
  1. Evaluate each SelectItem against an empty row (&[]):
       SelectItem::Expr { expr, alias } → eval(&expr, &[])?
       name = alias.unwrap_or("?column?")
  2. Return QueryResult::Rows { columns, rows: vec![output_row] }
     (always exactly 1 row)

Note: SELECT 1 → Rows { columns: [ColumnMeta::computed("?column?", DataType::Int)],
                          rows: [[Value::Int(1)]] }
      SELECT 1+1 AS two → rows: [[Value::Int(2)]], columns: [name="two"]
```

### INSERT VALUES

```
Input:  InsertStmt { table, columns: Option<Vec<String>>, source: InsertSource::Values(rows) }

Algorithm:
  1. Resolve table: resolver.resolve_table(...)? → ResolvedTable { def, columns: schema_cols }
  2. Build column index map:
       If stmt.columns is None:
         col_map = schema_cols in order (map position 0..n)
       Else:
         col_map = for each named col, find its schema position
         if any name not found: Err(ColumnNotFound)
  3. For each row in InsertSource::Values:
       a. Evaluate each expression: eval(&expr, &[])?  → Value
       b. Build full_values: Vec<Value> of length schema_cols.len()
          - For positions in col_map: use the provided value
          - For positions NOT in col_map: Value::Null
          (DEFAULT handling is Phase 4.3c — use Null for now)
       c. If full_values.len() != stmt_row.len() after mapping → Err(TypeMismatch)
       d. TableEngine::insert_row(storage, txn, &def, &schema_cols, full_values)?
  4. Return QueryResult::Affected { count: rows.len() as u64, last_insert_id: None }
     (last_insert_id is None until AUTO_INCREMENT is implemented in 4.3c)
```

### INSERT SELECT (deferred)

```
Source: InsertSource::Select(_) → Err(NotImplemented { feature: "INSERT SELECT — Phase 4.6" })
Source: InsertSource::DefaultValues → Err(NotImplemented { feature: "DEFAULT VALUES — Phase 4.3c" })
```

### UPDATE

```
Input: UpdateStmt { table, assignments: Vec<Assignment>, where_clause }

Algorithm:
  1. Resolve table → ResolvedTable { def, columns: schema_cols }
  2. Get snapshot → txn.active_snapshot()?
  3. Scan all rows: TableEngine::scan_table(storage, &def, &schema_cols, snap)?
  4. For each (rid, current_values):
       a. Apply WHERE filter if present
       b. If row matches:
            new_values = current_values.clone()
            for each Assignment { column, value }:
              col_pos = find column index in schema_cols by name
              if not found: Err(ColumnNotFound)
              evaluated = eval(&value, &current_values)?
              new_values[col_pos] = evaluated
            TableEngine::update_row(storage, txn, &def, &schema_cols, rid, new_values)?
            count += 1
  5. Return QueryResult::Affected { count, last_insert_id: None }
```

### DELETE

```
Input: DeleteStmt { table, where_clause }

Algorithm:
  1. Resolve table → ResolvedTable { def, columns: schema_cols }
  2. Get snapshot → txn.active_snapshot()?
  3. Scan: TableEngine::scan_table(storage, &def, &schema_cols, snap)?
  4. Collect matching rids:
       For each (rid, row_values):
         if where_clause.is_none() or is_truthy(eval(&where_clause, &row_values)?):
           candidates.push(rid)
     IMPORTANT: collect all matching rids BEFORE deleting any.
     Deleting during iteration would cause scan to see stale slot states.
  5. For each rid in candidates:
       TableEngine::delete_row(storage, txn, &def, rid)?
       count += 1
  6. Return QueryResult::Affected { count, last_insert_id: None }
```

### CREATE TABLE

```
Input: CreateTableStmt { if_not_exists, table, columns: Vec<ast::ColumnDef>,
                         table_constraints }

Algorithm:
  1. Check if table already exists:
       exists = resolver.table_exists(schema, &table.name)?
       if exists && !if_not_exists: Err(TableAlreadyExists)
       if exists && if_not_exists: return Ok(QueryResult::Empty)  ← no-op
  2. Convert ast::ColumnDef → catalog::ColumnDef for each column:
       col_type = datatype_to_column_type(&col.data_type)?
         (returns Err(NotImplemented) for DataType::Decimal and DataType::Date —
          those ColumnType variants do not exist in the catalog yet)
       nullable = !col.constraints.contains(ColumnConstraint::NotNull)
  3. CatalogWriter::create_table(storage, txn, schema, &table.name)? → table_id
  4. For each catalog::ColumnDef in order:
       writer.create_column(ColumnDef { table_id, col_idx: i as u16, ... })?
  5. Return QueryResult::Empty
```

**`datatype_to_column_type(dt: &DataType) → Result<ColumnType, DbError>`:**

| DataType | ColumnType |
|---|---|
| Bool | Bool |
| Int | Int |
| BigInt | BigInt |
| Real | Float |
| Text | Text |
| Bytes | Bytes |
| Timestamp | Timestamp |
| Uuid | Uuid |
| Decimal | `Err(NotImplemented { feature: "DECIMAL column type — Phase 4.3" })` |
| Date | `Err(NotImplemented { feature: "DATE column type — Phase 4.19" })` |

### DROP TABLE

```
Input: DropTableStmt { if_exists, tables: Vec<TableRef>, cascade: _ }

For each table in tables:
  1. exists = resolver.table_exists(schema, &table.name)?
     if !exists && !if_exists: Err(TableNotFound)
     if !exists && if_exists: continue (skip silently)
  2. Resolve: resolver.resolve_table(...)? → table_id
  3. CatalogWriter::delete_table(storage, txn, table_id)?
     (marks catalog rows as MVCC-deleted; actual heap pages orphaned until VACUUM)

Return QueryResult::Empty
```

### CREATE INDEX

```
Input: CreateIndexStmt { if_not_exists, unique, name, table, columns }

Algorithm:
  1. Resolve table → ResolvedTable { def, ... }
  2. Check index name uniqueness (via resolver.list_indexes — skip for Phase 4.5;
     duplicate index name → catalog violation caught on next catalog scan)
  3. Allocate B-Tree root page for the index:
       root_page_id = storage.alloc_page(PageType::Index)?
       root_page = Page::new(PageType::Index, root_page_id)
       storage.write_page(root_page_id, &root_page)?
  4. writer.create_index(IndexDef {
         index_id: 0,           // allocated by writer
         table_id: def.id,
         name: name.clone(),
         root_page_id,
         is_unique: unique,
         is_primary: false,
     })?
  5. Return QueryResult::Empty

Note: The index is empty. For a newly created (empty) table, this is correct.
For a table with existing rows, the index is out of date from the start — this
is an accepted limitation for Phase 4.5. A proper index build is deferred to
Phase 6.
```

### DROP INDEX

```
Input: DropIndexStmt { if_exists, name, table }

Algorithm:
  1. Resolve the index by name from catalog (via CatalogReader::list_indexes
     for the given table, or scan nexus_indexes):
       Use SchemaResolver + CatalogReader to find the IndexDef by name.
       If not found && !if_exists: Err(NotImplemented or TableNotFound)
       If not found && if_exists: return Ok(QueryResult::Empty)
  2. CatalogWriter::delete_index(storage, txn, index_id)?
  3. Return QueryResult::Empty
```

### BEGIN / COMMIT / ROLLBACK

Handled at the autocommit dispatch level (see entry point spec above).
- `BEGIN` when no txn active: `txn.begin()?` → `QueryResult::Empty`
- `BEGIN` when txn active: `Err(TransactionAlreadyActive)`
- `COMMIT` when txn active: `txn.commit()?` → `QueryResult::Empty`
- `COMMIT` when no txn active: `Err(NoActiveTransaction)`
- `ROLLBACK` when txn active: `txn.rollback(storage)?` → `QueryResult::Empty`
- `ROLLBACK` when no txn active: `Err(NoActiveTransaction)`

### SET

```
Input: SetStmt { variable, value }

For Phase 4.5: always returns QueryResult::Empty.
Session variables are implemented in Phase 5 (wire protocol + session state).
```

---

## `SchemaResolver` usage

The executor needs a `SchemaResolver` for every statement that accesses tables.
The resolver requires a snapshot and a default schema.

```rust
fn make_resolver<'a>(
    storage: &'a dyn StorageEngine,
    txn: &TxnManager,
) -> Result<SchemaResolver<'a>, DbError> {
    let snap = txn.active_snapshot()
        .unwrap_or_else(|_| txn.snapshot());
    SchemaResolver::new(storage, snap, "public")
}
```

The default schema is `"public"` for Phase 4.5.

---

## ColumnMeta construction for SELECT

### Wildcard (`SELECT *`)

For each `ColumnDef` in `ResolvedTable.columns` (sorted by `col_idx`):

```rust
ColumnMeta {
    name:       col.name.clone(),
    data_type:  column_type_to_datatype(col.col_type),
    nullable:   col.nullable,
    table_name: Some(table_def.table_name.clone()),
}
```

### Computed expression (`SELECT expr [AS alias]`)

```rust
ColumnMeta::computed(
    alias.unwrap_or("?column?"),
    DataType::Text,   // safe fallback — proper type inference is Phase 6
)
```

Exception: if `expr` is a plain `Expr::Column { col_idx, .. }`, use the
catalog column's `DataType` and `nullable` instead of the fallback.

---

## Inputs / Outputs

| Statement | Input | Output |
|---|---|---|
| SELECT | analyzed SelectStmt | `QueryResult::Rows` |
| INSERT | InsertStmt + VALUES | `QueryResult::Affected { count, last_insert_id: None }` |
| UPDATE | UpdateStmt | `QueryResult::Affected { count, last_insert_id: None }` |
| DELETE | DeleteStmt | `QueryResult::Affected { count, last_insert_id: None }` |
| CREATE TABLE | CreateTableStmt | `QueryResult::Empty` |
| DROP TABLE | DropTableStmt | `QueryResult::Empty` |
| CREATE INDEX | CreateIndexStmt | `QueryResult::Empty` |
| DROP INDEX | DropIndexStmt | `QueryResult::Empty` |
| BEGIN/COMMIT/ROLLBACK | Stmt::Begin/Commit/Rollback | `QueryResult::Empty` |
| SET | SetStmt | `QueryResult::Empty` |

---

## Error cases

| Situation | Error |
|---|---|
| SELECT on non-existent table | `TableNotFound` |
| INSERT column not in schema | `ColumnNotFound` |
| UPDATE column not in schema | `ColumnNotFound` |
| `CREATE TABLE` for existing table (no `IF NOT EXISTS`) | `TableAlreadyExists` |
| `DROP TABLE` non-existent (no `IF EXISTS`) | `TableNotFound` |
| Unsupported DataType in CREATE TABLE | `NotImplemented` |
| JOIN in SELECT | `NotImplemented` |
| GROUP BY / ORDER BY / LIMIT / DISTINCT | `NotImplemented` |
| INSERT SELECT / DEFAULT VALUES | `NotImplemented` |
| Subquery in FROM | `NotImplemented` |
| INSERT wrong number of values | `TypeMismatch` |
| Any coercion failure | `InvalidCoercion` |
| Evaluation error in WHERE / SET | propagated from `eval()` |
| I/O error from storage/WAL | `Io` |

---

## Acceptance criteria

- [ ] `execute(Stmt::Select(...), ...)` returns `QueryResult::Rows` with correct data
- [ ] `SELECT *` returns all columns with correct `ColumnMeta` (name, data_type, nullable)
- [ ] `SELECT expr AS alias` returns correct alias in `ColumnMeta`
- [ ] `SELECT 1` (no FROM) returns `Rows { rows: [[Int(1)]] }`
- [ ] `SELECT ... WHERE expr` correctly filters rows
- [ ] `INSERT` inserts rows visible on the next scan with the same transaction
- [ ] `INSERT (col1, col2)` with named columns maps values correctly
- [ ] `INSERT` with wrong column count returns `TypeMismatch`
- [ ] `UPDATE ... WHERE ...` updates matching rows only, count is correct
- [ ] `DELETE ... WHERE ...` deletes matching rows only, count is correct
- [ ] `DELETE` without WHERE deletes all rows
- [ ] `CREATE TABLE` creates a visible table in the catalog
- [ ] `CREATE TABLE ... IF NOT EXISTS` on existing table returns `Empty` without error
- [ ] `DROP TABLE` makes the table invisible to subsequent queries
- [ ] `DROP TABLE IF EXISTS` on non-existent table returns `Empty`
- [ ] `CREATE INDEX` creates a catalog entry and allocates a B-Tree root page
- [ ] `BEGIN / COMMIT / ROLLBACK` work correctly in explicit transaction mode
- [ ] Multi-statement explicit transaction: INSERT, then SELECT in same txn sees the row
- [ ] Autocommit: each statement is independently committed
- [ ] `NotImplemented` returned for JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT
- [ ] Integration test: full round-trip `CREATE TABLE → INSERT → SELECT → UPDATE → SELECT → DELETE → SELECT`
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- JOIN (4.8), GROUP BY (4.9), ORDER BY (4.10), LIMIT (4.10), DISTINCT (4.12)
- Subqueries in FROM (4.11)
- INSERT SELECT (4.6)
- AUTO_INCREMENT / LAST_INSERT_ID (4.3c / 4.14)
- SHOW TABLES / DESCRIBE (4.20)
- TRUNCATE TABLE (4.21)
- ALTER TABLE (4.22)
- Session variable state in SET (Phase 5)
- Index build for CREATE INDEX on non-empty tables (Phase 6)
- Foreign key constraint checking (Phase 6 / Phase 13)
- NOT NULL enforcement (Phase 4.25 — strict mode framework)

---

## Dependencies

- `axiomdb-sql`: `Stmt`, `ast::*`, `eval`, `is_truthy`, `TableEngine`, `QueryResult`, `ColumnMeta`, `Row`, `analyzer::analyze` — all in same crate
- `axiomdb-catalog`: `SchemaResolver`, `CatalogWriter`, `CatalogReader` — already a dep
- `axiomdb-storage`: `StorageEngine`, `Page`, `PageType` — already a dep
- `axiomdb-wal`: `TxnManager` — already a dep (added in 4.5b)
- `axiomdb-types`: `Value`, `DataType` — already a dep

No new crate dependencies. Lives in `axiomdb-sql/src/executor.rs`.
