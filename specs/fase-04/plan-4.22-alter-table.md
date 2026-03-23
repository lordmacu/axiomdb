# Plan: 4.22 — ALTER TABLE

## Files to create/modify

| File | Action | What changes |
|---|---|---|
| `crates/axiomdb-core/src/error.rs` | modify | Add `ColumnAlreadyExists` variant (SQLSTATE 42701) |
| `crates/axiomdb-catalog/src/writer.rs` | modify | Add `delete_column`, `rename_column`, `rename_table` |
| `crates/axiomdb-sql/src/parser/ddl.rs` | modify | Add `parse_alter_table` function |
| `crates/axiomdb-sql/src/parser/mod.rs` | modify | Add `Token::Alter` dispatch |
| `crates/axiomdb-sql/src/executor.rs` | modify | Add `execute_alter_table` + `rewrite_rows` + dispatch |
| `crates/axiomdb-sql/tests/integration_executor.rs` | modify | ALTER TABLE tests |

---

## Implementation phases (in dependency order)

### Phase A — `DbError::ColumnAlreadyExists`

```rust
/// SQLSTATE 42701 — duplicate_column
#[error("column '{name}' already exists in table '{table}'")]
ColumnAlreadyExists { name: String, table: String },
```

Add to `sqlstate()`:
```rust
DbError::ColumnAlreadyExists { .. } => "42701",
```

---

### Phase B — `CatalogWriter` new methods

Three new methods, all following the pattern from `delete_index`:

#### `delete_column(table_id, col_idx)`
```rust
pub fn delete_column(&mut self, table_id: TableId, col_idx: u16) -> Result<(), DbError> {
    let txn_id = self.txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let snap = self.txn.active_snapshot()?;
    let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;
    for (page_id, slot_id, data) in rows {
        let (def, _) = ColumnDef::from_bytes(&data)?;
        if def.table_id == table_id && def.col_idx == col_idx {
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
            let mut key = [0u8; 6];
            key[0..4].copy_from_slice(&table_id.to_le_bytes());
            key[4..6].copy_from_slice(&col_idx.to_le_bytes());
            self.txn.record_delete(SYSTEM_TABLE_COLUMNS, &key, &data, page_id, slot_id)?;
            return Ok(());
        }
    }
    Err(DbError::Internal { message: format!("column col_idx={col_idx} not found for delete") })
}
```

#### `rename_column(table_id, col_idx, new_name)`
```rust
pub fn rename_column(&mut self, table_id: TableId, col_idx: u16, new_name: String) -> Result<(), DbError> {
    // 1. Scan to find the old ColumnDef
    // 2. delete_column(table_id, col_idx)
    // 3. create_column(ColumnDef { ..old_def, name: new_name })
}
```

#### `rename_table(table_id, new_name, schema)`
```rust
pub fn rename_table(&mut self, table_id: TableId, new_name: String, schema: &str) -> Result<(), DbError> {
    let txn_id = self.txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let snap = self.txn.active_snapshot()?;
    let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;
    for (page_id, slot_id, data) in rows {
        let (def, _) = TableDef::from_bytes(&data)?;
        if def.id == table_id {
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
            let key = table_id.to_le_bytes();
            self.txn.record_delete(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?;
            // Insert new def with updated name
            let new_def = TableDef { table_name: new_name, ..def };
            let new_data = new_def.to_bytes();
            let (pg2, sl2) = HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id)?;
            self.txn.record_insert(SYSTEM_TABLE_TABLES, &key, &new_data, pg2, sl2)?;
            return Ok(());
        }
    }
    Err(DbError::TableNotFound { name: format!("{schema}.???") })
}
```

---

### Phase C — Parser: `parse_alter_table`

Add to `parse_stmt` in `mod.rs`:
```rust
Token::Alter => {
    self.advance();
    self.expect(&Token::Table)?;
    ddl::parse_alter_table(self)
}
```

New function in `ddl.rs`:
```rust
pub(crate) fn parse_alter_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let table = p.parse_table_ref()?;
    let mut operations = Vec::new();

    loop {
        let op = match p.peek().clone() {
            Token::Add => {
                p.advance();
                p.eat(&Token::Column); // optional keyword
                let col_def = parse_column_def(p)?;
                AlterTableOp::AddColumn(col_def)
            }
            Token::Drop => {
                p.advance();
                p.eat(&Token::Column);
                let if_exists = if matches!((p.peek(), p.peek_at(1)), (Token::If, Token::Exists)) {
                    p.advance(); p.advance(); true
                } else { false };
                let name = p.parse_identifier()?;
                AlterTableOp::DropColumn { name, if_exists }
            }
            Token::Rename => {
                p.advance();
                match p.peek().clone() {
                    Token::Column => {
                        p.advance();
                        let old_name = p.parse_identifier()?;
                        p.expect(&Token::To)?;
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameColumn { old_name, new_name }
                    }
                    Token::To => {
                        p.advance();
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameTable(new_name)
                    }
                    other => return Err(DbError::ParseError {
                        message: format!("expected COLUMN or TO after RENAME, found {other:?}"),
                    }),
                }
            }
            Token::Modify => {
                p.advance();
                p.eat(&Token::Column);
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE MODIFY COLUMN — Phase N".into(),
                });
            }
            _ => break,
        };
        operations.push(op);
        if !p.eat(&Token::Comma) { break; }
    }

    if operations.is_empty() {
        return Err(DbError::ParseError {
            message: "ALTER TABLE: expected ADD, DROP, or RENAME".into(),
        });
    }

    Ok(Stmt::AlterTable(AlterTableStmt { table, operations }))
}
```

**New tokens needed:** `Token::Rename` and `Token::Modify` — check if they exist in lexer.

---

### Phase D — Executor: `execute_alter_table` + `rewrite_rows`

Replace the `AlterTable` `NotImplemented` stub in dispatch:
```rust
Stmt::AlterTable(s) => execute_alter_table(s, storage, txn),
```

#### `rewrite_rows` helper
```rust
fn rewrite_rows(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    old_columns: &[ColumnDef],
    new_columns: &[ColumnDef],
    transform: &dyn Fn(Row) -> Row,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, table_def, old_columns, snap)?;
    for (rid, old_values) in rows {
        let new_values = transform(old_values);
        TableEngine::delete_row(storage, txn, table_def, rid)?;
        TableEngine::insert_row(storage, txn, table_def, new_columns, new_values)?;
    }
    Ok(())
}
```

#### `execute_alter_table`
```rust
fn execute_alter_table(stmt: AlterTableStmt, storage, txn) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let snap = txn.active_snapshot()?;

    // Resolve table
    let reader = CatalogReader::new(storage, snap)?;
    let table_def = reader.get_table(schema, &stmt.table.name)?
        .ok_or_else(|| DbError::TableNotFound { name: stmt.table.name.clone() })?;
    let mut columns = reader.list_columns(table_def.id)?;

    for op in stmt.operations {
        match op {
            AlterTableOp::AddColumn(col_def) => {
                execute_add_column(storage, txn, &table_def, &mut columns, col_def, schema)?;
            }
            AlterTableOp::DropColumn { name, if_exists } => {
                execute_drop_column(storage, txn, &table_def, &mut columns, &name, if_exists, schema)?;
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                execute_rename_column(storage, txn, &table_def, &columns, &old_name, &new_name, schema)?;
                // Refresh columns list for subsequent operations in same statement
                let snap2 = txn.active_snapshot()?;
                columns = CatalogReader::new(storage, snap2)?.list_columns(table_def.id)?;
            }
            AlterTableOp::RenameTable(new_name) => {
                execute_rename_table(storage, txn, &table_def, &new_name, schema)?;
                // Can't do more ops after RENAME TABLE (table_def is stale) — OK since parser
                // allows only one op per ALTER TABLE statement for RENAME TO.
            }
            _ => return Err(DbError::NotImplemented {
                feature: "ALTER TABLE MODIFY COLUMN / ADD CONSTRAINT — Phase N".into(),
            }),
        }
    }

    // Invalidate session cache for this table
    // (called from execute_with_ctx which has ctx; from execute it's a no-op)
    Ok(QueryResult::Empty)
}
```

#### `execute_add_column`
```rust
fn execute_add_column(storage, txn, table_def, columns, col_def, schema) -> Result<(), DbError>:
    1. Check no column with same name exists → ColumnAlreadyExists
    2. Evaluate DEFAULT expr (or use NULL) → default_value: Value
    3. new_col_idx = columns.iter().map(|c| c.col_idx).max().unwrap_or(0) + 1 (as u16)
    4. Build new CatalogColumnDef { table_id, col_idx: new_col_idx, name, col_type, nullable, auto_increment }
    5. CatalogWriter::create_column(new_def)
    6. new_columns = columns + [new_def] (sorted by col_idx)
    7. rewrite_rows(storage, txn, table_def, columns, &new_columns, |mut row| {
           row.push(default_value.clone()); row
       })
    8. columns.push(new_def)
```

#### `execute_drop_column`
```rust
fn execute_drop_column(storage, txn, table_def, columns, name, if_exists, schema) -> Result<(), DbError>:
    1. Find column by name → Option<(drop_pos, &ColumnDef)>
    2. If not found and if_exists → return Ok(QueryResult::Empty)
    3. If not found → ColumnNotFound
    4. drop_pos = position of column in sorted columns list
    5. Build new_columns = columns without the dropped column
    6. rewrite_rows(storage, txn, table_def, columns, &new_columns, |mut row| {
           row.remove(drop_pos); row
       })
    7. CatalogWriter::delete_column(table_def.id, dropped_col.col_idx)
```

#### `execute_rename_column`
```rust
fn execute_rename_column(storage, txn, table_def, columns, old_name, new_name, schema) -> Result<(), DbError>:
    1. Find column by old_name → ColumnNotFound if missing
    2. Check new_name not already in use → ColumnAlreadyExists
    3. CatalogWriter::rename_column(table_def.id, found_col.col_idx, new_name)
    // No row rewriting needed
```

#### `execute_rename_table`
```rust
fn execute_rename_table(storage, txn, table_def, new_name, schema) -> Result<(), DbError>:
    1. Check new_name not already in use in same schema → TableAlreadyExists
    2. CatalogWriter::rename_table(table_def.id, new_name, schema)
    // No row rewriting needed
```

---

### Phase E — Session cache invalidation

The ctx-aware executor (`execute_with_ctx`) must invalidate the session cache after ALTER TABLE:

```rust
// In execute_with_ctx dispatch:
Stmt::AlterTable(s) => {
    let result = execute_alter_table(s.clone(), storage, txn);
    ctx.invalidate_all(); // schema changed — drop all cached schemas
    result
}
```

---

## Tests to write

### Parser tests (in `integration_ddl_parser.rs`)
- `ALTER TABLE t ADD COLUMN c INT` parses correctly
- `ALTER TABLE t ADD c TEXT NOT NULL DEFAULT 'x'` (no COLUMN keyword)
- `ALTER TABLE t DROP COLUMN c`
- `ALTER TABLE t DROP COLUMN IF EXISTS c`
- `ALTER TABLE t RENAME COLUMN a TO b`
- `ALTER TABLE t RENAME TO new_name`
- Multiple ops: `ALTER TABLE t ADD COLUMN x INT, DROP COLUMN y`
- Invalid: `ALTER TABLE t` (no op) → ParseError

### Executor tests (in `integration_executor.rs`)
- ADD COLUMN: column appears in DESCRIBE, existing rows have NULL
- ADD COLUMN with DEFAULT: existing rows have default value
- ADD COLUMN duplicate name → ColumnAlreadyExists
- DROP COLUMN: column gone from DESCRIBE, remaining data correct
- DROP COLUMN IF EXISTS on nonexistent → no error
- RENAME COLUMN: new name in DESCRIBE, data unchanged
- RENAME COLUMN to existing name → ColumnAlreadyExists
- RENAME TABLE: SHOW TABLES shows new name
- RENAME TABLE to existing → TableAlreadyExists
- After ADD COLUMN: INSERT and SELECT work correctly with new schema
- After DROP COLUMN: INSERT and SELECT work correctly with reduced schema

---

## Anti-patterns to avoid

- **DO NOT renumber col_idx after DROP COLUMN** — the remaining columns keep
  their original `col_idx` values. The row codec is positional based on sort
  order of the column list, not on the numeric col_idx value. Renumbering would
  break existing rows that were encoded with the old col_idx mapping.

- **DO NOT forget to invalidate SessionContext cache** after ALTER TABLE — the
  schema cache (SchemaCache) stores resolved column lists. Without invalidation,
  subsequent queries would use the stale pre-ALTER schema and produce wrong results
  or ColumnIndexOutOfBounds errors.

- **DO NOT execute `rewrite_rows` before updating the catalog** for ADD COLUMN —
  the new column must be in the catalog first so the new rows are correctly
  validated on insert. For DROP COLUMN, do the row rewrite FIRST, then update
  the catalog, so that if the rewrite fails the catalog still matches the rows.

## Risks

- **Rename token conflicts**: `Token::Rename` may not exist in the lexer.
  `Token::Modify` may not exist. Check before parser implementation and add if
  needed. `Token::To` already exists (used in `... TO new_name`).

- **Multi-operation ALTER TABLE**: each operation sees the UPDATED column list
  from previous operations. Pass `&mut columns` and update it after each operation
  to ensure correct behavior for `ALTER TABLE t ADD COLUMN a INT, ADD COLUMN b TEXT`.
