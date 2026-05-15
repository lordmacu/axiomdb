// ── BACKUP / RESTORE executor (Phase 20.7) ────────────────────────────────────
//
// Included directly into executor/mod.rs — all names from that module's scope
// are available here. Do NOT add `use` statements for names already imported
// in mod.rs (HashMap, TxnManager, ColumnMeta, QueryResult, Path, Read, Write…).
//
// Binary format (.axbk):
//   Offset  Size  Field
//   0       8     magic = 0x4158494F4D424B01 ("AXIOMBK\1")
//   8       1     kind  = 0 (Full) | 1 (Incremental)
//   9       7     _pad
//   16      8     backup_lsn   (checkpoint LSN at backup time)
//   24      8     page_count   (storage.page_count() at backup time)
//   32      4     page_size    (always PAGE_SIZE = 16384)
//   36      4     _pad2
//   40      8     base_lsn     (Full: 0; Incremental: base backup_lsn)
//   48      8     delta_count  (Full: page_count; Incremental: # changed pages)
//   56      72    base_path    (NUL-terminated UTF-8; max 71 chars + NUL)
//   128+    ...   page entries: { page_id: u64, page_bytes: [u8; PAGE_SIZE] }

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Seek, SeekFrom},
};

use axiomdb_storage::PAGE_SIZE;

const BACKUP_MAGIC: [u8; 8] = *b"\x41\x58\x49\x4F\x4D\x42\x4B\x01";
const KIND_FULL: u8 = 0;
const KIND_INCREMENTAL: u8 = 1;
const BACKUP_HEADER_SIZE: usize = 128;
const PAGE_ENTRY_SIZE: usize = 8 + PAGE_SIZE;

// Byte offset of `checksum: u32` within a raw page buffer.
// PageHeader (repr(C)): magic:u64(0..8) + page_type:u8(8) + flags:u8(9)
//   + item_count:u16(10..12) + checksum:u32(12..16)
const PAGE_CHECKSUM_OFFSET: usize = 12;

fn backup_err(msg: impl Into<String>) -> DbError {
    DbError::BackupError { message: msg.into() }
}

fn backup_status(msg: String) -> QueryResult {
    QueryResult::Rows {
        columns: vec![ColumnMeta {
            name: "status".into(),
            data_type: axiomdb_types::DataType::Text,
            nullable: false,
            table_name: None,
        }],
        rows: vec![vec![axiomdb_types::Value::Text(msg)]],
    }
}

fn write_backup_header(
    w: &mut BufWriter<File>,
    kind: u8,
    backup_lsn: u64,
    page_count: u64,
    base_lsn: u64,
    delta_count: u64,
    base_path: &str,
) -> Result<(), DbError> {
    let bp = base_path.as_bytes();
    if bp.len() > 71 {
        return Err(backup_err("base_path exceeds 71 bytes"));
    }
    let mut hdr = [0u8; BACKUP_HEADER_SIZE];
    hdr[0..8].copy_from_slice(&BACKUP_MAGIC);
    hdr[8] = kind;
    hdr[16..24].copy_from_slice(&backup_lsn.to_le_bytes());
    hdr[24..32].copy_from_slice(&page_count.to_le_bytes());
    hdr[32..36].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    hdr[40..48].copy_from_slice(&base_lsn.to_le_bytes());
    hdr[48..56].copy_from_slice(&delta_count.to_le_bytes());
    hdr[56..56 + bp.len()].copy_from_slice(bp);
    use std::io::Write as _;
    w.write_all(&hdr).map_err(DbError::Io)
}

fn read_backup_header(f: &mut File, path: &str) -> Result<(u8, u64, u64, u64, u64, String), DbError> {
    use std::io::Read as _;
    let mut hdr = [0u8; BACKUP_HEADER_SIZE];
    f.read_exact(&mut hdr)
        .map_err(|_| backup_err(format!("cannot read header: {path}")))?;
    if hdr[0..8] != BACKUP_MAGIC {
        return Err(backup_err(format!("not a valid .axbk file: {path}")));
    }
    if hdr[7] != 0x01 {
        return Err(backup_err(format!(
            "unsupported backup version {}: {path}",
            hdr[7]
        )));
    }
    let kind = hdr[8];
    let backup_lsn = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    let page_count = u64::from_le_bytes(hdr[24..32].try_into().unwrap());
    let base_lsn = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
    let delta_count = u64::from_le_bytes(hdr[48..56].try_into().unwrap());
    let bp_bytes = &hdr[56..128];
    let nul = bp_bytes.iter().position(|&b| b == 0).unwrap_or(72);
    let base_path = std::str::from_utf8(&bp_bytes[..nul])
        .map_err(|_| backup_err(format!("invalid base_path encoding: {path}")))?
        .to_string();
    Ok((kind, backup_lsn, page_count, base_lsn, delta_count, base_path))
}

fn write_page_entry(w: &mut BufWriter<File>, page_id: u64, page_bytes: &[u8]) -> Result<(), DbError> {
    use std::io::Write as _;
    w.write_all(&page_id.to_le_bytes()).map_err(DbError::Io)?;
    w.write_all(page_bytes).map_err(DbError::Io)
}

/// Build a `page_id → checksum` map by scanning all entries in an open .axbk file.
/// The file cursor must be positioned right after the header before calling.
fn build_base_checksums(f: &mut File) -> HashMap<u64, u32> {
    use std::io::Read as _;
    let mut map = HashMap::new();
    let mut entry = vec![0u8; PAGE_ENTRY_SIZE];
    while f.read_exact(&mut entry).is_ok() {
        let page_id = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let cksum = u32::from_le_bytes(
            entry[8 + PAGE_CHECKSUM_OFFSET..8 + PAGE_CHECKSUM_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        map.insert(page_id, cksum);
    }
    map
}

// ── BACKUP ────────────────────────────────────────────────────────────────────

fn execute_backup(
    stmt: BackupStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError> {
    if std::path::Path::new(&stmt.dest).exists() {
        return Err(backup_err(format!(
            "destination already exists: {}",
            stmt.dest
        )));
    }
    match stmt.incremental_from {
        None => backup_full(stmt.dest, storage, txn),
        Some(base_path) => backup_incremental(stmt.dest, base_path, storage, txn),
    }
}

fn backup_full(
    dest: String,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError> {
    use std::io::Write as _;
    let checkpoint_lsn = txn.checkpoint(storage)?;
    let page_count = storage.page_count();

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
        .map_err(DbError::Io)?;
    let mut w = BufWriter::new(file);

    write_backup_header(&mut w, KIND_FULL, checkpoint_lsn, page_count, 0, page_count, "")?;

    let mut written: u64 = 0;
    for page_id in 0..page_count {
        if page_id % 64 == 0 {
            storage.prefetch_hint(page_id, 64);
        }
        let page_ref = storage.read_page(page_id)?;
        write_page_entry(&mut w, page_id, page_ref.as_bytes())?;
        written += 1;
    }
    w.flush().map_err(DbError::Io)?;

    let mb = (written * PAGE_SIZE as u64) / (1024 * 1024);
    Ok(backup_status(format!(
        "Full backup: {written} pages ({mb} MB) written to '{dest}'"
    )))
}

fn backup_incremental(
    dest: String,
    base_path: String,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError> {
    use std::io::Write as _;

    // 1. Open and validate the base backup.
    let mut base_file = File::open(&base_path)
        .map_err(|_| backup_err(format!("base backup not found: {base_path}")))?;
    let (base_kind, base_lsn, _, _, _, _) = read_backup_header(&mut base_file, &base_path)?;
    if base_kind != KIND_FULL {
        return Err(backup_err(format!(
            "base backup must be a Full backup, got Incremental: {base_path}"
        )));
    }

    // 2. Build checksum map (file cursor is right after header).
    let base_checksums = build_base_checksums(&mut base_file);

    // 3. Checkpoint current DB.
    let checkpoint_lsn = txn.checkpoint(storage)?;
    let page_count = storage.page_count();

    // 4. Find changed pages.
    let mut changed_pages: Vec<u64> = Vec::new();
    for page_id in 0..page_count {
        if page_id % 64 == 0 {
            storage.prefetch_hint(page_id, 64);
        }
        let page_ref = storage.read_page(page_id)?;
        let current_cksum = u32::from_le_bytes(
            page_ref.as_bytes()[PAGE_CHECKSUM_OFFSET..PAGE_CHECKSUM_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let base_cksum = base_checksums.get(&page_id).copied().unwrap_or(u32::MAX);
        if current_cksum != base_cksum {
            changed_pages.push(page_id);
        }
    }

    // 5. Resolve canonical base path for the header (≤71 bytes).
    let base_abs = std::fs::canonicalize(&base_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| base_path.clone());
    if base_abs.len() > 71 {
        return Err(backup_err(
            "base_path exceeds 71 bytes — use a shorter path or a symlink",
        ));
    }

    let delta_count = changed_pages.len() as u64;

    // 6. Write incremental .axbk.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
        .map_err(DbError::Io)?;
    let mut w = BufWriter::new(file);
    write_backup_header(
        &mut w,
        KIND_INCREMENTAL,
        checkpoint_lsn,
        page_count,
        base_lsn,
        delta_count,
        &base_abs,
    )?;
    for &page_id in &changed_pages {
        let page_ref = storage.read_page(page_id)?;
        write_page_entry(&mut w, page_id, page_ref.as_bytes())?;
    }
    w.flush().map_err(DbError::Io)?;

    Ok(backup_status(format!(
        "Incremental backup: {delta_count} of {page_count} pages changed, written to '{dest}'"
    )))
}

// ── RESTORE ───────────────────────────────────────────────────────────────────

fn execute_restore(stmt: RestoreStmt) -> Result<QueryResult, DbError> {
    if std::path::Path::new(&stmt.dest_path).exists() {
        return Err(backup_err(format!(
            "restore destination already exists: {}",
            stmt.dest_path
        )));
    }
    restore_from_source(&stmt.source, &stmt.dest_path)
}

fn restore_from_source(source: &str, dest_path: &str) -> Result<QueryResult, DbError> {
    let mut f = File::open(source)
        .map_err(|_| backup_err(format!("base backup not found: {source}")))?;
    let (kind, _backup_lsn, page_count, _base_lsn, delta_count, base_path) =
        read_backup_header(&mut f, source)?;

    if kind == KIND_INCREMENTAL {
        if !std::path::Path::new(&base_path).exists() {
            return Err(backup_err(format!("base backup not found: {base_path}")));
        }
        // First restore the base backup.
        let base_pages = write_pages_to_dest(&base_path, dest_path, None)?;
        // Then apply incremental pages (f is positioned right after its header).
        let _applied = write_pages_to_dest(source, dest_path, Some(&mut f))?;
        Ok(backup_status(format!(
            "Restored {page_count} pages to '{dest_path}' (base: {base_pages} + {delta_count} incremental)"
        )))
    } else {
        // Full backup: write all pages.
        let written = write_pages_to_dest(source, dest_path, Some(&mut f))?;
        Ok(backup_status(format!(
            "Restored {written} pages to '{dest_path}'"
        )))
    }
}

/// Write page entries from an .axbk file to `dest_path`.
///
/// If `file` is `Some`, reads from that already-opened file (cursor must be
/// right after the header). If `None`, opens `source_path` fresh and seeks
/// past the header.
///
/// Creates `dest_path` if it doesn't exist; overwrites individual pages if it
/// does (used for applying incremental pages on top of a restored full backup).
///
/// Returns the number of pages written.
fn write_pages_to_dest(
    source_path: &str,
    dest_path: &str,
    file: Option<&mut File>,
) -> Result<u64, DbError> {
    use std::io::Read as _;
    use std::os::unix::fs::FileExt;

    let mut owned_f;
    let f: &mut File = match file {
        Some(f) => f,
        None => {
            owned_f = File::open(source_path).map_err(DbError::Io)?;
            owned_f
                .seek(SeekFrom::Start(BACKUP_HEADER_SIZE as u64))
                .map_err(DbError::Io)?;
            &mut owned_f
        }
    };

    let dest_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest_path)
        .map_err(DbError::Io)?;

    let mut count = 0u64;
    let mut entry = vec![0u8; PAGE_ENTRY_SIZE];
    while f.read_exact(&mut entry).is_ok() {
        let page_id = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let offset = page_id * PAGE_SIZE as u64;
        dest_file.write_at(&entry[8..], offset).map_err(DbError::Io)?;
        count += 1;
    }
    dest_file.sync_all().map_err(DbError::Io)?;
    Ok(count)
}
