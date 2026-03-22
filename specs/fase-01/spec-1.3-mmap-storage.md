# Spec: 1.3 — MmapStorage

## Qué construir
Motor de storage basado en mmap: abrir/crear un archivo `.db`, leer y escribir
páginas con acceso zero-copy directo al mmap. Sin free list aún (eso es 1.5).

## Inputs / Outputs
- `create(path)` → `MmapStorage` con página 0 (Meta) inicializada
- `open(path)` → `MmapStorage` validando magic + checksum de página 0
- `read_page(page_id)` → `&Page` (zero-copy desde mmap), verifica checksum
- `write_page(page_id, &Page)` → copia bytes al mmap
- `flush()` → msync para garantizar durabilidad
- `page_count()` → u64

## Layout del archivo
```
offset 0          : Page 0 — Meta (header + DbFileMeta en body)
offset PAGE_SIZE  : Page 1
offset 2*PAGE_SIZE: Page 2
...
```

## DbFileMeta (en body de página 0, offset 64 del archivo)
```
offset  size  campo
0       8     db_magic: u64    — 0x4E455855_53444201 ("NEXUSDB\1")
8       4     version: u32     — versión del formato (1)
12      4     _pad: u32
16      8     page_count: u64  — total de páginas en el archivo
24      ...   reservado
```

## Crecimiento de archivo
- Unidad de crecimiento: **64 páginas = 1MB** (reduce syscalls de truncate)
- Al crear: archivo inicia con 64 páginas pre-allocadas
- Al escribir fuera del rango actual: error `PageNotFound` (alloc es 1.5)

## Criterios de aceptación
- [ ] `create` produce archivo válido con página 0 de tipo Meta
- [ ] `open` sobre archivo creado con `create` funciona correctamente
- [ ] `open` sobre archivo inexistente retorna `Err(DbError::Io(...))`
- [ ] `read_page(0)` retorna la página Meta con checksum válido
- [ ] `write_page` + `read_page` roundtrip conserva datos exactos
- [ ] `read_page` fuera de rango retorna `Err(DbError::PageNotFound)`
- [ ] `flush` completa sin error
- [ ] Test de crash: escribir → drop → reabrir → leer → mismo contenido

## Fuera del alcance
- Alloc/free de páginas (1.5)
- MemoryStorage (1.4)
- Concurrencia (Fase 7)

## Dependencias
- `nexusdb-storage::page` (ya existe)
- crate `memmap2`
