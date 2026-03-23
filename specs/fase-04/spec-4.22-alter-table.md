# Spec: 4.22 — ALTER TABLE

## What to build

Four `ALTER TABLE` operations, all blocking (no concurrent DDL):

1. **`ADD COLUMN name type [NOT NULL] [DEFAULT expr]`** — adds a new column at
   the end of the schema. Existing rows are rewritten to include the new column's
   default value (NULL, or the evaluated DEFAULT expression if provided).

2. **`DROP COLUMN name [IF EXISTS]`** — removes a column. Existing rows are
   rewritten without the dropped column's value. The remaining columns keep their
   original `col_idx` values (no renumbering needed because `col_idx` is an
   ordering key, not a byte offset).

3. **`RENAME COLUMN old_name TO new_name`** — renames a column. Catalog-only
   change: no row rewriting required because rows are indexed by `col_idx`, not
   by column name.

4. **`RENAME TO new_name`** — renames the table. Catalog-only change: no row
   rewriting. Note: references in other tables (FK) are not updated (no FK engine
   yet in Phase 4.22).

**Why row rewriting for ADD/DROP:** The row codec encodes values positionally.
The null bitmap is derived from the schema column count at decode time. Adding a
column makes existing bitmaps ambiguous (bit N−1 is 0/"not null" but no bytes
follow for column N). Dropping a column leaves a value at the wrong positional
slot. Rewriting guarantees the rows always match the current schema.

---

## Inputs / Outputs

### ADD COLUMN
```sql
ALTER TABLE users ADD COLUMN age INT;
ALTER TABLE users ADD COLUMN email TEXT NOT NULL DEFAULT 'unknown';
```
- Input: table reference + `ColumnDef` (name, type, constraints)
- Output: `QueryResult::Empty`
- Side effect: new ColumnDef in catalog with `col_idx = max_existing + 1`;
  all existing rows rewritten with new column value (NULL or DEFAULT)
- Error: `TableNotFound` if table doesn't exist; `ColumnAlreadyExists` error (new)
  if a column with the same name already exists

### DROP COLUMN
```sql
ALTER TABLE users DROP COLUMN age;
ALTER TABLE users DROP COLUMN IF EXISTS age;
```
- Input: table reference + column name + `if_exists: bool`
- Output: `QueryResult::Empty`
- Side effect: ColumnDef deleted from catalog; all existing rows rewritten
  without the dropped column's value
- Error: `ColumnNotFound` if column doesn't exist and `IF EXISTS` not specified

### RENAME COLUMN
```sql
ALTER TABLE users RENAME COLUMN email TO contact_email;
```
- Input: table reference + old column name + new column name
- Output: `QueryResult::Empty`
- Side effect: catalog ColumnDef updated with new name, same `col_idx` and type
- Error: `ColumnNotFound` if `old_name` doesn't exist; `ColumnAlreadyExists` if
  `new_name` already exists

### RENAME TABLE
```sql
ALTER TABLE users RENAME TO customers;
```
- Input: table reference + new table name
- Output: `QueryResult::Empty`
- Side effect: catalog TableDef updated with new name
- Error: `TableNotFound` if table doesn't exist; `TableAlreadyExists` if
  `new_name` already in use in the same schema

---

## Row rewriting algorithm (ADD and DROP COLUMN)

Used by both ADD COLUMN and DROP COLUMN:

```
fn rewrite_rows(
    storage, txn,
    table_def: &TableDef,
    old_columns: &[ColumnDef],
    new_columns: &[ColumnDef],
    transform: fn(old_row: &[Value]) -> Vec<Value>,
) -> Result<(), DbError>:
    1. snap = txn.active_snapshot()
    2. rows = TableEngine::scan_table(storage, table_def, old_columns, snap)
       → Vec<(RecordId, Row)>
    3. For each (rid, old_values):
       a. new_values = transform(old_values)
       b. TableEngine::delete_row(storage, txn, table_def, rid)
       c. TableEngine::insert_row(storage, txn, table_def, new_columns, new_values)
```

**For ADD COLUMN** — transform appends NULL (or DEFAULT) at position `col_idx`:
```
transform(old_row) = old_row + [default_value]
```

**For DROP COLUMN** (dropping column at sorted position `drop_pos`):
```
transform(old_row) = old_row[0..drop_pos] + old_row[drop_pos+1..]
```
`drop_pos` is the position of the dropped column in the sorted (by col_idx)
column list, NOT the `col_idx` value itself.

---

## Catalog operations needed (new CatalogWriter methods)

### `delete_column(table_id: TableId, col_idx: u16)`
Scans `axiom_columns`, finds the row where `table_id` and `col_idx` match,
marks it as deleted in the heap via MVCC delete + WAL entry.

### `rename_column(table_id: TableId, col_idx: u16, new_name: String)`
Reads the existing ColumnDef, deletes the old row, inserts a new row with
`name = new_name` (all other fields preserved).

### `rename_table(table_id: TableId, new_name: String, schema: &str)`
Reads the existing TableDef, deletes the old row, inserts a new row with
`table_name = new_name` (schema, id, and data_root_page_id preserved).

---

## New error variant

```rust
/// A column with this name already exists in the table.
/// SQLSTATE 42701
#[error("column '{name}' already exists in table '{table}'")]
ColumnAlreadyExists { name: String, table: String },
```

---

## Parser — ALTER TABLE

`ALTER TABLE` is not currently parsed. Add `Token::Alter` dispatch in
`parse_stmt`:

```
Token::Alter → advance, expect Token::Table, parse_table_ref
  then parse one or more operations separated by comma:
    ADD [COLUMN] column_def
    DROP [COLUMN] [IF EXISTS] column_name
    RENAME COLUMN old_name TO new_name
    RENAME TO new_table_name
    MODIFY COLUMN column_def  (→ NotImplemented for Phase 4.22)
    ADD CONSTRAINT ...        (→ NotImplemented — Phase 4.22b)
    DROP CONSTRAINT name      (→ NotImplemented — Phase 4.22b)
```

MySQL compatibility: `COLUMN` keyword is optional in `ADD`/`DROP`.

---

## Acceptance criteria

- [ ] `ALTER TABLE t ADD COLUMN c INT` adds column to catalog and rewrites rows
- [ ] Added column has `col_idx = max_existing + 1`
- [ ] Existing rows have NULL for the new column after ADD
- [ ] `ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'` fills existing rows with 'x'
- [ ] `ALTER TABLE t ADD COLUMN c INT` on a table with non-zero rows → rows correct after
- [ ] `ALTER TABLE t ADD COLUMN c INT` when `c` already exists → `ColumnAlreadyExists`
- [ ] `ALTER TABLE t DROP COLUMN c` removes column from catalog and rewrites rows
- [ ] Remaining columns have correct values after DROP (no positional shift errors)
- [ ] `ALTER TABLE t DROP COLUMN nonexistent` → `ColumnNotFound`
- [ ] `ALTER TABLE t DROP COLUMN IF EXISTS nonexistent` → `Empty` (no error)
- [ ] `ALTER TABLE t RENAME COLUMN a TO b` → correct name in DESCRIBE output
- [ ] `ALTER TABLE t RENAME COLUMN nonexistent TO b` → `ColumnNotFound`
- [ ] `ALTER TABLE t RENAME COLUMN a TO b` when `b` exists → `ColumnAlreadyExists`
- [ ] `ALTER TABLE t RENAME TO new_name` → SHOW TABLES shows new name
- [ ] `ALTER TABLE t RENAME TO existing_name` → `TableAlreadyExists`
- [ ] `ALTER TABLE nonexistent ADD COLUMN c INT` → `TableNotFound`
- [ ] `cargo test --workspace` passes clean

---

## Out of scope

- `MODIFY COLUMN` / `ALTER COLUMN TYPE` — requires type-coerced row rewriting (Phase N)
- `ADD CONSTRAINT` / `DROP CONSTRAINT` — Phase 4.22b
- Column position control (`FIRST`, `AFTER col`) — Phase N
- Non-blocking DDL / online schema change — Phase N
- FK reference updates on RENAME TABLE — Phase N (no FK engine yet)

## ⚠️ DEFERRED

- `MODIFY COLUMN` → NotImplemented, Phase N
- `ADD/DROP CONSTRAINT` → NotImplemented, Phase 4.22b
- Renumbering `col_idx` after DROP COLUMN → not needed (positional codec handles sparse col_idx)

## Dependencies

- New `DbError::ColumnAlreadyExists` variant + SQLSTATE 42701
- `CatalogWriter::delete_column`, `rename_column`, `rename_table`
- `rewrite_rows` helper in executor
- Parser `Token::Alter` dispatch in `parse_stmt`
- Semantic analyzer: `AlterTable` passes through (no column resolution needed — operations reference columns by name, not col_idx, before resolution)
