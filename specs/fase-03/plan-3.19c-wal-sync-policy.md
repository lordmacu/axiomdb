# Plan: 3.19c — WAL sync policy and platform method selection

## Files to create/modify

- `crates/axiomdb-wal/src/sync.rs` — new WAL-specific sync-method abstraction
  and platform implementations
- `crates/axiomdb-wal/src/writer.rs` — replace direct `sync_data()` use with the
  explicit WAL sync policy; keep metadata-sync paths explicit
- `crates/axiomdb-wal/src/txn.rs` — route immediate commit and pipeline leader
  flush through the renamed/selected WAL durable method
- `crates/axiomdb-wal/src/lib.rs` — export sync types only if needed outside the crate
- `crates/axiomdb-wal/tests/integration_wal_writer.rs` — writer-level method and
  durability regressions
- `crates/axiomdb-wal/tests/integration_group_commit.rs` — verify `6.19` still
  advances visibility only after the selected durable sync completes
- `crates/axiomdb-network/src/mysql/database.rs` — update comments or names if
  `wal_flush_and_fsync()` becomes a more precise API
- `crates/axiomdb-storage/src/config.rs` — only if the method is exposed via
  `DbConfig` in this subphase

## Algorithm / Data structure

### 1. Add an explicit DML sync-method enum

Create a small WAL-local enum, conceptually:

```text
WalSyncMethod {
  Auto,
  Fsync,
  Fdatasync,
  FullFsync,
  StdSyncAllFallback,
}
```

`Auto` resolves once, when the writer is created/opened:

```text
resolve_auto():
  if macOS:
    return Fsync
  else if unix && fdatasync available:
    return Fdatasync
  else if unix:
    return Fsync
  else:
    return StdSyncAllFallback
```

Rules:
- explicit unsupported methods fail early;
- `FullFsync` is opt-in and platform-gated;
- metadata-changing operations are not controlled by this enum in `3.19c`:
  they remain on the existing metadata-sync path.

### 2. Move the actual syscall choice out of `std::fs::File::sync_data()`

Pseudocode:

```text
commit_data_sync():
  flush BufWriter to kernel
  wal_sync_file(file, resolved_dml_method)

commit_metadata_sync():
  flush BufWriter to kernel
  file.sync_all()
```

`wal_sync_file(...)` issues the intended primitive directly:

```text
match method:
  Fsync -> libc::fsync(fd)
  Fdatasync -> libc::fdatasync(fd)
  FullFsync -> fcntl(fd, F_FULLFSYNC)
  StdSyncAllFallback -> file.sync_all()
```

Map syscall errors through the existing `classify_io(...)` path.

### 3. Keep `3.19b` tail semantics unchanged

Do not touch:
- `logical_end`
- `reserved_end`
- `scan_valid_tail`
- reader/recovery stop conditions

The only change in this subphase is which durable primitive is invoked after
the bytes are already flushed to the kernel.

### 4. Keep `6.19` semantics unchanged

The leader-based pipeline remains:

```text
append Commit entry
pipeline.acquire(commit_lsn)
leader -> wal_flush_and_fsync()
advance_committed only after durable method returns Ok
followers await release_ok/release_err
```

Only the underlying durable method changes.

## Implementation phases

1. Add `sync.rs` with the sync enum, platform resolution, syscall wrappers, and
   focused unit tests.
2. Replace `WalWriter::commit_data_sync()` internals to use the new helper while
   leaving `commit_metadata_sync()` and reservation-boundary metadata sync alone.
3. Rewire `TxnManager::commit()` and `wal_flush_and_fsync()` to use the renamed
   explicit durable path if needed.
4. Add/adjust WAL writer and group-commit integration tests.
5. Run the targeted autocommit benchmark and compare against the post-`3.19b`
   baseline on the same release build.

## Tests to write

- unit:
  - `Auto` resolves to the expected method on each supported platform
  - unsupported explicit method is rejected cleanly
  - metadata-sync path still stays separate from DML sync path
- integration:
  - committed WAL entries survive reopen/crash with the selected DML method
  - reserved-tail WAL files still reopen/scan/recover correctly
  - `6.19` pipeline followers do not advance visibility before the leader's
    selected durable method succeeds
- bench:
  - `python3 benches/comparison/local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb`
  - optional microbench for raw sync method cost if needed to explain results

## Anti-patterns to avoid

- Do not change the commit contract to send `OK` before durability.
- Do not leave the hot path on `File::sync_data()` and only rename methods.
- Do not silently use `FullFsync`/stronger semantics in `Auto`.
- Do not downgrade WAL create/rotate/grow/truncate to data-only sync.
- Do not mix parser/wire/protocol caching into this subphase.

## Risks

- Risk: the explicit syscall wrapper regresses correctness on non-macOS platforms.
  - Mitigation: keep `Auto` conservative and retain `StdSyncAllFallback`.

- Risk: explicit method selection changes durability semantics without documentation.
  - Mitigation: document `Auto` as “mainstream durable default”, keep stronger
    `FullFsync` explicit, and preserve metadata-sync barriers.

- Risk: benchmark still underperforms because the real bottleneck moves elsewhere.
  - Mitigation: keep the change tightly isolated and re-measure immediately; if
    the gain is insufficient, the next subphase should target request/response
    overhead, not more WAL guessing.

- Risk: Windows/non-Unix support becomes fragile.
  - Mitigation: use a fallback method there and keep Unix-specific paths behind
    compile-time guards.

## Assumptions

- The current measured hot-path blocker is the selected durable sync primitive,
  not WAL preallocation anymore.
- `3.19b` remains in place; `3.19c` builds on it rather than replacing it.
- The project is willing to align default WAL durability semantics more closely
  with PostgreSQL/MySQL/SQLite durable defaults, instead of the stronger
  accidental macOS behavior observed via `sync_data()`.
