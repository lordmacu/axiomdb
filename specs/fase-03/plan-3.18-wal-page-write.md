# Plan: 3.18 — WAL Record per Page (PageWrite)

## Files to create/modify

| Acción | Archivo | Qué cambia |
|---|---|---|
| Modify | `crates/axiomdb-wal/src/entry.rs` | Añade `EntryType::PageWrite = 9` |
| Modify | `crates/axiomdb-wal/src/txn.rs` | Añade `record_page_writes()` |
| Modify | `crates/axiomdb-wal/src/recovery.rs` | Maneja `EntryType::PageWrite` en scan + undo |
| Modify | `crates/axiomdb-sql/src/table.rs` | `insert_rows_batch()` usa `record_page_writes()` |
| Modify | `crates/axiomdb-wal/tests/integration_group_commit.rs` | Añade tests PageWrite |
| Modify | `docs-site/src/internals/wal.md` | Documenta EntryType::PageWrite |

---

## Algoritmo — `record_page_writes`

```rust
pub fn record_page_writes(
    &mut self,
    table_id: u32,
    // (page_id, post-modification page bytes, slot_ids inserted on this page)
    page_writes: &[(u64, &[u8; PAGE_SIZE], &[u16])],
) -> Result<(), DbError> {
    let n = page_writes.len();
    if n == 0 { return Ok(()); }

    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    let lsn_base = self.wal.reserve_lsns(n);
    self.wal_scratch.clear();

    for (i, (page_id, page_bytes, slot_ids)) in page_writes.iter().enumerate() {
        // key = page_id as 8 bytes LE
        let key: [u8; 8] = page_id.to_le_bytes();

        // new_value = [page_bytes: PAGE_SIZE][num_slots: u16 LE][slot_id × N: u16 LE]
        let mut new_value = Vec::with_capacity(PAGE_SIZE + 2 + slot_ids.len() * 2);
        new_value.extend_from_slice(page_bytes.as_slice());
        new_value.extend_from_slice(&(slot_ids.len() as u16).to_le_bytes());
        for &slot_id in slot_ids.iter() {
            new_value.extend_from_slice(&slot_id.to_le_bytes());
        }

        let entry = WalEntry::new(
            lsn_base + i as u64,
            txn_id,
            EntryType::PageWrite,
            table_id,
            key.to_vec(),
            vec![],       // old_value empty
            new_value,
        );
        entry.serialize_into(&mut self.wal_scratch);
    }

    self.wal.write_batch(&self.wal_scratch)?;

    // Enqueue undo ops (same as today — in-process rollback uses these)
    for (page_id, _page_bytes, slot_ids) in page_writes {
        for &slot_id in slot_ids.iter() {
            active.undo_ops.push(UndoOp::UndoInsert {
                page_id: *page_id,
                slot_id,
            });
        }
    }

    Ok(())
}
```

---

## Algoritmo — `insert_rows_batch` wire-up (table.rs)

```rust
// ANTES (Phase 3.17):
txn.record_insert_batch(table_def.id, &phys_locs, &encoded_rows)?;

// DESPUÉS (Phase 3.18):
// 1. Group phys_locs by page_id → HashMap<page_id, Vec<slot_id>>
let mut page_slot_map: HashMap<u64, Vec<u16>> = HashMap::new();
for &(page_id, slot_id) in &phys_locs {
    page_slot_map.entry(page_id).or_default().push(slot_id);
}

// 2. Read final page bytes for each affected page (mmap cache hit)
//    Sort by page_id for deterministic WAL order
let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);

let mut page_write_args: Vec<(u64, [u8; PAGE_SIZE], Vec<u16>)> =
    Vec::with_capacity(sorted_pages.len());
for (page_id, slot_ids) in sorted_pages {
    let page_bytes = *storage.read_page(page_id)?.as_bytes();
    page_write_args.push((page_id, page_bytes, slot_ids));
}

// 3. Emit one PageWrite WAL entry per affected page
let pw_refs: Vec<(u64, &[u8; PAGE_SIZE], &[u16])> = page_write_args
    .iter()
    .map(|(pid, bytes, slots)| (*pid, bytes, slots.as_slice()))
    .collect();
txn.record_page_writes(table_def.id, &pw_refs)?;
```

---

## Algoritmo — crash recovery handling (recovery.rs)

Add arm in the `match entry.entry_type` block:

```rust
EntryType::PageWrite => {
    if let Some(ops) = in_progress.get_mut(&entry.txn_id) {
        // new_value = [page_bytes: PAGE_SIZE][num_slots: u16][slot_id × N: u16]
        let page_id = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0;8]));
        if entry.new_value.len() >= PAGE_SIZE + 2 {
            let num_slots = u16::from_le_bytes([
                entry.new_value[PAGE_SIZE],
                entry.new_value[PAGE_SIZE + 1],
            ]) as usize;
            let slots_bytes = &entry.new_value[PAGE_SIZE + 2..];
            for i in 0..num_slots {
                let offset = i * 2;
                if offset + 2 <= slots_bytes.len() {
                    let slot_id = u16::from_le_bytes([
                        slots_bytes[offset],
                        slots_bytes[offset + 1],
                    ]);
                    ops.push(RecoveryOp::Insert { page_id, slot_id });
                }
            }
        }
    }
}
```

No other changes to recovery needed — `RecoveryOp::Insert` → `mark_slot_dead` is already handled.

---

## Fases de implementación

### Fase 1 — entry.rs (5 min)
1. Add `PageWrite = 9` to `EntryType`
2. Add `9 => Ok(Self::PageWrite)` to `TryFrom<u8>`
3. Run `cargo test -p axiomdb-wal` — entry round-trip tests pass

### Fase 2 — txn.rs: `record_page_writes` (20 min)
1. Add method following pseudocode above
2. Import `PAGE_SIZE` from `axiomdb_storage`
3. Unit test: `test_record_page_writes_wal_entries_parseable`
   - Write N rows to a page, call record_page_writes
   - Read WAL back with WalReader, verify entry type = PageWrite, verify new_value[0..PAGE_SIZE] matches page bytes

### Fase 3 — recovery.rs: handle PageWrite (15 min)
1. Add `EntryType::PageWrite` arm in `recover()`
2. Add same arm in `is_needed()` (treat same as Insert — irrelevant for is_needed but must not panic)
3. Integration test:
   - Begin txn, insert N rows (batch), crash without commit
   - Recover, verify all slots are dead

### Fase 4 — table.rs: wire-up (15 min)
1. Replace `record_insert_batch` call with the groupby + `record_page_writes` logic
2. Add `use std::collections::HashMap;` import if needed
3. `cargo test --workspace` to confirm nothing breaks

### Fase 5 — benchmark (5 min)
1. Run `cargo bench --bench executor_e2e -- insert_multi_row`
2. Report comparison vs 3.17 baseline (211K/s)

---

## Tests a escribir

### Unit test — entry roundtrip
```
test_page_write_entry_roundtrip:
  Construct WalEntry { entry_type: PageWrite, key: page_id_bytes,
    new_value: page_bytes ++ num_slots ++ slot_ids }
  → to_bytes() → from_bytes() → verify all fields equal
  → verify entry_type == PageWrite
```

### Integration test — crash recovery with PageWrite
```
test_crash_recovery_undoes_uncommitted_page_write:
  1. Create fresh DB + storage page
  2. TxnManager with deferred mode OFF
  3. BEGIN txn
  4. Insert N rows via insert_batch → get phys_locs
  5. Group phys_locs → read page bytes → record_page_writes()
  6. Do NOT commit — drop TxnManager (simulate crash)
  7. Reopen with TxnManager::open_with_recovery()
  8. Verify: for each (page_id, slot_id) in phys_locs → slot is dead
  9. Verify: recovery result shows 1 undone txn
```

### Integration test — committed PageWrite survives restart
```
test_committed_page_write_rows_visible_after_restart:
  1. BEGIN → insert N rows → record_page_writes() → COMMIT (fsync)
  2. Drop TxnManager (clean shutdown)
  3. Reopen with TxnManager::open() (no recovery needed)
  4. Scan heap → verify N rows visible with correct data
```

---

## Anti-patterns a evitar

- **NO olvidar el arm en `is_needed()`** — si PageWrite no está manejado, `is_needed()`
  podría devolver `false` incorrectamente cuando hay PageWrite en una txn in-progress
- **NO usar `unwrap()` en el parsing de new_value en recovery** — usar `try_into` con
  fallback o early-return silencioso si el slice es demasiado corto (legacy entries)
- **NO asumir que new_value.len() == PAGE_SIZE + 2 + N*2 exactamente** — validar bounds
  antes de parsear slot_ids para que un entry corrupto no cause panic en recovery
- **NO cambiar HeapChain::insert_batch** — obtener page bytes con read_page() después del batch
- **NO olvidar ordenar pages por page_id** — el orden determinístico hace los tests reproducibles

---

## Risks

| Riesgo | Mitigación |
|---|---|
| `read_page()` después de `insert_batch()` devuelve bytes stale (pre-inserción) | No aplica: mmap storage — `write_page()` actualiza el buffer mmap in-place; `read_page()` lee del mismo buffer → datos frescos garantizados |
| WAL entry de 16KB+ más grande que BufWriter capacity (64KB) | No aplica: 64KB / ~16.8KB = 3+ entries por flush; para batches de >3 páginas el BufWriter va a hacer varios flushes automáticamente — correcto |
| Tests de durabilidad existentes fallan porque esperan Insert entries | Falso: los tests de durabilidad verifican comportamiento observable (row visibilidad), no el tipo de WAL entry |
| Recovery marca mal los slots si el slice new_value está corrupto | Mitigado: validar `new_value.len() >= PAGE_SIZE + 2` antes de parsear; slots malformados se ignoran silenciosamente |
