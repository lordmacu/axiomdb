# Plan: 4.5b — Table Engine

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/nexusdb-catalog/src/schema.rs` | modify | Add `data_root_page_id` to `TableDef`; update `to_bytes`/`from_bytes` |
| `crates/nexusdb-catalog/src/writer.rs` | modify | `create_table()` allocs + inits the data root page |
| `crates/nexusdb-storage/src/heap_chain.rs` | modify | Add `HeapChain::read_row()` |
| `crates/nexusdb-storage/src/lib.rs` | modify | Re-export `HeapChain::read_row` if needed (via `heap_chain`) |
| `crates/nexusdb-sql/Cargo.toml` | modify | Add `nexusdb-wal` as regular dep |
| `crates/nexusdb-sql/src/table.rs` | **create** | `TableEngine` struct + 4 methods + helpers |
| `crates/nexusdb-sql/src/lib.rs` | modify | `pub mod table` + re-export `TableEngine` |

---

## Algorithm / Data structures

### Step 1 — Extend `TableDef`

New struct:
```rust
pub struct TableDef {
    pub id: TableId,
    pub schema_name: String,
    pub table_name: String,
    pub data_root_page_id: u64,  // heap chain root for user row data
}
```

New `to_bytes` format: `[table_id:4][root_page_id:8][schema_len:1][schema][name_len:1][name]`

```
to_bytes:
  buf.extend(id.to_le_bytes())           // 4 bytes
  buf.extend(data_root_page_id.to_le_bytes()) // 8 bytes  ← NEW
  buf.push(schema.len() as u8)
  buf.extend(schema bytes)
  buf.push(name.len() as u8)
  buf.extend(name bytes)

from_bytes:
  check len >= 14  (was 6 — 4 id + 8 root + 1 schema_len + at least 1)
  id = u32 from bytes[0..4]
  root_page_id = u64 from bytes[4..12]  ← NEW
  schema_len = bytes[12]
  pos = 13
  ... (rest unchanged, pos offsets shift by 8)
```

Old minimum length was 6 bytes; new minimum is **14 bytes** (4+8+1+0+1+0).

### Step 2 — `CatalogWriter::create_table`

Current code (simplified):
```
alloc table_id
build TableDef { id, schema_name, table_name }
HeapChain::insert into nexus_tables
txn.record_insert
```

New code:
```
alloc table_id
alloc data_root = storage.alloc_page(PageType::Data)
init page: let p = Page::new(PageType::Data, data_root); storage.write_page(data_root, &p)
build TableDef { id, schema_name, table_name, data_root_page_id: data_root }
HeapChain::insert into nexus_tables  (now encodes root_page_id too)
txn.record_insert
```

### Step 3 — `HeapChain::read_row`

```rust
pub fn read_row(
    storage: &dyn StorageEngine,
    page_id: u64,
    slot_id: u16,
) -> Result<Option<Vec<u8>>, DbError> {
    let raw = *storage.read_page(page_id)?.as_bytes();
    let page = Page::from_bytes(raw)?;
    match crate::heap::read_tuple(&page, slot_id)? {
        None => Ok(None),            // dead slot
        Some((_header, data)) => Ok(Some(data.to_vec())),
    }
}
```

### Step 4 — `TableEngine` module

```
// nexusdb-sql/src/table.rs

pub struct TableEngine;

// Private helpers:
fn column_data_types(columns: &[ColumnDef]) -> Vec<DataType>
fn encode_rid(page_id: u64, slot_id: u16) -> [u8; 10]

impl TableEngine {
    scan_table(storage, table_def, columns, snap)
    insert_row(storage, txn, table_def, columns, values)
    delete_row(storage, txn, table_def, columns, record_id)
    update_row(storage, txn, table_def, columns, record_id, new_values)
}
```

#### `scan_table` algorithm

```
column_types = column_data_types(columns)
raw_rows = HeapChain::scan_visible(storage, table_def.data_root_page_id, snap)?
result = []
for (page_id, slot_id, bytes) in raw_rows:
    values = decode_row(&bytes, &column_types)?
    result.push((RecordId { page_id, slot_id }, values))
return Ok(result)
```

#### `insert_row` algorithm

```
if values.len() != columns.len():
    return Err(TypeMismatch)
column_types = column_data_types(columns)
coerced = for each (v, col): coerce(v, col_data_type, Strict)?
encoded = encode_row(&coerced, &column_types)?
txn_id = txn.active_txn_id().ok_or(NoActiveTransaction)?
(page_id, slot_id) = HeapChain::insert(storage, table_def.data_root_page_id, &encoded, txn_id)?
key = encode_rid(page_id, slot_id)
txn.record_insert(table_def.id, &key, &encoded, page_id, slot_id)?
return Ok(RecordId { page_id, slot_id })
```

#### `delete_row` algorithm

```
old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?
    .ok_or(AlreadyDeleted { page_id, slot_id })?
txn_id = txn.active_txn_id().ok_or(NoActiveTransaction)?
HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?
key = encode_rid(record_id.page_id, record_id.slot_id)
txn.record_delete(table_def.id, &key, &old_bytes, record_id.page_id, record_id.slot_id)?
return Ok(())
```

#### `update_row` algorithm

```
old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?
    .ok_or(AlreadyDeleted { page_id, slot_id })?
txn_id = txn.active_txn_id().ok_or(NoActiveTransaction)?
column_types = column_data_types(columns)

// Coerce + encode new values
if new_values.len() != columns.len(): return Err(TypeMismatch)
coerced = for each (v, col): coerce(v, col_data_type, Strict)?
new_encoded = encode_row(&coerced, &column_types)?

// Step 1: mark old row deleted
HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?
old_key = encode_rid(record_id.page_id, record_id.slot_id)
txn.record_delete(table_def.id, &old_key, &old_bytes, record_id.page_id, record_id.slot_id)?

// Step 2: insert new row
(new_page_id, new_slot_id) = HeapChain::insert(storage, table_def.data_root_page_id, &new_encoded, txn_id)?
new_key = encode_rid(new_page_id, new_slot_id)
txn.record_insert(table_def.id, &new_key, &new_encoded, new_page_id, new_slot_id)?

return Ok(RecordId { page_id: new_page_id, slot_id: new_slot_id })
```

#### `encode_rid` helper

```rust
fn encode_rid(page_id: u64, slot_id: u16) -> [u8; 10] {
    let mut buf = [0u8; 10];
    buf[..8].copy_from_slice(&page_id.to_le_bytes());
    buf[8..].copy_from_slice(&slot_id.to_le_bytes());
    buf
}
```

#### `column_data_types` helper

```rust
fn column_data_types(columns: &[ColumnDef]) -> Vec<DataType> {
    columns.iter().map(|c| match c.col_type {
        ColumnType::Bool      => DataType::Bool,
        ColumnType::Int       => DataType::Int,
        ColumnType::BigInt    => DataType::BigInt,
        ColumnType::Float     => DataType::Real,
        ColumnType::Text      => DataType::Text,
        ColumnType::Bytes     => DataType::Bytes,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid      => DataType::Uuid,
    }).collect()
}
```

---

## Implementation order

1. **`schema.rs`** — add field, update `to_bytes`/`from_bytes`, update tests.
   `cargo check -p nexusdb-catalog` must pass.

2. **`writer.rs`** — update `create_table()` to alloc + init the data page.
   `cargo test -p nexusdb-catalog` must pass (existing tests updated).

3. **`heap_chain.rs`** — add `read_row()`.
   `cargo test -p nexusdb-storage` must pass.

4. **`nexusdb-sql/Cargo.toml`** — add `nexusdb-wal = { workspace = true }`.

5. **`table.rs`** — create module with `TableEngine` + helpers.
   `cargo check -p nexusdb-sql` must pass.

6. **`lib.rs`** — add `pub mod table` + re-export.

7. **Write tests** in `table.rs` (unit) and `crates/nexusdb-sql/tests/` (integration).

8. **Full check**: `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### Unit tests in `nexusdb-catalog/src/schema.rs`

```
test_table_def_roundtrip_with_root_page
  — TableDef { id=1, data_root_page_id=42, schema="public", table="users" }
    → to_bytes() → from_bytes() → same struct

test_table_def_minimum_bytes
  — from_bytes with exactly 14 bytes of valid data succeeds

test_table_def_truncated_at_root_page
  — from_bytes with 6 bytes (old minimum) returns Err (now insufficient)

test_table_def_root_page_id_zero
  — data_root_page_id=0 roundtrips correctly (0 is valid in the codec)
```

### Unit tests in `nexusdb-storage/src/heap_chain.rs`

```
test_read_row_live_slot
  — insert a tuple, read_row(page_id, slot_id) returns Some(bytes)

test_read_row_dead_slot
  — insert then delete a tuple, read_row returns None

test_read_row_invalid_slot
  — read_row with slot_id >= num_slots returns Err(InvalidSlot)
```

### Unit tests in `nexusdb-sql/src/table.rs`

```
test_column_data_types_all_variants
  — each ColumnType maps to the expected DataType (8 cases)

test_encode_rid_roundtrip
  — encode_rid(page_id=7, slot_id=3) produces [7,0,0,0,0,0,0,0, 3,0]
```

### Integration tests in `crates/nexusdb-sql/tests/integration_table.rs`

All tests use `MemoryStorage` + `TxnManager::create` in a tempdir.

```
test_table_engine_empty_scan
  — create table, scan → 0 rows

test_table_engine_insert_and_scan
  — insert 3 rows, scan → 3 rows with correct values

test_table_engine_insert_mvcc_visibility
  — insert in txn A, scan with snap taken before commit → 0 rows
  — commit txn A, scan with snap after commit → 1 row

test_table_engine_delete_row
  — insert 2 rows, delete 1, scan → 1 row remaining

test_table_engine_delete_dead_slot_error
  — insert a row, delete it, delete again → Err(AlreadyDeleted)

test_table_engine_update_row
  — insert row with age=30, update to age=31
  — scan → 1 row, values[1] == Int(31)
  — old RecordId is dead (scan doesn't return it)

test_table_engine_update_changes_record_id
  — after update, returned RecordId is valid and has the new row

test_table_engine_coercion_on_insert
  — insert Text("42") into Int column → stored as Int(42) (coercion applied)

test_table_engine_insert_outside_txn_error
  — call insert_row without begin() → Err(NoActiveTransaction)

test_table_engine_scan_respects_snapshot
  — two rows inserted in separate txns; old snapshot sees only first row

test_table_engine_chain_growth
  — insert enough rows to overflow one page; scan returns all rows
```

---

## Anti-patterns to avoid

- **DO NOT** call `txn.record_insert` BEFORE `HeapChain::insert`. WAL entries must
  be written AFTER the heap mutation — crash recovery relies on the physical slot
  existing on disk before its WAL entry can be replayed.
- **DO NOT** call `HeapChain::delete` before reading old bytes. `read_row` must run
  first; after deletion, `read_tuple` returns `None` (dead slot).
- **DO NOT** use `record_update` for `update_row` — it assumes old and new slots are
  on the same page, which is not guaranteed when the page is full.
- **DO NOT** call `column_data_types` inside a hot loop — call it once per operation
  and reuse the result.
- **DO NOT** allow `data_root_page_id = 0` in a live `TableDef`. Page 0 is the meta
  page — writing user data to it would corrupt the database. The test
  `test_table_def_root_page_id_zero` verifies codec correctness but the writer
  must never produce this value.
- **DO NOT** `unwrap()` anywhere in `src/` code.

---

## Risks

| Risk | Mitigation |
|---|---|
| Existing catalog tests use old `TableDef` format (3 fields) | Update all `TableDef { id, schema_name, table_name }` constructors to include `data_root_page_id` — compiler will flag every missing field |
| `from_bytes` truncation check was `< 6`; new minimum is `14` — integration tests with hard-coded bytes will break | Search for literal `TableDef::from_bytes` test cases and update offset math |
| `CatalogWriter::create_table` integration tests don't verify `data_root_page_id` | Add assertion: `table_def.data_root_page_id != 0` in create tests |
| `nexusdb-wal` as regular dep may introduce a build cycle | Verify: `nexusdb-wal` depends on `nexusdb-storage`, `nexusdb-core`; it does NOT depend on `nexusdb-sql` — no cycle |
| `update_row` DELETE+INSERT creates two WAL entries; rollback applies both undo ops | `TxnManager` undo log applies ops in reverse chronological order: `UndoInsert(new)` runs first (kills new row), then `UndoDelete(old)` runs (restores old row) — correct |
