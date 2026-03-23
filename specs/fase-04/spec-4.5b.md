# Spec: 4.5b — Table Engine

## What to build (not how)

A stateless `TableEngine` that bridges the gap between the SQL executor (which
works with `Vec<Value>` rows) and the raw storage layer (which works with `&[u8]`
bytes in heap pages). The executor calls `TableEngine` for every DML statement
on a user table; the table engine handles encoding, decoding, MVCC, and WAL
logging transparently.

This spec also covers a required prerequisite: extending `TableDef` with a
`data_root_page_id` field so the table engine knows which heap page chains hold
user data. Without this field, there is no way to locate a table's rows.

---

## Prerequisite: `TableDef.data_root_page_id`

### Problem

`TableDef` currently stores `{id, schema_name, table_name}`. When the executor
needs to scan or write rows for table `users`, there is no way to know which
heap page chain holds the data.

### Solution

Add `data_root_page_id: u64` to `TableDef`. On `CREATE TABLE`, `CatalogWriter`
allocates an empty `Data` page and stores its ID here. From that point on, all
reads and writes for this table use `data_root_page_id` as the `HeapChain` root.

### Updated `TableDef`

```rust
pub struct TableDef {
    pub id: TableId,
    pub schema_name: String,
    pub table_name: String,
    pub data_root_page_id: u64,   // heap chain root for user row data
}
```

### Updated binary format for `nexus_tables`

Before: `[table_id:4][schema_len:1][schema bytes][name_len:1][name bytes]`
After:  `[table_id:4][root_page_id:8][schema_len:1][schema bytes][name_len:1][name bytes]`

The 8-byte `root_page_id` is inserted after `table_id`. All existing `to_bytes`
/ `from_bytes` code must be updated to match.

### Updated `CatalogWriter::create_table`

After allocating the `TableId`, the writer must:
1. Allocate a `Data` page with `storage.alloc_page(PageType::Data)`.
2. Initialize it as an empty heap page: `Page::new(PageType::Data, root_page_id)`.
3. Write the page to storage: `storage.write_page(root_page_id, &page)`.
4. Store `data_root_page_id` in the `TableDef` before inserting into `nexus_tables`.

---

## `TableEngine`

### Location

`axiomdb-sql/src/table.rs`

### New dependencies

`axiomdb-sql/Cargo.toml`: add `axiomdb-wal` as a regular (non-dev) dependency.

### Design: stateless unit struct

Follows the same pattern as `HeapChain`:

```rust
pub struct TableEngine;
```

All methods take `storage` and `txn` as explicit parameters. There is no
lifetime-bound state.

---

## Inputs / Outputs

### `TableEngine::scan_table`

```rust
pub fn scan_table(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
```

- Calls `HeapChain::scan_visible(storage, table_def.data_root_page_id, snap)`
- For each `(page_id, slot_id, raw_bytes)`, decodes with
  `decode_row(&raw_bytes, &column_data_types(columns))`
- Returns `(RecordId { page_id, slot_id }, values)` pairs
- An empty table returns `Ok(vec![])`
- `columns` must be sorted ascending by `col_idx` (i.e., column declaration order)

### `TableEngine::insert_row`

```rust
pub fn insert_row(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    columns: &[ColumnDef],
    values: Vec<Value>,
) -> Result<RecordId, DbError>
```

Steps (in order):
1. Validate `values.len() == columns.len()` — else `DbError::TypeMismatch`.
2. Apply coercion: for each column, call `coerce(values[i], column_data_type, Strict)`.
3. Encode: `encode_row(&coerced_values, &column_data_types(columns))`.
4. Obtain active `txn_id`: `txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?`.
5. Insert into heap: `HeapChain::insert(storage, table_def.data_root_page_id, &encoded, txn_id)` → `(page_id, slot_id)`.
6. WAL log: `txn.record_insert(table_def.id, &encode_rid(page_id, slot_id), &encoded, page_id, slot_id)`.
7. Return `RecordId { page_id, slot_id }`.

### `TableEngine::delete_row`

```rust
pub fn delete_row(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    columns: &[ColumnDef],
    record_id: RecordId,
) -> Result<(), DbError>
```

Steps (in order):
1. Read old bytes: `HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)`.
   - Returns `None` → `DbError::InvalidSlot` (row already dead or never existed).
2. Obtain active `txn_id`.
3. Delete: `HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)`.
4. WAL log: `txn.record_delete(table_def.id, &encode_rid(record_id), &old_bytes, record_id.page_id, record_id.slot_id)`.

### `TableEngine::update_row`

```rust
pub fn update_row(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    columns: &[ColumnDef],
    record_id: RecordId,
    new_values: Vec<Value>,
) -> Result<RecordId, DbError>
```

UPDATE = DELETE old row + INSERT new row (two physical operations, two WAL entries).

Steps (in order):
1. Read old bytes.
2. Obtain active `txn_id`.
3. Apply coercion to `new_values`.
4. Encode new values.
5. Mark old slot deleted: `HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)`.
6. WAL log delete: `txn.record_delete(table_def.id, &encode_rid(record_id), &old_bytes, ...)`.
7. Insert new row: `HeapChain::insert(storage, table_def.data_root_page_id, &new_encoded, txn_id)` → `new_rid`.
8. WAL log insert: `txn.record_insert(table_def.id, &encode_rid(new_rid), &new_encoded, ...)`.
9. Return `new_rid`.

**Why not use `record_update`?** `TxnManager::record_update` takes a single
`page_id` for both old and new slots, but the new row may land on a different
page if the old page is full. Using separate `record_delete` + `record_insert`
is always correct and crash-safe. A unified UPDATE WAL entry is an optimization
for Phase 7+.

---

## `HeapChain::read_row` (new method)

```rust
/// Reads the application payload of the tuple at `(page_id, slot_id)`.
///
/// Returns `None` if the slot is dead (already deleted or never written).
/// Returns `Err` on I/O error or invalid slot index.
///
/// The returned bytes are the data portion of the tuple (excluding RowHeader).
pub fn read_row(
    storage: &dyn StorageEngine,
    page_id: u64,
    slot_id: u16,
) -> Result<Option<Vec<u8>>, DbError>
```

Implementation: read the page, call `heap::read_tuple(page, slot_id)`, return
`Some(data.to_vec())` if the slot is live, `None` if dead.

---

## WAL key convention for user tables

Both `record_insert` and `record_delete` require a `key: &[u8]` identifying the
row. Since Phase 4.5b does not enforce primary key constraints yet, the key is
the physical location of the row encoded as 10 bytes:

```
key = [page_id: 8 bytes LE, slot_id: 2 bytes LE]
```

Helper: `fn encode_rid(page_id: u64, slot_id: u16) -> [u8; 10]` (private to the module).

This convention is safe: crash recovery already embeds the physical location
separately inside the WAL value bytes (via `encode_physical_loc`). The key
provides an additional lookup hint for future index-based WAL replay.

---

## `column_data_types` helper (private)

The row codec needs `&[DataType]` but `ColumnDef` stores `ColumnType` (the compact
catalog representation). A private helper extracts the types in column order:

```rust
fn column_data_types(columns: &[ColumnDef]) -> Vec<DataType>
```

Mapping (`ColumnType` → `DataType`):

| ColumnType | DataType |
|---|---|
| `Bool` | `Bool` |
| `Int` | `Int` |
| `BigInt` | `BigInt` |
| `Float` | `Real` |
| `Text` | `Text` |
| `Bytes` | `Bytes` |
| `Timestamp` | `Timestamp` |
| `Uuid` | `Uuid` |

`ColumnType::Decimal` and `ColumnType::Date` do not exist yet in the catalog;
this mapping will be extended when they are added.

---

## Error cases

| Situation | Error |
|---|---|
| `values.len() != columns.len()` on insert | `DbError::TypeMismatch` |
| Coercion fails (wrong type, overflow) | `DbError::InvalidCoercion` |
| NaN in a `Real` value | `DbError::InvalidValue` |
| `insert_row` / `delete_row` / `update_row` outside a transaction | `DbError::NoActiveTransaction` |
| `delete_row` / `update_row` targeting a dead slot | `DbError::AlreadyDeleted` |
| `delete_row` / `update_row` targeting out-of-range slot | `DbError::InvalidSlot` |
| Storage I/O failure | `DbError::Io` |
| Page full during insert (chain growth) | Handled internally by `HeapChain::insert` |

---

## Use cases

### 1. Full table scan

```rust
// executor needs to run: SELECT * FROM users
let table_def = resolver.resolve_table(schema, "users")?.def;
let columns   = resolver.resolve_table(schema, "users")?.columns;
let rows = TableEngine::scan_table(storage, &table_def, &columns, snap)?;
// rows: Vec<(RecordId, Vec<Value>)>
```

### 2. INSERT one row

```rust
// executor runs: INSERT INTO users (name, age) VALUES ('Alice', 30)
txn.begin()?;
let rid = TableEngine::insert_row(
    storage, txn, &table_def, &columns,
    vec![Value::Text("Alice".into()), Value::Int(30)],
)?;
txn.commit()?;
// rid: RecordId { page_id: 2, slot_id: 0 }
```

### 3. DELETE a row found by scan

```rust
txn.begin()?;
for (rid, values) in TableEngine::scan_table(storage, &table_def, &columns, snap)? {
    if values[1] == Value::Int(30) {
        TableEngine::delete_row(storage, txn, &table_def, &columns, rid)?;
    }
}
txn.commit()?;
```

### 4. UPDATE a row

```rust
txn.begin()?;
let new_rid = TableEngine::update_row(
    storage, txn, &table_def, &columns, old_rid,
    vec![Value::Text("Alice".into()), Value::Int(31)],
)?;
txn.commit()?;
// new_rid may differ from old_rid if the page overflowed
```

### 5. Empty table scan

```rust
// Fresh CREATE TABLE — data root page exists but is empty
let rows = TableEngine::scan_table(storage, &table_def, &columns, snap)?;
assert!(rows.is_empty());
```

---

## Acceptance criteria

- [ ] `TableDef` has `data_root_page_id: u64` field
- [ ] `TableDef::to_bytes()` emits `[table_id:4][root_page_id:8][schema...][name...]`
- [ ] `TableDef::from_bytes()` parses the new format correctly
- [ ] `TableDef::to_bytes()` / `from_bytes()` roundtrip for all fields including `data_root_page_id`
- [ ] `CatalogWriter::create_table()` allocates a `Data` page and stores its ID in `TableDef`
- [ ] `HeapChain::read_row()` returns `Some(bytes)` for live slots, `None` for dead slots
- [ ] `TableEngine::scan_table()` returns decoded `Vec<Value>` rows for all visible tuples
- [ ] `TableEngine::insert_row()` encodes + writes to heap + WAL-logs the insert
- [ ] `TableEngine::delete_row()` stamps deletion + WAL-logs with old bytes
- [ ] `TableEngine::update_row()` = delete + insert, two WAL entries
- [ ] All four operations respect MVCC: inserted rows visible only to snapshots after commit
- [ ] `scan_table` returns 0 rows on a freshly created (empty) table
- [ ] Coercion is applied in `insert_row` and `update_row` (strict mode)
- [ ] Integration test: CREATE TABLE → INSERT → scan → DELETE → scan shows 0 rows
- [ ] Integration test: UPDATE changes the values and the old RecordId becomes invisible
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- Primary key constraint enforcement (Phase 4.5)
- UNIQUE constraint checking (Phase 4.5)
- NOT NULL enforcement at the table engine level (Phase 4.5 — the analyzer checks this)
- Index maintenance on INSERT/DELETE/UPDATE (Phase 4.5 / Phase 6)
- UPDATE WAL using the single `record_update` entry (optimization for Phase 7+)
- Row-level locking (Phase 7 — MVCC handles isolation for now)
- Support for `ColumnType::Decimal` and `ColumnType::Date` (added when catalog supports them)

---

## Dependencies

- `axiomdb-catalog` — `TableDef`, `ColumnDef`, `CatalogWriter` (already a dep)
- `axiomdb-storage` — `HeapChain`, `StorageEngine` (already a dep)
- `axiomdb-types` — `Value`, `DataType`, `encode_row`, `decode_row`, `coerce` (already a dep)
- `axiomdb-wal` — `TxnManager` (**new regular dep** — currently dev-only)
- `axiomdb-core` — `RecordId`, `TransactionSnapshot`, `DbError` (already a dep)
