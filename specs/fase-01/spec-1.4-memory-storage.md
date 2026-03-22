# Spec: 1.4 — MemoryStorage

## Qué construir
Implementación en RAM del storage engine. Misma API que MmapStorage pero sin
ningún I/O. Usada en todos los tests unitarios de fases futuras.

## Inputs / Outputs
- `new()` → MemoryStorage vacío (page 0 Meta inicializada automáticamente)
- `read_page(page_id)` → `&Page`, verifica checksum
- `write_page(page_id, &Page)` → almacena copia en RAM
- `alloc_page_raw(page_type)` → u64 (page_id nuevo) — sin free list aún
- `flush()` → no-op (retorna Ok(()))
- `page_count()` → u64

## Criterios de aceptación
- [ ] `MemoryStorage::new()` retorna storage con página 0 válida
- [ ] `write_page` + `read_page` roundtrip conserva datos exactos
- [ ] `read_page` en página no escrita retorna `Err(PageNotFound)`
- [ ] `alloc_page_raw` retorna page_ids consecutivos desde 1
- [ ] `flush()` retorna `Ok(())`
- [ ] No requiere ningún archivo en disco

## Fuera del alcance
- Free list real (1.5)
- Trait StorageEngine (1.6)

## Dependencias
- `nexusdb-storage::page` (ya existe)
