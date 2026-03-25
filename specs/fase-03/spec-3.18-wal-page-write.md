# Spec: 3.18 — WAL Record per Page (PageWrite)

## What to build (not how)

Un nuevo tipo de entrada WAL `PageWrite` que reemplaza N entradas `Insert` por **1 entrada
por página afectada** durante bulk inserts. En lugar de serializar N entries individuales
(una por fila), el executor emite una entry compacta por página que contiene:

1. El `page_id` en `key`.
2. La cantidad de slots insertados por la transacción en esa página.
3. Los `slot_ids` de esas filas, para UNDO en crash recovery.

La entry **no** almacena bytes completos de la página. En la implementación actual
`PageWrite` sirve para compactar WAL y para UNDO de inserts no commiteados; el REDO
de páginas commiteadas sigue diferido.

Esto reduce el trabajo CPU de serialización WAL de O(N filas) a O(P páginas),
donde P ≈ N/200 para páginas de 16KB con filas típicas pequeñas.

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
new_val_len     4B LE   2 + num_slots × 2
new_value:
  [0..2]                num_slots: u16 LE — number of slots inserted by this txn
  [2..2+N×2]            slot_id_i: u16 LE × num_slots — which slots were inserted
crc32c          4B LE   checksum of all preceding bytes in this entry
entry_len_2     4B LE   total entry length (for backward scan)
```

**Total entry size** for una página con 200 filas insertadas:
`43B header/trailer + 8B key + 4B new_val_len + 2B num_slots + 200×2B = ~457B`

vs. 200 Insert entries: `200 × ~100B = ~20KB` → PageWrite es **mucho más pequeño**
porque evita repetir key/row bytes por fila.

---

## Inputs / Outputs

### New: `TxnManager::record_page_writes`

```rust
pub fn record_page_writes(
    &mut self,
    table_id: u32,
    page_writes: &[(u64, &[u16])],
    // (page_id, &[slot_id]) — one tuple per affected page
) -> Result<(), DbError>
```

- Emits one `PageWrite` WAL entry per element of `page_writes`
- Uses `reserve_lsns(page_writes.len())` + `write_batch()` — O(1) write_all
- Enqueues `UndoOp::UndoInsert { page_id, slot_id }` per slot (same as today, for in-process rollback)
- Returns `Ok(())` on success; `DbError::NoActiveTransaction` if no txn active

### Modified: `TableEngine::insert_rows_batch` (table.rs)

Replaces `txn.record_insert_batch()` with:
1. Group `phys_locs` by `page_id` → `HashMap<page_id, Vec<slot_id>>`
2. Build `(page_id, slot_ids)` tuples
3. Call `txn.record_page_writes(table_id, &page_writes)`

### Modified: `CrashRecovery::recover` (recovery.rs)

Adds handling for `EntryType::PageWrite`:
- On uncommitted txn: for each `(page_id, slot_id)` embedded in `new_value`: push `RecoveryOp::Insert { page_id, slot_id }`
- Undo execution: same as existing `RecoveryOp::Insert` → `mark_slot_dead(page_id, slot_id)`
- Redo of committed page images is **not** provided by this format and remains deferred

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
    slot_ids = entry.new_value[2..] decoded as []u16
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

- REDO of committed page images (power failure recovery) — deferred
- PageWrite for UPDATE or DELETE — only applies to batch INSERT
- Index maintenance WAL entries — secondary index entries remain per-row (separate B-Tree writes)
- Compression is unnecessary in the current compact slot-list format

---

## Dependencies

- `EntryType::Truncate = 8` already exists — `PageWrite = 9` does not conflict
- `WalWriter::reserve_lsns()` + `write_batch()` ✅ exist (Phase 3.17 infrastructure)
- `TxnManager::wal_scratch: Vec<u8>` ✅ exists — reused as accumulator for PageWrite serialization
- `HeapChain::insert_batch()` ✅ already writes pages to storage — no changes needed
- `key` must remain exactly 8 bytes (`page_id` as u64 LE)
- Crash recovery `RecoveryOp::Insert` already handles `mark_slot_dead` — no new undo logic needed

---

## ⚠️ DEFERRED

- Full committed-page redo still requires a separate page-image strategy
- Using PageWrite for partial-page inserts outside batch context → out of scope

---

✅ Spec written. You can now switch to `/effort high` for the Plan phase.
