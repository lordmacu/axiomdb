# Spec: 3.6b — Disk-Full Degraded Mode

These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `specs/fase-03/spec-3.6-checkpoint.md`
- `specs/fase-03/plan-3.6-checkpoint.md`
- `crates/axiomdb-core/src/error.rs`
- `crates/axiomdb-core/src/error_response.rs`
- `crates/axiomdb-storage/src/mmap.rs`
- `crates/axiomdb-wal/src/writer.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/group_commit.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-server/src/main.rs`
- `crates/axiomdb-embedded/src/lib.rs`

## What to build (not how)

When the operating system reports that the database volume is full during a
durable write path, AxiomDB must:

1. Surface a dedicated disk-full error, distinct from the existing logical
   `StorageFull` error.
2. Transition the database runtime into a durable **read-only degraded mode**.
3. Reject every later operation that could mutate durable state before touching
   WAL or storage again.
4. Continue serving read-only traffic so the process does not crash or hang.

This applies equally to:
- direct write transactions
- checkpoints
- WAL rotation
- group-commit fsync
- embedded mode
- MySQL wire mode

`StorageFull` remains reserved for the logical condition “no free pages are
available in the storage allocator”. Disk exhaustion and allocator exhaustion
must not share the same error variant.

## Inputs / Outputs

- Input:
  - `std::io::Error` originating from WAL append/flush/sync/truncate
  - `std::io::Error` originating from storage grow/create/flush
  - OS error kinds representing disk exhaustion:
    - `ENOSPC`
    - `EDQUOT`
- Output:
  - `DbError::DiskFull { operation: &'static str }`
  - runtime mode transition: `ReadWrite -> ReadOnlyDegraded`
- Errors:
  - the first mutating statement that hits disk exhaustion returns `DbError::DiskFull`
  - later mutating statements return `DbError::DiskFull` immediately, without
    re-entering WAL/storage paths

## Read-only degraded mode

In degraded mode:
- allowed:
  - `SELECT`
  - `SHOW`
  - `EXPLAIN`
  - `DESCRIBE`
  - `USE`
  - session-only `SET` statements that do not persist data pages
- rejected:
  - DML (`INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`)
  - DDL
  - `BEGIN`
  - `COMMIT`
  - `ROLLBACK`
  - any statement path that could dirty storage or WAL

The mode persists until the process is restarted and the database is reopened.
There is no automatic return to read-write mode in the same process.

## Use cases

1. `INSERT` hits `ENOSPC` during `WalWriter::append`.
   The statement returns `DbError::DiskFull`. The runtime enters
   `ReadOnlyDegraded`. A later `SELECT` still works.

2. `COMMIT` hits `ENOSPC` during WAL fsync.
   The committing connection receives `DbError::DiskFull`.
   The runtime enters `ReadOnlyDegraded`.

3. Group commit hits `ENOSPC` during the background fsync.
   Every waiter in that batch receives the disk-full error.
   Future writes are rejected fast.

4. Checkpoint hits `ENOSPC` during storage flush.
   The checkpoint fails, `checkpoint_lsn` is not advanced, and the runtime
   enters `ReadOnlyDegraded`.

5. Embedded mode hits `ENOSPC` during `db.execute(...)`.
   The returned error is `DbError::DiskFull`. A later `SELECT` still works
   from the same `Db` handle, but later writes do not.

## Acceptance criteria

- [ ] OS disk exhaustion is represented by `DbError::DiskFull`, not `DbError::StorageFull`
- [ ] `DbError::StorageFull` remains reserved for allocator/page exhaustion
- [ ] `ENOSPC` and `EDQUOT` from WAL append/flush/sync/truncate map to `DbError::DiskFull`
- [ ] `ENOSPC` and `EDQUOT` from storage create/grow/flush map to `DbError::DiskFull`
- [ ] The first failing mutating operation returns `DbError::DiskFull`
- [ ] The runtime mode flips to `ReadOnlyDegraded` exactly once and stays there until reopen
- [ ] In degraded mode, read-only statements still succeed
- [ ] In degraded mode, mutating statements are rejected before touching WAL/storage
- [ ] Group-commit waiters receive the disk-full error when the batch fsync fails with disk exhaustion
- [ ] A failed checkpoint due to disk exhaustion does not advance `checkpoint_lsn`
- [ ] MySQL clients receive a disk-full specific wire error, not a generic I/O message

## Out of scope

- Automatic recovery back to read-write mode without reopening the process
- Reclaiming space automatically (`VACUUM`, compaction, page scavenging)
- Distributed propagation or cluster-wide fencing
- Background operator hooks, alerting, or admin APIs

## Dependencies

- `crates/axiomdb-core/src/error.rs` — central error enum
- `crates/axiomdb-core/src/error_response.rs` — wire-visible code / SQLSTATE / fix hint
- `crates/axiomdb-storage/src/mmap.rs` — storage create/grow/flush paths
- `crates/axiomdb-wal/src/writer.rs` — WAL append/commit/rotate error mapping
- `crates/axiomdb-wal/src/txn.rs` — commit path propagation
- `crates/axiomdb-network/src/mysql/database.rs` — shared server runtime mode
- `crates/axiomdb-network/src/mysql/group_commit.rs` — batch fsync failure handling
- `crates/axiomdb-network/src/mysql/handler.rs` — statement gating in degraded mode
- `crates/axiomdb-server/src/main.rs` — startup / server wiring
- `crates/axiomdb-embedded/src/lib.rs` — embedded mode parity

## ⚠️ DEFERRED

- Automatic return to read-write mode after disk space is freed by an operator
- Administrative introspection for degraded-mode reason / timestamp
