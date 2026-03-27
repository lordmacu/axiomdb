# Spec: 3.19b — WAL durable fast path for `insert_autocommit`

## What to build (not how)

Reduce the durable-commit cost of single-row autocommit DML without relaxing
crash guarantees.

The WAL commit path must stop paying the current "grow file + sync metadata"
cost on every commit. For normal DML commits, AxiomDB must:

- durably flush WAL data with a **data-only** sync primitive when the platform
  supports it;
- avoid changing the WAL file length on the steady-state commit hot path by
  reserving WAL capacity ahead of time;
- preserve the existing write-ahead, rollback, crash-recovery, and MySQL-visible
  transaction semantics;
- stay compatible with the Phase `6.19` fsync pipeline so leaders and inline
  commits both use the same durable fast path.

This subphase targets the benchmark scenario driven by
`benches/comparison/local_bench.py --scenario insert_autocommit`, which issues
one `INSERT` per transaction over the MySQL wire protocol.

## Inputs / Outputs

- Input:
  - `WalWriter` in [`crates/axiomdb-wal/src/writer.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/src/writer.rs)
  - `TxnManager::commit()` and `TxnManager::wal_flush_and_fsync()` in [`crates/axiomdb-wal/src/txn.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/src/txn.rs)
  - WAL readers and recovery scans in [`crates/axiomdb-wal/src/reader.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/src/reader.rs)
  - Server fsync-pipeline path in [`crates/axiomdb-network/src/mysql/database.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/database.rs)
- Output:
  - normal DML commits no longer require a metadata flush on every commit;
  - WAL capacity is reserved in advance so steady-state commits write inside an
    already-sized file region;
  - recovery and backward/forward scans remain correct when the WAL file
    contains pre-reserved but unused tail bytes.
- Errors:
  - keep existing `DiskFull`, `WalGroupCommitFailed`, and I/O error behavior;
  - capacity growth failures must surface before acknowledging commit success.

## Use cases

1. A client sends 1000 consecutive `INSERT INTO bench_users VALUES (...)`
   statements with `autocommit=1` over `COM_QUERY`.
   - Every statement still commits durably before the `OK` packet is sent.
   - The commit path uses data-only sync inside already-reserved WAL capacity.

2. A connection commits an explicit transaction containing one or more DML
   statements.
   - Commit visibility and durability semantics remain unchanged.
   - The same fast durable WAL path is used by the final commit.

3. WAL append reaches the end of currently reserved capacity.
   - A new reservation step grows/reserves the durable WAL region.
   - The one-time growth cost is paid at the boundary, not on every commit.

4. The process crashes after WAL capacity was reserved but before all reserved
   bytes were populated with valid entries.
   - WAL open, forward scan, backward scan, and crash recovery stop at the last
     valid entry and ignore the unused reserved tail.

## Acceptance criteria

- [ ] A committed DML statement is never acknowledged to the client before its
      WAL record is durably persisted, exactly as today.
- [ ] Steady-state DML commits inside already-reserved WAL capacity use a
      data-only durability sync instead of a metadata sync on every commit.
- [ ] WAL capacity growth is amortized: file-size metadata changes happen only
      when the writer crosses a reservation boundary, not on every commit.
- [ ] `local_bench.py --scenario insert_autocommit --rows 1000 --table`
      improves by at least `5x` over the pre-change AxiomDB baseline on the
      same machine and release build.
- [ ] WAL open, `scan_forward`, `scan_backward`, and crash recovery remain
      correct when the physical WAL file contains unused reserved tail bytes.
- [ ] Phase `6.19` fsync-pipeline behavior is unchanged except for using the
      faster durable WAL primitive underneath.
- [ ] `cargo test -p axiomdb-wal`, the directly affected network tests, and the
      targeted `insert_autocommit` benchmark pass cleanly.

## Out of scope

- Relaxing durability semantics by sending `OK` before WAL durability
- Reworking `COM_QUERY` parsing/analyzing caches
- Prepared-statement protocol changes
- WAL format redesign beyond what is required to distinguish valid entries from
  reserved tail space

## Dependencies

- Phase `3.2` — WAL writer
- Phase `3.7` — WAL rotation
- Phase `3.8` — crash recovery
- Phase `6.19` — fsync pipeline integration in the MySQL server path

## ⚠️ DEFERRED

- Repeated `COM_QUERY` DML plan reuse / literal-normalized cache
  → pending in `27.8c`
- Any prepared-statement or embedded-API batching work
  → pending in `10.8` / future wire-performance subphases
