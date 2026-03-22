# Plan: B+ Tree (Fase 2)

## Archivos a crear / modificar

```
crates/nexusdb-index/
├── Cargo.toml                     — agregar deps: nexusdb-core, nexusdb-storage, bytemuck
└── src/
    ├── lib.rs                     — re-exports públicos
    ├── page_layout.rs             — InternalNodePage, LeafNodePage, RecordIdOnDisk (bytemuck)
    ├── node.rs                    — BTreeNode (abstracción en memoria sobre page_layout)
    ├── tree.rs                    — BTree, operaciones CRUD, CoW
    ├── iter.rs                    — RangeIter (lazy, linked list de hojas)
    └── prefix.rs                  — CompressedNode, prefix compression

crates/nexusdb-index/tests/
└── integration_btree.rs           — tests de integración

crates/nexusdb-index/benches/
└── btree.rs                       — benchmarks Criterion

Cargo.toml (workspace root)        — verificar que nexusdb-index ya está en members[]
```

---

## Constantes de layout (derivadas del spec)

```rust
// src/page_layout.rs
pub const MAX_KEY_LEN: usize = 64;
pub const ORDER_INTERNAL: usize = 223;
pub const ORDER_LEAF: usize = 211;
pub const NULL_PAGE: u64 = u64::MAX;  // sentinel: "no siguiente hoja" / "no child"

// Compile-time assertions (en page_layout.rs)
const _: () = assert!(size_of::<InternalNodePage>() <= PAGE_BODY_SIZE);
const _: () = assert!(size_of::<LeafNodePage>() <= PAGE_BODY_SIZE);
```

---

## Estructuras de datos en página (page_layout.rs)

```rust
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RecordIdOnDisk {
    pub page_id: u64,    // 8 bytes
    pub slot_id: u16,    // 2 bytes
    pub _pad:    u16,    // 2 bytes → total 12 bytes, aligned to 4
}

// ──── Nodo Interno ────────────────────────────────────────────────────
// Layout en body de página (16,320 bytes):
//   header:    8 B  (is_leaf + _pad + num_keys + _pad)
//   key_lens: 223 B (1 byte por key: longitud real, 0 = slot vacío)
//   _align:    1 B  (pad a múltiplo de 8 para children)
//   children: 1,792 B (224 * u64)
//   keys:    14,272 B (223 * [u8; 64])
//   ─────────────────
//   Total:   16,296 B ≤ 16,320 ✓

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InternalNodePage {
    pub is_leaf:   u8,
    pub _pad0:     u8,
    pub num_keys:  u16,
    pub _pad1:     [u8; 4],
    pub key_lens:  [u8; ORDER_INTERNAL],
    pub _align:    [u8; 1],
    pub children:  [u64; ORDER_INTERNAL + 1],
    pub keys:      [[u8; MAX_KEY_LEN]; ORDER_INTERNAL],
}

// ──── Nodo Hoja ───────────────────────────────────────────────────────
// Layout en body de página (16,320 bytes):
//   header:    8 B  (is_leaf + _pad + num_keys + _pad)
//   next_leaf: 8 B  (u64, NULL_PAGE si es la última hoja)
//   key_lens: 211 B
//   _align:    1 B  (pad a múltiplo de 4 para rids)
//   rids:     2,532 B (211 * 12)
//   keys:    13,504 B (211 * 64)
//   ─────────────────
//   Total:   16,264 B ≤ 16,320 ✓

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LeafNodePage {
    pub is_leaf:   u8,
    pub _pad0:     u8,
    pub num_keys:  u16,
    pub _pad1:     [u8; 4],
    pub next_leaf: u64,
    pub key_lens:  [u8; ORDER_LEAF],
    pub _align:    [u8; 1],
    pub rids:      [RecordIdOnDisk; ORDER_LEAF],
    pub keys:      [[u8; MAX_KEY_LEN]; ORDER_LEAF],
}
```

### Acceso zero-copy desde `Page::body()`

```rust
// en page_layout.rs — SAFETY comentado
pub fn read_internal(page: &Page) -> &InternalNodePage {
    // SAFETY: InternalNodePage es Pod, alineación 1 (todos u8/u16/u64 packed en repr(C)).
    // Page::body() tiene PAGE_BODY_SIZE bytes >= size_of::<InternalNodePage>().
    // El contenido fue escrito como InternalNodePage (verificado por is_leaf == 0).
    bytemuck::from_bytes(&page.body()[..size_of::<InternalNodePage>()])
}

pub fn read_leaf(page: &Page) -> &LeafNodePage {
    // SAFETY: análogo a read_internal. is_leaf == 1 verificado antes de llamar.
    bytemuck::from_bytes(&page.body()[..size_of::<LeafNodePage>()])
}

pub fn write_internal(page: &mut Page, node: &InternalNodePage) {
    let bytes: &[u8] = bytemuck::bytes_of(node);
    page.body_mut()[..bytes.len()].copy_from_slice(bytes);
    page.update_checksum();
}

pub fn write_leaf(page: &mut Page, node: &LeafNodePage) {
    let bytes: &[u8] = bytemuck::bytes_of(node);
    page.body_mut()[..bytes.len()].copy_from_slice(bytes);
    page.update_checksum();
}
```

---

## Abstracción en memoria (node.rs)

```rust
// BTreeNode es la versión en memoria, con Vec para facilitar operaciones
// Solo se convierte a/desde InternalNodePage/LeafNodePage en I/O
pub enum BTreeNode {
    Internal {
        page_id:  u64,
        num_keys: usize,
        keys:     Vec<Box<[u8]>>,     // keys[i]: separador entre children[i] y children[i+1]
        children: Vec<u64>,           // len = num_keys + 1
    },
    Leaf {
        page_id:   u64,
        num_keys:  usize,
        keys:      Vec<Box<[u8]>>,
        rids:      Vec<RecordId>,
        next_leaf: u64,
    },
}

impl BTreeNode {
    pub fn load(storage: &dyn StorageEngine, page_id: u64) -> Result<Self, DbError>;
    pub fn flush(&self, storage: &mut dyn StorageEngine) -> Result<(), DbError>;
    pub fn is_full(&self) -> bool;    // num_keys >= ORDER - 1
    pub fn is_underfull(&self) -> bool;  // num_keys < ORDER / 2
}
```

---

## BTree principal (tree.rs)

```rust
pub struct BTree {
    storage:  Box<dyn StorageEngine>,
    root_pid: AtomicU64,    // AtomicU64 para CoW root swap
}

impl BTree {
    pub fn new(storage: Box<dyn StorageEngine>, root_page_id: Option<u64>)
        -> Result<Self, DbError>;

    pub fn lookup(&self, key: &[u8]) -> Result<Option<RecordId>, DbError>;
    pub fn insert(&mut self, key: &[u8], rid: RecordId) -> Result<(), DbError>;
    pub fn delete(&mut self, key: &[u8]) -> Result<bool, DbError>;
    pub fn range<'a>(&'a self, from: Bound<&[u8]>, to: Bound<&[u8]>)
        -> Result<RangeIter<'a>, DbError>;
    pub fn root_page_id(&self) -> u64;
}
```

### Algoritmo de lookup

```
fn lookup(key):
  pid = root_pid.load(Acquire)
  loop:
    page = storage.read_page(pid)
    if page.is_leaf():
      return binary_search(page.keys, key).map(|i| page.rids[i])
    else:
      pid = page.children[upper_bound(page.keys, key)]
```

### Algoritmo de insert (CoW)

```
fn insert(key, rid):
  path = traverse_to_leaf(key)   // Vec<(page_id, index_in_parent)>

  // Copiar hoja
  leaf = load(path.last())
  new_leaf_pid = alloc_page(Index)
  new_leaf = leaf.clone_with_insert(key, rid)

  if !new_leaf.is_full():
    // Caso simple: escribir nueva hoja, actualizar padre
    write(new_leaf_pid, new_leaf)
    update_parent_pointer(path, new_leaf_pid)
    CAS(root_pid, old_root, new_root)
    free_old_pages(path)
    return Ok(())

  // Split de hoja
  (left, right, separator) = new_leaf.split()
  left_pid  = alloc_page(Index)
  right_pid = alloc_page(Index)
  write(left_pid, left)
  write(right_pid, right)

  // Propagar split hacia arriba (puede iterar si padre también se llena)
  propagate_split(path, separator, left_pid, right_pid)

  // CAS del root
  CAS(root_pid, old_root, new_root)

  // Liberar páginas huérfanas
  free_old_pages(path)
```

### Algoritmo de delete (CoW)

```
fn delete(key):
  path = traverse_to_leaf(key)
  leaf = load(path.last())

  if !leaf.contains(key): return Ok(false)

  new_leaf = leaf.clone_with_delete(key)

  if !new_leaf.is_underfull() || path.len() == 1 (es root):
    // Caso simple
    write(new_leaf_pid, new_leaf)
    CAS(root_pid, ...)
    return Ok(true)

  // Intentar redistribución con hermano
  sibling = load_sibling(path)
  if sibling.can_lend():
    redistribute(new_leaf, sibling)
  else:
    // Merge
    merged = merge(new_leaf, sibling)
    propagate_merge(path, ...)

  CAS(root_pid, ...)
  Ok(true)
```

---

## Iterator lazy (iter.rs)

```rust
pub struct RangeIter<'a> {
    storage:      &'a dyn StorageEngine,
    current_pid:  u64,         // página hoja actual
    slot_idx:     usize,       // posición dentro de la hoja actual
    end_bound:    Bound<Box<[u8]>>,
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Box<[u8]>, RecordId), DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        // 1. Si slot_idx >= num_keys de la página actual → leer next_leaf
        // 2. Verificar bound de fin → retornar None si superamos el límite
        // 3. Retornar (key.clone(), rid) del slot actual
        // 4. Incrementar slot_idx
    }
}
```

---

## Prefix compression (prefix.rs)

```rust
pub struct CompressedNode {
    pub common_prefix: Vec<u8>,
    pub suffixes:      Vec<Vec<u8>>,
    pub children:      Vec<u64>,
}

impl CompressedNode {
    pub fn from_keys(keys: &[Box<[u8]>], children: Vec<u64>) -> Self;
    pub fn reconstruct_key(&self, idx: usize) -> Vec<u8>;
    pub fn find_child(&self, search_key: &[u8]) -> u64;
    pub fn common_prefix_len(keys: &[Box<[u8]>]) -> usize;
}
```

> Nota: prefix compression en subfase 2.7 es opcional en el layout de página.
> Se implementa como transformación en memoria sobre `BTreeNode::Internal`.
> No cambia el layout en disco (los keys se almacenan sin comprimir en páginas).
> La compresión reduce el espacio en RAM y mejora cache locality en nodos internos.

---

## Fases de implementación

### Fase 2.1 — page_layout.rs + node.rs
1. Definir `RecordIdOnDisk`, `InternalNodePage`, `LeafNodePage` con bytemuck
2. Compile-time size asserts
3. Funciones `read_internal`, `read_leaf`, `write_internal`, `write_leaf`
4. `BTreeNode::load` y `BTreeNode::flush`
5. Tests unitarios: roundtrip serialize/deserialize de nodo

### Fase 2.2 — Lookup (tree.rs)
1. `BTree::new` — crear hoja raíz vacía
2. `BTree::lookup` — traverse interno + binary search en hoja
3. Tests: lookup en árbol vacío, lookup hit, lookup miss

### Fase 2.3 — Insert con split (tree.rs)
1. `BTreeNode::clone_with_insert` — insertar en hoja (orden)
2. `BTreeNode::split` — split de hoja, retorna (left, right, separator)
3. `BTree::insert` — caso sin split
4. `BTree::insert` — caso con split + propagación
5. Tests: 1K inserts aleatorios → lookup todos

### Fase 2.4 — Range scan (iter.rs)
1. `BTree::find_start_leaf` — navegar hasta primera hoja en rango
2. `RangeIter` con Bound
3. Tests: range [10..=50], range [..], range (42..)

### Fase 2.5 — Delete con merge (tree.rs)
1. `BTreeNode::clone_with_delete`
2. Redistribución desde hermano
3. Merge + propagación al padre
4. Tests: delete → lookup miss, merge reduce altura

### Fase 2.6 — Copy-on-Write (tree.rs refactor)
1. `AtomicU64` para `root_pid`
2. `free_old_pages` post-CAS
3. Test concurrencia: 4 readers + 1 writer simultáneos

### Fase 2.7 — Prefix compression (prefix.rs)
1. `find_common_prefix_len`
2. `CompressedNode` encode/decode
3. Integrar en `BTreeNode::Internal` load/flush
4. Test: nodo con 100 keys `"usuario:XXXXX"` → verificar ahorro

### Fase 2.8 — Tests + benchmarks
1. Test de integración completo (10K inserts + range + delete)
2. Test crash recovery (MmapStorage)
3. `benches/btree.rs` con Criterion: point lookup, range 1K, 1M inserts

---

## Tests a escribir

### Unit (src/)
```rust
#[cfg(test)]
mod tests {
    // page_layout.rs
    fn test_internal_node_roundtrip()  // serialize → deserialize → mismos datos
    fn test_leaf_node_roundtrip()
    fn test_size_constraints()         // size_of verificado en runtime también

    // node.rs
    fn test_load_flush_leaf()
    fn test_load_flush_internal()

    // tree.rs
    fn test_lookup_empty_tree()
    fn test_insert_single()
    fn test_insert_duplicate_error()
    fn test_insert_causes_leaf_split()
    fn test_insert_causes_root_split()
    fn test_delete_existing()
    fn test_delete_nonexistent()
    fn test_delete_causes_merge()
    fn test_range_full()
    fn test_range_bounds()
    fn test_cow_new_pages_allocated()
    fn test_prefix_compression_roundtrip()
}
```

### Integration (tests/integration_btree.rs)
```rust
fn test_btree_1k_sequential_inserts_lookup_all()
fn test_btree_1k_random_inserts_lookup_all()
fn test_btree_range_scan_correctness()
fn test_btree_delete_half_then_lookup()
fn test_btree_crash_recovery()          // flush → reopen → lookup
fn test_btree_concurrent_reads_during_write()
```

### Benchmarks (benches/btree.rs)
```rust
fn bench_point_lookup_1m()             // vs BTreeMap
fn bench_range_scan_10k()              // 10k resultado
fn bench_insert_sequential_1m()        // throughput
fn bench_insert_random_100k()          // con splits
```

---

## Antipatrones a evitar

- **NO** `unwrap()` en `src/` — siempre `?` o `map_err`
- **NO** `unsafe` sin comentario `// SAFETY:`
- **NO** cargar toda la página en Vec para operaciones simples (zero-copy primero)
- **NO** `Clone` de `Page` innecesario en el path caliente de lookup
- **NO** implementar CoW con `Mutex` — usar `AtomicU64` para el root
- **NO** locks en readers — lookup debe ser completamente lock-free

---

## Riesgos y mitigaciones

| Riesgo | Mitigación |
|---|---|
| `bytemuck::Pod` rechaza la struct por padding implícito | Verificar con `assert_eq!(size_of::<InternalNodePage>(), N)` y ajustar `_pad` |
| Split de nodo interno cuando padre también está lleno | Implementar `propagate_split` iterativo (no recursivo) para evitar stack overflow |
| CoW libera páginas que readers aún usan | En Fase 2: un solo writer a la vez (`&mut self`). La liberación es segura. En Fase 7 (MVCC): epoch-based reclamation |
| Merge incorrecto rompe linked list de hojas | Test explícito: después de merge, recorrer linked list y verificar que no hay saltos |
| `AtomicU64` CAS falla en contención alta | En Fase 2: `&mut self` en writes garantiza que no hay contención. CAS en Fase 7 |
