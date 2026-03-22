# Plan: WalWriter (Subfase 3.2)

## Archivos a crear/modificar

| Archivo | Acción | Qué hace |
|---|---|---|
| `crates/nexusdb-core/src/error.rs` | Modificar | Añadir `WalInvalidHeader` |
| `crates/nexusdb-wal/src/writer.rs` | Crear | `WalWriter` completo |
| `crates/nexusdb-wal/src/lib.rs` | Modificar | Exponer `writer` módulo |
| `crates/nexusdb-wal/tests/integration_wal_writer.rs` | Crear | Tests de integración |

---

## Constantes

```rust
pub const WAL_MAGIC: u64   = 0x4E455855_53574100; // "NEXUSWAL\0"
pub const WAL_VERSION: u16 = 1;
pub const WAL_HEADER_SIZE: usize = 16;
```

---

## Algoritmo `create(path)`

```
1. File::create_new(path)  — falla si ya existe (no sobrescribir WAL existente)
2. Escribir header (16 bytes):
   - magic    (8 bytes LE)
   - version  (2 bytes LE)
   - reserved (6 bytes zeros)
3. fsync del file
4. Envolver en BufWriter (capacidad 64KB — amortiza syscalls)
5. next_lsn = 1
```

## Algoritmo `open(path)`

```
1. OpenOptions::new().read(true).append(true).open(path)
2. Leer primeros 16 bytes
3. Verificar magic == WAL_MAGIC        → WalInvalidHeader si no
4. Verificar version == WAL_VERSION    → WalInvalidHeader si no
5. Escanear entries para encontrar el último LSN válido:
   - Leer entry por entry desde offset 16
   - Guardar el LSN del último entry que se parsea sin error
   - Parar al primer entry truncado o con CRC inválido
6. next_lsn = ultimo_lsn_valido + 1  (o 1 si no hay entries)
7. Seek al final del archivo
8. Envolver en BufWriter
```

**Por qué escanear en open():**
Si el proceso murió después de escribir entries parciales, el archivo puede terminar
con bytes corruptos. El scan encuentra el último entry completo y válido, posicionando
el writer justo después para continuar sin corromper el WAL.

## Algoritmo `append(entry)`

```
1. entry.lsn = self.next_lsn
2. bytes = entry.to_bytes()
3. self.writer.write_all(&bytes)   — escribe al BufWriter (RAM)
4. self.next_lsn += 1
5. self.offset += bytes.len() as u64
6. return Ok(lsn_asignado)
```

## Algoritmo `commit()`

```
1. self.writer.flush()             — vaciar BufWriter al OS buffer
2. self.writer.get_ref().sync_all() — fsync: garantizar que el OS lo bajó a disco
3. return Ok(())
```

---

## Struct

```rust
pub struct WalWriter {
    writer:   BufWriter<File>,
    next_lsn: u64,
    offset:   u64,   // posición en bytes (incluye header)
}
```

---

## Fases de implementación

### Paso 1 — Añadir WalInvalidHeader a DbError
```rust
#[error("archivo WAL inválido en '{path}': magic o versión incorrectos")]
WalInvalidHeader { path: String },
```

### Paso 2 — Implementar writer.rs
En orden:
1. Constantes `WAL_MAGIC`, `WAL_VERSION`, `WAL_HEADER_SIZE`
2. `fn write_header(file: &mut File) -> Result<(), DbError>`
3. `fn read_and_verify_header(file: &mut File, path: &Path) -> Result<(), DbError>`
4. `fn scan_last_lsn(file: &mut File) -> Result<u64, DbError>` — escanea entries, retorna último LSN válido
5. `WalWriter::create()`
6. `WalWriter::open()`
7. `WalWriter::append()`
8. `WalWriter::commit()`
9. `WalWriter::current_lsn()` y `WalWriter::file_offset()`
10. Tests unitarios `#[cfg(test)]`

### Paso 3 — Actualizar lib.rs

### Paso 4 — Tests de integración

---

## Tests a escribir

**Unitarios (writer.rs `#[cfg(test)]`):**
- `test_header_size_is_16` — verificar que write_header escribe exactamente 16 bytes
- `test_lsn_starts_at_1` — primer append retorna LSN 1
- `test_lsn_increments` — N appends → LSNs 1..=N

**Integración (tests/):**
- `test_create_writes_header` — archivo tiene magic y version correctos en bytes 0-15
- `test_open_rejects_invalid_magic` — magic incorrecto → WalInvalidHeader
- `test_open_rejects_unknown_version` — version 999 → WalInvalidHeader
- `test_append_without_commit_not_durable` — append × N, drop sin commit → reabrir → entries ausentes
- `test_append_commit_durable` — append × N + commit → reabrir con File::open y leer bytes → entries presentes
- `test_open_continues_lsn` — create + append(×3) + commit + drop → open → append → LSN es 4
- `test_file_offset_grows` — file_offset() crece con cada append
- `test_current_lsn_before_and_after` — current_lsn() == 0 antes, == N después de N appends
- `test_create_fails_if_exists` — create() sobre archivo existente → error Io
- `test_multiple_commits` — append + commit + append + commit → reabrir → todos los entries presentes

---

## Antipatrones a evitar

- **NO truncar el archivo en `open()`** — crash recovery necesita el contenido existente
- **NO fsync en cada `append()`** — destruiría el throughput (objetivo: 180k ops/s)
- **NO usar `unwrap()`** en producción
- **NO hacer seek innecesario** — `append(true)` en OpenOptions garantiza writes al final

## Riesgos

| Riesgo | Mitigación |
|---|---|
| BufWriter no vacía en drop | `commit()` explícito antes de drop. Drop de BufWriter hace flush pero NO fsync — documentar |
| scan_last_lsn lento en WAL grande | En Fase futura: checkpoints truncan el WAL. En Fase 3 el WAL es pequeño |
| File::create_new no disponible en Rust < 1.77 | workspace usa rust-version = "1.80" ✓ |
