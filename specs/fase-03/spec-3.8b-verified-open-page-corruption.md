# Spec: 3.8b — Verified Open And Early Page-Corruption Detection

These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `specs/fase-03/spec-3.8-crash-recovery.md`
- `specs/fase-03/plan-3.8-crash-recovery.md`
- `specs/fase-03/spec-3.10-durability-tests.md`
- `crates/axiomdb-storage/src/mmap.rs`
- `crates/axiomdb-storage/src/page.rs`
- `crates/axiomdb-storage/tests/integration_storage.rs`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/tests/integration_durability.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-embedded/src/lib.rs`

## What to build (not how)

Opening an existing AxiomDB database must fail fast if any persisted page is
already checksum-corrupted. Corruption must be detected during startup, before
the first query reads that page.

In addition, every real reopen path must run crash recovery through
`TxnManager::open_with_recovery(...)` instead of bypassing it with
`TxnManager::open(...)`.

This subphase closes the specific gap “partial page write detection on open”.
It does **not** attempt to repair a corrupted committed page from WAL; it only
ensures that corruption is surfaced during open and that the existing recovery
entrypoint is used consistently.

## Inputs / Outputs

- Input:
  - existing `.db` file with page checksums
  - existing `.wal` file
- Output:
  - `MmapStorage::open(...)` succeeds only if every page in the file verifies
  - `Database::open(...)` and embedded `Db::open(...)` always route through
    `TxnManager::open_with_recovery(...)` on reopen
- Errors:
  - corrupted page at startup returns `DbError::ChecksumMismatch { page_id, .. }`
  - WAL truncation/corrupt tail behavior remains unchanged: recovery stops at the
    last valid WAL entry, as it does today

## Use cases

1. Clean reopen.
   All pages verify, crash recovery runs, and the database becomes ready.

2. Corrupted data page before first query.
   Startup fails with `DbError::ChecksumMismatch` during `MmapStorage::open`.
   No connection is accepted and no embedded handle is returned.

3. Clean pages with a truncated WAL tail.
   Startup still succeeds because `open_with_recovery()` already treats truncated
   or checksum-bad WAL tail as end-of-valid-WAL.

4. Repeat reopen after a crash.
   `open_with_recovery()` remains idempotent.

## Acceptance criteria

- [ ] `MmapStorage::open()` verifies every page in the file, not only pages 0 and 1
- [ ] A checksum-corrupted page causes startup failure before any query is executed
- [ ] A clean database still reopens successfully
- [ ] `crates/axiomdb-network/src/mysql/database.rs` uses `TxnManager::open_with_recovery(...)` on reopen
- [ ] `crates/axiomdb-embedded/src/lib.rs` uses `TxnManager::open_with_recovery(...)` on reopen
- [ ] Repeated reopen remains idempotent
- [ ] Existing truncated-tail WAL recovery behavior remains unchanged

## Out of scope

- Repairing corrupted committed pages from WAL
- Full power-failure redo for committed pages
- Index/catalog page-image redo
- Ignoring checksum failures and continuing startup
- Background scrubbing after startup

## Dependencies

- `crates/axiomdb-storage/src/page.rs` — page checksum verification
- `crates/axiomdb-storage/src/mmap.rs` — startup open path
- `crates/axiomdb-wal/src/recovery.rs` — recovery semantics already implemented
- `crates/axiomdb-wal/src/txn.rs` — `open_with_recovery`
- `crates/axiomdb-network/src/mysql/database.rs` — server reopen path
- `crates/axiomdb-embedded/src/lib.rs` — embedded reopen path

## ⚠️ DEFERRED

- Full committed-page redo after power loss
  - current WAL coverage is not yet broad enough to promise complete redo for
    every persisted page type
  - track separately as `3.8c`
