# Spec: 1.5 — Free List

## Qué construir
Bitmap-based free list persistida en página 1 del archivo. Alloc en O(1) usando
trailing_zeros sobre words de 64 bits. Free en O(1). Crece automáticamente el
storage cuando se agota el espacio.

## Capacidad
- 1 página de bitmap cubre `(PAGE_SIZE - HEADER_SIZE) * 8 = 130,560 páginas`
- 130,560 páginas × 16KB = ~2GB de datos cubiertos por un solo bitmap
- Suficiente para todas las fases del proyecto sin multi-página

## Layout del bitmap (body de página 1)
```
bits: 0 = USED, 1 = FREE
word 0 → páginas 0-63
word 1 → páginas 64-127
...
Bit ordering: LSB-first dentro de cada u64
```

## Estado inicial al crear
- Páginas 0 y 1 = USED (meta y bitmap, reservadas permanentemente)
- Páginas 2..total_pages = FREE

## API pública

### FreeList (bitmap puro, sin dependencia de storage)
- `FreeList::new(total_pages, reserved: &[u64])` → inicializa bitmap
- `FreeList::from_bytes(bytes, total_pages)` → deserializa desde body de página
- `to_bytes(&self, buf: &mut [u8])` → serializa para escribir en página
- `alloc(&mut self) → Option<u64>` → primera página libre, la marca usada
- `free(&mut self, page_id) → Result<()>` → marca libre
- `total_pages(&self) → u64`
- `grow(&mut self, new_total)` → extiende bitmap, nuevas páginas = FREE

### MmapStorage (nuevos métodos)
- `grow(&mut self, extra_pages: u64) → Result<u64>` → extiende archivo y remap, retorna primer page_id nuevo
- `set_page_count(&mut self, count: u64) → Result<()>` → actualiza meta sin releer (offset fijo)

### MemoryStorage (nuevos métodos)
- `free_page(&mut self, page_id: u64) → Result<()>` → devuelve página al pool

## Criterios de aceptación
- [ ] `alloc` retorna page_ids únicos y crecientes desde 2
- [ ] `free` + `alloc` → reutiliza el page_id liberado (page menor primero)
- [ ] bitmap serializa/deserializa sin pérdida
- [ ] MmapStorage crece automáticamente cuando se agotan páginas
- [ ] grow persiste en disco (reopen ve el nuevo page_count)
- [ ] Páginas 0 y 1 nunca son retornadas por alloc
- [ ] double-free retorna error

## Fuera del alcance
- Multi-bitmap (más de 130,560 páginas — fases futuras)
- Trait StorageEngine (1.6)
