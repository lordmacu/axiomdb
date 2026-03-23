# Plan: 4.5 — Basic Executor

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-sql/src/executor.rs` | **create** | `execute()` + all sub-handlers + helpers |
| `crates/axiomdb-sql/src/lib.rs` | modify | `pub mod executor` + re-export `execute` |
| `crates/axiomdb-sql/tests/integration_executor.rs` | **create** | End-to-end integration tests |

No new crate. No new dependency (all deps already present from 4.5b).

---

## Algorithm / Data structures

### Module layout

```
executor.rs
  ├── pub fn execute(stmt, storage, txn) → Result<QueryResult, DbError>
  │     └── dispatch + autocommit logic
  │
  ├── fn execute_select(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_insert(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_update(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_delete(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_create_table(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_drop_table(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_create_index(stmt, storage, txn) → Result<QueryResult, DbError>
  ├── fn execute_drop_index(stmt, storage, txn) → Result<QueryResult, DbError>
  │
  ├── fn make_resolver(storage, txn) → Result<SchemaResolver, DbError>
  ├── fn datatype_to_column_type(dt) → Result<ColumnType, DbError>
  ├── fn column_type_to_datatype(ct) → DataType
  ├── fn build_column_meta(col, table_name) → ColumnMeta
  └── fn expr_column_name(expr, alias) → String
```

### Step 1 — `execute()` autocommit dispatch

```rust
pub fn execute(
    stmt: Stmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    if txn.active_txn_id().is_some() {
        // Inside an explicit transaction — execute without auto-commit.
        dispatch(stmt, storage, txn)
    } else {
        match stmt {
            Stmt::Begin => { txn.begin()?; Ok(QueryResult::Empty) }
            Stmt::Commit => { txn.commit()?; Ok(QueryResult::Empty) }
            Stmt::Rollback => { txn.rollback(storage)?; Ok(QueryResult::Empty) }
            other => txn.autocommit(storage, |txn| dispatch(other, storage, txn)),
        }
    }
}

fn dispatch(stmt: Stmt, storage: &mut dyn StorageEngine, txn: &mut TxnManager)
    -> Result<QueryResult, DbError>
{
    match stmt {
        Stmt::Select(s)      => execute_select(s, storage, txn),
        Stmt::Insert(s)      => execute_insert(s, storage, txn),
        Stmt::Update(s)      => execute_update(s, storage, txn),
        Stmt::Delete(s)      => execute_delete(s, storage, txn),
        Stmt::CreateTable(s) => execute_create_table(s, storage, txn),
        Stmt::DropTable(s)   => execute_drop_table(s, storage, txn),
        Stmt::CreateIndex(s) => execute_create_index(s, storage, txn),
        Stmt::DropIndex(s)   => execute_drop_index(s, storage, txn),
        Stmt::Set(_)         => Ok(QueryResult::Empty),   // stub
        Stmt::Begin | Stmt::Commit | Stmt::Rollback => {
            // Inside an active txn: handle transaction nesting / errors.
            match stmt {
                Stmt::Begin    => Err(DbError::TransactionAlreadyActive {
                    txn_id: txn.active_txn_id().unwrap_or(0)
                }),
                Stmt::Commit   => { txn.commit()?; Ok(QueryResult::Empty) }
                Stmt::Rollback => { txn.rollback(storage)?; Ok(QueryResult::Empty) }
                _ => unreachable!(),
            }
        }
        // Unimplemented statements:
        Stmt::TruncateTable(_) =>
            Err(DbError::NotImplemented { feature: "TRUNCATE TABLE — Phase 4.21".into() }),
        Stmt::AlterTable(_) =>
            Err(DbError::NotImplemented { feature: "ALTER TABLE — Phase 4.22".into() }),
        Stmt::ShowTables(_) | Stmt::ShowColumns(_) =>
            Err(DbError::NotImplemented { feature: "SHOW/DESCRIBE — Phase 4.20".into() }),
    }
}
```

### Step 2 — `execute_select`

```
fn execute_select(stmt: SelectStmt, storage, txn):
  // Guard unsupported clauses first.
  if !stmt.joins.is_empty()        → NotImplemented("JOIN — Phase 4.8")
  if !stmt.group_by.is_empty()     → NotImplemented("GROUP BY — Phase 4.9")
  if !stmt.order_by.is_empty()     → NotImplemented("ORDER BY — Phase 4.10")
  if stmt.limit.is_some()          → NotImplemented("LIMIT — Phase 4.10")
  if stmt.distinct                 → NotImplemented("DISTINCT — Phase 4.12")

  match stmt.from {
    None => {
      // SELECT without FROM (4.5a).
      let mut out_row = Vec::new();
      let mut out_cols = Vec::new();
      for item in stmt.columns {
        match item {
          Expr { expr, alias } => {
            let v = eval(&expr, &[])?;
            let name = alias.unwrap_or_else(|| "?column?".into());
            out_row.push(v);
            out_cols.push(ColumnMeta::computed(name, DataType::Text));
          }
          Wildcard | QualifiedWildcard(_) =>
            return Err(NotImplemented("SELECT * without FROM"))
        }
      }
      return Ok(QueryResult::Rows { columns: out_cols, rows: vec![out_row] });
    }

    Some(FromClause::Subquery { .. }) =>
      return Err(NotImplemented("subquery in FROM — Phase 4.11")),

    Some(FromClause::Table(table_ref)) => {
      let resolver = make_resolver(storage, txn)?;
      let resolved = resolver.resolve_table(table_ref.schema.as_deref(), &table_ref.name)?;

      let snap = txn.active_snapshot()?;
      let raw_rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap)?;

      // Build ColumnMeta for the output schema.
      let out_cols = build_select_column_meta(&stmt.columns, &resolved)?;

      // Filter + project.
      let mut rows: Vec<Row> = Vec::new();
      for (_rid, values) in raw_rows {
        // WHERE filter.
        if let Some(ref wc) = stmt.where_clause {
          let result = eval(wc, &values)?;
          if !is_truthy(&result) { continue; }
        }
        // Projection.
        let out_row = project_row(&stmt.columns, &values, &resolved)?;
        rows.push(out_row);
      }

      Ok(QueryResult::Rows { columns: out_cols, rows })
    }
  }
```

#### `build_select_column_meta` helper

```
fn build_select_column_meta(items, resolved) → Result<Vec<ColumnMeta>>:
  let mut cols = Vec::new();
  for item in items:
    match item:
      Wildcard → for col in resolved.columns:
                   cols.push(build_column_meta(&col, &resolved.def.table_name))
      QualifiedWildcard(t) → same but filter by table name matching t
      Expr { expr, alias } → {
        let name = alias.unwrap_or_else(|| expr_column_name(&expr));
        let (dt, nullable) = infer_expr_type(&expr, &resolved.columns);
        cols.push(ColumnMeta { name, data_type: dt, nullable, table_name: None })
      }
  Ok(cols)
```

#### `project_row` helper

```
fn project_row(items, values, resolved) → Result<Row>:
  let mut out = Vec::new();
  for item in items:
    match item:
      Wildcard → out.extend(values.iter().cloned())
      QualifiedWildcard(t) →
        // All columns from this table. Since Phase 4.5 has no JOINs, t is always
        // the single table in FROM. Include all values.
        out.extend(values.iter().cloned())
      Expr { expr, .. } → out.push(eval(&expr, values)?)
  Ok(out)
```

#### `infer_expr_type` helper

```
fn infer_expr_type(expr, columns) → (DataType, bool):
  match expr:
    Expr::Column { col_idx, .. } →
      if col_idx < columns.len():
        let col = &columns[col_idx];
        (column_type_to_datatype(col.col_type), col.nullable)
      else:
        (DataType::Text, true)  // safe fallback
    _ → (DataType::Text, true)  // proper type inference in Phase 6
```

### Step 3 — `execute_insert`

```
fn execute_insert(stmt: InsertStmt, storage, txn):
  let resolver = make_resolver(storage, txn)?;
  let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
  let schema_cols = resolved.columns;

  // Build positional mapping: schema_index → values_index or None (fill Null).
  let col_map: Vec<usize> = match stmt.columns {
    None => (0..schema_cols.len()).collect(),
    Some(named) => {
      // named is Vec<String> of column names from the statement.
      // Build mapping: for each schema column, what position in the VALUES row?
      let mut map = vec![usize::MAX; schema_cols.len()];
      for (val_pos, name) in named.iter().enumerate() {
        let schema_pos = schema_cols.iter().position(|c| c.name == *name)
          .ok_or(DbError::ColumnNotFound { name: name.clone(), table: resolved.def.table_name.clone() })?;
        map[schema_pos] = val_pos;
      }
      map  // entries with MAX = not provided → Value::Null
    }
  };

  let source = match stmt.source {
    InsertSource::Values(rows) => rows,
    InsertSource::Select(_) =>
      return Err(DbError::NotImplemented { feature: "INSERT SELECT — Phase 4.6".into() }),
    InsertSource::DefaultValues =>
      return Err(DbError::NotImplemented { feature: "DEFAULT VALUES — Phase 4.3c".into() }),
  };

  let mut count = 0u64;
  for value_exprs in source {
    // Evaluate each expression against an empty row (INSERT VALUES has no FROM).
    let provided: Vec<Value> = value_exprs.iter()
      .map(|e| eval(e, &[]))
      .collect::<Result<Vec<_>, _>>()?;

    // Build full_values: one Value per schema column.
    let full_values: Vec<Value> = col_map.iter().map(|&idx| {
      if idx == usize::MAX { Value::Null }
      else { provided[idx].clone() }
    }).collect();

    TableEngine::insert_row(storage, txn, &resolved.def, &schema_cols, full_values)?;
    count += 1;
  }

  Ok(QueryResult::Affected { count, last_insert_id: None })
```

### Step 4 — `execute_update`

```
fn execute_update(stmt: UpdateStmt, storage, txn):
  let resolver = make_resolver(storage, txn)?;
  let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
  let schema_cols = resolved.columns;

  // Build assignment column indices ONCE (not per row).
  let assignments: Vec<(usize, &Expr)> = stmt.assignments.iter()
    .map(|a| {
      let pos = schema_cols.iter().position(|c| c.name == a.column)
        .ok_or_else(|| DbError::ColumnNotFound {
          name: a.column.clone(), table: resolved.def.table_name.clone()
        })?;
      Ok((pos, &a.value))
    })
    .collect::<Result<Vec<_>, DbError>>()?;

  let snap = txn.active_snapshot()?;
  let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap)?;

  let mut count = 0u64;
  for (rid, current_values) in rows {
    // WHERE filter.
    if let Some(ref wc) = stmt.where_clause {
      if !is_truthy(&eval(wc, &current_values)?) { continue; }
    }
    // Apply assignments.
    let mut new_values = current_values.clone();
    for (col_pos, val_expr) in &assignments {
      new_values[*col_pos] = eval(val_expr, &current_values)?;
    }
    TableEngine::update_row(storage, txn, &resolved.def, &schema_cols, rid, new_values)?;
    count += 1;
  }

  Ok(QueryResult::Affected { count, last_insert_id: None })
```

### Step 5 — `execute_delete`

```
fn execute_delete(stmt: DeleteStmt, storage, txn):
  let resolver = make_resolver(storage, txn)?;
  let resolved = resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?;
  let schema_cols = resolved.columns;

  let snap = txn.active_snapshot()?;
  let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap)?;

  // Collect matching RecordIds BEFORE deleting (must not mutate while iterating).
  let to_delete: Vec<RecordId> = rows.into_iter()
    .filter_map(|(rid, values)| {
      match &stmt.where_clause {
        None => Some(Ok(rid)),
        Some(wc) => match eval(wc, &values) {
          Ok(v) if is_truthy(&v) => Some(Ok(rid)),
          Ok(_) => None,
          Err(e) => Some(Err(e)),
        }
      }
    })
    .collect::<Result<Vec<_>, DbError>>()?;

  let count = to_delete.len() as u64;
  for rid in to_delete {
    TableEngine::delete_row(storage, txn, &resolved.def, rid)?;
  }

  Ok(QueryResult::Affected { count, last_insert_id: None })
```

### Step 6 — `execute_create_table`

```
fn execute_create_table(stmt: CreateTableStmt, storage, txn):
  let schema = stmt.table.schema.as_deref().unwrap_or("public");

  // Check existence.
  let resolver = make_resolver(storage, txn)?;
  if resolver.table_exists(Some(schema), &stmt.table.name)? {
    if stmt.if_not_exists { return Ok(QueryResult::Empty); }
    return Err(DbError::TableAlreadyExists {
      schema: schema.to_string(), name: stmt.table.name.clone()
    });
  }

  let mut writer = CatalogWriter::new(storage, txn)?;
  let table_id = writer.create_table(schema, &stmt.table.name)?;

  for (i, col_def) in stmt.columns.iter().enumerate() {
    let col_type = datatype_to_column_type(&col_def.data_type)?;
    let nullable = !col_def.constraints.iter()
      .any(|c| matches!(c, ColumnConstraint::NotNull));
    writer.create_column(CatalogColumnDef {
      table_id,
      col_idx: i as u16,
      name: col_def.name.clone(),
      col_type,
      nullable,
    })?;
  }

  Ok(QueryResult::Empty)
```

### Step 7 — `execute_drop_table`

```
fn execute_drop_table(stmt: DropTableStmt, storage, txn):
  let mut writer = CatalogWriter::new(storage, txn)?;

  for table_ref in stmt.tables {
    let schema = table_ref.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;
    let reader = CatalogReader::new(storage, snap)?;

    match reader.get_table(schema, &table_ref.name)? {
      None if stmt.if_exists => continue,
      None => return Err(DbError::TableNotFound { name: table_ref.name.clone() }),
      Some(def) => writer.delete_table(def.id)?,
    }
  }

  Ok(QueryResult::Empty)
```

### Step 8 — `execute_create_index`

```
fn execute_create_index(stmt: CreateIndexStmt, storage, txn):
  let schema = stmt.table.schema.as_deref().unwrap_or("public");
  let resolver = make_resolver(storage, txn)?;
  let resolved = resolver.resolve_table(Some(schema), &stmt.table.name)?;

  // Allocate an empty B-Tree root page for the index.
  let root_page_id = storage.alloc_page(PageType::Index)?;
  let root_page = Page::new(PageType::Index, root_page_id);
  storage.write_page(root_page_id, &root_page)?;

  let mut writer = CatalogWriter::new(storage, txn)?;
  writer.create_index(IndexDef {
    index_id: 0,                       // allocated by create_index
    table_id: resolved.def.id,
    name: stmt.name.clone(),
    root_page_id,
    is_unique: stmt.unique,
    is_primary: false,
  })?;

  Ok(QueryResult::Empty)
```

### Step 9 — `execute_drop_index`

```
fn execute_drop_index(stmt: DropIndexStmt, storage, txn):
  let snap = txn.active_snapshot()?;
  let reader = CatalogReader::new(storage, snap)?;

  // Resolve table_id (MySQL requires ON table in DROP INDEX).
  let table_id = if let Some(ref table_ref) = stmt.table {
    let schema = table_ref.schema.as_deref().unwrap_or("public");
    match reader.get_table(schema, &table_ref.name)? {
      Some(def) => Some(def.id),
      None if stmt.if_exists => return Ok(QueryResult::Empty),
      None => return Err(DbError::TableNotFound { name: table_ref.name.clone().unwrap_or_default() }),
    }
  } else { None };

  // Find index by name.
  let index_id = if let Some(tid) = table_id {
    let indexes = reader.list_indexes(tid)?;
    indexes.into_iter().find(|i| i.name == stmt.name).map(|i| i.index_id)
  } else {
    None  // without table, can't scan all — error or not-found
  };

  match index_id {
    None if stmt.if_exists => Ok(QueryResult::Empty),
    None => Err(DbError::NotImplemented {
      feature: format!("DROP INDEX {} — index not found", stmt.name)
    }),
    Some(id) => {
      let mut writer = CatalogWriter::new(storage, txn)?;
      writer.delete_index(id)?;
      Ok(QueryResult::Empty)
    }
  }
```

### Private helpers

```rust
fn make_resolver<'a>(storage: &'a dyn StorageEngine, txn: &TxnManager)
    -> Result<SchemaResolver<'a>, DbError>
{
    let snap = txn.active_snapshot()
        .unwrap_or_else(|_| txn.snapshot());
    SchemaResolver::new(storage, snap, "public")
}

fn datatype_to_column_type(dt: &DataType) -> Result<ColumnType, DbError> {
    match dt {
        DataType::Bool      => Ok(ColumnType::Bool),
        DataType::Int       => Ok(ColumnType::Int),
        DataType::BigInt    => Ok(ColumnType::BigInt),
        DataType::Real      => Ok(ColumnType::Float),
        DataType::Text      => Ok(ColumnType::Text),
        DataType::Bytes     => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid      => Ok(ColumnType::Uuid),
        DataType::Decimal   => Err(DbError::NotImplemented {
            feature: "DECIMAL column type — Phase 4.3".into()
        }),
        DataType::Date      => Err(DbError::NotImplemented {
            feature: "DATE column type — Phase 4.19".into()
        }),
    }
}

fn column_type_to_datatype(ct: ColumnType) -> DataType {
    match ct {
        ColumnType::Bool      => DataType::Bool,
        ColumnType::Int       => DataType::Int,
        ColumnType::BigInt    => DataType::BigInt,
        ColumnType::Float     => DataType::Real,
        ColumnType::Text      => DataType::Text,
        ColumnType::Bytes     => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid      => DataType::Uuid,
    }
}

fn build_column_meta(col: &CatalogColumnDef, table_name: &str) -> ColumnMeta {
    ColumnMeta {
        name: col.name.clone(),
        data_type: column_type_to_datatype(col.col_type),
        nullable: col.nullable,
        table_name: Some(table_name.to_string()),
    }
}

fn expr_column_name(expr: &Expr, alias: Option<String>) -> String {
    if let Some(a) = alias { return a; }
    match expr {
        Expr::Column { name, .. } => name.clone(),
        _ => "?column?".into(),
    }
}
```

---

## Implementation order

1. **Create `executor.rs`** with imports, `execute()`, `dispatch()`, and all sub-functions.
   `cargo check -p axiomdb-sql` must pass.

2. **Export from `lib.rs`**: `pub mod executor; pub use executor::execute;`

3. **Write unit tests** in `executor.rs` — helper tests (`datatype_to_column_type`, `column_type_to_datatype`, `expr_column_name`).

4. **Write integration tests** in `tests/integration_executor.rs`.
   Start with `CREATE TABLE → INSERT → SELECT`, verify it passes.
   Add each statement type incrementally.

5. **Full workspace check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Unit tests in `executor.rs`

```
test_datatype_to_column_type_all_supported
  — Bool/Int/BigInt/Real/Text/Bytes/Timestamp/Uuid all map correctly

test_datatype_to_column_type_unsupported
  — Decimal and Date return NotImplemented

test_column_type_to_datatype_roundtrip
  — column_type_to_datatype(datatype_to_column_type(dt).unwrap()) == dt for all supported

test_expr_column_name_column_expr
  — Column { name: "age", .. } → "age"

test_expr_column_name_other_expr
  — Literal(Int(1)) → "?column?"

test_expr_column_name_alias_wins
  — alias Some("years") → "years" regardless of expr
```

### Integration tests in `tests/integration_executor.rs`

All tests use `MemoryStorage` + `TxnManager` in tempdir. SQL strings are parsed
and analyzed before execution.

```
// Round-trip basics
test_create_table_insert_select
  — CREATE TABLE users (id INT, name TEXT)
  — INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')
  — SELECT * FROM users  → 2 rows

test_select_with_where_filter
  — same table, SELECT * FROM users WHERE id = 1 → 1 row

test_select_projection
  — SELECT name FROM users → 1 column ('name')

test_select_alias
  — SELECT id AS user_id FROM users → ColumnMeta.name = "user_id"

test_select_without_from
  — SELECT 1 → Rows { rows: [[Int(1)]] }

test_select_without_from_expr
  — SELECT 1 + 2 AS total → rows: [[Int(3)]], columns: [name="total"]

// INSERT
test_insert_named_columns
  — INSERT INTO users (name, id) VALUES ('Charlie', 3)
  — verifies column reordering works correctly

test_insert_missing_column_is_null
  — INSERT INTO users (id) VALUES (4) (no name provided)
  — SELECT name FROM users WHERE id = 4 → [Null]

test_insert_wrong_column_count
  — INSERT INTO users VALUES (1) (only 1 value for 2-column table) → TypeMismatch

test_insert_unknown_column
  — INSERT INTO users (id, unknown_col) VALUES (1, 'x') → ColumnNotFound

// UPDATE
test_update_matching_rows
  — INSERT 3 rows, UPDATE users SET name = 'X' WHERE id = 2
  — affected count = 1, scan shows updated value

test_update_no_match
  — UPDATE users SET name = 'Y' WHERE id = 999 → Affected { count: 0 }

test_update_unknown_column
  — UPDATE users SET nonexistent = 1 WHERE id = 1 → ColumnNotFound

// DELETE
test_delete_with_where
  — INSERT 3 rows, DELETE FROM users WHERE id = 2
  — count = 1, scan shows 2 remaining rows

test_delete_without_where
  — INSERT 3 rows, DELETE FROM users → count = 3, scan shows 0 rows

// DDL
test_create_table_if_not_exists
  — CREATE TABLE t (id INT)
  — CREATE TABLE IF NOT EXISTS t (id INT) → Empty (no error)

test_create_table_duplicate_error
  — CREATE TABLE t (id INT) twice → TableAlreadyExists

test_drop_table
  — CREATE TABLE t, DROP TABLE t
  — SELECT * FROM t → TableNotFound

test_drop_table_if_exists
  — DROP TABLE IF EXISTS nonexistent → Empty (no error)

test_create_and_drop_index
  — CREATE TABLE t, CREATE INDEX idx ON t (id)
  — catalog has index, DROP INDEX idx ON t, catalog no longer has index

// Transaction control
test_explicit_transaction_begin_commit
  — BEGIN, INSERT INTO t VALUES (1), COMMIT
  — SELECT sees the row

test_explicit_transaction_rollback
  — BEGIN, INSERT INTO t VALUES (1), ROLLBACK
  — SELECT sees 0 rows

test_autocommit_per_statement
  — INSERT without BEGIN — each INSERT is independently visible

// Error paths
test_select_nonexistent_table
  — SELECT * FROM nonexistent → TableNotFound

test_join_returns_not_implemented
  — SELECT * FROM t JOIN t2 ON ... → NotImplemented

test_order_by_returns_not_implemented
  — SELECT * FROM t ORDER BY id → NotImplemented

// Full round-trip
test_full_crud_roundtrip
  — CREATE TABLE, INSERT 5 rows, SELECT (verify 5), UPDATE 2, SELECT (verify),
    DELETE 1, SELECT (verify 4 remaining), DROP TABLE, SELECT → TableNotFound
```

---

## Anti-patterns to avoid

- **DO NOT** scan inside the delete loop. Collect rids first, then delete. Modifying
  a heap page while iterating scan results produces undefined behavior (scan might
  re-visit or skip updated slots).
- **DO NOT** build `SchemaResolver` more than once per statement. One resolver per
  call is correct; creating multiple with different snapshots in the same statement
  is a consistency bug.
- **DO NOT** call `txn.active_snapshot()?` before `dispatch()` is invoked — the
  active snapshot is only valid inside `autocommit` (after `begin()`).
- **DO NOT** use `txn.snapshot()` inside statements — always use
  `txn.active_snapshot()?` so that INSERT-then-SELECT within the same transaction
  sees the written rows (read-your-own-writes).
- **DO NOT** `unwrap()` anywhere in `src/` code.
- **DO NOT** silently swallow `NotImplemented` — always return it to the caller.

---

## Risks

| Risk | Mitigation |
|---|---|
| `execute_drop_table` calls `CatalogWriter::delete_table` which re-reads catalog under `active_snapshot` — creates two borrows of storage | `delete_table` internally uses `active_snapshot()` itself, not a passed snapshot — no double-borrow. Verify with `cargo check`. |
| `execute_create_table` calls `make_resolver` which borrows `storage` immutably, then `CatalogWriter::new` borrows mutably | Drop resolver before constructing writer — resolver is dropped after the `table_exists` check. Explicit `drop(resolver)` if needed. |
| `execute_update` scans (reads) and then updates (writes) in two separate passes — what if another transaction writes between scan and update? | Phase 4.5 is single-writer (one explicit txn at a time). Concurrent writes are Phase 7 (SSI). No risk here. |
| `INSERT` with empty column list (`stmt.columns = None`) must map all columns in schema order | Explicitly test: 2-column table, INSERT with no column list, verify both columns appear in correct order. |
| `execute_drop_index` without a table ref cannot scan all indexes efficiently | For Phase 4.5, DROP INDEX always requires ON table (MySQL syntax). If stmt.table is None, return NotImplemented. |
