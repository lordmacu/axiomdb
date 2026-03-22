# Plan: Optimización de rendimiento del B+ Tree

## Archivos a modificar

- `crates/nexusdb-index/src/page_layout.rs` — binary search en `find_child_idx`
- `crates/nexusdb-index/src/tree.rs` — eliminar alloc en lookup + in-place en insert

## Subfase 2.5.1 — Lookup sin allocations

### Paso 1 — Binary search en `InternalNodePage::find_child_idx`

**Archivo:** `page_layout.rs`, función `find_child_idx` (línea ~134)

```
Actual:
    (0..n).find(|&i| self.key_at(i) > key).unwrap_or(n)

Nuevo (binary search):
    // Invariante: keys[0..n] están ordenadas por el B+ Tree.
    // Buscamos el primer i tal que keys[i] > key → ese es el child idx.
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if self.key_at(mid) <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
```

Este es el patrón estándar de "lower bound" adaptado para `>`.

### Paso 2 — Eliminar `find_child_compressed` y sus allocations

**Archivo:** `tree.rs`

1. Eliminar la función `find_child_compressed` completa.
2. En `lookup`, reemplazar:
   ```rust
   // Antes:
   pid = Self::find_child_compressed(&node, key);
   // Después:
   pid = node.child_at(node.find_child_idx(key));
   ```
3. Verificar que `use crate::prefix::CompressedNode` ya no se usa en `tree.rs`.
   Si no se usa en ningún otro lugar del crate, eliminar el import (pero mantener el módulo).

### Tests subfase 2.5.1
- Correr todos los tests del workspace: `cargo test --workspace`
- Correr benchmark para verificar: `cargo bench --bench btree -- point_lookup`
- Objetivo: ≥ 800K ops/s para 1M keys

---

## Subfase 2.5.2 — Insert in-place

### Paso 3 — In-place en `insert_leaf` (caso sin split)

**Archivo:** `tree.rs`, función `insert_leaf` (línea ~203)

Caso actual (sin split, líneas ~215-224):
```rust
if node.num_keys() < ORDER_LEAF {
    let new_pid = storage.alloc_page(PageType::Index)?;
    let mut p = Page::new(PageType::Index, new_pid);
    let n = cast_leaf_mut(&mut p);
    *n = node;
    n.insert_at(ins_pos, key, rid);
    p.update_checksum();
    storage.write_page(new_pid, &p)?;
    storage.free_page(old_pid)?;
    return Ok(InsertResult::Ok(new_pid));
}
```

Nuevo (in-place):
```rust
if node.num_keys() < ORDER_LEAF {
    let mut p = Page::new(PageType::Index, old_pid);
    let n = cast_leaf_mut(&mut p);
    *n = node;
    n.insert_at(ins_pos, key, rid);
    p.update_checksum();
    storage.write_page(old_pid, &p)?;
    // Sin alloc_page, sin free_page
    return Ok(InsertResult::Ok(old_pid));
}
```

La página sigue usando `old_pid` — el padre no necesita actualizar su puntero child,
lo que permite optimizar el nivel superior también.

### Paso 4 — In-place en `cow_update_child`

**Archivo:** `tree.rs`, función `cow_update_child` (línea ~379)

Renombrar a `in_place_update_child` y cambiar implementación:

```
Actual:
    node.set_child_at(child_idx, new_child);
    let new_pid = storage.alloc_page(PageType::Index)?;
    let mut p = Page::new(PageType::Index, new_pid);
    *cast_internal_mut(&mut p) = node;
    p.update_checksum();
    storage.write_page(new_pid, &p)?;
    storage.free_page(old_pid)?;
    Ok(new_pid)

Nuevo:
    node.set_child_at(child_idx, new_child);
    let mut p = Page::new(PageType::Index, old_pid);
    *cast_internal_mut(&mut p) = node;
    p.update_checksum();
    storage.write_page(old_pid, &p)?;
    Ok(old_pid)
```

Actualizar todos los call sites de `cow_update_child` en `insert_subtree` para que
llamen a la función renombrada.

### Paso 5 — Verificar que split sigue siendo CoW

Los splits NO se tocan:
- `insert_leaf` (caso split): alloca `left_pid` y `right_pid`, libera `old_pid` ✓
- `split_internal`: ídem ✓
- `alloc_root`: siempre alloca página nueva ✓

El CAS en `root_pid` solo se ejecuta cuando cambia el root (split que llega a raíz).
Con in-place, si no hay split, `root_pid` no cambia → el CAS no se ejecuta → cero overhead.

Esperar un momento: con in-place en insert_leaf retornando `Ok(old_pid)`, el parent
recibe `InsertResult::Ok(old_pid)` y llama `cow_update_child(parent_pid, parent_node, child_idx, old_pid)`.
Pero `new_child == old_pid` → el child pointer no cambió → `set_child_at` escribe el mismo valor.
Con `in_place_update_child` esto sigue siendo correcto: reescribimos la página del padre
con el mismo child pointer (no-op semántico), pero actualizamos el checksum.

**Optimización adicional:** Si `new_child == old_child_pid`, skip the write entirely.
Esto evita reescribir los nodos internos cuando el hijo no cambió de página.

En `insert_subtree`, el match en `InsertResult::Ok`:
```rust
InsertResult::Ok(new_child_pid) => {
    // Si new_child_pid == child_pid (in-place, sin cambio de pid):
    // no hay que actualizar el padre.
    if new_child_pid == child_pid {
        return Ok(InsertResult::Ok(pid)); // pid no cambió tampoco
    }
    let new_pid = Self::in_place_update_child(storage, pid, node, child_idx, new_child_pid)?;
    Ok(InsertResult::Ok(new_pid))
}
```

Esto hace que un insert sin split (el caso más común) escribe SOLO la hoja y no toca
ningún nodo interno. El árbol completo queda intacto salvo la página hoja modificada.

### Tests subfase 2.5.2
- `cargo test --workspace` — todos los tests deben pasar
- `cargo bench --bench btree -- insert` — verificar ≥ 180K ops/s
- Test específico de correctness: insertar 1M keys y luego lookup de todas → sin errores

---

## Antipatrones a evitar

- **NO** usar `unwrap()` en ningún lugar (ya está prohibido en src/)
- **NO** romper el CAS pattern en `root_pid` — se mantiene para Fase 7
- **NO** cambiar el path de splits — solo el path sin split es in-place
- **NO** eliminar `CompressedNode` — se puede necesitar para análisis o bulk load futuro

## Riesgos

| Riesgo | Mitigación |
|---|---|
| In-place rompería lectura concurrente | No hay lectores concurrentes con `&mut self` — Rust lo garantiza |
| Binary search con comparación `<=` en vez de `<` puede regresar child erróneo | Verificar con test de invariante: lookup después de 1M inserts |
| `new_child == old_pid` optimization introduce un bug de correctness | Agregar test: insert → lookup del mismo key inmediatamente |

## Orden de implementación

```
1. [page_layout.rs] Binary search en find_child_idx
2. [tree.rs] Eliminar find_child_compressed, usar node.find_child_idx directo
3. cargo test && cargo bench -- point_lookup → verificar ≥800K ops/s
4. [tree.rs] insert_leaf in-place (sin split)
5. [tree.rs] cow_update_child → in_place_update_child
6. [tree.rs] Optimization: skip parent write si pid no cambió
7. cargo test && cargo bench -- insert → verificar ≥180K ops/s
8. Protocolo de cierre completo
```
