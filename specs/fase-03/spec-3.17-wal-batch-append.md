# Spec: 3.17 — WAL Batch Append

## What to build (not how)

`TxnManager::record_insert_batch()` — escribe N WAL Insert entries en **una sola llamada**
a `WalWriter::write_batch()`, en lugar de N llamadas individuales a `append_with_buf()`.

La infraestructura ya existe en `WalWriter` (`reserve_lsns`, `write_batch`). El cambio
es solo en `TxnManager`: acumular todos los entries en un `Vec<u8>` y vaciarlos de una
vez, en lugar de hacerlo fila a fila.

El comportamiento observable (durabilidad, crash recovery, MVCC) es **idéntico** al
actual. Solo cambia el número de llamadas internas a `BufWriter::write_all`.

---

## Inputs / Outputs

### `TxnManager::record_insert_batch`

```rust
pub fn record_insert_batch(
    &mut self,
    table_id: u32,
    phys_locs: &[(u64, u16)],   // (page_id, slot_id) por fila — output de HeapChain::insert_batch
    values: &[Vec<u8>],         // fila encoded por fila — mismo orden que phys_locs
) -> Result<(), DbError>
```

- Input: `table_id`, slice de `(page_id, slot_id)`, slice de bytes encoded por fila
- `phys_locs.len() == values.len()` — precondición; si no se cumple: `DbError::Internal`
- Output: `Ok(())` — todos los entries escritos al BufWriter, undo ops enqueued
- Errors: `DbError::NoActiveTransaction` si no hay txn activa; I/O errors del WAL

---

## Algoritmo

```
1. Verificar que hay txn activa → NoActiveTransaction si no
2. txn_id = active.txn_id
3. N = phys_locs.len()
4. Si N == 0 → return Ok(()) (noop)

5. lsn_base = wal.reserve_lsns(N)   ← reserva N LSNs en una operación

6. Serializar todos los entries en batch_buf (Vec<u8>):
   for i in 0..N:
     key = encode_physical_loc(page_id, slot_id)    // 10 bytes
     new_value = [key (10B)] ++ values[i]           // physical loc prepended para crash recovery
     entry = WalEntry {
       lsn: lsn_base + i as u64,
       txn_id,
       entry_type: EntryType::Insert,
       table_id,
       key: key.to_vec(),        // RecordId encoded = 10 bytes
       old_value: vec![],
       new_value,
     }
     entry.serialize_into(&mut batch_buf)            // append to shared buffer

7. wal.write_batch(&batch_buf)   ← UN SOLO write_all al BufWriter

8. Para cada (page_id, slot_id):
   active.undo_ops.push(UndoOp::UndoInsert { page_id, slot_id })
```

**Reuso del batch_buf**: usar el campo `wal_scratch` ya existente en `TxnManager` como
buffer acumulador. Limpiarlo antes del loop (igual que hace `append_with_buf`), pero
en este caso NO limpiarlo entre entries — acumular todos en el mismo Vec<u8>.

---

## Use cases

### 1. Happy path — 1000 filas en batch
```
TxnManager::record_insert_batch(1, &[(pg=5, sl=0)...(pg=10, sl=199)], &[...1000 rows...])
→ reserve_lsns(1000)
→ loop: serialize 1000 entries into wal_scratch
→ write_batch(&wal_scratch)  ← UNA sola llamada write_all
→ undo_ops: 1000 × UndoInsert
→ Ok(())
```

### 2. N=0
```
record_insert_batch(1, &[], &[]) → Ok(()) inmediato, sin writes
```

### 3. N=1 (debe funcionar igual que record_insert)
```
record_insert_batch(1, &[(5, 0)], &[row_bytes])
→ mismo resultado que record_insert(1, key, row_bytes, 5, 0)
```

### 4. Crash recovery sin cambios
```
WAL replay: cada entry en batch_buf es un WalEntry::Insert estándar
→ recovery los lee uno a uno con WalReader (sin cambios en recovery.rs)
→ RowHeader.txn_id_created = txn_id correcto en cada slot
→ comportamiento idéntico al path anterior
```

---

## Acceptance criteria

- [ ] `record_insert_batch(N rows)` produce exactamente los mismos entries WAL en disco
  que N llamadas a `record_insert()` — verificable comparando bytes de WAL
- [ ] Una sola llamada a `write_batch()` (no N llamadas a `append_with_buf()`)
- [ ] `reserve_lsns(N)` asigna LSNs consecutivos correctos
- [ ] `undo_ops` contiene exactamente N `UndoInsert` entries tras la llamada
- [ ] `record_insert_batch(0 rows)` → `Ok(())`, sin side effects
- [ ] Crash recovery con entries escritos por `record_insert_batch` funciona sin cambios
- [ ] Los tests de durabilidad existentes siguen pasando (sin cambios en recovery.rs)

---

## Out of scope

- Cambios en `WalWriter` — ya tiene `reserve_lsns` y `write_batch`
- Cambios en crash recovery — los entries son WalEntry::Insert estándar
- `record_delete_batch`, `record_update_batch` — fuera de scope (son menos frecuentes)
- Wire-up en `insert_rows_batch` — ese es 4.16c
- WAL record per page (PageWrite) — eso es 3.18

---

## Dependencies

- `WalWriter::reserve_lsns()` @ `writer.rs:214` ✅ existe
- `WalWriter::write_batch()` @ `writer.rs:232` ✅ existe
- `WalEntry::serialize_into()` @ `entry.rs:152` ✅ existe
- `encode_physical_loc()` @ `txn.rs` ✅ existe (usado en `record_insert`)
- `TxnManager::wal_scratch: Vec<u8>` ✅ existe — reutilizar como batch buffer
