# Spec: 20.7 — Incremental Backup

Phase: 20 — Types + Import/Export  
Task: Incremental backup — page-checksum diff + full restore  
Status: approved

## Context

AxiomDB stores data in `axiomdb.db` (mmap, page-based, 16 KB/page). The WAL
(`axiomdb.wal`) provides durability and crash recovery. The `Checkpointer`
already flushes all dirty pages and stamps a checkpoint LSN in the meta page
(page 0). Each `PageHeader` already carries a `checksum: u32` (CRC32c over
the body). These two primitives — checkpoint and page checksum — are the
foundation for offline backup without per-page LSN stamping.

Phase 18 will add PITR (WAL archiving, per-page LSN stamping, hot backup).
Phase 20.7 delivers offline backup: full + one-level incremental + restore.

## Goal

Implement `BACKUP DATABASE TO` / `RESTORE DATABASE FROM ... TO` SQL statements
that produce and consume portable `.axbk` binary backup files, supporting full
and incremental (page-diff) backup strategies.

## Non-goals

- Hot backup (while writes are in flight) — Phase 18.7
- PITR (WAL archiving, point-in-time restore) — Phase 18.6
- Cloud/S3 destination — Phase 18
- Backup encryption — Phase 18
- Multi-level incremental chain (inc → inc → inc) — deferred; only full→inc is supported here
- In-place restore (replacing a running database) — always restores to a new path
- `INCREMENTAL FROM` chain validation (the base must be full, not another incremental)

## Behavior

### SQL Syntax

```sql
-- Full backup
BACKUP DATABASE TO '/var/backups/db-2026-05-15.axbk';

-- Incremental backup (diff against a prior full backup)
BACKUP DATABASE TO '/var/backups/db-inc1.axbk'
    INCREMENTAL FROM '/var/backups/db-2026-05-15.axbk';

-- Restore from full backup (always to new path)
RESTORE DATABASE FROM '/var/backups/db-2026-05-15.axbk'
    TO '/var/data/restored.db';

-- Restore from incremental backup (auto-chains to full using base_path in header)
RESTORE DATABASE FROM '/var/backups/db-inc1.axbk'
    TO '/var/data/restored.db';
```

### Result rows

All four forms return a single row result set with one text column `status`:

```
BACKUP DATABASE TO ... 
→ "Full backup: 1024 pages (16 MB) written to '/var/backups/db-2026-05-15.axbk'"

BACKUP DATABASE TO ... INCREMENTAL FROM ...
→ "Incremental backup: 38 of 1024 pages changed, written to '/var/backups/db-inc1.axbk'"

RESTORE DATABASE FROM ... TO ...
→ "Restored 1024 pages to '/var/data/restored.db'"

RESTORE DATABASE FROM ... TO ... (incremental)
→ "Restored 1024 pages to '/var/data/restored.db' (base: 986 + 38 incremental)"
```

### AST

```rust
/// BACKUP DATABASE TO 'path' [INCREMENTAL FROM 'base_path']
pub struct BackupStmt {
    pub dest: String,                     // destination .axbk path
    pub incremental_from: Option<String>, // None = full backup
}

/// RESTORE DATABASE FROM 'source' TO 'dest_path'
pub struct RestoreStmt {
    pub source: String,   // .axbk file to restore from
    pub dest_path: String, // new .db file path to create
}
```

Added to `Stmt` enum:
```rust
Backup(BackupStmt),
Restore(RestoreStmt),
```

### Public API

```rust
// axiomdb-sql/src/backup.rs

/// Executes BACKUP DATABASE TO ... [INCREMENTAL FROM ...]
///
/// Algorithm (full):
///   1. CHECKPOINT (flush dirty pages + write checkpoint_lsn to meta)
///   2. Scan pages 0..page_count from storage
///   3. Write BackupFile (kind=Full) to dest_path
///
/// Algorithm (incremental):
///   1. Validate base_path is a valid Full backup .axbk
///   2. CHECKPOINT
///   3. Load base page checksums: HashMap<page_id, checksum>
///   4. Scan pages 0..page_count from current storage
///   5. Include page if: page not in base OR checksum differs OR page_id >= base_page_count
///   6. Write BackupFile (kind=Incremental) to dest_path
pub fn execute_backup(
    stmt: &BackupStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<QueryResult, DbError>;

/// Executes RESTORE DATABASE FROM 'source' TO 'dest_path'
///
/// Algorithm (full backup):
///   1. Open source .axbk, validate magic + version
///   2. Create dest_path (must not exist)
///   3. Write pages in order to dest_path
///
/// Algorithm (incremental backup):
///   1. Open source .axbk, validate magic + kind=Incremental
///   2. Read base_path from header, restore base backup to dest_path
///   3. Open incremental pages, overwrite each in dest_path
///
/// Caller is responsible for not passing a dest_path that is an open database.
pub fn execute_restore(stmt: &RestoreStmt) -> Result<QueryResult, DbError>;
```

### .axbk Binary Format

```
Offset  Size  Field             Description
────────────────────────────────────────────────────────────────────────
0       8     magic             0x4158494F4D424B01  ("AXIOMBK\1")
8       1     kind              0 = Full, 1 = Incremental
9       7     _pad              reserved, must be 0
16      8     backup_lsn        checkpoint_lsn at backup time
24      8     page_count        storage.page_count() at backup time
32      4     page_size         always 16384 (PAGE_SIZE)
36      4     _pad2             reserved, must be 0
40      8     base_lsn          Full: 0; Incremental: base backup's backup_lsn
48      8     delta_count       Full: page_count; Incremental: # changed pages
56      72    base_path         null-terminated UTF-8; Full: empty; Incremental: base .axbk absolute path (max 71 chars + NUL)
128     ...   page entries      sequence of BackupPageEntry (see below)

BackupPageEntry:
  0     8     page_id           u64, little-endian
  8     16384 page_bytes        raw page content (includes PageHeader + body)

Total entry size: 16392 bytes
```

Compatibility rule: `magic` byte 7 (`\1`) is a format version. Future versions
bump this byte. Readers must reject unknown versions with a clear error.

`base_path` max 71 bytes (+ NUL = 72 total) is intentional: long paths must be
handled by the user (symlink or short path). Paths longer than 71 bytes return
`DbError::BackupError { message: "base_path exceeds 71 bytes" }`.

### Error Cases

| Condition | Error | Message |
|---|---|---|
| dest_path parent directory does not exist | `DbError::IoError` | from OS |
| dest_path already exists | `DbError::BackupError` | `"destination already exists: {path}"` |
| source .axbk has wrong magic | `DbError::BackupError` | `"not a valid .axbk file: {path}"` |
| source .axbk has unsupported version | `DbError::BackupError` | `"unsupported backup version {v}: {path}"` |
| INCREMENTAL FROM points to an incremental (not full) backup | `DbError::BackupError` | `"base backup must be a Full backup, got Incremental: {path}"` |
| restore dest_path already exists | `DbError::BackupError` | `"restore destination already exists: {path}"` |
| restore dest_path directory does not exist | `DbError::IoError` | from OS |
| incremental backup's base_path not found at restore time | `DbError::BackupError` | `"base backup not found: {base_path}"` |
| page checksum mismatch on restore | `DbError::CorruptedPage` | existing variant |
| base_path in incremental header exceeds 71 bytes | `DbError::BackupError` | `"base_path exceeds 71 bytes"` |

### DbError addition

```rust
// in axiomdb-core/src/error.rs — add variant:
#[error("backup error: {message}")]
BackupError { message: String },
```

## Edge Cases

- [ ] Full backup of a database with 0 user tables (only meta + freelist pages) — must include pages 0 and 1
- [ ] Incremental backup with 0 changed pages — writes header only + 0 entries; valid file
- [ ] Incremental backup where new pages were added (page_count > base page_count) — all new pages included automatically
- [ ] Incremental backup where pages were freed (page_count < base page_count) — freed pages not included; restore sees fewer pages than base
- [ ] dest_path on a different filesystem (cross-device copy) — works via normal file I/O (not rename)
- [ ] Concurrent writes during backup — checkpoint runs before scan; pages modified after checkpoint are still in consistent committed state (MVCC); minor drift is acceptable for offline backup
- [ ] Page 0 (meta) always included in every backup — never skipped (checksum changes after every checkpoint)
- [ ] Page 1 (freelist) always included in every backup — freelist state must be restored
- [ ] restore to a path that is currently an open MmapStorage — not checked; caller's responsibility (documented)
- [ ] very large database (>4GB = >262144 pages) — u64 page_count handles it

## Performance Budget

| Operation | Target | Max acceptable |
|---|---|---|
| Full backup, 1 GB DB (~65K pages) | < 5 s | 15 s |
| Incremental backup, 1% change rate (~650 pages) | < 1 s | 5 s |
| Restore (full, 1 GB) | < 5 s | 15 s |

Sequential I/O throughput is the bottleneck; no intermediate buffering beyond
one page at a time. Use `storage.prefetch_hint(page_id, 64)` every 64 pages
during full scans.

## Implementation Location

| Component | File |
|---|---|
| Backup engine | `crates/axiomdb-sql/src/backup.rs` |
| AST additions | `crates/axiomdb-sql/src/ast.rs` |
| Parser | `crates/axiomdb-sql/src/parser/` (new `parse_backup` / `parse_restore` arms) |
| Analyzer | `crates/axiomdb-sql/src/analyzer/` (pass-through; no schema needed) |
| Executor dispatch | `crates/axiomdb-sql/src/executor/exec_dispatch.rs` |
| DbError variant | `crates/axiomdb-core/src/error.rs` |
| Integration tests | `crates/axiomdb-sql/tests/integration_backup.rs` |

No new crate needed. The backup engine lives in `axiomdb-sql` alongside
COPY, CHECKPOINT, and VACUUM.

## Dependencies

- Depends on: Phase 3 (WAL + Checkpointer), Phase 20.5 (COPY — proves file I/O pattern in executor)
- Blocks: nothing in Phase 20; Phase 18 PITR will extend this

## Done Criteria

- [ ] `BACKUP DATABASE TO '/path/full.axbk'` produces a valid .axbk file
- [ ] `BACKUP DATABASE TO '/path/inc.axbk' INCREMENTAL FROM '/path/full.axbk'` produces a valid incremental .axbk with only changed pages
- [ ] `RESTORE DATABASE FROM '/path/full.axbk' TO '/path/restored.db'` produces a usable .db file that `MmapStorage::open()` can open without errors
- [ ] `RESTORE DATABASE FROM '/path/inc.axbk' TO '/path/restored.db'` restores full + applies delta; result passes `integrity::verify_all_pages()`
- [ ] Round-trip test: backup → restore → query all tables → same results as original
- [ ] Incremental with 0 changes: file is valid, delta_count=0
- [ ] All error cases in the table above return the specified error variant
- [ ] `cargo nextest run -p axiomdb-sql` passes (≥12 new tests in `integration_backup.rs`)
- [ ] Wire smoke: `BACKUP` + `RESTORE` statements execute via pymysql without error
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `cargo nextest run --workspace` clean

## References

- Checkpoint impl: `crates/axiomdb-wal/src/checkpoint.rs`
- Page format: `crates/axiomdb-storage/src/page.rs` (`PageHeader.checksum`)
- StorageEngine trait: `crates/axiomdb-storage/src/engine.rs`
- Similar SQL executor pattern: `crates/axiomdb-sql/src/copy_file.rs` (COPY TO/FROM)
- `db.md` § WAL, § Phase 18 (future PITR)
- PostgreSQL: `src/bin/pg_basebackup/pg_basebackup.c` — page-level base backup
- MariaDB: `mariabackup` — per-page LSN filtering (Approach B — future)
