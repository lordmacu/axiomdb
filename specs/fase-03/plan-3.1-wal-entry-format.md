# Plan: WAL Entry Format (Subfase 3.1)

## Archivos a crear/modificar

| Archivo | Acción | Qué hace |
|---|---|---|
| `crates/nexusdb-core/src/error.rs` | Modificar | Añadir 3 variantes WAL al DbError |
| `crates/nexusdb-wal/Cargo.toml` | Modificar | Añadir dependencia `crc32c` |
| `crates/nexusdb-wal/src/lib.rs` | Reemplazar | Módulos públicos del crate |
| `crates/nexusdb-wal/src/entry.rs` | Crear | `EntryType`, `WalEntry`, serialización |
| `crates/nexusdb-wal/tests/integration_wal_entry.rs` | Crear | Tests de integración |

---

## Algoritmo de serialización — `WalEntry::to_bytes()`

```
1. Calcular entry_len = tamaño total (ver fórmula abajo)
2. Reservar Vec<u8> con capacidad entry_len
3. Escribir entry_len         (4 bytes LE)
4. Escribir lsn               (8 bytes LE)
5. Escribir txn_id            (8 bytes LE)
6. Escribir entry_type as u8  (1 byte)
7. Escribir table_id          (4 bytes LE)
8. Escribir key_len as u16    (2 bytes LE)
9. Escribir key bytes         (key_len bytes)
10. Escribir old_val_len      (4 bytes LE)
11. Escribir old_value bytes  (old_val_len bytes)
12. Escribir new_val_len      (4 bytes LE)
13. Escribir new_value bytes  (new_val_len bytes)
14. Calcular CRC32c de buf[0..pos]
15. Escribir crc32c           (4 bytes LE)
16. Escribir entry_len        (4 bytes LE) — copia para backward scan
```

**Fórmula de entry_len:**
```
4 + 8 + 8 + 1 + 4 + 2 + key.len() + 4 + old.len() + 4 + new.len() + 4 + 4
= 43 + key.len() + old.len() + new.len()
```

**Constante MIN_ENTRY_LEN = 43** (entry sin key ni valores).

---

## Algoritmo de deserialización — `WalEntry::from_bytes(buf)`

```
1. Verificar buf.len() >= 4 (al menos entry_len) → WalEntryTruncated si no
2. Leer entry_len (4 bytes LE)
3. Verificar buf.len() >= entry_len → WalEntryTruncated si no
4. Leer lsn, txn_id, entry_type, table_id, key_len
5. Verificar entry_type es valor conocido → WalUnknownEntryType si no
6. Leer key[0..key_len]
7. Leer old_val_len, old_value[0..old_val_len]
8. Leer new_val_len, new_value[0..new_val_len]
9. Leer crc32c esperado
10. Calcular CRC32c de buf[0..pos_antes_de_crc]
11. Comparar → WalChecksumMismatch si no coinciden
12. Leer entry_len_2 (ignorar — solo usado para backward scan externo)
13. Verificar entry_len_2 == entry_len → WalEntryTruncated si no coinciden
14. Retornar (WalEntry, entry_len)
```

---

## Fases de implementación

### Paso 1 — Añadir errores WAL a DbError
En `nexusdb-core/src/error.rs`, sección `// ── WAL`:
```rust
#[error("WAL entry en LSN {lsn} tiene checksum inválido: esperado {expected:#010x}, obtenido {got:#010x}")]
WalChecksumMismatch { lsn: u64, expected: u32, got: u32 },

#[error("WAL entry en LSN {lsn} está truncado — el archivo puede estar corrupto")]
WalEntryTruncated { lsn: u64 },

#[error("WAL entry tiene tipo desconocido: {byte:#04x}")]
WalUnknownEntryType { byte: u8 },
```

### Paso 2 — Añadir crc32c a nexusdb-wal/Cargo.toml
```toml
crc32c = "0.6"
```
(misma versión que nexusdb-storage — ya probada)

### Paso 3 — Implementar `entry.rs`
En orden:
1. `EntryType` enum `#[repr(u8)]` con `TryFrom<u8>`
2. `WalEntry` struct con los 7 campos públicos
3. `impl WalEntry` con:
   - `pub fn new(...)` constructor
   - `pub fn serialized_len(&self) -> usize`
   - `pub fn to_bytes(&self) -> Vec<u8>`
   - `pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize), DbError>`
4. Tests unitarios inline `#[cfg(test)]`

### Paso 4 — Actualizar `lib.rs`
Exponer el módulo y los tipos públicos.

### Paso 5 — Tests de integración
En `tests/integration_wal_entry.rs`:
- Roundtrip para los 7 tipos de entry
- Detección de corrupción (bit flip en key, payload, header)
- Buffer truncado (buf más corto que entry_len)
- Múltiples entries encadenados (parsear N entries de un buffer)
- Backward scan: verificar entry_len == entry_len_2

---

## Tests a escribir

**Unitarios (en entry.rs `#[cfg(test)]`):**
- `test_entry_type_roundtrip` — `u8 → EntryType → u8` para los 7 tipos
- `test_entry_type_unknown` — byte inválido → `WalUnknownEntryType`
- `test_serialized_len_matches_to_bytes` — `serialized_len() == to_bytes().len()`
- `test_min_entry_len_is_43` — BEGIN serializado tiene exactamente 43 bytes
- `test_entry_len_repeated_at_end` — los últimos 4 bytes == los primeros 4 bytes

**Integración (en tests/):**
- `test_roundtrip_all_entry_types` — to_bytes → from_bytes para Begin, Commit, Rollback, Insert, Delete, Update, Checkpoint
- `test_crc_corruption_detected` — flip de 1 bit en key, old_value, new_value → WalChecksumMismatch
- `test_header_corruption_detected` — flip en txn_id o table_id → WalChecksumMismatch
- `test_truncated_buffer` — buf[..entry_len-1] → WalEntryTruncated
- `test_empty_buffer` — buf[..0] → WalEntryTruncated (lsn=0)
- `test_chain_of_entries` — serializar 100 entries, parsear todos en loop
- `test_backward_scan_offset` — entry_len_2 permite calcular offset anterior correctamente

---

## Antipatrones a evitar

- **NO usar `unwrap()`** en código de producción — todo es `?` o `map_err`
- **NO usar `unsafe`** — no hay cast de punteros, solo slices y LE bytes
- **NO serializar con serde** — el formato binario es manual para control total del layout
- **NO guardar crc ni entry_len en el struct** — se calculan en serialize/deserialize
- **NO asumir alineación** — leer con `from_le_bytes([buf[i], buf[i+1], ...])`, igual que decode_rid

---

## Riesgos

| Riesgo | Mitigación |
|---|---|
| CRC cubre entry_len_2 accidentalmente | CRC se calcula antes de escribir entry_len_2 — test verifica esto |
| key_len excede MAX_KEY_LEN en Fase 3 | `from_bytes` no valida key_len contra MAX_KEY_LEN — eso es responsabilidad del WalWriter. El formato es agnóstico al tamaño |
| entry_len overflow con values muy grandes | u32 soporta hasta 4GB por entry — suficiente para cualquier operación SQL |
| LSN=0 es ambiguo (entry sin LSN asignado) | El WalWriter asigna LSN — `WalEntry::new` recibe lsn como parámetro. LSN=0 solo en tests |
