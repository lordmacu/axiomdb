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
    27     key_len   key           [u8]   — key bytes (physical location: page_id:8 + slot_id:2)
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

### Physical Key Encoding

The key field encodes the physical row location for data mutations:

```text
key_len = 10 bytes (always for INSERT/UPDATE/DELETE)
key[0..8]  = page_id as u64 LE
key[8..10] = slot_id as u16 LE
```

This means the WAL records the exact page and slot where the row was written —
recovery can replay the write to the exact same physical location without rebuilding
any in-memory state. The executor and B+ Tree need no memory during WAL replay.

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
}
```

Transaction entries (`Begin`, `Commit`, `Rollback`) carry no key or value payload —
`key_len = 0`, `old_val_len = 0`, `new_val_len = 0`. The minimum entry size of 43 bytes
applies to these records.

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

AxiomDB's WAL replay is **redo-only**. Uncommitted transactions are simply ignored
during the forward scan. Because the WAL records physical locations (page_id, slot_id),
the page that contained the uncommitted write is overwritten with the committed state
from the WAL. If the page has no committed mutations after the checkpoint, it retains
its pre-crash state (which was correct, because the checkpoint flushed all committed
changes up to `checkpoint_lsn`).

This avoids the UNDO pass required by logical WALs (like PostgreSQL's pg_wal), which
must undo changes to B+ Tree pages in reverse order. Physical WAL with redo-only
recovery is simpler and faster.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Faster Recovery — Single Forward Scan</span>
PostgreSQL's logical WAL requires two passes on recovery: a forward redo pass, then a backward undo pass to reverse uncommitted changes in B+ Tree pages. AxiomDB's physical WAL (recording exact <code>page_id + slot_id</code>) requires only one forward pass — uncommitted writes are simply overwritten by committed redo entries.
</div>
</div>

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
`Database` is wrapped in `Arc<tokio::sync::Mutex<Database>>` — only one connection
executes DML at a time. This eliminates write–write conflicts without record-level
locking (Phase 7 will lift this constraint).

### Group Commit (Phase 3.19)

Under the default single-fsync-per-commit model, N concurrent connections pay N
sequential `fsync` calls. **Group Commit** batches those fsyncs: connections write
their `Commit` WAL entries to the `BufWriter` (fast, RAM only) and register with the
`CommitCoordinator` instead of fsyncing inline. A background Tokio task wakes every
`group_commit_interval_ms` (or immediately when `group_commit_max_batch` connections
are waiting), acquires the Database lock, executes a **single** `flush + fsync`, then
notifies all waiting connections.

```
Disabled (default):
  Conn A → lock → DML → commit() [flush+fsync inline] → unlock → OK
  Conn B →                        lock → DML → commit() [flush+fsync] → unlock → OK
  Cost: 2 fsyncs

Enabled (group_commit_interval_ms = 1):
  Conn A → lock → DML → commit_deferred() → unlock → await rx ──────┐
  Conn B →         lock → DML → commit_deferred() → unlock → await ──┤
  Background task:  lock → flush+fsync → advance_committed → unlock  │
                    notify A ──────────────────────────────────────── ┘
                    notify B
  Cost: 1 fsync for both A and B
```

#### Durability Guarantee

A connection does **not** receive `Ok` until the fsync covering its `Commit` entry
completes. `max_committed` advances only after `advance_committed()` is called — which
happens only inside the background task, after a successful fsync. If the process
crashes before the fsync, the transaction is lost and no client received `Ok`. The
durability guarantee is identical to non-group-commit mode; only the throughput changes.

#### Key Structures

| Component | Location | Role |
|---|---|---|
| `CommitCoordinator` | `axiomdb-network/src/mysql/commit_coordinator.rs` | Pending queue (`std::sync::Mutex<Vec<CommitTicket>>`), `Notify` trigger |
| `CommitTicket` | same file | `txn_id + oneshot::Sender<Result<(), DbError>>` per waiting connection |
| `TxnManager::deferred_commit_mode` | `axiomdb-wal/src/txn.rs` | When `true`, `commit()` skips fsync and sets `pending_deferred_txn_id` |
| `TxnManager::advance_committed()` | same file | Advances `max_committed` to `max(batch_txn_ids)` after fsync |
| `spawn_group_commit_task()` | `axiomdb-network/src/mysql/group_commit.rs` | Long-running Tokio task; `Weak<Mutex<Database>>` exits on DB drop |

#### Configuration

```toml
# axiomdb.toml
group_commit_interval_ms = 1   # 0 = disabled (default); 1ms recommended for production
group_commit_max_batch   = 64  # trigger fsync immediately when 64 connections are waiting
```

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
PostgreSQL uses group commit with <code>synchronous_commit=on</code> (the default) and still pays one fsync per transaction under low concurrency. AxiomDB's coordinator batches across all concurrent connections with a configurable interval, reducing fsync overhead from O(N connections) to O(1) per batch window — the same improvement PostgreSQL achieves only at high concurrency.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — std::sync::Mutex for CommitCoordinator</span>
The <code>CommitCoordinator::pending</code> queue uses <code>std::sync::Mutex</code> (not Tokio's async mutex) so that <code>register_pending()</code> can be called from synchronous code inside <code>Database::execute_query</code> without infecting the function signature with <code>async</code>. The lock is held only for an O(1) Vec push — never across an <code>.await</code> point, so no deadlock risk and no blocking of the Tokio runtime.
</div>
</div>
