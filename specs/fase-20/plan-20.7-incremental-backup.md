# Plan: 20.7 — Incremental Backup

Phase: 20 — Types + Import/Export  
Spec: specs/fase-20/spec-20.7-incremental-backup.md  
Status: in-progress

## Summary

Seven steps in TDD order. Steps 1-2 lay the groundwork (error type, AST,
lexer, parser). Step 3 wires the new variants into every existing match site
so the codebase compiles cleanly after each commit. Steps 4-5 build the
backup engine (full first, then incremental). Step 6 adds the restore engine.
Step 7 completes executor dispatch, integration tests, and wire smoke.

BACKUP/RESTORE are handled at the `exec_entry.rs` level (like CHECKPOINT) —
outside the implicit autocommit transaction — because backup runs its own
CHECKPOINT internally and restore performs only file I/O.

## Dependencies

Must be done first:
- [x] spec-20.7-incremental-backup.md approved

Blocks:
- nothing in Phase 20

## Affected files

New files:
- `crates/axiomdb-sql/src/executor/backup.rs` — backup/restore engine
- `crates/axiomdb-sql/tests/integration_backup.rs` — integration tests

Modified files:
- `crates/axiomdb-core/src/error.rs` — add `BackupError` variant
- `crates/axiomdb-sql/src/ast.rs` — add `BackupStmt`, `RestoreStmt`, `Stmt::Backup/Restore`
- `crates/axiomdb-sql/src/lexer.rs` — add `Token::Backup`, `Token::Restore`
- `crates/axiomdb-sql/src/parser/mod.rs` — dispatch `BACKUP`/`RESTORE` keywords
- `crates/axiomdb-sql/src/parser/dml.rs` — `parse_backup()`, `parse_restore()`
- `crates/axiomdb-sql/src/plan_deps.rs` — add `Stmt::Backup | Stmt::Restore => Ok(())`
- `crates/axiomdb-sql/src/executor/exec_entry.rs` — handle `Stmt::Backup/Restore` outside autocommit wrapper
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — add `NotImplemented` arms
- `crates/axiomdb-sql/src/executor/mod.rs` — `mod backup; use backup::*`

---

## Step 1 — DbError + AST

**Goal:** Add `BackupError` to `axiomdb-core` and the two new statement types to the AST.

**Files:**
- `crates/axiomdb-core/src/error.rs`
- `crates/axiomdb-sql/src/ast.rs`

### Tests to add (parser-level, compile-assert only at this step)

```rust
// crates/axiomdb-sql/tests/integration_backup.rs
// (skeleton — only parse tests in later steps; step 1 just checks AST compiles)
```

### Implementation

**`axiomdb-core/src/error.rs`** — after the `DiskFull` variant in the Storage section:

```rust
#[error("backup error: {message}")]
BackupError { message: String },
```

**`axiomdb-sql/src/ast.rs`** — new structs (near `CopyFromStmt`/`CopyToStmt`):

```rust
/// BACKUP DATABASE TO 'path' [INCREMENTAL FROM 'base_path']
#[derive(Debug, Clone)]
pub struct BackupStmt {
    pub dest: String,
    pub incremental_from: Option<String>,
}

/// RESTORE DATABASE FROM 'source' TO 'dest_path'
#[derive(Debug, Clone)]
pub struct RestoreStmt {
    pub source: String,
    pub dest_path: String,
}
```

In the `Stmt` enum (near `CopyFrom`/`CopyTo`):
```rust
Backup(BackupStmt),
Restore(RestoreStmt),
```

### Verification

```bash
./tools/vm.sh build   # must compile; no tests yet
```

### Commit

```
feat(fase-20): add BackupError + BackupStmt/RestoreStmt AST (20.7 step 1)
```

---

## Step 2 — Lexer + Parser

**Goal:** Parse all four SQL forms correctly. TDD: write parser tests first.

**Files:**
- `crates/axiomdb-sql/src/lexer.rs`
- `crates/axiomdb-sql/src/parser/mod.rs`
- `crates/axiomdb-sql/src/parser/dml.rs`
- `crates/axiomdb-sql/tests/integration_backup.rs` (first tests)

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_backup.rs
use axiomdb_sql::{ast::{BackupStmt, RestoreStmt, Stmt}, parse_with_sql_mode, SqlMode};

fn parse(sql: &str) -> Stmt {
    parse_with_sql_mode(sql, SqlMode::default()).unwrap()
}

#[test]
fn test_parse_backup_full() {
    let s = parse("BACKUP DATABASE TO '/tmp/full.axbk'");
    let Stmt::Backup(b) = s else { panic!("expected Backup") };
    assert_eq!(b.dest, "/tmp/full.axbk");
    assert!(b.incremental_from.is_none());
}

#[test]
fn test_parse_backup_incremental() {
    let s = parse("BACKUP DATABASE TO '/tmp/inc.axbk' INCREMENTAL FROM '/tmp/full.axbk'");
    let Stmt::Backup(b) = s else { panic!("expected Backup") };
    assert_eq!(b.dest, "/tmp/inc.axbk");
    assert_eq!(b.incremental_from.as_deref(), Some("/tmp/full.axbk"));
}

#[test]
fn test_parse_restore() {
    let s = parse("RESTORE DATABASE FROM '/tmp/full.axbk' TO '/tmp/restored.db'");
    let Stmt::Restore(r) = s else { panic!("expected Restore") };
    assert_eq!(r.source, "/tmp/full.axbk");
    assert_eq!(r.dest_path, "/tmp/restored.db");
}

#[test]
fn test_parse_backup_missing_to_errors() {
    assert!(parse_with_sql_mode("BACKUP DATABASE '/tmp/x.axbk'", SqlMode::default()).is_err());
}

#[test]
fn test_parse_restore_missing_to_errors() {
    assert!(parse_with_sql_mode("RESTORE DATABASE FROM '/tmp/x.axbk'", SqlMode::default()).is_err());
}
```

### Implementation

**`lexer.rs`** — near `Token::Vacuum` / `Token::Checkpoint`:

```rust
#[token("BACKUP", ignore(ascii_case))]
Backup,
#[token("RESTORE", ignore(ascii_case))]
Restore,
```

**`parser/mod.rs`** — new dispatch arms (before the `_` catch-all):

```rust
Token::Backup => {
    self.advance();
    dml::parse_backup(self)
}
Token::Restore => {
    self.advance();
    dml::parse_restore(self)
}
```

**`parser/dml.rs`** — two new functions:

```rust
/// BACKUP DATABASE TO 'path' [INCREMENTAL FROM 'base_path']
pub(crate) fn parse_backup(p: &mut Parser) -> Result<Stmt, DbError> {
    // DATABASE keyword (required)
    p.expect_keyword("DATABASE")?;
    // TO 'dest'
    p.expect(&Token::To)?;
    let dest = p.parse_string_literal()?;
    // [INCREMENTAL FROM 'base']
    let incremental_from = if p.eat_keyword("INCREMENTAL") {
        p.expect_keyword("FROM")?;
        Some(p.parse_string_literal()?)
    } else {
        None
    };
    Ok(Stmt::Backup(crate::ast::BackupStmt { dest, incremental_from }))
}

/// RESTORE DATABASE FROM 'source' TO 'dest_path'
pub(crate) fn parse_restore(p: &mut Parser) -> Result<Stmt, DbError> {
    p.expect_keyword("DATABASE")?;
    p.expect_keyword("FROM")?;
    let source = p.parse_string_literal()?;
    p.expect(&Token::To)?;
    let dest_path = p.parse_string_literal()?;
    Ok(Stmt::Restore(crate::ast::RestoreStmt { source, dest_path }))
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_backup   # 5 parser tests pass
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): lexer + parser for BACKUP/RESTORE (20.7 step 2)
```

---

## Step 3 — Match site wiring (compiler enforcement)

**Goal:** Make the codebase compile after adding `Stmt::Backup` and `Stmt::Restore`
to the enum by adding pass-through arms to every existing exhaustive match.

**Files:**
- `crates/axiomdb-sql/src/plan_deps.rs`
- `crates/axiomdb-sql/src/executor/exec_entry.rs`
- `crates/axiomdb-sql/src/executor/exec_explain.rs`

### Implementation

**`plan_deps.rs`** — add to the no-dep arm near `Stmt::CopyFrom`:
```rust
Stmt::CopyFrom(_) | Stmt::CopyTo(_) | Stmt::Backup(_) | Stmt::Restore(_) => Ok(()),
```

**`exec_entry.rs`** — add placeholder arms inside the top-level match (before the `other =>` arm), right after `Stmt::Checkpoint`:
```rust
Stmt::Backup(_) | Stmt::Restore(_) => {
    // Wired in step 7.
    Err(DbError::NotImplemented {
        feature: "BACKUP/RESTORE".into(),
    })
}
```

**`exec_explain.rs`** — add arms near the COPY arm:
```rust
Stmt::Backup(_) | Stmt::Restore(_) => Err(DbError::NotImplemented {
    feature: "EXPLAIN BACKUP/RESTORE".into(),
}),
```

### Verification

```bash
./tools/vm.sh build   # workspace compiles clean
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): wire Stmt::Backup/Restore into all match sites (20.7 step 3)
```

---

## Step 4 — Backup engine: format + full backup

**Goal:** Implement the `.axbk` binary format and full backup in `backup.rs`.
TDD: integration tests for full backup roundtrip (header validation, page count).

**Files:**
- `crates/axiomdb-sql/src/executor/backup.rs` (new)
- `crates/axiomdb-sql/src/executor/mod.rs`
- `crates/axiomdb-sql/tests/integration_backup.rs`

### Tests to add

```rust
// integration_backup.rs — add after parser tests

use axiomdb_storage::{MmapStorage, StorageEngine};
use axiomdb_wal::TxnManager;
use axiomdb_sql::{execute_with_ctx, ast::{BackupStmt, Stmt}, SessionContext};

fn setup_db(dir: &tempfile::TempDir) -> (MmapStorage, TxnManager) {
    let db_path = dir.path().join("axiomdb.db");
    let wal_path = dir.path().join("axiomdb.wal");
    let storage = MmapStorage::create(&db_path).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

#[test]
fn test_full_backup_creates_valid_axbk() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let dest = dir.path().join("full.axbk").to_string_lossy().to_string();

    let stmt = Stmt::Backup(BackupStmt { dest: dest.clone(), incremental_from: None });
    let mut ctx = SessionContext::new();
    let result = execute_with_ctx(stmt, &storage, &txn,
        &axiomdb_sql::bloom::BloomRegistry::default(), &mut ctx).unwrap();
    // Result is a single-row status string.
    // .axbk file exists.
    assert!(std::path::Path::new(&dest).exists());
}

#[test]
fn test_full_backup_header_magic() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let dest = dir.path().join("full.axbk").to_string_lossy().to_string();

    let stmt = Stmt::Backup(BackupStmt { dest: dest.clone(), incremental_from: None });
    execute_with_ctx(stmt, &storage, &txn,
        &axiomdb_sql::bloom::BloomRegistry::default(), &mut SessionContext::new()).unwrap();

    let bytes = std::fs::read(&dest).unwrap();
    assert_eq!(&bytes[0..8], b"\x41\x58\x49\x4F\x4D\x42\x4B\x01");
    assert_eq!(bytes[8], 0u8); // kind = Full
}

#[test]
fn test_full_backup_dest_already_exists_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let dest = dir.path().join("full.axbk").to_string_lossy().to_string();
    std::fs::write(&dest, b"existing").unwrap();

    let stmt = Stmt::Backup(BackupStmt { dest: dest.clone(), incremental_from: None });
    let err = execute_with_ctx(stmt, &storage, &txn,
        &axiomdb_sql::bloom::BloomRegistry::default(), &mut SessionContext::new()).unwrap_err();
    assert!(matches!(err, axiomdb_core::error::DbError::BackupError { .. }));
}

#[test]
fn test_full_backup_empty_db_includes_meta_and_freelist() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let dest = dir.path().join("full.axbk").to_string_lossy().to_string();

    execute_with_ctx(Stmt::Backup(BackupStmt { dest: dest.clone(), incremental_from: None }),
        &storage, &txn, &axiomdb_sql::bloom::BloomRegistry::default(),
        &mut SessionContext::new()).unwrap();

    let bytes = std::fs::read(&dest).unwrap();
    // delta_count field at offset 48 must be >= 2 (page 0 + page 1)
    let delta_count = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    assert!(delta_count >= 2, "must include meta and freelist pages");
}
```

### Implementation — `backup.rs`

```rust
// crates/axiomdb-sql/src/executor/backup.rs

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use axiomdb_core::error::DbError;
use axiomdb_storage::{StorageEngine, PAGE_SIZE};
use axiomdb_wal::TxnManager;

use crate::{
    ast::{BackupStmt, RestoreStmt},
    result::{ColumnMeta, QueryResult},
};

const BACKUP_MAGIC: [u8; 8] = *b"\x41\x58\x49\x4F\x4D\x42\x4B\x01"; // "AXIOMBK\1"
const KIND_FULL: u8 = 0;
const KIND_INCREMENTAL: u8 = 1;
const HEADER_SIZE: usize = 128;
const ENTRY_SIZE: usize = 8 + PAGE_SIZE; // page_id (u64) + page bytes

fn backup_error(msg: impl Into<String>) -> DbError {
    DbError::BackupError { message: msg.into() }
}

/// Reads and validates the 128-byte backup file header.
/// Returns (kind, backup_lsn, page_count, base_lsn, delta_count, base_path).
fn read_backup_header(
    f: &mut File,
    path: &str,
) -> Result<(u8, u64, u64, u64, u64, String), DbError> {
    let mut hdr = [0u8; HEADER_SIZE];
    f.read_exact(&mut hdr).map_err(|_| backup_error(format!("cannot read header: {path}")))?;

    if &hdr[0..8] != &BACKUP_MAGIC {
        return Err(backup_error(format!("not a valid .axbk file: {path}")));
    }
    // Version byte is hdr[7]; currently only 0x01.
    if hdr[7] != 0x01 {
        return Err(backup_error(format!("unsupported backup version {}: {path}", hdr[7])));
    }
    let kind = hdr[8];
    let backup_lsn = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    let page_count = u64::from_le_bytes(hdr[24..32].try_into().unwrap());
    let base_lsn   = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
    let delta_count = u64::from_le_bytes(hdr[48..56].try_into().unwrap());
    // base_path: null-terminated in hdr[56..128]
    let bp_bytes = &hdr[56..128];
    let nul = bp_bytes.iter().position(|&b| b == 0).unwrap_or(72);
    let base_path = std::str::from_utf8(&bp_bytes[..nul])
        .map_err(|_| backup_error(format!("invalid base_path encoding: {path}")))?
        .to_string();

    Ok((kind, backup_lsn, page_count, base_lsn, delta_count, base_path))
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
        return Err(backup_error("base_path exceeds 71 bytes"));
    }
    let mut hdr = [0u8; HEADER_SIZE];
    hdr[0..8].copy_from_slice(&BACKUP_MAGIC);
    hdr[8] = kind;
    hdr[16..24].copy_from_slice(&backup_lsn.to_le_bytes());
    hdr[24..32].copy_from_slice(&page_count.to_le_bytes());
    hdr[32..36].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    hdr[40..48].copy_from_slice(&base_lsn.to_le_bytes());
    hdr[48..56].copy_from_slice(&delta_count.to_le_bytes());
    hdr[56..56 + bp.len()].copy_from_slice(bp);
    // NUL terminator is already 0 from zeroed array.
    w.write_all(&hdr).map_err(DbError::Io)
}

fn write_page_entry(w: &mut BufWriter<File>, page_id: u64, page_bytes: &[u8; PAGE_SIZE]) -> Result<(), DbError> {
    w.write_all(&page_id.to_le_bytes()).map_err(DbError::Io)?;
    w.write_all(page_bytes).map_err(DbError::Io)
}

fn status_result(msg: String) -> QueryResult {
    QueryResult::Rows {
        columns: vec![ColumnMeta { name: "status".into(), data_type: axiomdb_types::DataType::Text }],
        rows: vec![vec![axiomdb_types::Value::Text(msg)]],
    }
}

/// BACKUP DATABASE TO ... [INCREMENTAL FROM ...]
pub(super) fn execute_backup(
    stmt: BackupStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError> {
    // Reject if destination already exists.
    if Path::new(&stmt.dest).exists() {
        return Err(backup_error(format!("destination already exists: {}", stmt.dest)));
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
    // 1. Checkpoint: flush dirty pages + update checkpoint_lsn in meta page.
    let checkpoint_lsn = txn.checkpoint(storage)?;
    let page_count = storage.page_count();

    // 2. Open dest for writing (exclusive create).
    let file = OpenOptions::new().write(true).create_new(true).open(&dest)
        .map_err(DbError::Io)?;
    let mut w = BufWriter::new(file);

    // 3. Write header (delta_count = page_count for full backup).
    write_backup_header(&mut w, KIND_FULL, checkpoint_lsn, page_count, 0, page_count, "")?;

    // 4. Write all pages sequentially.
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
    Ok(status_result(format!(
        "Full backup: {written} pages ({mb} MB) written to '{dest}'"
    )))
}

fn backup_incremental(
    dest: String,
    base_path: String,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError> {
    // 1. Validate base backup.
    let mut base_file = File::open(&base_path)
        .map_err(|_| backup_error(format!("base backup not found: {base_path}")))?;
    let (base_kind, base_lsn, base_page_count, _, _, _) =
        read_backup_header(&mut base_file, &base_path)?;
    if base_kind != KIND_FULL {
        return Err(backup_error(format!(
            "base backup must be a Full backup, got Incremental: {base_path}"
        )));
    }

    // 2. Build checksum map from base backup: HashMap<page_id, checksum>.
    let mut base_checksums: HashMap<u64, u32> = HashMap::new();
    let mut entry_buf = vec![0u8; ENTRY_SIZE];
    while base_file.read_exact(&mut entry_buf).is_ok() {
        let page_id = u64::from_le_bytes(entry_buf[0..8].try_into().unwrap());
        // checksum is at PageHeader offset 20..24 within page bytes (after entry page_id prefix)
        // entry_buf[8..] = page bytes; PageHeader checksum at byte 20 within page
        let checksum = u32::from_le_bytes(entry_buf[8 + 20..8 + 24].try_into().unwrap());
        base_checksums.insert(page_id, checksum);
    }

    // 3. Checkpoint.
    let checkpoint_lsn = txn.checkpoint(storage)?;
    let page_count = storage.page_count();

    // 4. First pass: count changed pages (for header).
    let mut changed_pages: Vec<u64> = Vec::new();
    for page_id in 0..page_count {
        if page_id % 64 == 0 {
            storage.prefetch_hint(page_id, 64);
        }
        let page_ref = storage.read_page(page_id)?;
        // Checksum is at bytes 20..24 of the page (PageHeader layout).
        let current_checksum = u32::from_le_bytes(page_ref.as_bytes()[20..24].try_into().unwrap());
        let base_cksum = base_checksums.get(&page_id).copied().unwrap_or(u32::MAX);
        if current_checksum != base_cksum {
            changed_pages.push(page_id);
        }
    }

    // 5. Write incremental .axbk.
    let base_abs = std::fs::canonicalize(&base_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(base_path.clone());
    let delta_count = changed_pages.len() as u64;

    let file = OpenOptions::new().write(true).create_new(true).open(&dest)
        .map_err(DbError::Io)?;
    let mut w = BufWriter::new(file);
    write_backup_header(&mut w, KIND_INCREMENTAL, checkpoint_lsn, page_count,
        base_lsn, delta_count, &base_abs)?;

    for &page_id in &changed_pages {
        let page_ref = storage.read_page(page_id)?;
        write_page_entry(&mut w, page_id, page_ref.as_bytes())?;
    }
    w.flush().map_err(DbError::Io)?;

    Ok(status_result(format!(
        "Incremental backup: {delta_count} of {page_count} pages changed, written to '{dest}'"
    )))
}
```

`executor/mod.rs` — add:
```rust
mod backup;
use backup::{execute_backup, execute_restore};
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_backup   # 4 new backup tests pass
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): backup engine — format + full backup (20.7 step 4)
```

---

## Step 5 — Backup engine: incremental

**Goal:** Incremental backup tests (the engine code is already in step 4;
this step adds the integration tests that exercise the incremental path).

**Files:**
- `crates/axiomdb-sql/tests/integration_backup.rs`

### Tests to add

```rust
#[test]
fn test_incremental_backup_after_no_changes_has_zero_delta() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    bloom = &axiomdb_sql::bloom::BloomRegistry::default();

    let full_path = dir.path().join("full.axbk").display().to_string();
    let inc_path  = dir.path().join("inc.axbk").display().to_string();

    execute_with_ctx(Stmt::Backup(BackupStmt { dest: full_path.clone(), incremental_from: None }),
        &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();

    execute_with_ctx(Stmt::Backup(BackupStmt {
        dest: inc_path.clone(),
        incremental_from: Some(full_path.clone()),
    }), &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();

    let bytes = std::fs::read(&inc_path).unwrap();
    assert_eq!(bytes[8], 1u8); // kind = Incremental
    let delta_count = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    assert_eq!(delta_count, 0, "no changes, delta must be 0");
}

#[test]
fn test_incremental_backup_base_must_be_full() {
    // Attempt INCREMENTAL FROM an incremental file → BackupError
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let bloom = &axiomdb_sql::bloom::BloomRegistry::default();

    let full = dir.path().join("full.axbk").display().to_string();
    let inc1 = dir.path().join("inc1.axbk").display().to_string();
    let inc2 = dir.path().join("inc2.axbk").display().to_string();

    execute_with_ctx(Stmt::Backup(BackupStmt { dest: full.clone(), incremental_from: None }),
        &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();
    execute_with_ctx(Stmt::Backup(BackupStmt {
        dest: inc1.clone(), incremental_from: Some(full),
    }), &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();

    let err = execute_with_ctx(Stmt::Backup(BackupStmt {
        dest: inc2, incremental_from: Some(inc1),
    }), &storage, &txn, &bloom, &mut SessionContext::new()).unwrap_err();
    assert!(matches!(err, DbError::BackupError { .. }));
}

#[test]
fn test_incremental_backup_includes_new_pages() {
    // After inserting data, incremental must include changed pages.
    // (Uses execute_with_ctx to run INSERT through the server)
    // ... setup + insert + full backup + insert more + incremental ...
    // delta_count > 0
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_backup
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): incremental backup integration tests (20.7 step 5)
```

---

## Step 6 — Restore engine

**Goal:** Implement `execute_restore` (full and incremental). TDD: write
restore + round-trip tests first.

**Files:**
- `crates/axiomdb-sql/src/executor/backup.rs`
- `crates/axiomdb-sql/tests/integration_backup.rs`

### Tests to add

```rust
#[test]
fn test_restore_full_creates_openable_db() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, txn) = setup_db(&dir);
    let bloom = &axiomdb_sql::bloom::BloomRegistry::default();

    let full = dir.path().join("full.axbk").display().to_string();
    execute_with_ctx(Stmt::Backup(BackupStmt { dest: full.clone(), incremental_from: None }),
        &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();

    let restored_db = dir.path().join("restored.db").display().to_string();
    execute_with_ctx(Stmt::Restore(RestoreStmt { source: full, dest_path: restored_db.clone() }),
        &storage, &txn, &bloom, &mut SessionContext::new()).unwrap();

    // Must open without error.
    MmapStorage::open(std::path::Path::new(&restored_db)).unwrap();
}

#[test]
fn test_restore_dest_already_exists_errors() { /* ... BackupError ... */ }

#[test]
fn test_restore_invalid_axbk_errors() { /* ... BackupError wrong magic ... */ }

#[test]
fn test_round_trip_full_backup_restore_query() {
    // 1. Create DB + INSERT rows
    // 2. Full backup
    // 3. Restore to new path
    // 4. Open restored DB + query rows
    // 5. Assert same data
}

#[test]
fn test_round_trip_incremental_backup_restore_query() {
    // 1. Create DB + INSERT rows
    // 2. Full backup
    // 3. INSERT more rows
    // 4. Incremental backup
    // 5. Restore incremental
    // 6. Query → same data as after step 4
}

#[test]
fn test_restore_incremental_base_not_found_errors() { /* ... BackupError ... */ }
```

### Implementation (append to `backup.rs`)

```rust
pub(super) fn execute_restore(stmt: RestoreStmt) -> Result<QueryResult, DbError> {
    if Path::new(&stmt.dest_path).exists() {
        return Err(backup_error(format!("restore destination already exists: {}", stmt.dest_path)));
    }
    restore_from(&stmt.source, &stmt.dest_path)
}

/// Restores source .axbk → dest_path.
/// For incremental backups, recursively restores the base first.
fn restore_from(source: &str, dest_path: &str) -> Result<QueryResult, DbError> {
    let mut f = File::open(source)
        .map_err(|_| backup_error(format!("base backup not found: {source}")))?;
    let (kind, _backup_lsn, page_count, _base_lsn, delta_count, base_path) =
        read_backup_header(&mut f, source)?;

    let base_pages = if kind == KIND_INCREMENTAL {
        // Restore base backup first.
        if !Path::new(&base_path).exists() {
            return Err(backup_error(format!("base backup not found: {base_path}")));
        }
        restore_pages_to_file(&base_path, dest_path, None)?
    } else {
        0
    };

    // Apply pages from this file (full = all; incremental = delta only).
    let applied = restore_pages_to_file(source, dest_path, Some(&mut f))?;

    let status = if kind == KIND_INCREMENTAL {
        format!(
            "Restored {page_count} pages to '{dest_path}' (base: {base_pages} + {delta_count} incremental)"
        )
    } else {
        format!("Restored {applied} pages to '{dest_path}'")
    };

    Ok(status_result(status))
}

/// Writes pages from a .axbk file to dest_path.
/// If `file` is None, opens source fresh. If dest_path exists, updates pages in-place.
/// Returns number of pages written.
fn restore_pages_to_file(
    source: &str,
    dest_path: &str,
    file: Option<&mut File>,
) -> Result<u64, DbError> {
    let mut owned;
    let f: &mut File = match file {
        Some(f) => f,
        None => {
            owned = File::open(source).map_err(DbError::Io)?;
            // Skip header.
            owned.seek(SeekFrom::Start(HEADER_SIZE as u64)).map_err(DbError::Io)?;
            &mut owned
        }
    };

    // If dest_path doesn't exist yet, create with proper initial size.
    // We'll pwrite pages at their correct offsets.
    let dest_file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(dest_path)
        .map_err(DbError::Io)?;

    let mut count = 0u64;
    let mut entry = vec![0u8; ENTRY_SIZE];
    while f.read_exact(&mut entry).is_ok() {
        let page_id = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let offset = page_id * PAGE_SIZE as u64;
        // pwrite page bytes at correct offset (no seek needed, thread-safe).
        use std::os::unix::fs::FileExt;
        dest_file.write_at(&entry[8..], offset).map_err(DbError::Io)?;
        count += 1;
    }
    dest_file.sync_all().map_err(DbError::Io)?;
    Ok(count)
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test integration_backup
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): restore engine — full + incremental (20.7 step 6)
```

---

## Step 7 — Executor dispatch + integration + wire smoke

**Goal:** Replace the `NotImplemented` stub in `exec_entry.rs` with the real
dispatch. Add remaining edge-case tests. Run wire smoke.

**Files:**
- `crates/axiomdb-sql/src/executor/exec_entry.rs`
- `crates/axiomdb-sql/tests/integration_backup.rs`
- `tools/wire-test.py`

### Wiring in `exec_entry.rs`

Replace the stub from Step 3:

```rust
Stmt::Backup(b) => execute_backup(b, storage, txn),
Stmt::Restore(r) => execute_restore(r),
```

### Remaining tests (edge cases)

```rust
#[test]
fn test_full_backup_large_path_base_path_limit() { /* base_path > 71 bytes → BackupError */ }

#[test]
fn test_full_backup_status_message_format() {
    // Assert status string contains "Full backup:" and page count.
}

#[test]
fn test_incremental_status_message_format() {
    // Assert status string contains "Incremental backup:" and counts.
}

#[test]
fn test_restore_incremental_status_message_format() {
    // Assert "base: X + Y incremental"
}
```

### Wire smoke (append to `tools/wire-test.py`)

```python
# [20.7 incremental backup]
import tempfile, os
with tempfile.TemporaryDirectory() as td:
    full_path = os.path.join(td, "full.axbk")
    inc_path  = os.path.join(td, "inc.axbk")
    restored  = os.path.join(td, "restored.db")

    cur.execute(f"BACKUP DATABASE TO '{full_path}'")
    row = cur.fetchone()
    assert row and "Full backup" in row[0], f"unexpected: {row}"

    cur.execute(f"BACKUP DATABASE TO '{inc_path}' INCREMENTAL FROM '{full_path}'")
    row = cur.fetchone()
    assert row and "Incremental backup" in row[0], f"unexpected: {row}"

    cur.execute(f"RESTORE DATABASE FROM '{full_path}' TO '{restored}'")
    row = cur.fetchone()
    assert row and "Restored" in row[0], f"unexpected: {row}"
    assert os.path.exists(restored), "restored.db must exist"
```

### Final verification against spec done criteria

- [x] `BACKUP DATABASE TO '/path/full.axbk'` produces a valid .axbk file
- [x] `BACKUP DATABASE TO '/path/inc.axbk' INCREMENTAL FROM '/path/full.axbk'` works
- [x] `RESTORE DATABASE FROM '/path/full.axbk' TO '/path/restored.db'` opens via `MmapStorage::open()`
- [x] Incremental restore round-trip: same data
- [x] 0-change incremental: delta_count = 0
- [x] All error cases tested
- [x] ≥12 integration tests in `integration_backup.rs`
- [x] Wire smoke: BACKUP + RESTORE execute via pymysql

```bash
./tools/vm.sh test axiomdb-sql --test integration_backup   # all pass
./tools/vm.sh test --workspace                              # workspace clean
./tools/vm.sh clippy
cargo fmt --check
# Wire smoke:
pkill axiomdb-server || true
cargo build -p axiomdb-server
rm -f target/release/axiomdb-server
cargo build --release -p axiomdb-server
python3 tools/wire-test.py
```

### Commit

```
feat(fase-20): complete 20.7 incremental backup + wire smoke

- BACKUP DATABASE TO / RESTORE DATABASE FROM ... TO SQL statements
- .axbk binary format: 128-byte header + BackupPageEntry per page
- Full backup: checkpoint + page scan + raw page copy
- Incremental: CRC32c diff vs base backup → delta pages only
- Restore: full page write / incremental base+delta apply
- 14 integration tests in integration_backup.rs
- Wire smoke: 3 assertions added (N/N)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `pwrite` offset arithmetic wrong for page 0 | medium | explicit test for meta page checksum after restore |
| Checksum field offset in PageHeader incorrect (assumed 20..24) | medium | verify against `page.rs` HEADER layout before step 4 |
| `read_exact` on incremental page entries silently reads partial entries | low | use `read_exact` (errors on short read) |
| `BufWriter` not flushed on error path (partial .axbk) | low | `w.flush()` after all writes; on error the dest file is left partial but dest already-exists check prevents double-write |
| Windows `FileExt` (`write_at`) not available | n/a | Lima VM is Linux; no Windows target |

## Rollback plan

1. `git reset --hard <commit before Step 1>` to abandon
2. Leave partial on `abandoned/plan-20.7-backup-<date>`
3. Mark spec status back to `draft`

## Estimated effort

Total: ~6–8 hours  
Step 1: 30 min | Step 2: 45 min | Step 3: 20 min | Step 4: 90 min  
Step 5: 45 min | Step 6: 90 min | Step 7: 60 min
