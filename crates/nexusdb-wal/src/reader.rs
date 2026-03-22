//! WalReader — lectura streaming del archivo WAL.
//!
//! ## Diseño
//!
//! `WalReader` es stateless — verifica el header en `open()` pero no mantiene
//! un file handle abierto. Cada scan abre su propio `File` handle, lo que elimina
//! shared mutable state y permite múltiples scans independientes.
//!
//! - [`WalReader::scan_forward`]: `BufReader<File>` — amortiza syscalls en lectura secuencial.
//! - [`WalReader::scan_backward`]: `File` seekable directo — los seeks invalidan el buffer
//!   de `BufReader`, por lo que backward usa el file handle directamente.
//!
//! ## Comportamiento ante corrupción
//!
//! Ambos iteradores retornan `Result<WalEntry>`. En el primer error (entry truncado,
//! CRC inválido, tipo desconocido), el item es `Err(...)` y el iterator termina.
//! El caller decide si propagar el error o ignorarlo según el caso de uso.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use nexusdb_core::error::DbError;

use crate::entry::WalEntry;
use crate::writer::{WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};

// ── WalReader ─────────────────────────────────────────────────────────────────

/// Lector del archivo WAL. Stateless — abre un `File` por cada scan.
pub struct WalReader {
    path: PathBuf,
}

impl WalReader {
    /// Abre un archivo WAL existente y verifica su header.
    ///
    /// No mantiene el file handle abierto — solo valida que el archivo
    /// es un WAL válido (magic + versión correctos).
    ///
    /// # Errores
    /// - [`DbError::WalInvalidHeader`] si el magic, versión o tamaño del header son incorrectos
    /// - [`DbError::Io`] si el archivo no existe o no se puede leer
    pub fn open(path: &Path) -> Result<Self, DbError> {
        verify_header(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Retorna un iterator que lee entries hacia adelante desde `from_lsn`.
    ///
    /// Entries con `LSN < from_lsn` se saltan en un scan lineal desde el inicio.
    /// Para leer todo el WAL usar `from_lsn = 0` (o `from_lsn = 1`).
    ///
    /// El iterator se detiene (retornando `Some(Err(...))`) en el primer entry
    /// truncado o corrupto, luego retorna `None`.
    pub fn scan_forward(&self, from_lsn: u64) -> Result<ForwardIter, DbError> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        // Saltar el header — ya fue verificado en open()
        reader.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        Ok(ForwardIter {
            reader,
            from_lsn,
            done: false,
        })
    }

    /// Retorna un iterator que lee entries hacia atrás desde el último entry válido.
    ///
    /// Retorna entries en orden LSN **decreciente** — el más reciente primero.
    /// Útil para ROLLBACK (deshacer operaciones de lo más reciente a lo más antiguo).
    pub fn scan_backward(&self) -> Result<BackwardIter, DbError> {
        let mut file = File::open(&self.path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        Ok(BackwardIter {
            file,
            cursor: file_len,
            done: false,
        })
    }
}

// ── ForwardIter ───────────────────────────────────────────────────────────────

/// Iterator de lectura secuencial del WAL (LSN creciente).
pub struct ForwardIter {
    reader: BufReader<File>,
    from_lsn: u64,
    done: bool,
}

impl Iterator for ForwardIter {
    type Item = Result<WalEntry, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            // ── Leer entry_len (4 bytes) ──────────────────────────────────────
            let mut len_buf = [0u8; 4];
            match self.reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // EOF limpio — el WAL terminó sin entries truncados
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e.into()));
                }
            }

            let entry_len = u32::from_le_bytes(len_buf) as usize;

            if entry_len < crate::entry::MIN_ENTRY_LEN {
                self.done = true;
                return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
            }

            // ── Leer el resto del entry ───────────────────────────────────────
            let rest_len = entry_len - 4;
            let mut buf = vec![0u8; entry_len];
            buf[0..4].copy_from_slice(&len_buf);

            match self.reader.read_exact(&mut buf[4..]) {
                Ok(()) => {}
                Err(_) => {
                    self.done = true;
                    return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
                }
            }

            let _ = rest_len; // usado implícitamente en el slice arriba

            // ── Parsear y verificar CRC ───────────────────────────────────────
            match WalEntry::from_bytes(&buf) {
                Ok((entry, _consumed)) => {
                    if entry.lsn < self.from_lsn {
                        // Saltar este entry y continuar al siguiente
                        continue;
                    }
                    return Some(Ok(entry));
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ── BackwardIter ──────────────────────────────────────────────────────────────

/// Iterator de lectura inversa del WAL (LSN decreciente).
///
/// Usa el campo `entry_len_2` (últimos 4 bytes de cada entry) para navegar
/// hacia atrás sin leer el archivo completo.
pub struct BackwardIter {
    file: File,
    /// Posición en el archivo del byte inmediatamente después del último entry no leído.
    /// Inicialmente = file_len.  Decrece con cada entry leído.
    cursor: u64,
    done: bool,
}

impl Iterator for BackwardIter {
    type Item = Result<WalEntry, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        // Llegamos al inicio del área de entries
        if self.cursor <= WAL_HEADER_SIZE as u64 {
            return None;
        }

        // ── Leer entry_len_2 (últimos 4 bytes del entry actual) ───────────────
        if self.cursor < WAL_HEADER_SIZE as u64 + 4 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let trailer_pos = self.cursor - 4;
        if let Err(e) = self.file.seek(SeekFrom::Start(trailer_pos)) {
            self.done = true;
            return Some(Err(e.into()));
        }

        let mut len_buf = [0u8; 4];
        if self.file.read_exact(&mut len_buf).is_err() {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let entry_len = u32::from_le_bytes(len_buf) as u64;

        if entry_len < crate::entry::MIN_ENTRY_LEN as u64 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Calcular inicio del entry ─────────────────────────────────────────
        if self.cursor < entry_len {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        let entry_start = self.cursor - entry_len;

        if entry_start < WAL_HEADER_SIZE as u64 {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Leer el entry completo ────────────────────────────────────────────
        if let Err(e) = self.file.seek(SeekFrom::Start(entry_start)) {
            self.done = true;
            return Some(Err(e.into()));
        }

        let mut buf = vec![0u8; entry_len as usize];
        if self.file.read_exact(&mut buf).is_err() {
            self.done = true;
            return Some(Err(DbError::WalEntryTruncated { lsn: 0 }));
        }

        // ── Parsear y verificar CRC ───────────────────────────────────────────
        match WalEntry::from_bytes(&buf) {
            Ok((entry, _)) => {
                self.cursor = entry_start;
                Some(Ok(entry))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

// ── Helpers privados ──────────────────────────────────────────────────────────

fn verify_header(path: &Path) -> Result<(), DbError> {
    let mut file = File::open(path)?;
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

// ── Tests unitarios ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryType;
    use crate::writer::WalWriter;
    use tempfile::tempdir;

    fn write_entries(path: &Path, count: u64) -> Vec<WalEntry> {
        let mut writer = WalWriter::create(path).unwrap();
        let mut entries = Vec::new();
        for i in 0..count {
            let mut e = WalEntry::new(
                0,
                i + 1,
                EntryType::Insert,
                1,
                format!("key:{:04}", i).into_bytes(),
                vec![],
                vec![i as u8, 0, 0, 0],
            );
            writer.append(&mut e).unwrap();
            entries.push(e);
        }
        writer.commit().unwrap();
        entries
    }

    #[test]
    fn test_open_valid_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        WalWriter::create(&path).unwrap();
        assert!(WalReader::open(&path).is_ok());
    }

    #[test]
    fn test_open_nonexistent() {
        let path = std::path::Path::new("/tmp/nonexistent_wal_file_nexusdb.wal");
        assert!(matches!(WalReader::open(path), Err(DbError::Io(_))));
    }

    #[test]
    fn test_open_invalid_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.wal");
        std::fs::write(&path, b"BADMAGIC00000000").unwrap();
        assert!(matches!(
            WalReader::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ));
    }

    #[test]
    fn test_forward_empty_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        WalWriter::create(&path).unwrap();
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.scan_forward(0).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_backward_empty_wal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        WalWriter::create(&path).unwrap();
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.scan_backward().unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_forward_all_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let written = write_entries(&path, 10);

        let reader = WalReader::open(&path).unwrap();
        let read: Vec<WalEntry> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(read.len(), 10);
        for (i, entry) in read.iter().enumerate() {
            assert_eq!(entry.lsn, written[i].lsn);
            assert_eq!(entry.key, written[i].key);
        }
    }

    #[test]
    fn test_forward_from_lsn_skips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 10);

        let reader = WalReader::open(&path).unwrap();
        let read: Vec<WalEntry> = reader
            .scan_forward(6)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(read.len(), 5); // LSN 6, 7, 8, 9, 10
        assert_eq!(read[0].lsn, 6);
        assert_eq!(read[4].lsn, 10);
    }

    #[test]
    fn test_backward_all_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 5);

        let reader = WalReader::open(&path).unwrap();
        let lsns: Vec<u64> = reader
            .scan_backward()
            .unwrap()
            .map(|r| r.unwrap().lsn)
            .collect();

        assert_eq!(lsns, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_backward_matches_forward_reversed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        write_entries(&path, 8);

        let reader = WalReader::open(&path).unwrap();

        let forward: Vec<WalEntry> = reader
            .scan_forward(0)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        let backward: Vec<WalEntry> = reader
            .scan_backward()
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(forward.len(), backward.len());
        for (f, b) in forward.iter().zip(backward.iter().rev()) {
            assert_eq!(f.lsn, b.lsn);
            assert_eq!(f.key, b.key);
        }
    }
}
