# WAL and Crash Recovery

The Write-Ahead Log (WAL) is AxiomDB's durability mechanism. Before any change
reaches the storage engine's pages, a record of that change is appended to the WAL
file. On crash recovery, the WAL is replayed to reconstruct any changes that were
committed but not yet flushed to the data file.

---

## WAL File Layout

The WAL file starts with a 32-byte file header followed by an unbounded sequence of
WAL entries.

### File Header — 32 bytes

```text
Offset  Size  Field
     0     4  magic      — 0x57414C4E ("WALN") — identifies a valid WAL file
     4     2  version    — WAL format version (currently 1)
     6    26  _reserved  — Future use
```

`WalReader::open` verifies the magic and version before any scan. An incorrect
magic returns `DbError::WalInvalidHeader`.

---

## Entry Binary Format

Each WAL entry is a self-delimiting binary record. The total entry length is stored
both at the beginning and at the end to support both forward and backward scanning.

```text
Offset       Size         Field
──────── ─────────── ─────────────────────────────────────────────────────
     0           4   entry_len     u32 LE — total entry length in bytes
     4           8   lsn           u64 LE — Log Sequence Number (globally monotonic)
    12           8   txn_id        u64 LE — Transaction ID (0 = autocommit)
    20           1   entry_type    u8     — EntryType (see below)
    21           4   table_id      u32 LE — table identifier (0 = system operations)
    25           2   key_len       u16 LE — key length in bytes (0 for BEGIN/COMMIT/ROLLBACK)
    27     key_len   key           [u8]   — mutation key bytes (heap RID or clustered PK)
     ?           4   old_val_len   u32 LE — old value length (0 for INSERT, BEGIN, COMMIT, ROLLBACK)
     ?   old_len    old_value      [u8]   — old encoded row (empty on INSERT)
     ?           4   new_val_len   u32 LE — new value length (0 for DELETE, BEGIN, COMMIT, ROLLBACK)
     ?   new_len    new_value      [u8]   — new encoded row (empty on DELETE)
     ?           4   crc32c        u32 LE — CRC32c of all preceding bytes in this entry
     ?           4   entry_len_2   u32 LE — copy of entry_len for backward scan

Minimum size (no key, no values): 4+8+8+1+4+2 + 4+4+4+4 = 43 bytes
```

### Why entry_len_2 at the end

To traverse the WAL backward (during ROLLBACK or crash recovery), the reader needs to
find the start of the previous entry given only the current position (end of entry).

```
entry_start = current_position - entry_len_2
```

The reader seeks to `entry_start`, reads `entry_len`, verifies it equals `entry_len_2`,
then reads the full entry. If the lengths do not match, the entry is corrupt.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — O(1) Backward Traversal Without an Index</span>
Storing <code>entry_len</code> at both ends of every entry enables backward scanning with a single seek per entry — no secondary index or reverse pointer table needed. The cost is 4 bytes per entry (overhead for a WAL with 10M entries: 40 MB, negligible relative to data payload).
</div>
</div>

### Mutation Key Encoding

Heap and clustered mutations do not use the same key contract:

```text
Heap INSERT / UPDATE / DELETE / UpdateInPlace:
  key_len = 10
  key[0..8]  = page_id as u64 LE
  key[8..10] = slot_id as u16 LE

ClusteredInsert / ClusteredDeleteMark / ClusteredUpdate (Phases 39.11 / 39.12):
  key_len = primary_key_bytes.len()
  key     = encoded primary-key bytes
```

Heap mutations still record the exact page and slot where the row was written,
so redo can target the same physical location directly. Clustered mutations do
not: clustered pages defragment, split, merge, and relocate rows, so `(page_id,
slot_id)` is not a stable undo key. Their payloads instead store the exact
logical row image and the latest clustered `root_pid`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — PK Undo, Not Slot Undo</span>
InnoDB's clustered-row undo is keyed by clustered identity rather than a heap-style slot address. AxiomDB adopts the same constraint in 39.11: once clustered rows can relocate inside slotted pages, the only stable rollback key is the primary key plus the exact old row image.
</div>
</div>

---

## Entry Types

```rust
pub enum EntryType {
    Begin      = 1,  // START of an explicit transaction
    Commit     = 2,  // COMMIT — all preceding entries for this txn_id are durable
    Rollback   = 3,  // ROLLBACK — all preceding entries for this txn_id must be undone
    Insert     = 4,  // INSERT: old_value is empty; new_value is the encoded new row
    Delete     = 5,  // DELETE: old_value is the encoded row before deletion; new_value empty
    Update     = 6,  // UPDATE: both old_value and new_value are present
    Checkpoint = 7,  // CHECKPOINT: marks the LSN up to which pages are flushed to disk
    Truncate   = 8,  // Full-table delete (DELETE without WHERE, TRUNCATE TABLE)
    PageWrite  = 9,  // Bulk insert page image + slot list
    UpdateInPlace = 10, // Stable-RID same-slot update
    ClusteredInsert = 12, // Clustered insert keyed by PK + exact new row image
    ClusteredDeleteMark = 13, // Clustered delete-mark keyed by PK + old/new row image
    ClusteredUpdate = 14, // Clustered update keyed by PK + old/new row image
}
```

Transaction entries (`Begin`, `Commit`, `Rollback`) carry no key or value payload —
`key_len = 0`, `old_val_len = 0`, `new_val_len = 0`. The minimum entry size of 43 bytes
applies to these records.

`PageWrite` and `UpdateInPlace` are physical optimization records. They do not change
SQL-visible semantics; they only change how AxiomDB amortizes I/O for common write
patterns while preserving rollback and crash recovery guarantees.

---

## WalEntry::Truncate — Full-Table Delete

`WalEntry::Truncate` (entry type 8) is emitted instead of N individual `Delete`
entries when a statement deletes every row in a table: `DELETE FROM t` without a
WHERE clause, and `TRUNCATE TABLE t`.

### Binary Format

```text
Field           Value
─────────────── ────────────────────────────────────────────────────────
entry_type      8 (Truncate)
table_id        the target table's ID (u32 LE)
key_len         8
key[0..8]       root_page_id of the HeapChain as u64 LE
old_val_len     0 (empty — no per-row data stored)
new_val_len     0 (empty)
```

The key encodes the heap chain's root page rather than a single slot, because
the undo operation scans the entire chain.

### Why One Entry Instead of N

For a 10,000-row table, the per-row path writes 10,000 `Delete` WAL entries. Each
entry carries at minimum 43 bytes of header plus the encoded row payload (old_value),
which may be hundreds of bytes. `WalEntry::Truncate` replaces all N entries with a
single 51-byte record (43-byte minimum + 8-byte key).

```
Per-row Delete path (N = 10,000 rows, avg 100-byte payload):
  WAL entries: 10,000
  WAL bytes written: 10,000 × (43 + 10 + 100) ≈ 1.5 MB

Truncate path:
  WAL entries: 1
  WAL bytes written: 51 bytes
```

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">10,000× Fewer WAL Entries Than MySQL InnoDB for Full-Table DELETE</span>
MySQL InnoDB writes one undo log entry per deleted row for every DELETE — including <code>DELETE FROM t</code> without a WHERE clause. For a 10K-row table, InnoDB writes ~10,000 undo records; AxiomDB writes 1 WAL entry. This is the same optimization that MariaDB's storage engine API exposes via <code>ha_delete_all_rows()</code>, but AxiomDB applies it at the WAL level, not just the engine level.
</div>
</div>

### Undo — Rollback and Crash Recovery

Because `WalEntry::Truncate` stores no per-row state, undo cannot simply replay
individual slot reverts from the WAL. Instead, undo calls
`HeapChain::clear_deletions_by_txn(txn_id)`, which scans the heap chain and clears
the `txn_id_deleted` stamp on every slot that was deleted by this transaction:

```
Undo of WalEntry::Truncate for txn_id T:
  for each page in the HeapChain:
    read_page(page_id)
    for each slot on the page:
      if slot.txn_id_deleted == T:
        slot.txn_id_deleted = 0
        slot.deleted = 0
    write_page(page_id, page)
```

The physical heap is fully restored: all rows that were alive before the DELETE
become visible again to transactions with a snapshot predating txn_id T.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Undo via Heap Scan, Not Stored Slot List</span>
An alternative design would store the list of (page_id, slot_id) pairs inside the <code>Truncate</code> entry itself, enabling O(N) targeted undo without a full scan. We chose the scan approach because: (1) WAL writes are on the critical path of every DELETE; (2) undo (rollback and crash recovery) is rare relative to DELETE frequency; (3) the scan is O(P) in pages, not O(N) in rows, and P ≪ N at 200 rows/page. The trade-off mirrors MariaDB's <code>ha_delete_all_rows()</code> philosophy: optimize the common path (write), accept a bounded cost on the uncommon path (undo).
</div>
</div>

### Crash Recovery Handling

During WAL replay, when the recovery engine encounters `WalEntry::Truncate` for a
committed transaction, it calls `HeapChain::delete_batch()` with all live slot IDs
found by `scan_rids_visible()` — re-applying the deletion to any pages that may not
have been flushed before the crash. If the transaction was not committed (no matching
`Commit` entry in the WAL), the entry is skipped: the heap still contains the
pre-delete state because the crash occurred before the commit was durable.

---

## WalEntry::UpdateInPlace — Stable-RID UPDATE

`WalEntry::UpdateInPlace` (entry type 10) records a same-slot heap rewrite. It is
emitted when UPDATE can preserve the original `(page_id, slot_id)` because the new
encoded row still fits in the existing heap slot.

Since `6.20`, the executor may emit many `UpdateInPlace` records through one
`record_update_in_place_batch(...)` call. The on-disk format does not change:
the optimization is only in how normal entries are serialized and appended
(`reserve_lsns(...) + write_batch(...)` once per statement instead of one append
call per row).

### Binary Format

```text
Field           Value
─────────────── ───────────────────────────────────────────────────────────────
entry_type      10 (UpdateInPlace)
table_id        target table ID
key             logical row key carried by the caller
old_value       [page_id:8][slot_id:2][old tuple image...]
new_value       [page_id:8][slot_id:2][new tuple image...]
```

The tuple image is the full logical row image stored in the slot:

```text
[RowHeader || encoded row bytes]
```

Undo and crash recovery decode the physical location from the first 10 bytes and then
restore the old tuple image directly into the same slot.

### Why a New Entry Type Instead of Reusing Update

Classic `Update` in AxiomDB means logical delete+insert and therefore carries two
different physical locations. `UpdateInPlace` means “same physical location, bytes
changed in place”. Reusing `Update` would blur those two recovery contracts and make
undo logic branch on payload shape instead of entry type.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Physical Contract Is Explicit</span>
PostgreSQL HOT also distinguishes between “new tuple version elsewhere” and “same-page
optimization” at the storage-contract level. AxiomDB keeps that distinction explicit in
the WAL so recovery can restore the exact old tuple image without guessing which UPDATE
shape was used.
</div>
</div>

### Undo and Recovery

Rollback and crash recovery treat `UpdateInPlace` as a direct restore:

```text
read page(page_id)
restore old tuple image at slot_id
write page(page_id)
```

If the transaction committed, recovery leaves the rewritten bytes in place. If the
transaction did not commit, recovery restores `old_value` to the same slot.

---

## Clustered Mutation Entries (Phases 39.11 / 39.12)

Phase `39.11` adds the first WAL contract for clustered rows, and Phase `39.12`
extends it into clustered crash recovery:

```text
key       = encoded primary-key bytes
old_value = ClusteredRowImage?   // absent on insert
new_value = ClusteredRowImage?   // absent on pure delete undo payload

ClusteredRowImage:
  [root_pid: u64]
  [RowHeader: 24B]
  [row_len: u32]
  [row_data bytes]
```

`TxnManager` now tracks the latest clustered `root_pid` per `table_id` inside the
active transaction. Rollback and `ROLLBACK TO SAVEPOINT` use that tracked root
and clustered-tree helpers:

- undo clustered insert → `delete_physical_by_key(...)`
- undo clustered delete-mark / update → `restore_exact_row_image(...)`

Phases `39.14`, `39.16`, and `39.17` are the first SQL-visible executor users of that contract:

- a fresh clustered SQL insert records `ClusteredInsert`
- reusing a snapshot-invisible delete-marked clustered PK records
  `ClusteredUpdate`, because rollback must restore the old tombstone image, not
  simply delete the new row
- clustered SQL update now records the exact old clustered row image before the
  rewrite, even for same-leaf in-place updates and relocate-updates
- clustered SQL delete now records the exact old clustered row image before the
  delete-mark so rollback/savepoints can restore the prior `txn_id_deleted = 0`
  state exactly
- clustered secondary bookmark entries still use the ordinary B+ Tree undo path,
  but `39.16` extends that undo to both halves of a rewritten secondary key:
  rollback can delete newly inserted bookmark entries and reinsert the old
  physical bookmark entry against the current index root

The invariant is intentionally logical: rollback restores the old primary-key
row state, not the exact pre-change page topology. A relocate-update may split
or merge the tree on the forward path, and rollback may restore the old row into
a different physical leaf as long as the visible row state matches the original.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Restore State, Not Topology</span>
PostgreSQL's B-tree WAL is page-topology-oriented, but that is the wrong first cut for AxiomDB's clustered rewrite because clustered slots are not stable across defragment, split, and merge. 39.11/39.12 therefore restore exact row state by PK and row image instead of trying to replay clustered page topology physically.
</div>
</div>

`39.12` now uses the same payloads during crash recovery:

- reverse-undo in-progress clustered inserts by `delete_physical_by_key(...)`
- reverse-undo in-progress clustered delete-mark/update by `restore_exact_row_image(...)`
- track the current clustered root per table while recovery undoes those writes
- seed `TxnManager::open_with_recovery(...)` with the final recovered root map

`TxnManager::open(...)` also reconstructs the latest committed clustered root
per table from surviving WAL history on a clean reopen.

---

## Checkpoint Protocol — 5 Steps

A checkpoint ensures that all dirty pages below a given LSN are written to the `.db`
file so that WAL entries before that LSN can be safely truncated.

```
Step 1: Write a Checkpoint entry to the WAL with the current LSN.
        This entry marks the start of the checkpoint.

Step 2: Call storage.flush() — ensures all dirty mmap pages are written
        to disk via msync(). After this point, every page modification
        with LSN ≤ checkpoint_lsn is on disk.

Step 3: Update the meta page (page 0) with the new checkpoint_lsn.
        This is the commit point: if we crash after step 3, recovery
        can skip all WAL entries with LSN ≤ checkpoint_lsn.

Step 4: Write the updated meta page to disk (flush again, just for page 0).

Step 5: Optionally truncate the WAL file, removing all entries with
        LSN ≤ checkpoint_lsn. (WAL rotation is planned — currently
        the WAL grows indefinitely and is truncated on checkpoint.)
```

If the process crashes between step 2 and step 3, the checkpoint LSN in the meta
page still points to the previous checkpoint. Recovery replays from the old
checkpoint LSN — this is safe because step 2 already flushed the pages.

---

## Crash Recovery State Machine

AxiomDB tracks its recovery state through five well-defined phases. The state
transitions are strictly sequential; no transition can be skipped.

```
CRASHED
   │
   │  detect: last shutdown was not clean (no clean-close marker)
   ▼
RECOVERING
   │
   │  open .db file: verify meta page checksum and format version
   │  open .wal file: verify WAL header magic and version
   ▼
REPLAYING_WAL
   │
   │  scan WAL forward from checkpoint_lsn
   │  for each entry with LSN > checkpoint_lsn:
   │    if entry.txn_id is in the committed_set:
   │      replay the mutation (redo)
   │    else:
   │      skip (uncommitted changes are discarded by ignoring)
   │
   │  committed_set = {txn_id for all txn_ids with a Commit entry in the WAL}
   ▼
VERIFYING
   │
   │  run heap structural check (all slot offsets within bounds,
   │  no overlapping tuples, free_start < free_end)
   │  run MVCC consistency check (xmin ≤ xmax for all live rows)
   ▼
READY
   │
   │  normal operation resumes
```

### Why no UNDO pass

AxiomDB's replay path is **redo-only** for the classic heap WAL entries that are
already replayable. Uncommitted transactions are simply ignored during the
forward scan. Because that heap WAL records physical locations `(page_id,
slot_id)`, the page that contained the uncommitted write is overwritten with the
committed state from the WAL. If the page has no committed mutations after the
checkpoint, it retains its pre-crash state (which was correct, because the
checkpoint flushed all committed changes up to `checkpoint_lsn`).

This avoids the UNDO pass required by logical WALs (like PostgreSQL's pg_wal), which
must undo changes to B+ Tree pages in reverse order. Physical WAL with redo-only
recovery is simpler and faster.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Faster Recovery — Single Forward Scan</span>
PostgreSQL's logical WAL requires two passes on recovery: a forward redo pass, then a backward undo pass to reverse uncommitted changes in B+ Tree pages. AxiomDB's classic heap WAL (recording exact <code>page_id + slot_id</code>) requires only one forward pass — uncommitted writes are simply overwritten by committed redo entries.
</div>
</div>

For clustered entries, `39.12` adds the first recovery extension on top of that
model: unresolved clustered transactions are now undone by primary key and exact
row image instead of returning `NotImplemented`. The remaining gap is narrower:
clustered root persistence still depends on surviving WAL history and is not yet
checkpoint/rotation-stable.

---

## WalReader Design

`WalReader` is stateless. It stores only the file path. Each scan call opens a new
`File` handle.

**Forward scan** (`scan_forward`): uses `BufReader<File>` to amortize syscall overhead
on sequential reads. Reads are sequential and predictable — the OS readahead prefetches
the next WAL sectors automatically.

**Backward scan** (`scan_backward`): uses a seekable `File` directly. `BufReader`
would be counterproductive here because seeks invalidate the read buffer. Each backward
step seeks to `current_pos - 4` to read `entry_len_2`, then seeks back to
`current_pos - entry_len_2` to read the full entry.

**Corruption handling**: both iterators return `Result<WalEntry>`. On the first corrupt
entry (truncated bytes, CRC mismatch, unknown entry type), the iterator yields an `Err`
and stops. The caller decides whether to propagate or recover gracefully.

---

## WAL and Concurrency

### Single-Writer Model

WAL writes are serialized through a single `WalWriter` inside `TxnManager`. The
server runtime uses `Arc<tokio::sync::RwLock<Database>>`: readers may overlap,
but mutating statements still serialize behind the write guard. This eliminates
write-write conflicts without record-level locking (Phase 13.7 will lift this
constraint).

### WAL Fsync Pipeline (Phase 6.19)

The old timer-based `CommitCoordinator` from `3.19` is now superseded in the
server path by an always-on **leader-based fsync pipeline** inspired by
MariaDB's `group_commit_lock`.

Connections still write `Commit` entries into the WAL `BufWriter`, but the
handoff after that changed:

1. the connection calls `pipeline.acquire(commit_lsn, txn_id)`
2. if another leader already flushed past `commit_lsn` → `Expired`
3. if no leader is active → `Acquired`, this connection performs `flush+fsync`
4. if a leader is active → `Queued(rx)`, this connection releases the DB lock
   and awaits confirmation

```
Conn A → lock → DML → commit_deferred() → pipeline.acquire(42) → Acquired
         flush+fsync → release_ok(42) → unlock → OK

Conn B →           lock → DML → commit_deferred() → pipeline.acquire(43) → Queued(rx)
                   unlock → await rx ──────────────────────────────────────────────┐
Leader A fsync completes → flushed_lsn = 43 → wake B ─────────────────────────────┘

Conn C → lock → DML → commit_deferred() → pipeline.acquire(41) → Expired → OK
```

#### Durability Guarantee

A connection does **not** receive `Ok` until the fsync covering its `Commit`
entry completes. `max_committed` advances only after the leader confirms
durability. If the process crashes before that fsync, the transaction is lost
and no client received `Ok`. The durability guarantee is therefore identical to
inline fsync; only the scheduling changes.

#### Key Structures

| Component | Location | Role |
|---|---|---|
| `FsyncPipeline` | `axiomdb-wal/src/fsync_pipeline.rs` | Shared state: `flushed_lsn`, `leader_active`, `pending_lsn`, waiter queue |
| `AcquireResult` | same file | `Expired` / `Acquired` / `Queued(rx)` outcome for each commit |
| `TxnManager::deferred_commit_mode` | `axiomdb-wal/src/txn.rs` | Internal hook used by the server path to defer inline fsync until the pipeline leader runs |
| `TxnManager::advance_committed()` | same file | Advances `max_committed` to `max(batch_txn_ids)` after fsync |
| `Database::take_commit_rx()` | `axiomdb-network/src/mysql/database.rs` | Bridges SQL execution to pipeline acquire / leader fsync / follower await |

### PageWrite Entry (Phase 3.18)

`WalEntry::PageWrite` (entry type 9) replaces N `Insert` entries with **one entry per
heap page** during bulk inserts. Instead of serializing one entry per row, the executor
groups rows by their target page and writes a single entry per page.

```text
key:       page_id as u64 LE (8 bytes)
old_value: empty
new_value: [page_bytes: PAGE_SIZE][num_slots: u16 LE][slot_id × N: u16 LE]
```

The `page_bytes` field contains the full post-modification page (16 KB for the default
page size). The embedded `slot_ids` let crash recovery undo uncommitted `PageWrite`
entries at slot granularity — identical in effect to undoing N individual `Insert` entries.

**CPU cost comparison for 10K-row bulk insert (~42 pages at 16 KB):**

```
Insert path (3.17):  10,000 × serialize_into() + 10,000 × CRC32c  ← O(N rows)
PageWrite (3.18):        42 × serialize_into() +     42 × CRC32c  ← O(P pages) — 238× less
```

**WAL file size comparison for 10K rows:**

```
Insert entries:  10,000 × ~100B = ~1 MB
PageWrite:           42 × ~16.9 KB = ~710 KB  ← 30% smaller
```

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">238× Fewer WAL Serializations Than Per-Row Logging</span>
For a 10K-row bulk INSERT, AxiomDB writes 42 WAL entries (one per 16KB page) instead of 10,000. Crash recovery scans 42 entries instead of 10,000 — proportionally faster. PostgreSQL's COPY command uses the same page-image strategy for bulk loads; AxiomDB applies it automatically to all multi-row INSERT statements.
</div>
</div>

**Crash recovery for uncommitted PageWrite:**

```
for each PageWrite entry in uncommitted txn:
  page_id   = entry.key[0..8] as u64 LE
  num_slots = entry.new_value[PAGE_SIZE..+2] as u16 LE
  for i in 0..num_slots:
    slot_id = entry.new_value[PAGE_SIZE+2+i*2..+2] as u16 LE
    mark_slot_dead(storage, page_id, slot_id)   // same as undoing Insert
```

### Batch WAL Append (Phase 3.17)

For bulk inserts (`INSERT INTO t VALUES (r1),(r2),...`) `TxnManager::record_insert_batch()`
writes all N Insert WAL entries in **a single `write_all` call**:

```
Per-row path (before 3.17):
  for each of N rows: append_with_buf(entry, scratch)  ← N × write_all to BufWriter

Batch path (3.17):
  lsn_base = wal.reserve_lsns(N)
  for each row: entry.serialize_into(&mut wal_scratch)  ← accumulate in RAM
  wal.write_batch(&wal_scratch)                         ← 1 × write_all
```

The entries written to disk are byte-for-byte identical to the per-row path —
crash recovery reads them the same way. The improvement is purely in CPU and
syscall overhead: O(1) BufWriter calls instead of O(N).

Combined with `HeapChain::insert_batch()` (O(P) page writes for P pages) and
a single parse+analyze pass for multi-row VALUES, the full bulk INSERT pipeline
is O(P) in both storage I/O and WAL I/O, where P = number of pages filled ≈ N/200.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
MariaDB's `group_commit_lock` avoids waiting for a timer before piggybacking followers. AxiomDB now does the same: instead of batching only on a timeout window, queued commits can piggyback immediately on an in-flight leader fsync, which is exactly the case that matters for fast single-connection autocommit.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Keep the Lock, Remove the Timer</span>
`FsyncPipeline` still uses a tiny synchronous mutex for the O(1) leader election state check, but AxiomDB rejects the old Tokio background task and configurable timer window. The lock is held only for state mutation; the actual <code>flush+fsync</code> still runs outside that mutex and under the existing database write lock.
</div>
</div>

---

## Compact PageWrite Format

The `WalEntry::PageWrite` entry was updated to eliminate the 16 KB page image:

**Old format** (per page):
```
new_value = [page_bytes: 16384 B][num_slots: u16 LE][slot_ids: u16 × N]
```

**New compact format** (per page):
```
new_value = [num_slots: u16 LE][slot_ids: u16 × N]
```

Crash recovery only needs slot IDs to mark inserted slots dead on undo — it never
uses the stored page bytes. Eliminating them reduces WAL size from ~820 KB to ~20 KB
per 10K-row batch (**40× reduction**).

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">WAL Size Advantage</span>
Compact PageWrite reduces WAL data from 16 KB/page (full snapshot, like PostgreSQL's
full-page-write mode) to ~400 B/page (slot list only). For 10K-row batch INSERT:
820 KB → 20 KB, matching MariaDB's InnoDB redo log density of ~50 B/row.
</div>
</div>
