# Spec: WAL Entry Format (Subfase 3.1)

## Qué construir (no cómo)

El tipo `WalEntry` y su serialización/deserialización binaria. Define el layout exacto
en disco de cada registro del Write-Ahead Log: campos, tamaños, orden de bytes,
y checksum de integridad. No incluye I/O (eso es 3.2 WalWriter y 3.3 WalReader).

---

## Decisiones de diseño fijadas

| Aspecto | Decisión | Razón |
|---|---|---|
| Granularidad | **Lógica** (operación semántica) | Entries pequeñas, recovery reaplica operación, compatible con replicación (Fase 18) y MVCC (Fase 7) |
| Scope del WAL | **Global** (`nexusdb.wal`) | Un solo LSN atómico, un solo fsync, transacciones multi-tabla sin coordinación |
| Encoding de valores | **Raw bytes** (`&[u8]`) | Sin overhead de tipo, extensible: hoy RecordId (10B), en Fase 4 filas completas |
| Endianness | **Little-endian** | Consistente con el resto del codebase (PageHeader, page_layout) |
| Checksum | **CRC32c** | Mismo algoritmo que las páginas — una sola dependencia |
| Scan hacia atrás | **entry_len repetido al final** | Permite recorrer el WAL de atrás hacia adelante en ROLLBACK y crash recovery |

---

## Layout binario del WalEntry

```
Offset    Tamaño  Campo          Tipo    Descripción
       0       4  entry_len      u32 LE  Longitud total del entry (todos los bytes incluyendo este campo y el final)
       4       8  lsn            u64 LE  Log Sequence Number — monotónico global, nunca se repite
      12       8  txn_id         u64 LE  Transaction ID (0 = autocommit / sin transacción explícita)
      20       1  entry_type     u8      Tipo de operación (ver EntryType abajo)
      21       4  table_id       u32 LE  Identificador de tabla (0 = sistema/meta)
      25       2  key_len        u16 LE  Longitud de la key en bytes (0 en BEGIN/COMMIT/ROLLBACK/CHECKPOINT)
      27  key_len  key           [u8]    Key bytes — hasta MAX_KEY_LEN (64 bytes en Fase 3)
       ?       4  old_val_len    u32 LE  Longitud del valor anterior (0 en INSERT, CHECKPOINT, txn control)
       ?  old_len  old_value     [u8]    Valor anterior serializado (RecordId en Fase 3, Row en Fase 4+)
       ?       4  new_val_len    u32 LE  Longitud del valor nuevo (0 en DELETE, txn control)
       ?  new_len  new_value     [u8]    Valor nuevo serializado
       ?       4  crc32c         u32 LE  CRC32c de todos los bytes anteriores en este entry
       ?       4  entry_len_2    u32 LE  Repetición de entry_len — permite scan backward sin leer todo
```

**Tamaño mínimo** (BEGIN / COMMIT / ROLLBACK con key_len=0, old=0, new=0):
`4 + 8 + 8 + 1 + 4 + 2 + 0 + 4 + 0 + 4 + 0 + 4 + 4 = 43 bytes`

**Tamaño típico** (INSERT con key 8 bytes, RecordId 10 bytes):
`43 + 8 + 10 = 61 bytes`

**Por qué table_id es u32 y no u16:**
u16 limita a 65535 tablas. Bases de datos reales (multi-schema, particionado, catálogo interno)
pueden superar ese límite. u32 cuesta 2 bytes más pero elimina una limitación estructural.

**Por qué entry_len_2 al final:**
El WalReader en crash recovery necesita recorrer el WAL hacia atrás para descartar entries
sin COMMIT. Con entry_len al inicio se avanza; con entry_len_2 al final se retrocede:
`pos_anterior = pos_actual - entry_len_2`.

---

## EntryType — tipos de entry

```rust
#[repr(u8)]
pub enum EntryType {
    Begin      = 1,  // inicio de transacción explícita
    Commit     = 2,  // commit de transacción
    Rollback   = 3,  // rollback de transacción
    Insert     = 4,  // insertar key+value (old_val_len=0)
    Delete     = 5,  // eliminar key (new_val_len=0, old_value = valor antes del delete)
    Update     = 6,  // actualizar key: old_value → new_value
    Checkpoint = 7,  // punto de checkpoint (key=0, old=0, new=0)
}
```

**Semántica de campos por tipo:**

| EntryType | key | old_value | new_value |
|---|---|---|---|
| Begin | vacío | vacío | vacío |
| Commit | vacío | vacío | vacío |
| Rollback | vacío | vacío | vacío |
| Insert | key insertada | vacío | valor nuevo |
| Delete | key eliminada | valor antes del delete | vacío |
| Update | key actualizada | valor anterior | valor nuevo |
| Checkpoint | vacío | vacío | vacío |

---

## Struct Rust

```rust
/// Entry del Write-Ahead Log.
///
/// Representa una operación lógica. Se serializa a bytes con `WalEntry::to_bytes()`
/// y se deserializa con `WalEntry::from_bytes()`.
pub struct WalEntry {
    pub lsn:        u64,
    pub txn_id:     u64,
    pub entry_type: EntryType,
    pub table_id:   u32,
    pub key:        Vec<u8>,
    pub old_value:  Vec<u8>,
    pub new_value:  Vec<u8>,
}
```

El campo `entry_len` no aparece en el struct — se calcula en `to_bytes()` y se verifica en
`from_bytes()`. El CRC32c tampoco se guarda en memoria — se calcula al serializar y se
verifica al deserializar.

---

## Inputs / Outputs

### `WalEntry::to_bytes() -> Vec<u8>`
- Input: `&WalEntry`
- Output: bytes listos para escribir al archivo WAL (incluye entry_len, CRC, entry_len_2)
- Errores: ninguno — serialización es infallible si el entry es válido

### `WalEntry::from_bytes(buf: &[u8]) -> Result<(WalEntry, usize), DbError>`
- Input: slice de bytes (puede contener más de un entry)
- Output: `(entry, bytes_consumidos)` — permite parsear entries encadenados
- Errores:
  - `WalChecksumMismatch { lsn, expected, got }` — CRC no coincide
  - `WalEntryTruncated { lsn }` — el buffer termina antes de que acabe el entry
  - `WalUnknownEntryType { byte }` — type byte desconocido

### `WalEntry::serialized_len(&self) -> usize`
- Calcula el tamaño total serializado sin allocar — usado por WalWriter para prealocar buffer

---

## Casos de uso

1. **Serializar INSERT**: key=`b"user:001"`, new_value=`RecordId{page:5, slot:3}` serializado a 10B → bytes correctos, CRC válido
2. **Deserializar roundtrip**: `to_bytes()` → `from_bytes()` produce el entry original idéntico
3. **Detectar corrupción**: modificar 1 byte en el payload → `from_bytes()` retorna `WalChecksumMismatch`
4. **Buffer truncado**: pasar menos bytes de los que necesita el entry → `WalEntryTruncated`
5. **Tipo desconocido**: byte de tipo `0xFF` → `WalUnknownEntryType`
6. **Entry mínimo**: BEGIN con key=[], old=[], new=[] → 43 bytes exactos
7. **Scan backward**: dado `entry_len_2` al final, calcular offset del entry anterior

---

## Criterios de aceptación

- [ ] `WalEntry` struct público con los 7 campos
- [ ] `EntryType` enum `#[repr(u8)]` con los 7 tipos
- [ ] `WalEntry::to_bytes()` produce el layout exacto descrito (verificar offsets con test)
- [ ] `WalEntry::from_bytes()` parsea correctamente y retorna `bytes_consumidos`
- [ ] `to_bytes()` → `from_bytes()` roundtrip produce entry idéntico para los 7 tipos
- [ ] CRC32c cubre todos los bytes antes del campo crc32c (no el crc mismo ni entry_len_2)
- [ ] Corrupción de 1 byte en payload detectada por CRC → `WalChecksumMismatch`
- [ ] Buffer truncado → `WalEntryTruncated`
- [ ] `entry_len == entry_len_2` en todo entry serializado (invariante)
- [ ] `serialized_len()` == `to_bytes().len()` para todos los tipos
- [ ] Variantes de error están en `DbError` (no tipos nuevos)
- [ ] Cero `unwrap()` en código de producción
- [ ] Cero `unsafe`

---

## Fuera del alcance

- I/O a disco (WalWriter — subfase 3.2)
- Lectura desde archivo (WalReader — subfase 3.3)
- BEGIN / COMMIT / ROLLBACK SQL (subfase 3.4)
- Crash recovery (subfase 3.5)
- WAL por tabla (decisión descartada — WAL es global)
- Compresión de entries (fase futura si el WAL crece demasiado)
- Entries de más de 4GB (u32 para lengths es suficiente para cualquier operación SQL razonable)

---

## Dependencias

- `nexusdb-core`: `DbError` (añadir variantes `WalChecksumMismatch`, `WalEntryTruncated`, `WalUnknownEntryType`)
- `crc32c` (ya en workspace desde Fase 1)
- No depende de `nexusdb-storage` ni `nexusdb-index`
