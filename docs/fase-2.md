# Fase 2 — B+ Tree CoW

**Estado:** ✅ Completada
**Crate:** `nexusdb-index`
**Spec/Plan:** `specs/fase-02/`

---

## Qué se implementó

Un B+ Tree persistente sobre `StorageEngine` con:

- **Keys binarias variables** hasta 64 bytes (`Box<[u8]>` en API, `[u8; 64]` zero-padded en disco)
- **Copy-on-Write** con `AtomicU64` para la raíz (readers lock-free por diseño)
- **Insert con split** — split de hoja y propagación recursiva hasta raíz si es necesario
- **Delete con rebalanceo** — redistribución desde hermanos y merge
- **Range scan lazy** — iterador `RangeIter` que traversa el árbol para pasar entre hojas
- **Prefix compression** — `CompressedNode` en memoria para nodos internos

---

## Decisiones técnicas

| Decisión | Elección | Razón |
|---|---|---|
| KeyType en API | `&[u8]` (max 64 bytes) | Soporta u64, UUID, strings cortos sin overhead |
| Serialización a disco | `bytemuck::Pod` + `unsafe impl` manual | Arrays grandes (>32) no tienen Pod automático |
| Linked list de hojas | No usada en range scan | CoW invalida punteros `next_leaf`; iterator usa tree traversal |
| Concurrencia | `AtomicU64` root, `&mut self` en writes | Correcto para Fase 2; extensible a lock-free en Fase 7 |

### Por qué `next_leaf` no se usa en el iterador

Con CoW, al copiar una hoja `L_old → L_new`, la hoja anterior en la linked list
sigue apuntando a `L_old` (ya liberada). Mantener la linked list en sync requeriría
CoW de la hoja anterior también, lo cual requiere conocer su page_id durante cada insert.

**Solución adoptada**: el `RangeIter` traversa el árbol desde root para encontrar
la siguiente hoja. Costo: O(log n) por cruce de frontera entre hojas (aceptable).

---

## Layout de páginas en disco

### Nodo interno (`ORDER_INTERNAL = 223`, tamaño: 16,295 bytes)
```
[is_leaf=0: 1B][_pad: 1B][num_keys: 2B LE][_pad: 4B]  = 8B
[key_lens: 223B]                                         = 223B
[children: 224 × [u8;8] = 1,792B]                       = 1,792B
[keys: 223 × [u8;64] = 14,272B]                         = 14,272B
Total: 16,295B ≤ PAGE_BODY_SIZE (16,320B) ✓
```

### Nodo hoja (`ORDER_LEAF = 217`, tamaño: 16,291 bytes)
```
[is_leaf=1: 1B][_pad: 1B][num_keys: 2B LE][_pad: 4B][next_leaf: 8B LE]  = 16B
[key_lens: 217B]                                                           = 217B
[rids: 217 × [u8;10] = 2,170B]  (page_id:8B + slot_id:2B LE)             = 2,170B
[keys: 217 × [u8;64] = 13,888B]                                           = 13,888B
Total: 16,291B ≤ PAGE_BODY_SIZE (16,320B) ✓
```

---

## Archivos creados

```
crates/nexusdb-index/
├── Cargo.toml                      — deps: bytemuck, nexusdb-storage
└── src/
    ├── lib.rs                      — re-exports BTree, RangeIter
    ├── page_layout.rs              — InternalNodePage, LeafNodePage, bytemuck
    ├── tree.rs                     — BTree: insert, delete, lookup, range
    ├── iter.rs                     — RangeIter (lazy, tree traversal)
    └── prefix.rs                   — CompressedNode (prefix compression)
crates/nexusdb-index/tests/
└── integration_btree.rs            — 8 tests de integración
crates/nexusdb-index/benches/
└── btree.rs                        — Criterion: lookup, range scan, insert
specs/fase-02/
├── spec-btree.md
└── plan-btree.md
```

---

## Métricas de calidad

- **Tests:** 37 pasan (28 unit + 8 integration + 1 doctest)
- **Clippy:** 0 errores (`-D warnings`)
- **Fmt:** limpio
- **Benchmarks:** compilan y corren correctamente
  - 100K inserts aleatorios: sin panics
  - Point lookup 10K/100K/1M keys: funcional
  - Range scan 100/1K/10K results: funcional

---

## Bug notable corregido durante implementación

**`rotate_right` en split_internal panics cuando `child_idx == n`**

Causa: `kl[n+1..=n].rotate_right(1)` llama rotate_right(1) en una slice vacía → panic.
Fix: reemplazar por `copy_within(child_idx..n, child_idx+1)` que maneja rangos vacíos.

---

## Deferred items

Nada crítico. Los siguientes quedan para fases posteriores:

- `next_leaf` linked list mantenida en CoW (Fase 7, con MVCC y epoch reclamation)
- Keys > 64 bytes con overflow pages (Fase posterior)
- Bloom filter por índice (Fase 6)
- Partial / covering / sparse indexes (Fase 6)
