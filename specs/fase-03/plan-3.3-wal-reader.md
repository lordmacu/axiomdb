# Plan: 3.3 — WalReader

## Archivos a crear/modificar

- `crates/nexusdb-wal/src/reader.rs` — WalReader, ForwardIter, BackwardIter
- `crates/nexusdb-wal/src/lib.rs` — agregar `mod reader; pub use reader::WalReader;`
- `crates/nexusdb-wal/tests/integration_wal_reader.rs` — tests de integración

## Estructuras de datos

```rust
/// Lector del archivo WAL. Stateless — abre un File por cada scan.
pub struct WalReader {
    path: PathBuf,
}

/// Iterator forward — BufReader para amortizar syscalls.
pub struct ForwardIter {
    reader: BufReader<File>,
    from_lsn: u64,
    done: bool,    // true tras el primer error — el iterator termina
}

/// Iterator backward — File seekable directo (seeks invalidan BufReader).
pub struct BackwardIter {
    file: File,
    cursor: u64,   // posición del inicio del próximo entry a leer (hacia atrás)
    done: bool,
}
```

## Algoritmo

### `WalReader::open(path)`

```
1. File::open(path) → si falla, mapear a DbError::Io
2. Leer 16 bytes del header → si < 16 bytes, DbError::WalInvalidHeader
3. Verificar magic + version → si inválido, DbError::WalInvalidHeader
4. Retornar WalReader { path: path.to_path_buf() }
```

Nota: abrimos el archivo solo para verificar el header. No mantenemos el handle.

### `WalReader::scan_forward(from_lsn)`

```
1. File::open(path)
2. Verificar header (ya verificado en open, pero puede haber sido corrompido)
3. Seek a WAL_HEADER_SIZE
4. BufReader::new(file) con capacidad 64KB
5. Retornar ForwardIter { reader, from_lsn, done: false }
```

### `ForwardIter::next()`

```
1. Si done → return None
2. Leer 4 bytes (entry_len): si EOF → return None; si < 4 bytes → Err(Truncated), done=true
3. entry_len = u32::from_le_bytes(...)
4. Leer (entry_len - 4) bytes restantes → si < esperado → Err(Truncated), done=true
5. Construir slice completo (4 + resto) y llamar WalEntry::from_bytes()
6. Si Err → done=true, return Some(Err(...))
7. Si Ok((entry, _)):
   - Si entry.lsn < from_lsn → continuar (siguiente iteración, no retornar)
   - Si entry.lsn >= from_lsn → return Some(Ok(entry))
```

Optimización: en vez de leer 4 + N bytes en dos operaciones, usar un buffer pre-allocado.
Primero leer los 4 bytes, luego `read_exact` para los restantes `entry_len - 4`.

### `WalReader::scan_backward()`

```
1. File::open(path)
2. Verificar header
3. file_len = file.seek(End(0))
4. Si file_len == WAL_HEADER_SIZE → no hay entries, cursor = WAL_HEADER_SIZE
5. Retornar BackwardIter { file, cursor: file_len, done: false }
```

### `BackwardIter::next()`

```
1. Si done → return None
2. Si cursor <= WAL_HEADER_SIZE → return None  (llegamos al inicio)
3. Si cursor - WAL_HEADER_SIZE < 4 → Err(Truncated), done=true
4. file.seek(cursor - 4)
5. Leer 4 bytes → entry_len_2 (= longitud del entry que termina en cursor)
6. Si cursor < entry_len_2 → Err(Truncated), done=true
7. entry_start = cursor - entry_len_2
8. Si entry_start < WAL_HEADER_SIZE → Err(Truncated), done=true
9. file.seek(entry_start)
10. Leer entry_len_2 bytes → buf
11. WalEntry::from_bytes(&buf) → si Err → done=true, return Some(Err(...))
12. cursor = entry_start
13. return Some(Ok(entry))
```

## Fases de implementación

1. Crear `src/reader.rs` con `WalReader`, `ForwardIter`, `BackwardIter`
2. Exportar desde `src/lib.rs`
3. Escribir tests de integración en `tests/integration_wal_reader.rs`

## Tests a escribir

### Unitarios (en reader.rs)

- `test_open_valid_wal` — open() sobre WAL válido (vacío) → Ok
- `test_open_invalid_magic` — open() sobre archivo con magic incorrecto → Err(WalInvalidHeader)
- `test_open_nonexistent` — open() sobre path inexistente → Err(Io)
- `test_forward_empty_wal` — WAL con solo header → forward retorna None inmediatamente
- `test_backward_empty_wal` — ídem para backward

### Integración (`tests/integration_wal_reader.rs`)

- `test_forward_all_entries` — escribir N entries con writer, leer con forward desde LSN 0
- `test_forward_from_lsn` — skip de primeros K entries, verificar que se reciben desde LSN K+1
- `test_forward_stops_on_truncation` — escribir entries, truncar el archivo a mitad del último → forward retorna N-1 entries + Err al final
- `test_backward_all_entries` — verificar orden inverso de LSNs
- `test_backward_matches_forward_reversed` — backward debe ser el reverso exacto de forward
- `test_forward_crc_corruption` — flip de bit en payload de entry → Err(WalChecksumMismatch)

## Antipatrones a evitar

- **NO** leer todo el archivo en RAM en `open()` — el scan debe ser lazy/streaming
- **NO** usar `BufReader` en `BackwardIter` — los seeks invalidan el buffer interno
- **NO** compartir un `File` handle entre `ForwardIter` y `BackwardIter` — cada uno abre el suyo
- **NO** `unwrap()` en `src/reader.rs` — todo maneja `Result`
- **NO** retornar `Iterator<Item = WalEntry>` sin el `Result` — la corrupción es un caso real

## Riesgos

- **entry_len_2 corrupto en backward scan** → se detecta porque `WalEntry::from_bytes()` verifica
  el CRC y también verifica que `entry_len_2 == entry_len` → retorna `Err` → iterator termina
- **read_exact en ForwardIter puede bloquear en hardware lento** → aceptable, usamos `File` sincrónico
- **file_len cambia entre open() y scan** → para recovery, el WAL no se escribe concurrentemente
  con el read (recovery ocurre antes de abrir el motor) → no es un caso real
