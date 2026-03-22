# Plan: 1.3 — MmapStorage

## Archivos a crear/modificar
- `crates/nexusdb-storage/src/mmap.rs` — MmapStorage + DbFileMeta
- `crates/nexusdb-storage/src/lib.rs` — pub mod mmap
- `crates/nexusdb-storage/Cargo.toml` — agregar memmap2

## Dependencia
```toml
memmap2 = "0.9"
```

## Estructuras

```rust
const DB_FILE_MAGIC: u64 = 0x4E455855_53444201; // "NEXUSDB\1"
const DB_VERSION: u32    = 1;
const GROW_PAGES: u64    = 64; // 1MB por chunk

#[repr(C)]
struct DbFileMeta {
    db_magic:   u64,
    version:    u32,
    _pad:       u32,
    page_count: u64,
    _reserved:  [u8; PAGE_SIZE - HEADER_SIZE - 24],
}

pub struct MmapStorage {
    mmap: MmapMut,
    file: std::fs::File,
}
```

## Fases de implementación

1. Agregar `memmap2` a Cargo.toml
2. Definir `DbFileMeta` con const assert `size_of == PAGE_SIZE - HEADER_SIZE`
3. Implementar `MmapStorage::create(path)`
   - Crear archivo, truncar a `GROW_PAGES * PAGE_SIZE`
   - Mmap el archivo
   - Escribir página 0: `Page::new(Meta, 0)` + DbFileMeta en body
   - flush
4. Implementar `MmapStorage::open(path)`
   - Abrir archivo existente en modo read-write
   - Mmap
   - Verificar página 0: magic + checksum
5. Implementar `read_page(page_id) -> Result<&Page>`
   - Bounds check vs page_count de la meta
   - Cast ptr a &Page (SAFETY documented)
   - verify_checksum
6. Implementar `write_page(page_id, &Page)`
   - Bounds check
   - copy_from_slice bytes al offset correcto del mmap
7. Implementar `flush()` → `mmap.flush()`
8. Implementar `page_count()` → leer de meta
9. Método privado `meta(&self) -> &DbFileMeta`
10. Tests

## SAFETY para read_page
```
// SAFETY: offset = page_id * PAGE_SIZE está dentro de self.mmap (verificado por
// bounds check). El mmap está alineado a OS page (≥4KB, múltiplo de 64).
// PAGE_SIZE=16384 es múltiplo de 64, por lo que cada página está align(64).
// Page es repr(C, align(64)). No existen aliases mutables simultáneos porque
// write_page toma &mut self.
let page = unsafe { &*(ptr as *const Page) };
```

## Tests
- `test_create_and_open` — create + drop + open → meta válida
- `test_read_write_roundtrip` — write página 5 → read → datos idénticos
- `test_out_of_bounds_read` — page_id >= page_count → PageNotFound
- `test_flush_and_reopen` — write + flush + drop + open + read → datos persistidos
- `test_checksum_corruption_detected` — corromper byte en disco → read_page Err

## Antipatrones a evitar
- NO hacer MmapMut::map_anon — necesitamos un archivo real
- NO truncar el archivo después del mmap sin re-mapear
- NO calcular page_count desde file size en tiempo de lectura — usar la meta page
