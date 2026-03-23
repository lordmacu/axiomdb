# Plan: 3.12 — CatalogReader / CatalogWriter

## Files to create / modify

| File | Action | Description |
|---|---|---|
| `crates/axiomdb-storage/src/heap_chain.rs` | CREATE | `HeapChain`: multi-page insert, delete, scan |
| `crates/axiomdb-storage/src/meta.rs` | MODIFY | Add `alloc_table_id`, `alloc_index_id`, sequence constants |
| `crates/axiomdb-storage/src/lib.rs` | MODIFY | Re-export `HeapChain` |
| `crates/axiomdb-catalog/src/bootstrap.rs` | MODIFY | `init()` writes sequence initial values (1, 1) |
| `crates/axiomdb-catalog/src/writer.rs` | CREATE | `CatalogWriter` |
| `crates/axiomdb-catalog/src/reader.rs` | CREATE | `CatalogReader` |
| `crates/axiomdb-catalog/src/lib.rs` | MODIFY | Add writer/reader modules + re-exports |
| `crates/axiomdb-catalog/Cargo.toml` | MODIFY | Add `axiomdb-wal` dependency |
| `crates/axiomdb-core/src/error.rs` | MODIFY | Add `TableAlreadyExists`, `TableNotFound`, `IndexNotFound` |
| `crates/axiomdb-storage/src/meta.rs` | MODIFY | Add sequence body offsets |
| `crates/axiomdb-catalog/tests/integration_catalog_rw.rs` | CREATE | Integration tests |

---

## Algorithm / Data structures

### A — HeapChain layout in `_reserved`

`PageHeader._reserved: [u8; 28]` — first 8 bytes encode `next_page_id`:

```
_reserved[0..8] = next_page_id: u64 LE   (0 = end of chain)
_reserved[8..28] = unused (zero)
```

Helper functions (not methods — stateless utilities):

```rust
pub fn chain_next_page(page: &Page) -> u64 {
    let hdr = page.header();
    u64::from_le_bytes(hdr._reserved[0..8].try_into().unwrap())
}

pub fn chain_set_next_page(page: &mut Page, next: u64) {
    page.header_mut()._reserved[0..8].copy_from_slice(&next.to_le_bytes());
}
```

### B — HeapChain::insert algorithm

```
fn insert(storage, root_page_id, data, txn_id):
    current_page_id = root_page_id
    loop:
        page_bytes = storage.read_page(current_page_id)?
        page = Page::from_bytes(page_bytes)?
        next = chain_next_page(&page)

        if next == 0:
            // This is the last page. Try to insert here.
            match insert_tuple(&mut page, data, txn_id):
                Ok(slot_id):
                    page.update_checksum()
                    storage.write_page(current_page_id, &page)?
                    return Ok((current_page_id, slot_id))
                Err(HeapPageFull):
                    // Allocate new page, chain it, retry on new page.
                    new_page_id = storage.alloc_page(PageType::Data)?
                    new_page = Page::new(PageType::Data, new_page_id)
                    // Insert into new page (guaranteed to fit — it's empty).
                    slot_id = insert_tuple(&mut new_page, data, txn_id)?
                    new_page.update_checksum()
                    // Update chain pointer in previous last page.
                    // Re-read page (ownership / borrow constraints).
                    page_bytes2 = storage.read_page(current_page_id)?
                    let mut prev_page = Page::from_bytes(page_bytes2)?
                    chain_set_next_page(&mut prev_page, new_page_id)
                    prev_page.update_checksum()
                    // Write new page first, then update chain pointer.
                    storage.write_page(new_page_id, &new_page)?
                    storage.write_page(current_page_id, &prev_page)?
                    return Ok((new_page_id, slot_id))
        else:
            current_page_id = next
```

**Write order** (crash safety): write the new page fully before updating the
chain pointer in the previous page. If we crash after writing the new page
but before updating the pointer, the new page is orphaned (unreachable) but
the chain is intact. Orphaned pages are recovered by a future VACUUM.
If we crash after updating the pointer, recovery replays the WAL insert
which lands on the already-written new page correctly.

### C — HeapChain::scan_visible algorithm

```
fn scan_visible(storage, root_page_id, snap) -> Vec<(page_id, slot_id, data)>:
    result = []
    current = root_page_id
    while current != 0:
        page = storage.read_page(current)?
        next = chain_next_page(page)
        for (slot_id, data) in heap::scan_visible(page, snap):
            result.push((current, slot_id, data.to_vec()))
        current = next
    return result
```

### D — Sequence allocation

```rust
// In meta.rs:
pub const NEXT_TABLE_ID_BODY_OFFSET: usize = 64;
pub const NEXT_INDEX_ID_BODY_OFFSET: usize = 68;

fn alloc_sequence_u32(storage, body_offset) -> Result<u32>:
    raw = *storage.read_page(0)?.as_bytes()
    mut page = Page::from_bytes(raw)?
    off = HEADER_SIZE + body_offset
    current = u32::from_le_bytes(raw[off..off+4])
    if current == 0:
        return Err(DbError::CatalogNotInitialized)
    next = current.checked_add(1).ok_or(overflow error)?
    page.as_bytes_mut()[off..off+4].copy_from_slice(&next.to_le_bytes())
    page.update_checksum()
    storage.write_page(0, &page)?
    return Ok(current)
```

`CatalogBootstrap::init` must write `1` to both offsets if `catalog_schema_ver == 0`.
Reading back `0` means the catalog was never initialized → `CatalogNotInitialized`.

### E — CatalogWriter::create_table

```
fn create_table(schema, name) -> Result<TableId>:
    table_id = meta::alloc_table_id(self.storage)?
    def = TableDef { id: table_id, schema_name: schema.to_string(), table_name: name.to_string() }
    data = def.to_bytes()
    (page_id, slot_id) = HeapChain::insert(self.storage, self.page_ids.tables, &data, active_txn_id)?
    key = table_id.to_le_bytes()
    self.txn.record_insert(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?
    return Ok(table_id)
```

Active txn_id is retrieved via `self.txn.active_txn_id()` — a new accessor
that returns `Err(NoActiveTransaction)` if no txn is open.

### F — CatalogWriter::delete_table

```
fn delete_table(table_id) -> Result<()>:
    // Delete from axiom_tables
    snap = self.txn.active_snapshot()
    rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?
    for (page_id, slot_id, data) in rows:
        def = TableDef::from_bytes(&data)?
        if def.id == table_id:
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?
            key = table_id.to_le_bytes()
            self.txn.record_delete(SYSTEM_TABLE_TABLES, &key, &data, page_id, slot_id)?

    // Delete matching columns from axiom_columns
    rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?
    for (page_id, slot_id, data) in rows:
        def = ColumnDef::from_bytes(&data)?
        if def.table_id == table_id:
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?
            self.txn.record_delete(SYSTEM_TABLE_COLUMNS, &table_id.to_le_bytes(), &data, page_id, slot_id)?

    // Delete matching indexes from axiom_indexes
    rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?
    for (page_id, slot_id, data) in rows:
        def = IndexDef::from_bytes(&data)?
        if def.table_id == table_id:
            HeapChain::delete(self.storage, page_id, slot_id, txn_id)?
            self.txn.record_delete(SYSTEM_TABLE_INDEXES, &table_id.to_le_bytes(), &data, page_id, slot_id)?

    return Ok(())
```

### G — active_txn_id accessor in TxnManager

```rust
// In txn.rs — add to TxnManager:
pub fn active_txn_id(&self) -> Result<TxnId, DbError> {
    self.active
        .as_ref()
        .map(|a| a.txn_id)
        .ok_or(DbError::NoActiveTransaction)
}

pub fn active_snapshot(&self) -> Result<TransactionSnapshot, DbError> {
    let a = self.active.as_ref().ok_or(DbError::NoActiveTransaction)?;
    Ok(TransactionSnapshot::active(a.txn_id, a.snapshot_id_at_begin - 1))
}
```

### H — CatalogWriter::new

```rust
pub fn new(storage: &'a mut dyn StorageEngine, txn: &'a mut TxnManager)
    -> Result<Self, DbError>
{
    if !CatalogBootstrap::is_initialized(storage)? {
        return Err(DbError::CatalogNotInitialized);
    }
    let page_ids = CatalogBootstrap::page_ids(storage)?;
    Ok(Self { storage, txn, page_ids })
}
```

### I — CatalogReader::list_columns (order guarantee)

After collecting all visible `ColumnDef` rows, sort by `col_idx`:

```rust
let mut cols: Vec<ColumnDef> = scan_visible(...)
    .filter(|def| def.table_id == table_id)
    .collect();
cols.sort_by_key(|c| c.col_idx);
```

---

## Implementation phases

### Phase 1 — Sequences (meta.rs + bootstrap.rs)

1. Add `NEXT_TABLE_ID_BODY_OFFSET = 64`, `NEXT_INDEX_ID_BODY_OFFSET = 68`
   to `meta.rs`.
2. Add `read_meta_u32_at` helper (or reuse `read_meta_u32`).
3. Implement `alloc_table_id(storage)` and `alloc_index_id(storage)` in `meta.rs`.
4. Modify `CatalogBootstrap::init()`: after writing `catalog_schema_ver = 1`,
   also write `next_table_id = 1` and `next_index_id = 1` if not already set.
5. Add `write_meta_u32` helper to `meta.rs`.
6. Unit tests: `alloc_table_id` increments monotonically; returns error if
   `CatalogNotInitialized`; persists across storage reuse.

### Phase 2 — HeapChain (heap_chain.rs)

1. Implement `chain_next_page` and `chain_set_next_page` in `heap_chain.rs`.
   Compile-time assert: `_reserved` is at least 8 bytes.
2. Implement `HeapChain::insert` (see algorithm B).
3. Implement `HeapChain::delete` (thin wrapper over `heap::delete_tuple` +
   `storage.write_page`).
4. Implement `HeapChain::scan_visible` (see algorithm C).
5. Re-export from `axiomdb-storage/src/lib.rs`.
6. Unit tests:
   - Insert into root page → found in scan.
   - Insert until `HeapPageFull` → chain grows, second page exists.
   - All rows visible across pages in scan.
   - Deleted row not visible.

### Phase 3 — TxnManager accessors

1. Add `active_txn_id(&self) -> Result<TxnId>` to `TxnManager`.
2. Add `active_snapshot(&self) -> Result<TransactionSnapshot>` to `TxnManager`.
3. Unit tests for both: error when no active txn, correct values when txn exists.

### Phase 4 — CatalogWriter

1. Create `axiomdb-catalog/src/writer.rs`.
2. Add `axiomdb-wal` to `axiomdb-catalog/Cargo.toml`.
3. Define `SYSTEM_TABLE_TABLES`, `SYSTEM_TABLE_COLUMNS`, `SYSTEM_TABLE_INDEXES`
   as `pub const u32` in `writer.rs` (or a new `constants.rs`).
4. Implement `CatalogWriter::new`, `create_table`, `create_column`,
   `create_index`, `delete_table`, `delete_index`.
5. Each write: insert into heap → WAL record_insert/delete.
6. Add to `lib.rs`.

### Phase 5 — CatalogReader

1. Create `axiomdb-catalog/src/reader.rs`.
2. Implement `CatalogReader::new`, `get_table`, `get_table_by_id`,
   `list_tables`, `list_columns` (sorted by col_idx), `list_indexes`.
3. All reads use `HeapChain::scan_visible` with provided snapshot.
4. Add to `lib.rs`.

### Phase 6 — New DbError variants

1. Add `TableAlreadyExists`, `TableNotFound`, `IndexNotFound` to
   `axiomdb-core/src/error.rs`.

### Phase 7 — Integration tests

File: `crates/axiomdb-catalog/tests/integration_catalog_rw.rs`

Tests (using `MemoryStorage` + `TxnManager::create` with a temp WAL path):

```
test_create_and_get_table
test_create_column_list_ordered
test_create_index_get_index
test_delete_table_cascade_columns_indexes
test_delete_index
test_snapshot_isolation_create
test_snapshot_isolation_delete
test_rollback_create_table
test_multi_page_chain_insert_and_scan
test_sequence_persistence_across_reopen  (uses MmapStorage + tempfile)
test_sequence_no_collision_after_reopen
test_catalog_not_initialized_errors
```

---

## Tests to write

### Unit (fast, MemoryStorage)
- `alloc_table_id`: monotonically increasing (3 calls → 1, 2, 3)
- `alloc_index_id`: same
- `chain_next_page` / `chain_set_next_page`: roundtrip via write_page
- `HeapChain::insert`: single page case
- `HeapChain::insert`: multi-page case (fill page, chain grows)
- `HeapChain::scan_visible`: empty chain → empty result
- `HeapChain::scan_visible`: deleted row invisible to newer snapshot
- `TxnManager::active_txn_id`: Ok when active, Err when not
- `TxnManager::active_snapshot`: snapshot_id and current_txn_id correct

### Integration (may use MmapStorage + tempfile)
- `test_create_and_get_table`: create → commit → read → found
- `test_snapshot_isolation_create`: snapshot before commit → not found
- `test_rollback_create_table`: begin → create → rollback → not found
- `test_multi_page_chain_insert_and_scan`: fill root page + overflow
- `test_sequence_persistence_across_reopen`: sequences survive close/reopen
- `test_delete_table_cascade_columns_indexes`

---

## Anti-patterns to avoid

- **DO NOT** call `alloc_table_id` outside an active transaction — the ID is
  allocated but the row might never be written if the txn is never opened.
  The caller must ensure `begin()` is called before `CatalogWriter` is used.
  (Enforced by `record_insert` which requires `NoActiveTransaction`.)

- **DO NOT** update the chain pointer before writing the new page — see
  write order in algorithm B (crash safety invariant).

- **DO NOT** use `scan_all` (bypassing MVCC) in `CatalogWriter::delete_table` —
  use `active_snapshot()` to see the writer's own uncommitted inserts; otherwise
  a table created in the same transaction and immediately dropped would not be found.

- **DO NOT** sort columns in `scan_visible` — sorting happens in `list_columns`
  only (scan returns rows in insertion order, which may not match col_idx order).

- **DO NOT** add `unwrap()` in `src/` — propagate all errors with `?`.

---

## Risks

| Risk | Mitigation |
|---|---|
| `PageHeader._reserved` interpretation differs from heap.rs usage | Confirm `insert_tuple` does not touch `_reserved`; add assertion test |
| Borrow checker: `storage` borrowed twice (read chain + write slot) | Use copy-in/copy-out pattern: `*storage.read_page()?.as_bytes()` to clone, then `write_page` |
| `TxnManager` borrow conflict: `&mut self.txn` and `&mut self.storage` in same struct | Caller passes both separately; `CatalogWriter` holds both `&'a mut` with non-overlapping lifetimes |
| Sequence overflow (u32::MAX tables) | `checked_add(1).ok_or(DbError::Overflow)` — add `Overflow` variant to DbError if not present |
| WAL file path needed for integration tests with MmapStorage | Use `tempfile::tempdir()` for both storage path and WAL path |

---

## Dependency graph

```
axiomdb-core          (DbError, TransactionSnapshot, TxnId, TableId)
    ↑
axiomdb-storage       (StorageEngine, Page, heap_chain, meta)
    ↑
axiomdb-wal           (TxnManager)
    ↑
axiomdb-catalog       (CatalogBootstrap, schema, writer, reader)
```

No circular dependencies. `axiomdb-catalog` adds `axiomdb-wal` to its
`[dependencies]` in Cargo.toml.
