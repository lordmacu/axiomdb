# Spec: 3.19c — WAL sync policy and platform method selection

## What to build (not how)

Introduce an explicit WAL durability-sync policy for steady-state DML commits so
AxiomDB no longer depends on `std::fs::File::sync_data()` as the hot-path
primitive.

This subphase must make the WAL commit path choose a platform-appropriate sync
method explicitly, while preserving the existing transaction contract:

- a DML commit is still acknowledged only after WAL durability is confirmed;
- metadata-changing WAL operations (create, rotate, reserve/grow, truncate,
  checkpoint) remain on a metadata-sync path;
- the `3.19b` reserved-tail WAL semantics remain unchanged;
- the `6.19` fsync pipeline continues to coalesce commits, but uses the new
  selected WAL sync method underneath.

The main goal is to fix the remaining autocommit bottleneck discovered after
`3.19b`: on macOS, the current `sync_data()` path behaves like a much more
expensive media-flush primitive than the `fsync`/`fdatasync`-style durability
paths used by mainstream engines.

## Research synthesis

### AxiomDB files reviewed first

These files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-wal/src/writer.rs`
- `crates/axiomdb-wal/src/reader.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/fsync_pipeline.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-storage/src/config.rs`
- `specs/fase-03/spec-3.19b-wal-durable-fast-path.md`
- `specs/fase-06/spec-6.19-wal-fsync-pipeline.md`

### Research sources and what to borrow

- `research/postgres/src/backend/storage/file/fd.c`
  Borrow: explicit separation of sync methods instead of hiding everything
  behind one generic file sync call.
  Reject: PostgreSQL's VFD layer and full file-descriptor cache.
  Adapt: AxiomDB only needs a small WAL-specific sync abstraction.

- `research/postgres/src/bin/pg_test_fsync/pg_test_fsync.c`
  Borrow: treat sync method as something measurable and platform-dependent.
  Reject: shipping a standalone CLI before the core policy exists.
  Adapt: AxiomDB should encode the method choice explicitly and verify it with
  targeted tests/benchmarks.

- `research/sqlite/src/os_unix.c`
  Borrow: `F_FULLFSYNC` is opt-in, not the hidden default.
  Reject: SQLite's broader VFS and locking stack.
  Adapt: AxiomDB should make "stronger than fsync" durability an explicit
  method, not the accidental behavior of `sync_data()`.

- `research/sqlite/src/wal.c`
  Borrow: keep the WAL commit barrier explicit and independent from the reader
  data structures.
  Reject: moving commit durability into checkpointing or NORMAL-mode semantics.
  Adapt: AxiomDB keeps `OK only after durable commit`; only the primitive
  changes.

- `research/mariadb-server/storage/innobase/os/os0file.cc`
  Borrow: own the OS sync wrapper instead of delegating it to a generic runtime.
  Reject: the surrounding buffer pool and InnoDB file subsystem.
  Adapt: AxiomDB should issue the desired syscall directly for the WAL file.

- `research/mariadb-server/storage/innobase/log/log0log.cc`
  Borrow: separate "write log bytes" from "flush durable".
  Reject: copying InnoDB redo architecture or log buffer design.
  Adapt: AxiomDB already has this split after `3.19b`; `3.19c` fixes the
  durable flush primitive.

- `research/mariadb-server/storage/innobase/log/log0sync.cc`
  Borrow: group commit is useful only after the underlying durable primitive is
  sane.
  Reject: assuming more group-commit sophistication solves sequential
  request/response clients.
  Adapt: keep `6.19` as-is and optimize the primitive it calls.

## Inputs / Outputs

- Input:
  - `WalWriter` in `crates/axiomdb-wal/src/writer.rs`
  - `TxnManager::commit()` and `TxnManager::wal_flush_and_fsync()` in
    `crates/axiomdb-wal/src/txn.rs`
  - `FsyncPipeline` users in `crates/axiomdb-network/src/mysql/database.rs`
  - optional engine config in `crates/axiomdb-storage/src/config.rs` only if the
    chosen design needs a persisted method selection
- Output:
  - steady-state DML WAL durability uses an explicit selected method rather than
    `File::sync_data()`;
  - metadata-changing WAL operations still use a metadata-sync path;
  - autocommit durability remains synchronous and crash-safe;
  - the selected method is testable and visible in code, not implicit in stdlib behavior.
- Errors:
  - existing I/O and `DiskFull` behavior remain unchanged;
  - if an explicit sync method is chosen but unsupported on the current
    platform, opening/creating the writer must fail explicitly rather than
    silently using a stronger or weaker method than requested.

## Use cases

1. macOS autocommit `INSERT` over the MySQL wire protocol.
   - AxiomDB uses the platform default WAL DML sync method selected by policy.
   - The method must no longer route through `File::sync_data()`.
   - The client still receives `OK` only after the WAL is durably committed.

2. Linux durable DML commit.
   - AxiomDB may choose `fdatasync` when available.
   - Crash guarantees remain unchanged.

3. WAL capacity-reservation boundary or WAL rotation.
   - The writer still uses a metadata-sync path.
   - `3.19b`'s reserved-tail correctness remains intact.

4. `6.19` fsync-pipeline leader flush.
   - The pipeline still coalesces flushes exactly as today.
   - The leader calls the new selected durable method instead of the old one.

5. Explicit strict/full sync selection on a platform that supports it.
   - A stronger `fullfsync`-style mode remains possible, but it is opt-in.
   - Unsupported explicit methods fail early.

## Acceptance criteria

- [ ] Steady-state DML commits no longer depend on `std::fs::File::sync_data()`
      in the WAL hot path.
- [ ] Metadata-changing WAL operations (create, rotate, reserve/grow, truncate,
      checkpoint) remain on a metadata-sync path and do not silently downgrade
      to data-only sync.
- [ ] The chosen DML sync method is explicit in code and testable by platform.
- [ ] On macOS, the default DML sync method no longer behaves like the current
      `sync_data()`/`F_FULLFSYNC`-like path.
- [ ] `3.19b` logical-tail behavior remains correct: open/forward/backward scan
      and recovery ignore preallocated unused WAL tail bytes.
- [ ] `6.19` keeps the same visibility contract: no committed transaction
      becomes visible before WAL durability is confirmed.
- [ ] `local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb`
      improves by at least `5x` over the post-`3.19b` baseline on the same
      machine and release build.

## Out of scope

- Reintroducing or redesigning timer-based group commit
- Sending MySQL `OK` before WAL durability
- Buffer pool, redo manager, or InnoDB-style log subsystem rewrite
- Query parsing/protocol overhead work
- SQL/session exposure of the sync policy if that requires broader config
  plumbing than this subphase needs

## Dependencies

- `3.19b` — WAL logical end + preallocation
- `6.19` — always-on fsync pipeline
- `3.16` — configuration groundwork, only if user-facing policy config is wired here

## ⚠️ DEFERRED

- User-facing SQL/session variable for WAL sync method
  → pending in a later config/observability subphase if not wired here
- `pg_test_fsync`-style standalone benchmark/probe utility
  → pending in a later tooling subphase if the internal tests/bench are sufficient
