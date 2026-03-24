# Spec: 3.18 — WAL Record per Page (PageWrite)

## What to build (not how)

Un nuevo tipo de entrada WAL `PageWrite` que reemplaza N entradas `Insert` por **1 entrada
por página afectada** durante bulk inserts. En lugar de serializar N entries individuales
(una por fila), el executor emite una entry por página que contiene:

1. Los bytes completos de la página post-inserción (para REDO futuro — Phase 3.8b).
2. Los slot_ids de las filas insertadas en esa página (para UNDO en crash recovery).

Esto reduce el trabajo CPU de serialización WAL de O(N filas) a O(P páginas),
donde P ≈ N/200 para páginas de 8KB con filas típicas de ~40B.

**La semántica de durabilidad, MVCC y crash recovery es idéntica al path actual.**
El cambio es solo en la representación WAL — el comportamiento observable no cambia.

---

## Binary format — PageWrite entry

```text
Field           Size    Value
─────────────── ─────   ──────────────────────────────────────────────
entry_type      1B      9 (PageWrite)
table_id        4B LE   table identifier
key_len         2B LE   8
key[0..8]       8B      page_id as u64 LE
old_val_len     4B LE   0 (empty — no pre-image stored)
new_val_len     4B LE   PAGE_SIZE + 2 + num_slots × 2
new_value:
  [0..PAGE_SIZE]        full post-modification page bytes (8192B for 8KB pages)
  [PAGE_SIZE..+2]       num_slots: u16 LE — number of slots inserted by this txn
  [+2..+2+N×2]         slot_id_i: u16 LE × num_slots — which slots were inserted
crc32c          4B LE   checksum of all preceding bytes in this entry
entry_len_2     4B LE   total entry length (for backward scan)
```

**Total entry size** for a full page (200 rows):
`43B header/trailer + 8B key + 4B new_val_len + 8192B page + 2B num_slots + 200×2B = ~8.7KB`

vs. 200 Insert entries: `200 × ~100B = ~20KB` → PageWrite **≈ 2.3× smaller** for full pages.

---

## Inputs / Outputs

### New: `TxnManager::record_page_writes`

```rust
pub fn record_page_writes(
    &mut self,
    table_id: u32,
    page_writes: &[(u64, &[u8; PAGE_SIZE], &[u16])],
    // (page_id, page_bytes, &[slot_id]) — one tuple per affected page
) -> Result<(), DbError>
```

- Emits one `PageWrite` WAL entry per element of `page_writes`
- Uses `reserve_lsns(page_writes.len())` + `write_batch()` — O(1) write_all
- Enqueues `UndoOp::UndoInsert { page_id, slot_id }` per slot (same as today, for in-process rollback)
- Returns `Ok(())` on success; `DbError::NoActiveTransaction` if no txn active

### Modified: `TableEngine::insert_rows_batch` (table.rs)

Replaces `txn.record_insert_batch()` with:
1. Group `phys_locs` by `page_id` → `HashMap<page_id, Vec<slot_id>>`
2. For each unique `page_id`: read final page bytes from storage (mmap cache hit)
3. Call `txn.record_page_writes(table_id, &page_writes)`

### Modified: `CrashRecovery::recover` (recovery.rs)

Adds handling for `EntryType::PageWrite`:
- On uncommitted txn: for each `(page_id, slot_id)` embedded in `new_value`: push `RecoveryOp::Insert { page_id, slot_id }`
- Undo execution: same as existing `RecoveryOp::Insert` → `mark_slot_dead(page_id, slot_id)`
- No redo pass needed (deferred to Phase 3.8b)

### New: `EntryType::PageWrite = 9` (entry.rs)

---

## Use cases

### 1. Bulk INSERT — 10K rows, no secondary indexes

```
"INSERT INTO t VALUES (r1),(r2),...,(r10000)"
→ HeapChain::insert_batch() writes ~50 pages to storage
→ Group phys_locs by page_id: 50 unique pages
→ For each page: read_page() (mmap cache hit, ~100ns)
→ record_page_writes(): reserve_lsns(50) + 50×serialize_into() + 1 write_batch()
→ WAL: 50 PageWrite entries vs 10K Insert entries (200× fewer)
```

### 2. Process crash — uncommitted PageWrite txn

```
Session: BEGIN → insert_rows_batch() → crash (no COMMIT)
→ Recovery scans WAL: sees Begin + PageWrite entries, no Commit
→ For each PageWrite entry:
    page_id = entry.key[0..8]
    slot_ids = entry.new_value[PAGE_SIZE+2..] decoded as []u16
    for each slot_id: push RecoveryOp::Insert { page_id, slot_id }
→ Undo phase: for each RecoveryOp::Insert: mark_slot_dead(page_id, slot_id)
→ Result: all inserted rows hidden from future snapshots ✅
```

### 3. Committed bulk INSERT — restart without crash

```
Session: BEGIN → insert_rows_batch() → COMMIT (WAL fsynced)
→ No recovery needed — mmap pages are in OS page cache, available after restart
→ CrashRecovery::is_needed() returns false → skip recovery
→ All rows visible ✅
```

### 4. Single-row INSERT — unchanged

```
"INSERT INTO t VALUES (1, 'a')"
→ insert_row() (not insert_rows_batch)
→ txn.record_insert() → WalEntry::Insert (unchanged path)
→ PageWrite NOT used for single-row inserts
```

### 5. Rollback of uncommitted PageWrite txn

```
Session: BEGIN → insert_rows_batch() → ROLLBACK
→ txn.rollback() applies undo_ops in reverse:
    UndoInsert { page_id, slot_id } × N → mark_slot_dead() × N
→ Same as today — in-memory undo_ops are unaffected
→ All inserted rows hidden ✅
```

### 6. Recovery idempotency

```
Recovery run 1: mark_slot_dead(page_5, slot_0) → Ok
Recovery run 2: mark_slot_dead(page_5, slot_0) → AlreadyDeleted → Ok (silently skipped)
→ Safe to run multiple times ✅
```

---

## Acceptance criteria

- [ ] `EntryType::PageWrite = 9` exists and round-trips through `WalEntry::from_bytes`
- [ ] `record_page_writes()` produces WAL entries that can be read by `WalReader::scan_forward`
- [ ] Crash recovery correctly undoes an uncommitted bulk INSERT using PageWrite entries
  (same observable result as using Insert entries)
- [ ] Committed bulk INSERT survives restart with all rows visible
- [ ] Single-row INSERT path is unchanged (still emits WalEntry::Insert)
- [ ] INSERT path with secondary indexes is unchanged (still uses record_insert per row)
- [ ] `cargo test --workspace` passes (all existing crash recovery tests still pass)
- [ ] Benchmark `bench_insert_multi_row/10K` shows ≥ 2× improvement over the 3.17 baseline
  (211K/s → expected ≥ 420K/s)

---

## Out of scope

- REDO of committed PageWrite transactions (power failure recovery) — deferred to Phase 3.8b
- PageWrite for UPDATE or DELETE — only applies to batch INSERT
- Index maintenance WAL entries — secondary index entries remain per-row (separate B-Tree writes)
- Compression of page bytes in new_value — deferred post-Phase 8

---

## Dependencies

- `EntryType::Truncate = 8` already exists — `PageWrite = 9` does not conflict
- `WalWriter::reserve_lsns()` + `write_batch()` ✅ exist (Phase 3.17 infrastructure)
- `TxnManager::wal_scratch: Vec<u8>` ✅ exists — reused as accumulator for PageWrite serialization
- `HeapChain::insert_batch()` ✅ already writes pages to storage — no changes needed
- `Page::as_bytes()` must return `&[u8; PAGE_SIZE]` — verify this is the case
- Crash recovery `RecoveryOp::Insert` already handles `mark_slot_dead` — no new undo logic needed

---

## ⚠️ DEFERRED

- REDO pass for PageWrite committed entries (power failure durability) → Phase 3.8b
- Using PageWrite for partial-page inserts outside batch context → out of scope

---

✅ Spec written. You can now switch to `/effort high` for the Plan phase.
