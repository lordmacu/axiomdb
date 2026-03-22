//! WAL Entry — formato binario de cada registro del Write-Ahead Log.
//!
//! ## Layout en disco
//!
//! ```text
//! Offset    Tamaño  Campo
//!      0         4  entry_len      u32 LE — longitud total del entry
//!      4         8  lsn            u64 LE — Log Sequence Number, monotónico global
//!     12         8  txn_id         u64 LE — Transaction ID (0 = autocommit)
//!     20         1  entry_type     u8     — EntryType
//!     21         4  table_id       u32 LE — identificador de tabla (0 = sistema)
//!     25         2  key_len        u16 LE — longitud de key en bytes
//!     27  key_len   key            [u8]   — bytes de la key
//!      ?         4  old_val_len    u32 LE — longitud del valor anterior (0 en INSERT)
//!      ?  old_len   old_value      [u8]   — valor anterior (vacío en INSERT)
//!      ?         4  new_val_len    u32 LE — longitud del valor nuevo (0 en DELETE)
//!      ?  new_len   new_value      [u8]   — valor nuevo (vacío en DELETE)
//!      ?         4  crc32c         u32 LE — CRC32c de todos los bytes anteriores
//!      ?         4  entry_len_2    u32 LE — copia de entry_len para scan backward
//! ```
//!
//! **Tamaño mínimo** (BEGIN/COMMIT/ROLLBACK sin key ni valores): 43 bytes.
//!
//! ## Scan backward
//!
//! Para recorrer el WAL hacia atrás (ROLLBACK, crash recovery):
//! ```text
//! pos_inicio_entry = pos_fin_entry - entry_len_2
//! ```
//! donde `entry_len_2` son los últimos 4 bytes del entry.

use nexusdb_core::error::DbError;

// ── Constantes ────────────────────────────────────────────────────────────────

/// Tamaño del header fijo antes de los campos variables.
/// 4 (entry_len) + 8 (lsn) + 8 (txn_id) + 1 (entry_type) + 4 (table_id) + 2 (key_len) = 27
const FIXED_HEADER: usize = 27;

/// Tamaño del trailer fijo después de los campos variables.
/// 4 (old_val_len) + 4 (new_val_len) + 4 (crc32c) + 4 (entry_len_2) = 16
/// — más los payloads variables entre medias.
/// El overhead fijo total (sin payloads) es:
/// FIXED_HEADER + 4 (old_val_len) + 4 (new_val_len) + 4 (crc32c) + 4 (entry_len_2) = 43
pub const MIN_ENTRY_LEN: usize = 43;

// ── EntryType ─────────────────────────────────────────────────────────────────

/// Tipo de operación registrada en el WAL.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    /// Inicio de transacción explícita (`BEGIN`).
    Begin = 1,
    /// Confirmación de transacción (`COMMIT`).
    Commit = 2,
    /// Cancelación de transacción (`ROLLBACK`).
    Rollback = 3,
    /// Inserción de un nuevo par key→value. `old_value` vacío.
    Insert = 4,
    /// Eliminación de un par key→value. `new_value` vacío, `old_value` = valor antes del delete.
    Delete = 5,
    /// Modificación de un par key→value. Ambos `old_value` y `new_value` presentes.
    Update = 6,
    /// Punto de checkpoint — marca hasta dónde los datos están en disco. Sin payload.
    Checkpoint = 7,
}

impl TryFrom<u8> for EntryType {
    type Error = DbError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Commit),
            3 => Ok(Self::Rollback),
            4 => Ok(Self::Insert),
            5 => Ok(Self::Delete),
            6 => Ok(Self::Update),
            7 => Ok(Self::Checkpoint),
            _ => Err(DbError::WalUnknownEntryType { byte }),
        }
    }
}

// ── WalEntry ──────────────────────────────────────────────────────────────────

/// Registro lógico del Write-Ahead Log.
///
/// Representa una operación semántica (INSERT, DELETE, UPDATE, control de transacción).
/// La serialización a bytes se hace con [`WalEntry::to_bytes`]; la deserialización
/// con [`WalEntry::from_bytes`].
///
/// Los campos `entry_len` y `crc32c` no se almacenan en memoria — se calculan
/// automáticamente al serializar y se verifican al deserializar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    /// Log Sequence Number — número monotónico global. Asignado por el WalWriter.
    pub lsn: u64,
    /// Identificador de transacción. `0` = autocommit (sin transacción explícita).
    pub txn_id: u64,
    /// Tipo de operación.
    pub entry_type: EntryType,
    /// Identificador de la tabla afectada. `0` = sistema/meta.
    pub table_id: u32,
    /// Key de la operación. Vacío en entries de control (Begin, Commit, Rollback, Checkpoint).
    pub key: Vec<u8>,
    /// Valor anterior al cambio. Vacío en INSERT y entries de control.
    pub old_value: Vec<u8>,
    /// Valor nuevo tras el cambio. Vacío en DELETE y entries de control.
    pub new_value: Vec<u8>,
}

impl WalEntry {
    /// Crea un nuevo `WalEntry`.
    pub fn new(
        lsn: u64,
        txn_id: u64,
        entry_type: EntryType,
        table_id: u32,
        key: Vec<u8>,
        old_value: Vec<u8>,
        new_value: Vec<u8>,
    ) -> Self {
        Self {
            lsn,
            txn_id,
            entry_type,
            table_id,
            key,
            old_value,
            new_value,
        }
    }

    /// Calcula el tamaño total serializado en bytes, sin allocar.
    ///
    /// Útil para que el WalWriter prealoque el buffer exacto.
    pub fn serialized_len(&self) -> usize {
        MIN_ENTRY_LEN + self.key.len() + self.old_value.len() + self.new_value.len()
    }

    /// Serializa el entry al formato binario listo para escribir al archivo WAL.
    ///
    /// El resultado incluye `entry_len`, todos los campos, `crc32c` y `entry_len_2`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let total = self.serialized_len();
        let mut buf = Vec::with_capacity(total);

        let total_u32 = total as u32;

        // ── Header fijo ──────────────────────────────────────────
        buf.extend_from_slice(&total_u32.to_le_bytes()); // entry_len
        buf.extend_from_slice(&self.lsn.to_le_bytes()); // lsn
        buf.extend_from_slice(&self.txn_id.to_le_bytes()); // txn_id
        buf.push(self.entry_type as u8); // entry_type
        buf.extend_from_slice(&self.table_id.to_le_bytes()); // table_id
        buf.extend_from_slice(&(self.key.len() as u16).to_le_bytes()); // key_len

        // ── Payload variable ─────────────────────────────────────
        buf.extend_from_slice(&self.key); // key

        buf.extend_from_slice(&(self.old_value.len() as u32).to_le_bytes()); // old_val_len
        buf.extend_from_slice(&self.old_value); // old_value

        buf.extend_from_slice(&(self.new_value.len() as u32).to_le_bytes()); // new_val_len
        buf.extend_from_slice(&self.new_value); // new_value

        // ── CRC32c (cubre todo lo anterior) ──────────────────────
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes()); // crc32c

        // ── Trailer para backward scan ───────────────────────────
        buf.extend_from_slice(&total_u32.to_le_bytes()); // entry_len_2

        debug_assert_eq!(
            buf.len(),
            total,
            "serialized_len() no coincide con to_bytes().len()"
        );
        buf
    }

    /// Deserializa un `WalEntry` desde un slice de bytes.
    ///
    /// Retorna `(entry, bytes_consumidos)`. El caller puede llamar en loop
    /// incrementando el offset para parsear entries encadenados.
    ///
    /// # Errores
    /// - [`DbError::WalEntryTruncated`] — el buffer es más corto que el entry
    /// - [`DbError::WalChecksumMismatch`] — el CRC32c no coincide
    /// - [`DbError::WalUnknownEntryType`] — tipo de entry desconocido
    pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize), DbError> {
        // Necesitamos al menos 4 bytes para leer entry_len
        if buf.len() < 4 {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        let entry_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        if buf.len() < entry_len {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        // Necesitamos al menos MIN_ENTRY_LEN bytes para un entry válido
        if entry_len < MIN_ENTRY_LEN {
            return Err(DbError::WalEntryTruncated { lsn: 0 });
        }

        // ── Leer header fijo ─────────────────────────────────────
        let lsn = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        let txn_id = u64::from_le_bytes([
            buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
        ]);
        let entry_type = EntryType::try_from(buf[20])?;
        let table_id = u32::from_le_bytes([buf[21], buf[22], buf[23], buf[24]]);
        let key_len = u16::from_le_bytes([buf[25], buf[26]]) as usize;

        let mut pos = FIXED_HEADER;

        // ── Leer payload variable ────────────────────────────────
        if pos + key_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let key = buf[pos..pos + key_len].to_vec();
        pos += key_len;

        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let old_val_len =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;

        if pos + old_val_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let old_value = buf[pos..pos + old_val_len].to_vec();
        pos += old_val_len;

        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let new_val_len =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;

        if pos + new_val_len > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let new_value = buf[pos..pos + new_val_len].to_vec();
        pos += new_val_len;

        // ── Verificar CRC32c ─────────────────────────────────────
        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let stored_crc = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        let computed_crc = crc32c::crc32c(&buf[..pos]);
        if stored_crc != computed_crc {
            return Err(DbError::WalChecksumMismatch {
                lsn,
                expected: stored_crc,
                got: computed_crc,
            });
        }
        pos += 4;

        // ── Verificar entry_len_2 (backward scan) ────────────────
        if pos + 4 > entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }
        let entry_len_2 =
            u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        if entry_len_2 != entry_len {
            return Err(DbError::WalEntryTruncated { lsn });
        }

        Ok((
            Self {
                lsn,
                txn_id,
                entry_type,
                table_id,
                key,
                old_value,
                new_value,
            },
            entry_len,
        ))
    }
}

// ── Tests unitarios ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_insert(lsn: u64) -> WalEntry {
        WalEntry::new(
            lsn,
            42,
            EntryType::Insert,
            1,
            b"user:001".to_vec(),
            vec![],
            vec![5u8, 0, 0, 0, 0, 0, 0, 0, 3, 0], // RecordId simulado (10 bytes)
        )
    }

    #[test]
    fn test_entry_type_roundtrip() {
        for byte in [1u8, 2, 3, 4, 5, 6, 7] {
            let et = EntryType::try_from(byte).unwrap();
            assert_eq!(et as u8, byte);
        }
    }

    #[test]
    fn test_entry_type_unknown() {
        assert!(matches!(
            EntryType::try_from(0u8),
            Err(DbError::WalUnknownEntryType { byte: 0 })
        ));
        assert!(matches!(
            EntryType::try_from(255u8),
            Err(DbError::WalUnknownEntryType { byte: 255 })
        ));
    }

    #[test]
    fn test_serialized_len_matches_to_bytes() {
        let entry = make_insert(1);
        assert_eq!(entry.serialized_len(), entry.to_bytes().len());
    }

    #[test]
    fn test_min_entry_len_begin() {
        let begin = WalEntry::new(1, 0, EntryType::Begin, 0, vec![], vec![], vec![]);
        assert_eq!(begin.to_bytes().len(), MIN_ENTRY_LEN);
        assert_eq!(begin.serialized_len(), MIN_ENTRY_LEN);
    }

    #[test]
    fn test_entry_len_repeated_at_end() {
        let entry = make_insert(5);
        let bytes = entry.to_bytes();
        let len = bytes.len();
        // Primeros 4 bytes == últimos 4 bytes
        let front = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let back = u32::from_le_bytes([
            bytes[len - 4],
            bytes[len - 3],
            bytes[len - 2],
            bytes[len - 1],
        ]);
        assert_eq!(
            front, back,
            "entry_len al inicio debe coincidir con entry_len_2 al final"
        );
    }

    #[test]
    fn test_roundtrip_insert() {
        let entry = make_insert(100);
        let bytes = entry.to_bytes();
        let (parsed, consumed) = WalEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_roundtrip_begin() {
        let entry = WalEntry::new(1, 7, EntryType::Begin, 0, vec![], vec![], vec![]);
        let bytes = entry.to_bytes();
        let (parsed, consumed) = WalEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, MIN_ENTRY_LEN);
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_crc_corruption_detected() {
        let entry = make_insert(10);
        let mut bytes = entry.to_bytes();
        // Flip de un bit en el payload (posición 30 = dentro de key)
        bytes[30] ^= 0xFF;
        assert!(matches!(
            WalEntry::from_bytes(&bytes),
            Err(DbError::WalChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_truncated_buffer() {
        let entry = make_insert(20);
        let bytes = entry.to_bytes();
        // Buffer más corto que entry_len
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            WalEntry::from_bytes(truncated),
            Err(DbError::WalEntryTruncated { .. })
        ));
    }

    #[test]
    fn test_empty_buffer() {
        assert!(matches!(
            WalEntry::from_bytes(&[]),
            Err(DbError::WalEntryTruncated { lsn: 0 })
        ));
    }
}
