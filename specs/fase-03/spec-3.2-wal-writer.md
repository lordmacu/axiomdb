# Spec: WalWriter (Subfase 3.2)

## Qué construir (no cómo)

El `WalWriter` — componente que escribe entries al archivo WAL de forma append-only,
gestiona el LSN global, hace fsync selectivo en COMMIT, y mantiene un header mágico
para validación de integridad del archivo.

---

## Decisiones de diseño fijadas

| Aspecto | Decisión | Razón |
|---|---|---|
| fsync | **Solo en COMMIT** (y ROLLBACK) | Estándar industria — entries intermedios en BufWriter, solo el commit paga latencia de disco |
| Header de archivo | **16 bytes mágicos** al inicio | Detectar archivo inválido antes de parsear entries |
| LSN | **WalWriter es dueño** del contador AtomicU64 | LSN siempre monotónico, imposible duplicar desde el exterior |
| Buffering | **BufWriter<File>** con flush en commit | Amortiza syscalls de write, cero overhead en entries intermedios |
| Apertura | **Crear o reabrir** un archivo existente | Crash recovery requiere reabrir el WAL sin truncarlo |

---

## Header del archivo WAL

```
Offset  Tamaño  Campo
     0       8  magic      — 0x4E455855_53574100 ("NEXUSWAL\0" en LE)
     8       2  version    — u16 LE, actualmente 1
    10       6  _reserved  — zeros, reservado para flags futuros
Total: 16 bytes
```

Al crear: escribir header + fsync.
Al reabrir: leer y verificar magic + version antes de hacer append.

---

## API pública

### `WalWriter::create(path: &Path) -> Result<Self, DbError>`
- Crea un archivo WAL nuevo. Falla si ya existe.
- Escribe el header de 16 bytes y hace fsync.
- Inicializa `next_lsn = 1`.

### `WalWriter::open(path: &Path) -> Result<Self, DbError>`
- Abre un archivo WAL existente para continuar escribiendo.
- Verifica magic y version del header.
- Hace seek al final del archivo.
- Lee el LSN del último entry para inicializar `next_lsn = last_lsn + 1`.
- Falla si el archivo no existe o el header es inválido.

### `WalWriter::append(&mut self, entry: &mut WalEntry) -> Result<u64, DbError>`
- Asigna el próximo LSN al entry (`entry.lsn = self.next_lsn`).
- Serializa el entry con `WalEntry::to_bytes()`.
- Escribe al BufWriter (sin fsync).
- Incrementa `next_lsn`.
- Retorna el LSN asignado.
- **No hace fsync** — solo escribe al buffer en RAM.

### `WalWriter::commit(&mut self) -> Result<(), DbError>`
- Hace flush del BufWriter al OS.
- Hace fsync del file descriptor — garantiza durabilidad en disco.
- Retorna error si el fsync falla.

### `WalWriter::current_lsn(&self) -> u64`
- Retorna el valor de `next_lsn - 1` (último LSN asignado).
- `0` si no se ha escrito ningún entry.

### `WalWriter::file_offset(&self) -> u64`
- Retorna la posición actual en bytes en el archivo (tamaño total escrito).
- Usado por el WalReader para saber hasta dónde leer.

---

## Comportamiento en crash

Si el proceso muere entre `append()` y `commit()`:
- Los entries en el BufWriter se pierden (nunca llegaron a disco).
- El WAL en disco queda en el estado del último `commit()` exitoso.
- El WalReader/crash recovery ignorará entries incompletos (CRC los detecta).

Este es el comportamiento correcto: solo los entries con COMMIT en disco son duraderos.

---

## Inputs / Outputs

### `append`
- Input: `&mut WalEntry` (el LSN se asigna aquí, por eso es `&mut`)
- Output: `Ok(lsn_asignado: u64)` o error
- Errores: `Io` si falla el write al BufWriter

### `commit`
- Input: ninguno
- Output: `Ok(())` o error
- Errores: `Io` si falla fsync

### `create` / `open`
- Errores:
  - `Io` — no se puede crear/abrir el archivo
  - `WalInvalidHeader { path }` — magic o version incorrectos (solo en `open`)

---

## Casos de uso

1. **Crear WAL nuevo**: `create()` → archivo con header de 16 bytes en disco
2. **Append sin commit**: `append()` × N → entries en buffer, nada en disco aún
3. **Commit**: `append()` × N → `commit()` → todos los entries en disco, durables
4. **Reabrir WAL existente**: `open()` → verifica header → continúa desde next_lsn correcto
5. **Crash entre append y commit**: reabrir → entries no están (nunca llegaron a disco)
6. **LSN siempre creciente**: dos `append()` consecutivos retornan LSNs n y n+1

---

## Criterios de aceptación

- [ ] `WalWriter::create()` crea archivo con header de 16 bytes exactos
- [ ] `WalWriter::open()` rechaza archivo sin magic correcto → `WalInvalidHeader`
- [ ] `WalWriter::open()` rechaza archivo con version desconocida → `WalInvalidHeader`
- [ ] `append()` asigna LSN monotónico creciente (verificar con N llamadas consecutivas)
- [ ] `append()` sin `commit()` → entries NO están en disco (simular con reabrir)
- [ ] `append()` + `commit()` → entries SÍ están en disco (verificar leyendo el archivo)
- [ ] `current_lsn()` retorna 0 antes del primer append, LSN correcto después
- [ ] `file_offset()` crece con cada append
- [ ] Reabrir con `open()` → `next_lsn` continúa desde donde se dejó
- [ ] Cero `unwrap()` en código de producción
- [ ] Cero `unsafe`

---

## Fuera del alcance

- WalReader (subfase 3.3)
- Rotación/truncado del WAL (fase futura — checkpoint)
- Compresión (fase futura)
- WAL por tabla (descartado — WAL es global)
- Escritura concurrente desde múltiples threads (Fase 7 con Mutex<WalWriter>)

---

## Dependencias

- `nexusdb-wal`: `WalEntry`, `EntryType`, `MIN_ENTRY_LEN` (subfase 3.1 ✅)
- `nexusdb-core`: `DbError` — añadir `WalInvalidHeader { path: String }`
- `std::fs`, `std::io::BufWriter` — sin dependencias externas nuevas
