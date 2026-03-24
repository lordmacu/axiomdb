# Spec: 4.16c — Multi-row INSERT optimization

## What to build (not how)

Dos cambios coordinados:

1. **Wire-up WAL batch en `insert_rows_batch`** — reemplazar el loop per-row de
   `txn.record_insert()` por una sola llamada a `txn.record_insert_batch()` (3.17).
   Resultado: O(1) llamadas a `write_batch` en lugar de O(N).

2. **Benchmark end-to-end** — actualizar el bench `insert_sequential` para usar
   `INSERT INTO t VALUES (r1),(r2),...,(rN)` en un solo SQL string, midiendo el
   pipeline completo (1 parse + 1 analyze + 1 execute + O(P) heap writes + O(1) WAL writes).

El parser ya acepta multi-row VALUES. El executor ya llama a `insert_rows_batch()`.
`HeapChain::insert_batch()` ya es O(P pages). El único gap era el WAL — ahora cerrado
con 3.17.

---

## Inputs / Outputs

### Cambio en `TableEngine::insert_rows_batch` (`table.rs`)

```
ANTES:
  for ((page_id, slot_id), encoded) in phys_locs.zip(encoded_rows):
    key = encode_rid(page_id, slot_id)
    txn.record_insert(table_def.id, &key, encoded, page_id, slot_id)?  // O(N)

DESPUÉS:
  txn.record_insert_batch(table_def.id, &phys_locs, &encoded_rows)?    // O(1)
```

- Input/Output: mismos que antes — ningún cambio de API visible externamente
- El `RecordId` result Vec se construye después del batch call igual que antes

### Nuevo benchmark `bench_insert_multi_row`

```rust
// Mide: 1 SQL string con N VALUES rows → parse + analyze + execute
// Compara con bench_insert_batch_txn (N SQL strings en 1 txn)
//
// SQL: "INSERT INTO t VALUES (1,'a'),(2,'b'),...,(N,'z')"
// Tamaños: 100, 1_000, 10_000 rows
```

---

## Use cases

### 1. Happy path — 10K rows, sin secondary indexes
```
"INSERT INTO t VALUES (1,'a'),(2,'b'),...,(10000,'z')"
→ parse: 1 vez
→ analyze: 1 vez
→ execute_insert:
    eval all expr: 10K rows → full_batch
    insert_rows_batch():
      encode all rows: 10K encoded
      HeapChain::insert_batch(): O(50 pages) writes
      txn.record_insert_batch(): reserve_lsns(10K) + 1 write_batch
→ commit: 1 fsync
```

### 2. 10K rows con secondary indexes
```
Sin cambios — el path con índices en executor.rs sigue usando record_insert per-row
(los índices requieren lookup individual para UNIQUE check).
El spec de 3.18 abordará este caso.
```

### 3. N=1 (single-row INSERT no regresa)
```
El executor usa record_insert (no insert_rows_batch) para N=1 rows.
No cambia nada.
```

### 4. Benchmark muestra gain esperado
```
bench_insert_batch_txn/10K (N strings):   ~30K rows/s (baseline)
bench_insert_multi_row/10K (1 string):    ~90-120K rows/s (target)
                                          ~3-5× improvement
```

---

## Acceptance criteria

- [ ] `insert_rows_batch()` llama a `txn.record_insert_batch()` en lugar del loop
- [ ] El número de WAL entries en disco es idéntico al anterior (mismos datos, misma semántica)
- [ ] `cargo test --workspace` pasa sin cambios (incluyendo tests de durabilidad)
- [ ] Benchmark `bench_insert_multi_row/10K` muestra ≥ 3× mejora vs `bench_insert_batch_txn/10K`
- [ ] Crash recovery después de bulk INSERT produce filas visibles tras recovery (regression test)

---

## Out of scope

- INSERT con secondary indexes (sigue siendo per-row por los UNIQUE checks)
- WAL record per page (PageWrite) — eso es 3.18 / 4.16d
- Cambios en el parser o AST — multi-row VALUES ya funciona

---

## Dependencies

- `TxnManager::record_insert_batch()` — especificado en 3.17, debe estar implementado primero
- `HeapChain::insert_batch()` ✅ ya implementado
- `insert_rows_batch()` ✅ ya existe en `table.rs:188`
- Multi-row VALUES parser ✅ ya funciona (`InsertSource::Values(Vec<Vec<Expr>>)`)
