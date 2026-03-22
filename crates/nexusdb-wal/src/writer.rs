//! WalWriter — escritura append-only al archivo WAL global.
//!
//! ## Garantías
//!
//! - **Durabilidad**: solo los entries seguidos de [`WalWriter::commit`] están en disco.
//!   Entries escritos con [`WalWriter::append`] sin commit posterior se pierden en un crash.
//! - **LSN monotónico**: el WalWriter es el único dueño del contador de LSN.
//!   Ningún caller puede asignar LSNs duplicados o fuera de orden.
//! - **Integridad**: el header mágico de 16 bytes permite detectar archivos inválidos
//!   antes de intentar parsear entries.
//!
//! ## Uso típico
//!
//! ```rust,ignore
//! let mut w = WalWriter::create(path)?;
//!
//! // Entries de una transacción (van al BufWriter en RAM, sin fsync)
//! let mut begin = WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![]);
//! w.append(&mut begin)?;
//!
//! let mut insert = WalEntry::new(0, txn_id, EntryType::Insert, table_id, key, vec![], value);
//! w.append(&mut insert)?;
//!
//! let mut commit = WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![]);
//! w.append(&mut commit)?;
//!
//! // fsync — garantiza durabilidad en disco
//! w.commit()?;
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use nexusdb_core::error::DbError;

use crate::entry::WalEntry;

// ── Constantes del archivo WAL ────────────────────────────────────────────────

/// Magic number del archivo WAL: "NEXUSWAL\0" en little-endian.
pub const WAL_MAGIC: u64 = 0x004C_4157_5355_584E; // "NEXUSWAL" en bytes LE

/// Versión actual del formato WAL.
pub const WAL_VERSION: u16 = 1;

/// Tamaño del header del archivo WAL en bytes.
pub const WAL_HEADER_SIZE: usize = 16;

/// Capacidad del BufWriter interno — 64KB amortiza syscalls sin consumir memoria excesiva.
const BUF_CAPACITY: usize = 64 * 1024;

// ── WalWriter ─────────────────────────────────────────────────────────────────

/// Writer append-only para el archivo WAL global.
///
/// Gestiona el LSN global, bufferiza writes en RAM y hace fsync solo en commit.
pub struct WalWriter {
    writer: BufWriter<File>,
    next_lsn: u64,
    /// Posición actual en bytes en el archivo (incluye header + todos los entries escritos).
    offset: u64,
}

impl WalWriter {
    /// Crea un archivo WAL nuevo en `path`.
    ///
    /// Falla si el archivo ya existe — no sobrescribe WALs existentes.
    /// Escribe el header de 16 bytes y hace fsync antes de retornar.
    pub fn create(path: &Path) -> Result<Self, DbError> {
        let mut file = File::create_new(path)?;
        write_header(&mut file)?;
        file.sync_all()?;

        let offset = WAL_HEADER_SIZE as u64;
        Ok(Self {
            writer: BufWriter::with_capacity(BUF_CAPACITY, file),
            next_lsn: 1,
            offset,
        })
    }

    /// Abre un archivo WAL existente para continuar escribiendo.
    ///
    /// Verifica el header mágico y la versión. Escanea los entries existentes
    /// para recuperar el último LSN válido y posiciona el writer al final.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;

        read_and_verify_header(&mut file, path)?;

        let last_lsn = scan_last_lsn(&mut file)?;
        let next_lsn = last_lsn + 1;

        // Seek al final para continuar escribiendo (append mode ya lo garantiza,
        // pero lo hacemos explícito para que offset sea correcto)
        let offset = file.seek(SeekFrom::End(0))?;

        Ok(Self {
            writer: BufWriter::with_capacity(BUF_CAPACITY, file),
            next_lsn,
            offset,
        })
    }

    /// Asigna el próximo LSN al entry y lo escribe al buffer en RAM.
    ///
    /// **No hace fsync** — el entry no es durable hasta llamar a [`commit`](Self::commit).
    ///
    /// Retorna el LSN asignado al entry.
    pub fn append(&mut self, entry: &mut WalEntry) -> Result<u64, DbError> {
        let lsn = self.next_lsn;
        entry.lsn = lsn;

        let bytes = entry.to_bytes();
        self.writer.write_all(&bytes)?;

        self.next_lsn += 1;
        self.offset += bytes.len() as u64;

        Ok(lsn)
    }

    /// Vacía el buffer al OS y hace fsync — garantiza durabilidad en disco.
    ///
    /// Debe llamarse después de escribir el entry COMMIT de una transacción.
    /// Si el proceso muere antes de `commit()`, los entries en el buffer se pierden.
    pub fn commit(&mut self) -> Result<(), DbError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Retorna el último LSN asignado. `0` si no se ha escrito ningún entry.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn.saturating_sub(1)
    }

    /// Retorna la posición actual en bytes en el archivo (header + entries escritos).
    pub fn file_offset(&self) -> u64 {
        self.offset
    }
}

// ── Helpers privados ──────────────────────────────────────────────────────────

/// Escribe el header de 16 bytes al archivo.
fn write_header(file: &mut File) -> Result<(), DbError> {
    let mut header = [0u8; WAL_HEADER_SIZE];
    header[0..8].copy_from_slice(&WAL_MAGIC.to_le_bytes());
    header[8..10].copy_from_slice(&WAL_VERSION.to_le_bytes());
    // bytes 10-15: reserved, ya en zero
    file.write_all(&header)?;
    Ok(())
}

/// Lee y verifica el header del archivo WAL.
fn read_and_verify_header(file: &mut File, path: &Path) -> Result<(), DbError> {
    file.seek(SeekFrom::Start(0))?;

    let mut header = [0u8; WAL_HEADER_SIZE];
    file.read_exact(&mut header)
        .map_err(|_| DbError::WalInvalidHeader {
            path: path.display().to_string(),
        })?;

    let magic = u64::from_le_bytes([
        header[0], header[1], header[2], header[3], header[4], header[5], header[6], header[7],
    ]);
    let version = u16::from_le_bytes([header[8], header[9]]);

    if magic != WAL_MAGIC || version != WAL_VERSION {
        return Err(DbError::WalInvalidHeader {
            path: path.display().to_string(),
        });
    }

    Ok(())
}

/// Escanea los entries desde el offset 16 y retorna el LSN del último entry válido.
///
/// Se detiene al primer entry truncado o con CRC inválido — entries parciales
/// escritos antes de un crash no cuentan.
/// Retorna `0` si no hay entries válidos.
fn scan_last_lsn(file: &mut File) -> Result<u64, DbError> {
    file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;

    let file_len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;

    let data_len = (file_len as usize).saturating_sub(WAL_HEADER_SIZE);
    if data_len == 0 {
        return Ok(0);
    }

    let mut buf = vec![0u8; data_len];
    file.read_exact(&mut buf)?;

    let mut pos = 0usize;
    let mut last_lsn = 0u64;

    while pos < buf.len() {
        match WalEntry::from_bytes(&buf[pos..]) {
            Ok((entry, consumed)) => {
                last_lsn = entry.lsn;
                pos += consumed;
            }
            Err(_) => break, // entry truncado o corrupto — fin del WAL válido
        }
    }

    Ok(last_lsn)
}

// ── Tests unitarios ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryType;
    use tempfile::tempdir;

    fn make_insert(txn_id: u64, table_id: u32) -> WalEntry {
        WalEntry::new(
            0, // LSN asignado por el writer
            txn_id,
            EntryType::Insert,
            table_id,
            b"key:test".to_vec(),
            vec![],
            vec![1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
    }

    #[test]
    fn test_header_size_is_16() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), WAL_HEADER_SIZE);
    }

    #[test]
    fn test_lsn_starts_at_1() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        let mut entry = make_insert(1, 1);
        let lsn = w.append(&mut entry).unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(entry.lsn, 1);
    }

    #[test]
    fn test_lsn_increments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        for expected_lsn in 1u64..=10 {
            let mut entry = make_insert(1, 1);
            let lsn = w.append(&mut entry).unwrap();
            assert_eq!(lsn, expected_lsn);
        }
        assert_eq!(w.current_lsn(), 10);
    }

    #[test]
    fn test_current_lsn_zero_before_append() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let w = WalWriter::create(&path).unwrap();
        assert_eq!(w.current_lsn(), 0);
    }

    #[test]
    fn test_file_offset_grows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut w = WalWriter::create(&path).unwrap();

        let initial = w.file_offset();
        assert_eq!(initial, WAL_HEADER_SIZE as u64);

        let mut entry = make_insert(1, 1);
        w.append(&mut entry).unwrap();
        assert!(w.file_offset() > initial);
    }
}
