# Plan: 1.2 — Formato de página

## Archivos a crear/modificar
- `crates/nexusdb-storage/src/page.rs` — Page, PageHeader, PageType
- `crates/nexusdb-storage/src/lib.rs` — re-exportar módulo
- `crates/nexusdb-storage/Cargo.toml` — agregar crc32c, bytemuck

## Dependencias a agregar
```toml
# nexusdb-storage/Cargo.toml
crc32c   = "0.6"        # CRC32c con aceleración hardware (SSE4.2 / ARM CRC)
bytemuck = { version = "1", features = ["derive"] }
nexusdb-core = { workspace = true }
```

## Estructura de datos

```rust
pub const PAGE_SIZE: usize = 16_384;
pub const HEADER_SIZE: usize = 64;
pub const PAGE_MAGIC: u64 = 0x4E455855_53444200;

#[repr(u8)]
pub enum PageType { Meta=0, Data=1, Index=2, Overflow=3, Free=4 }

#[repr(C)]
struct PageHeader {
    magic:      u64,   // 8
    page_type:  u8,    // 1
    flags:      u8,    // 1
    item_count: u16,   // 2
    checksum:   u32,   // 4  → total 16
    page_id:    u64,   // 8  → 24
    lsn:        u64,   // 8  → 32
    free_start: u16,   // 2  → 34
    free_end:   u16,   // 2  → 36
    _reserved:  [u8;28], // 28 → 64
}

#[repr(C, align(64))]
pub struct Page {
    data: [u8; PAGE_SIZE],
}
```

## API pública

```rust
impl Page {
    pub fn new(page_type: PageType, page_id: u64) -> Self
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self, DbError>  // verifica magic+checksum
    pub fn header(&self) -> &PageHeader
    pub fn header_mut(&mut self) -> &mut PageHeader
    pub fn body(&self) -> &[u8]         // bytes[HEADER_SIZE..]
    pub fn body_mut(&mut self) -> &mut [u8]
    pub fn verify_checksum(&self) -> Result<(), DbError>
    pub fn update_checksum(&mut self)   // llamar antes de flush
}
```

## Fases de implementación
1. Agregar deps en Cargo.toml
2. Definir constantes y PageType
3. Definir PageHeader con `#[repr(C)]`, verificar size en const assert
4. Definir Page con `#[repr(C, align(64))]`, verificar size/align en const assert
5. Implementar `Page::new` — inicializar header, calcular checksum
6. Implementar `Page::from_bytes` — verificar magic, verificar checksum
7. Implementar `verify_checksum` y `update_checksum`
8. Tests unitarios

## Tests
- `test_page_size_and_alignment` — const asserts en runtime
- `test_new_page_valid` — nueva página pasa verify_checksum
- `test_checksum_detects_corruption` — mutar 1 byte → error
- `test_invalid_magic` — magic incorrecto → error
- `test_from_bytes_roundtrip` — new → as_bytes → from_bytes → OK

## Antipatrones a evitar
- NO usar `std::mem::transmute` directamente — usar bytemuck o punteros raw con SAFETY
- NO exponer PageHeader como pub fuera del crate — acceso solo por métodos de Page
- NO calcular checksum sobre el header (incluiría el campo checksum mismo) — solo sobre body

## Riesgos
- `bytemuck::Pod` requiere que no haya padding oculto → verificar con `size_of` asserts
- CRC32c vs CRC32: asegurarse de usar el crate `crc32c` (no `crc32fast`)
