# Plan: 3.17 + 4.16c — WAL Batch Append + Multi-row INSERT wire-up

## Files to create/modify

| Acción | Archivo | Qué cambia |
|---|---|---|
| Modify | `crates/axiomdb-wal/src/txn.rs` | Añade `record_insert_batch()` |
| Modify | `crates/axiomdb-sql/src/table.rs` | `insert_rows_batch()` usa `record_insert_batch()` |
| Modify | `crates/axiomdb-sql/benches/executor_e2e.rs` | Añade `bench_insert_multi_row` |
| Modify | `crates/axiomdb-wal/tests/integration_group_commit.rs` | Añade test para batch WAL |

---

## Algoritmo — `record_insert_batch`

```rust
pub fn record_insert_batch(
    &mut self,
    table_id: u32,
    phys_locs: &[(u64, u16)],  // (page_id, slot_id)
    values: &[Vec<u8>],
) -> Result<(), DbError> {
    let n = phys_locs.len();
    debug_assert_eq!(n, values.len(), "phys_locs and values must have same len");
    if n == 0 { return Ok(()); }

    let active = self.active.as_mut().ok_or(DbError::NoActiveTransaction)?;
    let txn_id = active.txn_id;

    // 1. Reserve N LSNs atomically
    let lsn_base = self.wal.reserve_lsns(n);

    // 2. Accumulate all N entries into wal_scratch
    //    Clear once before the loop (not between entries)
    self.wal_scratch.clear();

    for (i, ((page_id, slot_id), value)) in phys_locs.iter().zip(values.iter()).enumerate() {
        // Prepend physical location for crash recovery (same as record_insert)
        let phys_prefix = encode_physical_loc(*page_id, *slot_id);
        let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
        new_value.extend_from_slice(&phys_prefix);
        new_value.extend_from_slice(value);

        let key = encode_physical_loc(*page_id, *slot_id);  // RecordId key = same 10 bytes

        let mut entry = WalEntry::new(
            lsn_base + i as u64,  // pre-assigned LSN (NOT 0)
            txn_id,
            EntryType::Insert,
            table_id,
            key.to_vec(),
            vec![],
            new_value,
        );
        entry.serialize_into(&mut self.wal_scratch);
    }

    // 3. One write_all for all N entries
    self.wal.write_batch(&self.wal_scratch)?;

    // 4. Enqueue undo ops
    for (page_id, slot_id) in phys_locs {
        active.undo_ops.push(UndoOp::UndoInsert { page_id: *page_id, slot_id: *slot_id });
    }

    Ok(())
}
```

**Nota sobre LSN pre-asignado**: `WalEntry::new(lsn, ...)` — en el batch usamos el LSN
pre-asignado directamente en el constructor en lugar de dejarlo en 0 para que
`append_with_buf` lo asigne. `write_batch` no modifica LSNs, por lo que debemos
asignarlos nosotros antes de serializar.

**Nota sobre wal_scratch**: se limpia UNA VEZ antes del loop y se acumulan todos
los entries. Al final, `wal_scratch` contiene los N entries concatenados listos para
`write_batch`. Capacidad del Vec crece hasta el batch más grande visto y se retiene.

---

## Wire-up en `insert_rows_batch` (table.rs)

```rust
// ANTES (línea 222-233):
let mut result = Vec::with_capacity(phys_locs.len());
for ((page_id, slot_id), encoded) in phys_locs.iter().zip(encoded_rows.iter()) {
    let key = encode_rid(*page_id, *slot_id);
    txn.record_insert(table_def.id, &key, encoded, *page_id, *slot_id)?;
    result.push(RecordId { page_id: *page_id, slot_id: *slot_id });
}

// DESPUÉS:
txn.record_insert_batch(table_def.id, &phys_locs, &encoded_rows)?;
let result: Vec<RecordId> = phys_locs
    .iter()
    .map(|(page_id, slot_id)| RecordId { page_id: *page_id, slot_id: *slot_id })
    .collect();
```

---

## Fases de implementación

### Fase 1 — `record_insert_batch` en TxnManager (15 min)
1. Añadir el método siguiendo el pseudocódigo arriba
2. Unit test en `txn.rs`: `test_record_insert_batch_produces_same_wal_as_individual`
   - Insertar N filas con `record_insert_batch` y comparar los bytes del WAL con los que
     produce `record_insert` × N (deben ser idénticos)

### Fase 2 — Wire-up en `insert_rows_batch` (5 min)
1. Reemplazar el loop por `record_insert_batch`
2. `cargo test --workspace` para confirmar que no rompemos nada

### Fase 3 — Benchmark + medición (10 min)
1. Añadir `bench_insert_multi_row` en `executor_e2e.rs`
2. Correr `cargo bench --bench executor_e2e` y reportar números

---

## Tests a escribir

### Unit test en `integration_group_commit.rs` (o en `txn.rs`)
```
test_record_insert_batch_same_wal_as_individual:
  - crear 2 TxnManagers sobre 2 WAL files distintos
  - txn1: N × record_insert (path actual)
  - txn2: 1 × record_insert_batch (path nuevo)
  - comparar: los bytes del WAL entre los dos Begin..entries..Commit deben ser iguales
    (mismos entry_len, mismos LSNs, mismos payloads, mismos CRC32c)

test_record_insert_batch_empty_is_noop:
  - record_insert_batch(id, &[], &[]) → Ok(()), WAL no cambia

test_record_insert_batch_undo_ops_correct:
  - N inserts vía record_insert_batch
  - rollback → verificar N slots muertos en heap
```

### Bench (`executor_e2e.rs`)
```
bench_insert_multi_row/100    — "INSERT INTO t VALUES (r1),...,(r100)"
bench_insert_multi_row/1_000  — 1K rows
bench_insert_multi_row/10_000 — 10K rows (target: ≥ 3× vs bench_insert_batch_txn)
```

---

## Anti-patterns a evitar

- **NO limpiar `wal_scratch` entre entries** — acumular todos en el mismo buffer
- **NO usar `append_with_buf` en el loop** — perderíamos el beneficio del batch
- **NO asignar LSN = 0 y luego confiar en write_batch** — `write_batch` no asigna LSNs;
  debemos pre-asignarlos con `reserve_lsns` antes de serializar
- **NO cambiar el formato de los entries** — deben ser WalEntry::Insert estándar para
  que crash recovery funcione sin cambios

---

## Risks

| Riesgo | Mitigación |
|---|---|
| LSNs no-contiguos si hay otra actividad de WAL entre `reserve_lsns` y `write_batch` | No aplica: single-writer constraint garantiza que nadie más escribe entre los dos calls |
| `wal_scratch` crece mucho para batches grandes (10K rows × ~60B ≈ 600 KB) | Aceptable: Vec<u8> a 600KB en RAM durante un batch es trivial; la capacidad se retiene para futuras calls |
| Tests de crash recovery fallan si el format cambia | Los entries siguen siendo WalEntry::Insert — el recovery los lee igual que antes |
