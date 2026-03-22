# Spec: 1.6 — Trait StorageEngine

## Qué construir
Trait `StorageEngine` que unifica MmapStorage y MemoryStorage con una interfaz
intercambiable. Permite que todo el código de fases futuras (B-tree, WAL, SQL)
trabaje con cualquier storage sin acoplamientos concretos.

## Dónde vive
`nexusdb-storage::engine` — NO en nexusdb-core, porque usa `Page` y `PageType`
que están en nexusdb-storage. Ponerlo en nexusdb-core crearía un ciclo de deps.

nexusdb-core mantiene: `RecordId`, `PageId`, `TxnId`, `Index` trait (abstractos, sin Page).

## API del trait

```rust
pub trait StorageEngine: Send {
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError>;
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;
    fn flush(&mut self) -> Result<(), DbError>;
    fn page_count(&self) -> u64;
}
```

## Integración FreeList en MmapStorage
- `MmapStorage` embebe `FreeList` (cargada de página 1 al abrir/crear)
- `alloc_page`: usa freelist, crece si está lleno
- `free_page`: marca en freelist, persiste página 1
- Página 1 siempre es de tipo `Free` (bitmap page)

## Criterios de aceptación
- [ ] `MmapStorage` implementa `StorageEngine`
- [ ] `MemoryStorage` implementa `StorageEngine`
- [ ] `Box<dyn StorageEngine>` compila y funciona correctamente
- [ ] `MmapStorage::alloc_page` retorna page_ids únicos sin solaparse
- [ ] `MmapStorage::free_page` + `alloc_page` reutiliza el page_id
- [ ] FreeList de MmapStorage persiste entre reopen
- [ ] nexusdb-core::traits::StorageEngine eliminado (reemplazado por el nuevo)

## Fuera del alcance
- `Sync` (interior mutability, locks — Fase 7 MVCC)
- Encriptación, compresión
