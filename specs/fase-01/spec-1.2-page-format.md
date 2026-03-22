# Spec: 1.2 — Formato de página

## Qué construir
Estructura de página en disco/memoria: `Page`, `PageHeader`, `PageType`.
Base de todo el storage engine — todas las fases futuras leen/escriben páginas.

## Inputs / Outputs
- Input: buffer crudo `[u8; PAGE_SIZE]` desde mmap o RAM
- Output: acceso tipado a header + data sin copia

## Constantes
- `PAGE_SIZE = 16_384` (16KB)
- `HEADER_SIZE = 64` (1 cache line)
- `PAGE_MAGIC = 0x4E455855_53444200u64` ("NEXUSDB\0")

## Layout de PageHeader (64 bytes, repr(C))
```
offset  size  campo
0       8     magic: u64          — detectar corrupción / archivo inválido
8       1     page_type: u8       — PageType discriminant
9       1     flags: u8           — bits: dirty, pinned, compressed, ...
10      2     item_count: u16     — número de ítems en la página
12      4     checksum: u32       — CRC32c de bytes [64..PAGE_SIZE]
16      8     page_id: u64        — posición lógica en el archivo
24      8     lsn: u64            — Log Sequence Number del último write
32      2     free_start: u16     — offset inicio espacio libre
34      2     free_end: u16       — offset fin espacio libre
36      28    _reserved: [u8;28]  — padding hasta 64 bytes
```

## PageType
```rust
Meta = 0      — página 0, cabecera del archivo
Data = 1      — filas de tabla (heap)
Index = 2     — nodo de B+ Tree
Overflow = 3  — datos que no caben en una página Data
Free = 4      — página en la free list
```

## Casos de uso
1. Crear página nueva en RAM → header inicializado, checksum calculado
2. Leer página desde disco → verificar magic + checksum, error si inválido
3. Escribir datos en página → actualizar free_start, recalcular checksum antes de flush

## Criterios de aceptación
- [ ] `size_of::<PageHeader>() == 64`
- [ ] `size_of::<Page>() == PAGE_SIZE`
- [ ] `align_of::<Page>() == 64`
- [ ] `Page::new(PageType::Data, page_id)` produce página válida
- [ ] `page.verify_checksum()` retorna `Ok(())` en página íntegra
- [ ] `page.verify_checksum()` retorna `Err(DbError::ChecksumMismatch)` si se corrompe 1 byte
- [ ] `page.header().magic == PAGE_MAGIC`

## Fuera del alcance
- Leer/escribir desde disco (eso es 1.3)
- Serialización de filas dentro del data area (fases posteriores)

## Dependencias
- `nexusdb-core` con `DbError` (ya existe)
- crate `crc32c` para checksum hardware-accelerado
