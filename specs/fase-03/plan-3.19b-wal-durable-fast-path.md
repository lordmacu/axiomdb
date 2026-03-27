# Plan: 3.19b — WAL durable fast path for `insert_autocommit`

## Files to create/modify

- `crates/axiomdb-wal/src/writer.rs` — split metadata-sync vs data-sync commit
  paths; track logical WAL end separately from reserved capacity; add
  reservation/growth helpers
- `crates/axiomdb-wal/src/reader.rs` — make forward/backward WAL scans stop at
  the last valid entry rather than blindly trusting physical EOF
- `crates/axiomdb-wal/src/txn.rs` — route normal commit and fsync-pipeline
  leader flush through the new writer API
- `crates/axiomdb-wal/src/lib.rs` — export any new writer helpers if needed
- `crates/axiomdb-wal/Cargo.toml` — add platform syscall support only if the
  chosen reservation API needs it
- `crates/axiomdb-wal/tests/integration_durability.rs` — crash/reopen coverage
  for reserved-tail WAL files
- `crates/axiomdb-wal/tests/integration_group_commit.rs` — verify the Phase
  `6.19` pipeline still advances durability only after the new commit primitive
- `crates/axiomdb-network/src/mysql/database.rs` — rename/adapt the leader path
  if `wal_flush_and_fsync()` becomes a more explicit durable-commit API

## Algorithm / Data structure

### 1. Separate logical WAL end from reserved capacity

Add two notions to `WalWriter`:

- `logical_end`: byte offset immediately after the last valid WAL entry
- `reserved_end`: byte offset up to which the file already has capacity reserved

Pseudocode:

```text
open():
  (last_lsn, logical_end) = scan_last_valid_entry()
  reserved_end = physical_file_len
  next_lsn = max(last_lsn, header.start_lsn) + 1
  offset = logical_end

append(bytes):
  ensure_capacity(offset + bytes.len)
  write bytes at logical_end
  logical_end += bytes.len
  offset = logical_end
```

### 2. Make capacity growth amortized

Reserve/grow WAL space in chunks, not per entry/commit.

Pseudocode:

```text
ensure_capacity(required_end):
  if required_end <= reserved_end:
    return

  new_reserved_end = round_up(required_end, PREALLOC_CHUNK)
  physically reserve/grow file to new_reserved_end
  durably sync the metadata change once
  reserved_end = new_reserved_end
```

Key rule:

- the expensive metadata sync is allowed only at reservation boundaries;
- steady-state commits inside the reserved region must avoid metadata sync.

### 3. Split durable sync modes

`WalWriter` needs two durability modes:

- `sync_metadata()` for create / rotate / truncate / reservation-boundary growth
- `sync_data()` for normal DML commit durability

Pseudocode:

```text
commit_dml():
  flush BufWriter to kernel
  sync data only

commit_metadata_change():
  flush BufWriter to kernel
  sync data + metadata
```

Fallback rule:

- if the platform cannot provide a meaningful data-only sync, fall back to the
  current full-sync path without changing correctness.

### 4. Teach readers/recovery about reserved tail bytes

Pre-reserved WAL space means physical EOF is no longer the same thing as the
last valid entry. The reader side must therefore stop at the last valid entry,
not at `file.metadata().len()`.

Pseudocode:

```text
scan_last_valid_entry():
  pos = WAL_HEADER_SIZE
  last_good = WAL_HEADER_SIZE
  last_lsn = 0
  while pos < physical_len:
    if next 4 bytes are zero/padding:
      break
    try parse WalEntry
      success => advance last_good / last_lsn
      failure => break
  return (last_lsn, last_good)

scan_backward():
  start cursor at logical_end from scan_last_valid_entry(), not physical EOF
```

This keeps crash recovery and reverse scans correct even if the reserved file
tail contains zeros or partially unused space.

## Implementation phases

1. Add logical-end discovery and reserved-capacity tracking in `WalWriter`.
2. Add explicit `commit_data_sync` vs `commit_metadata_sync` writer APIs and
   switch `TxnManager` to use them.
3. Update reader/open/recovery code so reserved tail bytes are treated as clean
   padding, not corruption.
4. Rewire the Phase `6.19` fsync-pipeline leader path to call the renamed WAL
   durable-commit API.
5. Re-run the targeted autocommit benchmark and record the before/after result.

## Tests to write

- unit: opening a WAL with reserved zero tail returns the last valid LSN and
  logical end correctly
- unit: backward scan starts at logical end, not physical EOF
- unit: reservation growth does not change the last valid WAL entry sequence
- integration: commit inside reserved capacity survives crash/reopen
- integration: reservation-boundary growth survives crash/reopen
- integration: fsync-pipeline leader/follower semantics remain correct with the
  new writer primitive
- bench: `local_bench.py --scenario insert_autocommit --rows 1000 --table`
  before/after comparison on the same release binary

## Anti-patterns to avoid

- Do not send MySQL `OK` before the WAL is durably committed
- Do not update WAL file-length metadata on every single commit
- Do not make reader/recovery trust physical EOF once preallocation exists
- Do not mix `COM_QUERY` parser caching into this same subphase; keep the blast
  radius centered on WAL durability

## Risks

- Reserved tail interpreted as corruption by existing scans
  - Mitigation: compute and use `logical_end` everywhere a scan currently uses
    physical EOF
- Platform-specific reservation API differences
  - Mitigation: keep a correct fallback path and isolate syscall details inside
    `writer.rs`
- Metadata growth not durable before subsequent data-only sync
  - Mitigation: reservation-boundary growth must finish with a metadata sync
    before later commits rely on that reserved region
- Benchmark improves but still remains below MariaDB/MySQL
  - Mitigation: explicitly defer repeated-`COM_QUERY` DML reuse to `27.8c`
    instead of conflating the two bottlenecks
