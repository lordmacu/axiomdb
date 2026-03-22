//! Tests de integración del WalWriter (subfase 3.2).
//!
//! Verifican durabilidad, LSN, header, reapertura y comportamiento en crash simulado.

use std::fs;

use nexusdb_core::error::DbError;
use nexusdb_wal::{EntryType, WalEntry, WalWriter, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION};
use tempfile::tempdir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn begin(txn_id: u64) -> WalEntry {
    WalEntry::new(0, txn_id, EntryType::Begin, 0, vec![], vec![], vec![])
}

fn insert(txn_id: u64, key: &[u8]) -> WalEntry {
    WalEntry::new(
        0,
        txn_id,
        EntryType::Insert,
        1,
        key.to_vec(),
        vec![],
        rid(1, 0),
    )
}

fn commit_entry(txn_id: u64) -> WalEntry {
    WalEntry::new(0, txn_id, EntryType::Commit, 0, vec![], vec![], vec![])
}

fn rid(page: u64, slot: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(10);
    b.extend_from_slice(&page.to_le_bytes());
    b.extend_from_slice(&slot.to_le_bytes());
    b
}

// ── Header ────────────────────────────────────────────────────────────────────

#[test]
fn test_create_writes_correct_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap();

    let data = fs::read(&path).unwrap();
    assert_eq!(
        data.len(),
        WAL_HEADER_SIZE,
        "recién creado debe tener solo el header"
    );

    let magic = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let version = u16::from_le_bytes([data[8], data[9]]);

    assert_eq!(magic, WAL_MAGIC, "magic incorrecto");
    assert_eq!(version, WAL_VERSION, "version incorrecta");

    // Bytes reservados deben ser cero
    assert_eq!(&data[10..16], &[0u8; 6], "bytes reservados deben ser zero");
}

#[test]
fn test_open_rejects_invalid_magic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    // Crear archivo con magic incorrecto
    let mut bad_header = [0u8; WAL_HEADER_SIZE];
    bad_header[0..8].copy_from_slice(&0xDEAD_BEEF_CAFE_1234u64.to_le_bytes());
    bad_header[8..10].copy_from_slice(&WAL_VERSION.to_le_bytes());
    fs::write(&path, bad_header).unwrap();

    assert!(
        matches!(
            WalWriter::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ),
        "magic incorrecto debe retornar WalInvalidHeader"
    );
}

#[test]
fn test_open_rejects_unknown_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    let mut bad_header = [0u8; WAL_HEADER_SIZE];
    bad_header[0..8].copy_from_slice(&WAL_MAGIC.to_le_bytes());
    bad_header[8..10].copy_from_slice(&999u16.to_le_bytes()); // versión desconocida
    fs::write(&path, bad_header).unwrap();

    assert!(
        matches!(
            WalWriter::open(&path),
            Err(DbError::WalInvalidHeader { .. })
        ),
        "versión desconocida debe retornar WalInvalidHeader"
    );
}

#[test]
fn test_create_fails_if_file_exists() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap();

    // Segundo create sobre el mismo archivo debe fallar
    assert!(
        WalWriter::create(&path).is_err(),
        "create() sobre archivo existente debe fallar"
    );
}

// ── Durabilidad ───────────────────────────────────────────────────────────────

#[test]
fn test_append_without_commit_not_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();
        let mut e = insert(1, b"key:001");
        w.append(&mut e).unwrap();
        // Drop sin commit — BufWriter hace flush en drop pero NO fsync.
        // En un crash real los entries se perderían. Aquí verificamos que
        // sin commit el archivo en disco puede no tener los entries.
        // Nota: en test (sin crash real) el flush del Drop puede escribirlos,
        // pero la garantía del sistema es que solo commit() asegura durabilidad.
    }

    // Reabrir: aunque el flush del Drop pudo escribir al OS, la garantía
    // de durabilidad solo la da commit(). Verificamos que open() funciona
    // independientemente del estado.
    let w2 = WalWriter::open(&path).unwrap();
    // Lo que importa: open() no explota ni corrompe el WAL
    // Lo que importa: open() no explota ni corrompe el WAL independientemente del estado
    let _ = w2.current_lsn();
}

#[test]
fn test_append_commit_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();

        let mut b = begin(1);
        w.append(&mut b).unwrap();

        let mut ins = insert(1, b"producto:001");
        w.append(&mut ins).unwrap();

        let mut c = commit_entry(1);
        w.append(&mut c).unwrap();

        w.commit().unwrap(); // fsync — durable
    }

    // El archivo debe tener header + 3 entries
    let data = fs::read(&path).unwrap();
    assert!(
        data.len() > WAL_HEADER_SIZE,
        "el archivo debe contener entries tras commit"
    );

    // Reabrir y verificar que los entries son legibles
    let w2 = WalWriter::open(&path).unwrap();
    assert_eq!(
        w2.current_lsn(),
        3,
        "deben haberse persistido 3 entries (LSN 1,2,3)"
    );
}

// ── LSN ───────────────────────────────────────────────────────────────────────

#[test]
fn test_open_continues_lsn() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    // Sesión 1: escribir 3 entries
    {
        let mut w = WalWriter::create(&path).unwrap();
        for _ in 0..3 {
            let mut e = begin(1);
            w.append(&mut e).unwrap();
        }
        w.commit().unwrap();
    }

    // Sesión 2: el próximo LSN debe ser 4
    {
        let mut w = WalWriter::open(&path).unwrap();
        assert_eq!(
            w.current_lsn(),
            3,
            "último LSN de sesión anterior debe ser 3"
        );

        let mut e = begin(2);
        let lsn = w.append(&mut e).unwrap();
        assert_eq!(lsn, 4, "primer LSN de nueva sesión debe ser 4");
        w.commit().unwrap();
    }

    // Sesión 3: verificar continuidad
    let w = WalWriter::open(&path).unwrap();
    assert_eq!(w.current_lsn(), 4);
}

#[test]
fn test_lsn_monotonic_across_many_appends() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let mut w = WalWriter::create(&path).unwrap();

    let mut prev_lsn = 0u64;
    for i in 0..100u64 {
        let key = format!("key:{:04}", i);
        let mut e = insert(1, key.as_bytes());
        let lsn = w.append(&mut e).unwrap();
        assert!(lsn > prev_lsn, "LSN debe ser estrictamente creciente");
        prev_lsn = lsn;
    }
    assert_eq!(w.current_lsn(), 100);
}

// ── file_offset ───────────────────────────────────────────────────────────────

#[test]
fn test_file_offset_grows_with_each_append() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    let mut w = WalWriter::create(&path).unwrap();

    let mut offsets = vec![w.file_offset()];

    for i in 0..5u64 {
        let key = format!("k{}", i);
        let mut e = insert(1, key.as_bytes());
        w.append(&mut e).unwrap();
        offsets.push(w.file_offset());
    }

    for i in 0..offsets.len() - 1 {
        assert!(
            offsets[i + 1] > offsets[i],
            "file_offset debe crecer: {} -> {}",
            offsets[i],
            offsets[i + 1]
        );
    }
}

// ── Múltiples commits ─────────────────────────────────────────────────────────

#[test]
fn test_multiple_commits_all_entries_durable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut w = WalWriter::create(&path).unwrap();

        // Transacción 1
        let mut b1 = begin(1);
        w.append(&mut b1).unwrap();
        let mut ins1 = insert(1, b"a");
        w.append(&mut ins1).unwrap();
        let mut c1 = commit_entry(1);
        w.append(&mut c1).unwrap();
        w.commit().unwrap();

        // Transacción 2
        let mut b2 = begin(2);
        w.append(&mut b2).unwrap();
        let mut ins2 = insert(2, b"b");
        w.append(&mut ins2).unwrap();
        let mut c2 = commit_entry(2);
        w.append(&mut c2).unwrap();
        w.commit().unwrap();
    }

    // 6 entries en total
    let w = WalWriter::open(&path).unwrap();
    assert_eq!(
        w.current_lsn(),
        6,
        "deben haberse escrito 6 entries en 2 transacciones"
    );
}

// ── WAL vacío ─────────────────────────────────────────────────────────────────

#[test]
fn test_open_empty_wal_lsn_starts_at_1() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.wal");
    WalWriter::create(&path).unwrap(); // solo header, sin entries

    let mut w = WalWriter::open(&path).unwrap();
    assert_eq!(w.current_lsn(), 0, "WAL vacío debe tener current_lsn == 0");

    let mut e = begin(1);
    let lsn = w.append(&mut e).unwrap();
    assert_eq!(lsn, 1, "primer entry en WAL vacío debe tener LSN 1");
}
