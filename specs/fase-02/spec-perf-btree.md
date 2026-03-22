# Spec: Optimización de rendimiento del B+ Tree (subfases 2.5.1 y 2.5.2)

## Qué construir (no cómo)

Eliminar los dos bloqueantes de rendimiento del B+ Tree que impiden cumplir el
presupuesto definido en CLAUDE.md:

1. **Point lookup demasiado lento** — 96K ops/s vs objetivo 800K ops/s.
2. **Insert demasiado lento** — 60-104K ops/s vs objetivo 180K ops/s.

## Contexto del problema

### Blocker 1 — `find_child_compressed` alloca en el hot path

En `tree.rs::lookup`, cada nodo interno visitado llama a `find_child_compressed`,
que construye tres `Vec` por nodo:

```rust
let keys: Vec<Box<[u8]>> = (0..n).map(|i| node.key_at(i).to_vec().into_boxed_slice()).collect();
let children: Vec<u64> = (0..=n).map(|i| node.child_at(i)).collect();
let compressed = CompressedNode::from_keys(&keys, children);
```

Cada heap allocation en el hot path destruye el cache del CPU. Un árbol de 1M keys
tiene 3-4 niveles → 9-12 heap allocations + cientos de copias de strings por lookup.

Además, `InternalNodePage::find_child_idx` usa linear scan O(n) sobre hasta 223 keys.

### Blocker 2 — CoW en inserts sin concurrencia

`insert_leaf` y `cow_update_child` siempre hacen alloc+write+free por cada página en
el path, incluso cuando no hay split y no hay lector concurrente. Con `&mut self`, el
ownership exclusivo garantiza que ningún lector puede ver el estado intermedio — el CoW
es innecesario para cada página individual (solo se necesita para garantizar que `root_pid`
cambia atómicamente, lo cual ya está manejado por `AtomicU64`).

## Subfase 2.5.1 — Lookup sin allocations

### Inputs / Outputs
- Input: `BTree::lookup(key: &[u8])` — sin cambios en la firma
- Output: `Result<Option<RecordId>, DbError>` — sin cambios
- Comportamiento observable: idéntico al actual (mismos resultados)
- Rendimiento: ≥ 800K ops/s para 1M keys

### Qué cambia

**`page_layout.rs` — `InternalNodePage::find_child_idx`:**
Reemplazar el linear scan por binary search sobre las keys del nodo.
No se necesita allocation. Las keys ya están ordenadas por invariante del B+ Tree.

```
Antes: (0..n).find(|&i| self.key_at(i) > key).unwrap_or(n)   → O(n) lineal
Después: binary search sobre (0..n)                             → O(log n)
```

**`tree.rs` — `find_child_compressed`:**
Eliminar el método. Reemplazar la llamada en `lookup` por `node.find_child_idx(key)`
directo sobre el `InternalNodePage`, sin construcción de `CompressedNode`.

La prefix compression (`CompressedNode`) queda disponible como utilidad para otros
usos futuros (bulk load, análisis estadístico), pero NO se usa en el hot path de lookup.

### Casos de uso
1. Lookup en árbol vacío → `Ok(None)`
2. Lookup de key existente en árbol de 1M → `Ok(Some(rid))`
3. Lookup de key inexistente → `Ok(None)`
4. Nodo interno con keys sin prefijo común → binary search funciona igual
5. Nodo interno con keys con prefijo largo → binary search funciona igual

### Criterios de aceptación
- [ ] `point_lookup/nexusdb_btree/1000000` ≥ 800K ops/s en benchmark
- [ ] Todos los tests existentes del B+ Tree siguen pasando
- [ ] Cero heap allocations en el path de lookup (verificable eliminando el Vec)
- [ ] `find_child_idx` usa binary search (verificable leyendo el código)

### Fuera del alcance
- Cambiar el formato en disco
- Cambiar la API pública de `BTree`
- Modificar `CompressedNode` (solo dejar de llamarlo desde el hot path)

### Dependencias
- Ninguna

---

## Subfase 2.5.2 — Insert in-place cuando no hay split

### Inputs / Outputs
- Input: `BTree::insert(key: &[u8], rid: RecordId)` — sin cambios en la firma
- Output: `Result<(), DbError>` — sin cambios
- Comportamiento observable: idéntico (mismos datos, mismos errores)
- Rendimiento: ≥ 180K ops/s insert secuencial 1M

### Qué cambia

**`tree.rs` — `insert_leaf` (caso sin split):**
Cuando `num_keys < ORDER_LEAF`, en lugar de alloc+write+free, escribir directamente
sobre `old_pid`. Elimina 1 alloc_page + 1 free_page por insert no-split.

**`tree.rs` — `cow_update_child`:**
Renombrar / reemplazar por `in_place_update_child`: en lugar de alloc+write+free,
escribir la página modificada directamente en `old_pid`. Elimina N alloc+free por
insert no-split donde N = profundidad del árbol.

**Splits siguen siendo CoW:**
Cuando hay split (leaf o internal), se siguen allocando dos páginas nuevas y liberando
la original — esto es correcto e inevitable porque un split crea dos nodos de una página.

**`root_pid` sigue usando CAS:**
El patrón `compare_exchange` en `root_pid` se mantiene porque prepara la base para
concurrencia en Fase 7. Con `&mut self` siempre tiene éxito.

### Invariante de corrección
Con `&mut self`, Rust garantiza exclusividad — ningún otro lector puede acceder a las
páginas durante el insert. Por tanto, modificar una página in-place es correcto:
no hay ventana donde otro hilo vea una página en estado intermedio.

### Casos de uso
1. Insert secuencial 1M sin splits frecuentes → in-place dominante
2. Insert aleatorio 100K con splits frecuentes → in-place en los niveles superiores
3. Insert que causa split en hoja → CoW en hoja (alloc 2 nuevas), in-place en internos
4. Insert que causa split hasta raíz → todo CoW (nueva raíz)

### Criterios de aceptación
- [ ] `insert_1m_sequential/nexusdb_btree_1m` ≥ 180K ops/s
- [ ] `insert_sequential/nexusdb_btree_1k` ≥ 180K ops/s
- [ ] Todos los tests existentes siguen pasando
- [ ] Los datos son correctos después de 1M inserts + 1M lookups

### Fuera del alcance
- Concurrencia (se maneja en Fase 7)
- Cambiar el comportamiento de splits
- Bulk load / bulk insert

### Dependencias
- Subfase 2.5.1 completada (no técnica, solo orden lógico)
