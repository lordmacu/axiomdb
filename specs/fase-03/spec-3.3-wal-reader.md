# Spec: 3.3 — WalReader

## Qué construir

Un lector del archivo WAL que expone dos modos de scan:

- **Forward**: desde el inicio del WAL (o desde un LSN específico) hacia el final,
  parando en el primer entry truncado/corrupto — comportamiento correcto para crash recovery.
- **Backward**: desde el último entry válido hacia el inicio, usando el trailer `entry_len_2`
  para navegar sin leer el archivo entero — necesario para ROLLBACK.

El WalReader no mantiene el archivo abierto entre scans — cada iterador abre su propio
`File` handle. Esto elimina shared mutable state y permite múltiples scans concurrentes.

## Inputs / Outputs

- Input: `path: &Path` — ruta al archivo WAL existente
- Output (forward): `impl Iterator<Item = Result<WalEntry, DbError>>`
- Output (backward): `impl Iterator<Item = Result<WalEntry, DbError>>`
- Errores construcción: `DbError::WalInvalidHeader` si el header es inválido o el archivo no existe

### Comportamiento del iterator

**Forward:**
- Abre `File` en modo lectura, envuelto en `BufReader<File>` (64KB buffer)
- Verifica header mágico antes de empezar a iterar
- Salta entries con `LSN < from_lsn` (scan lineal desde WAL_HEADER_SIZE)
- Para cada entry: parsea, verifica CRC — si falla retorna el error y el iterator termina
- Llega a EOF → el iterator termina limpiamente (`None`)
- Entry truncado o corrupto → el item es `Err(...)` y el iterator termina

**Backward:**
- Abre `File` en modo lectura (seekable, sin BufReader — los seeks invalidan el buffer)
- Verifica header mágico
- Posición inicial: `file_end` — 4 bytes → leer `entry_len_2` → seek a `file_end - entry_len_2`
- Parsea el entry completo (lee `entry_len_2` bytes)
- Mueve cursor: `current_pos -= entry_len_2`
- Repite hasta llegar a `WAL_HEADER_SIZE` (inicio del área de entries)
- Cualquier error → el item es `Err(...)` y el iterator termina

## Casos de uso

1. **Crash recovery (happy path)**: WAL con 100 entries todos válidos → forward retorna 100 entries
2. **Crash recovery con tail truncado**: WAL con 50 entries válidos + bytes parciales al final → forward retorna 50 entries y luego `Err(WalEntryTruncated)`
3. **from_lsn skip**: forward con `from_lsn=51` salta los primeros 50 entries y retorna solo desde LSN 51
4. **Backward completo**: retorna entries en orden LSN decreciente (último → primero)
5. **WAL vacío** (solo header): forward y backward terminan en `None` de inmediato
6. **WAL corrupto en el medio**: entry 30 de 100 tiene CRC malo → forward retorna `Ok` para 1-29, `Err(WalChecksumMismatch)` en 30, fin

## Criterios de aceptación

- [ ] `WalReader::open()` verifica header y retorna error en archivo inválido
- [ ] `scan_forward(0)` retorna todos los entries del WAL en orden LSN creciente
- [ ] `scan_forward(N)` salta entries con `LSN < N` y retorna desde LSN N en adelante
- [ ] Forward se detiene en el primer entry corrupto/truncado retornando `Err`
- [ ] `scan_backward()` retorna todos los entries en orden LSN decreciente
- [ ] Backward se detiene en el primer entry corrupto retornando `Err`
- [ ] WAL vacío (solo header): ambos iteradores terminan limpiamente con `None`
- [ ] Ambos iteradores abren su propio file handle — no hay shared mutable state en `WalReader`
- [ ] Tests de integración en `tests/integration_wal_reader.rs`
- [ ] Sin `unwrap()` en `src/reader.rs`

## Fuera del alcance

- Indexado de LSNs para O(1) seek — scan lineal es suficiente para esta fase
- WAL de múltiples segmentos (rotation) — un archivo único
- Zero-copy con mmap — los entries tienen payloads variables con `Vec<u8>` owned
- Lectura concurrente thread-safe — el iterator se usa en un thread a la vez

## Dependencias

- `WalEntry::from_bytes()` — subfase 3.1 ✅
- `WalWriter` + constantes `WAL_HEADER_SIZE`, `WAL_MAGIC`, `WAL_VERSION` — subfase 3.2 ✅
- `DbError::WalEntryTruncated`, `WalChecksumMismatch`, `WalInvalidHeader` — ya en nexusdb-core ✅
