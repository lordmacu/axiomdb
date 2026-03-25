# Plan: 3.6b — Disk-Full Degraded Mode

## Files to create / modify

- `crates/axiomdb-core/src/error.rs` — add `DbError::DiskFull { operation }`
- `crates/axiomdb-core/src/error_response.rs` — add user-facing hint + MySQL-visible disk-full mapping
- `crates/axiomdb-core/src/lib.rs` — export any new shared runtime-mode type if introduced here
- `crates/axiomdb-storage/src/mmap.rs` — classify `ENOSPC` / `EDQUOT` in create/grow/flush
- `crates/axiomdb-wal/src/writer.rs` — classify `ENOSPC` / `EDQUOT` in append/flush/fsync/rotate
- `crates/axiomdb-wal/src/txn.rs` — preserve `DiskFull` on commit path and avoid collapsing it into generic WAL errors
- `crates/axiomdb-network/src/mysql/database.rs` — own the shared runtime mode and gate mutating statements
- `crates/axiomdb-network/src/mysql/group_commit.rs` — transition runtime mode on disk-full fsync failure
- `crates/axiomdb-network/src/mysql/handler.rs` — reject mutating commands fast when runtime is degraded
- `crates/axiomdb-server/src/main.rs` — wire the shared runtime mode into connection handling
- `crates/axiomdb-embedded/src/lib.rs` — mirror degraded-mode gating in embedded mode

## Algorithm / Data structure

### 1. Dedicated error classification

Introduce a helper that preserves non-disk I/O as `DbError::Io`, but maps:

```rust
fn classify_io(err: std::io::Error, operation: &'static str) -> DbError {
    match err.raw_os_error() {
        Some(code) if code == libc::ENOSPC || code == libc::EDQUOT => {
            DbError::DiskFull { operation }
        }
        _ => DbError::Io(err),
    }
}
```

This helper must be used only at durable OS boundaries:
- `set_len`
- `write_all`
- `flush`
- `sync_all`
- WAL truncation
- mmap flush

Do **not** use it for logical allocator failures already represented by
`DbError::StorageFull`.

### 2. Shared runtime mode

Use one shared runtime state for the whole opened database:

```rust
enum RuntimeMode {
    ReadWrite = 0,
    ReadOnlyDegraded = 1,
}
```

The server-side `Database` stores it in an `Arc<AtomicU8>` so:
- the background group-commit task can flip it
- each connection can read it without locking
- the accept loop can share the same state

The embedded `Db` stores the same mode directly on the handle.

### 3. Transition rule

If any durable write path returns `DbError::DiskFull`:
- set runtime mode to `ReadOnlyDegraded`
- return the same `DbError::DiskFull` to the caller
- do not clear the degraded flag later in the same process

The mode transition is one-way.

### 4. Statement gating

Before entering the durable execution path:
- if runtime mode is `ReadOnlyDegraded`
- and the statement is not read-only / session-only
- return `DbError::DiskFull { operation: "database is in read-only degraded mode" }`

The gate must run in:
- MySQL text query path
- prepared statement execute path
- embedded execution path

### 5. Group commit

When the background batch fsync fails:
- if the error is `DiskFull`, flip runtime mode to degraded
- reply to every waiter with `DbError::DiskFull`
- do not wrap it in a generic `WalGroupCommitFailed`

This keeps later statement behavior consistent with direct commit failures.

## Implementation phases

1. Add `DbError::DiskFull` plus `classify_io()` helper usage in storage/WAL durable I/O boundaries.
2. Add shared runtime mode and one-way degraded transition to the server `Database` and embedded `Db`.
3. Gate mutating statements in MySQL and embedded execution paths before they re-enter WAL/storage.
4. Update group commit to propagate `DiskFull` directly and flip runtime mode.
5. Add tests for direct write failure, checkpoint failure, and group-commit failure.

## Tests to write

- unit:
  - `classify_io()` maps `ENOSPC` and `EDQUOT` to `DbError::DiskFull`
  - `classify_io()` preserves unrelated I/O as `DbError::Io`
  - runtime mode is one-way (`ReadWrite -> ReadOnlyDegraded`)
- integration:
  - simulated WAL append/commit disk-full transitions runtime to degraded
  - simulated storage flush disk-full leaves reads working and writes rejected
  - embedded mode mirrors the same behavior
- wire:
  - MySQL `INSERT` on disk-full returns disk-full error packet
  - a later `SELECT` still succeeds
  - a later `INSERT` is rejected immediately without mutating state

## Anti-patterns to avoid

- Do **not** reuse `DbError::StorageFull` for OS disk exhaustion.
- Do **not** call `std::process::exit()` on disk-full.
- Do **not** allow later writes to continue “best effort” after the first disk-full event.
- Do **not** hide `DiskFull` inside a generic wrapper error in group commit.
- Do **not** auto-clear degraded mode without reopening the database.

## Risks

- `raw_os_error()` is platform-specific.
  Mitigation: only classify explicit `ENOSPC` / `EDQUOT`; everything else stays `DbError::Io`.
- Read-only/session-only statement classification could be inconsistent across text / prepared / embedded paths.
  Mitigation: define a single shared helper for “may mutate durable state”.
- A commit failure could leave callers unsure whether data persisted.
  Mitigation: degraded mode prevents any further writes in the same process.
