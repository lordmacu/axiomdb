# Spec: B+ Tree (Fase 2)

## Qué construir (no cómo)

Un B+ Tree persistente que vive en páginas del `StorageEngine`, con soporte para
keys binarios de longitud variable (hasta 64 bytes), operaciones CRUD completas,
range scan por linked list de hojas, Copy-on-Write con raíz atómica, y compresión
de prefijos en nodos internos.

---

## Decisiones de diseño fijadas

| Aspecto | Decisión | Razón |
|---|---|---|
| Tipo de key en API | `Box<[u8]>` | Inmutable, 2 words, soporta u64/UUID/strings serializado |
| Concurrencia | CoW real + `AtomicU64` root | Lock-free readers; no refactor en Fase 7 |
| Serialización a página | `bytemuck` / `repr(C)` manual | Layout fijo, zero-copy cast, sin dependencia pesada |
| Max key length en disco | 64 bytes | Cubre u64 (8), UUID (16), strings cortos, claves compuestas |
| Crate destino | `nexusdb-index` | Ya existe como stub |

---

## Constantes de layout de página

Page body disponible: `16,384 - 64 = 16,320 bytes`

### Nodo interno (`ORDER_INTERNAL = 223`)

```
[header:    8 bytes] is_leaf=0, pad, num_keys (u16), pad
[key_lens: 223 bytes] longitud real de cada key (1 byte cada una)
[_pad:       1 byte]  alinear a múltiplo de 8 para children
[children: 1,792 bytes] (223+1) * 8 bytes = 224 punteros u64
[keys:    14,272 bytes] 223 * 64 bytes = arrays fijos zero-padded
────────────────────────────────────────────────
Total usado: 16,296 bytes ≤ 16,320 ✓
```

### Nodo hoja (`ORDER_LEAF = 211`)

```
[header:    16 bytes] is_leaf=1, pad, num_keys (u16), pad + next_leaf (u64)
[key_lens: 211 bytes] longitud real de cada key
[_pad:       1 byte]  alinear rids a 4 bytes
[rids:     2,532 bytes] 211 * 12 bytes = {page_id: u64, slot_id: u16, _pad: u16}
[keys:    13,504 bytes] 211 * 64 bytes
────────────────────────────────────────────────
Total usado: 16,264 bytes ≤ 16,320 ✓
```

---

## Inputs / Outputs

### `BTree::new(storage, root_page_id) -> BTree`
- Crea árbol vacío. Alloca una hoja raíz si `root_page_id == None`.

### `BTree::lookup(key: &[u8]) -> Result<Option<RecordId>>`
- Input: slice de bytes, longitud 1..=64
- Output: `Some(RecordId)` si existe, `None` si no
- Errores: `KeyTooLong { len }`, `StorageError`

### `BTree::insert(key: &[u8], rid: RecordId) -> Result<()>`
- Input: key + RecordId
- Output: `Ok(())` o error
- Errores: `DuplicateKey { key }`, `KeyTooLong`, `StorageError`
- Semántica CoW: copia el path root→hoja, CAS del root al final

### `BTree::delete(key: &[u8]) -> Result<bool>`
- Input: key
- Output: `true` si existía y se eliminó, `false` si no existía
- Con merge/redistribución cuando nodo queda < ORDER/2 keys

### `BTree::range(from: Bound<&[u8]>, to: Bound<&[u8]>) -> RangeIter`
- Iterator que recorre linked list de hojas
- Soporta `Bound::Included`, `Bound::Excluded`, `Bound::Unbounded`

### `BTree::root_page_id() -> u64`
- Retorna el page_id actual de la raíz (para persistir en catálogo)

---

## Casos de uso

1. **Lookup PK entero**: `key = 42u64.to_be_bytes()` → `Some(RecordId)`
2. **Lookup inexistente**: key no en árbol → `None`
3. **Insert que causa split de hoja**: hoja llena → split → nuevo nodo interno
4. **Insert que propaga split al root**: root lleno → nuevo root
5. **Range scan `[10..=50]`**: recorre linked list, retorna exactamente las filas en rango
6. **Delete con redistribución**: nodo queda con ORDER/2 - 1 keys → toma prestada de hermano
7. **Delete con merge**: hermano también mínimo → merge + actualizar padre
8. **Concurrent reads durante write CoW**: readers ven snapshot anterior hasta CAS del root

---

## Criterios de aceptación

### 2.1 — Estructuras de nodo
- [ ] `InternalNodePage` y `LeafNodePage` son `bytemuck::Pod + Zeroable`
- [ ] `size_of::<InternalNodePage>() <= PAGE_BODY_SIZE` (compile-time assert)
- [ ] `size_of::<LeafNodePage>() <= PAGE_BODY_SIZE` (compile-time assert)
- [ ] Conversión zero-copy: cast de `Page::body()` a `&InternalNodePage` sin copia
- [ ] Linked list de hojas: `next_leaf` apunta a siguiente hoja o `u64::MAX` si es la última

### 2.2 — Lookup
- [ ] Lookup de key existente retorna `RecordId` correcto
- [ ] Lookup de key inexistente retorna `None`
- [ ] Complejidad O(log n): no recorre hojas innecesariamente
- [ ] No requiere ningún lock

### 2.3 — Insert con split
- [ ] Insert en hoja vacía funciona
- [ ] Insert en hoja llena hace split correcto (mitad izquierda / mitad derecha)
- [ ] Split propaga key al nodo interno padre
- [ ] Split del root crea nuevo root (árbol crece en altura)
- [ ] Después de N inserts, todos los keys son recuperables con lookup
- [ ] `DuplicateKey` error si el key ya existe

### 2.4 — Range scan
- [ ] `Bound::Unbounded` retorna todas las filas en orden
- [ ] `Bound::Included(k)` incluye k
- [ ] `Bound::Excluded(k)` excluye k
- [ ] Orden del iterator es ascendente por key (comparación lexicográfica de bytes)
- [ ] Iterator es lazy (no carga todas las páginas a memoria)

### 2.5 — Delete con merge
- [ ] Delete de key existente retorna `true`
- [ ] Delete de key inexistente retorna `false`
- [ ] Redistribución cuando hermano tiene keys de sobra (robar del vecino)
- [ ] Merge cuando hermano también está en mínimo
- [ ] Merge reduce altura del árbol si el root queda vacío

### 2.6 — Copy-on-Write
- [ ] `root_page_id()` usa `AtomicU64::load(Acquire)`
- [ ] Write copia solo el path root→hoja (O(log n) páginas nuevas)
- [ ] Swap del root es `AtomicU64::compare_exchange` (CAS)
- [ ] Páginas huérfanas (versión anterior) son liberadas con `free_page()` post-commit
- [ ] Concurrent readers: threads que tienen referencia a root anterior ven datos consistentes

### 2.7 — Prefix compression
- [ ] Nodos internos calculan prefijo común de sus keys
- [ ] `CompressedNode::reconstruct_key(idx)` retorna key original completa
- [ ] Keys con prefijo común ocupan menos bytes en el nodo
- [ ] Lookup y range scan funcionan igual con compresión activada

### 2.8 — Tests + benchmarks
- [ ] Tests unitarios para cada operación (MemoryStorage, sin I/O)
- [ ] Test de integración: insert 10K rows + lookup todos + range scan + delete mitad
- [ ] Test de crash recovery: insert → flush → reabrir → lookup
- [ ] Benchmark vs `std::collections::BTreeMap` para point lookup y range scan
- [ ] Benchmark: 1M inserts secuenciales (mide throughput y splits)

---

## Fuera del alcance

- Claves compuestas multi-columna (Fase 6)
- Bloom filter por índice (Fase 6)
- Partial / covering indexes (Fase 6)
- Sparse index (Fase 6)
- Collations / sort keys (Fase 6)
- rkyv zero-copy deserialization (decisión: bytemuck es suficiente)
- Keys > 64 bytes (overflow pages — Fase posterior)

---

## Dependencias

- `nexusdb-core`: `RecordId`, `PageId`, `DbError`
- `nexusdb-storage`: `StorageEngine` trait, `Page`, `PAGE_SIZE`, `HEADER_SIZE`, `PageType::Index`
- Crates nuevas: `bytemuck = { version = "1", features = ["derive"] }` (ya en workspace deps)
