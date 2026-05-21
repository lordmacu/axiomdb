# Spec: subphase 3 — write-ahead integration (the crux)

Phase: redo-recovery (project B) — subphase 3
Task: Make `MmapStorage`'s live read/write path frame-log-aware, additively.
Status: draft
Effort: **max** (data-loss risk; touches the hot read/write path).

## Context

Subphase 2 built an isolated page-frame log (`FrameLog` + `WalIndex` + `FrameRef`
in `crates/axiomdb-wal/src/wal_frame.rs`) that does NOT touch the live path yet.
Subphase 3 wires the live `read_page`/`write_page` of `MmapStorage`
(`crates/axiomdb-storage/src/mmap.rs`) to it. Architecture is **Option A (SQLite-WAL
write-ahead)**, LOCKED in `spec-redo-recovery.md`. Reference:
`research/sqlite/src/wal.c` (`walFindFrame` read path, `walFrames` write path).

**Crate-cycle resolution (decided in brainstorm):** `axiomdb-storage` cannot depend
on `axiomdb-wal` (the dependency is the reverse). Therefore `FrameLog`/`WalIndex`/
`FrameRef` **move from `axiomdb-wal` to `axiomdb-storage`** so `MmapStorage` can own
them directly — no circular dependency, no `dyn` dispatch on the hot path. The
**LSN source is a storage-owned `AtomicU64`** (not the WAL writer's `next_lsn`); the
recovery nexus between the physical page-frame log and the logical WAL is the
`commit_marker = txn_id`, not a shared LSN. Invariant: *redo from page frames
(commit_marker), undo from logical records; the nexus is txn_id*.

## Goal

When the frame log is enabled, every page write also appends a page-image frame
(stamped with a storage LSN) and `read_page` serves the latest frame for a page;
when it is disabled (the subphase-3 default), behavior is byte-for-byte identical to
today.

## Non-goals

These belong to later subphases and MUST NOT be done here:

- **Frame-only writes / removing the main-file `pwrite`** — deferred to subphase 4.
  Subphase 3 keeps the main file authoritative (dual-write when enabled) so `flush()`
  and durability are unchanged and the tree stays green.
- **Dropping the per-commit `storage.flush()` / commit boundary / commit_marker on
  the last frame** — subphase 4.
- **Recovery (rebuild the live index from committed frames on open) → T0 green** —
  subphase 5. T0 stays RED (`#[ignore]`d) after this subphase.
- **Checkpoint (frames → main file) and retiring doublewrite** — subphase 6.
- **Enabling the frame log by default** — subphase 4. In subphase 3 it is opt-in.
- **Any change to `MemoryStorage`** (RAM backend for unit tests; not durable).
- **Any change to the `StorageEngine` trait signatures** — frame-awareness is
  internal to `MmapStorage`.

## Behavior

### Crate move (mechanical, do first)

Move `wal_frame.rs` from `axiomdb-wal` to `axiomdb-storage`:
- `crates/axiomdb-wal/src/wal_frame.rs` → `crates/axiomdb-storage/src/wal_frame.rs`.
- In it, `use axiomdb_storage::PAGE_SIZE` → `use crate::page::PAGE_SIZE`.
- `axiomdb-storage/src/lib.rs`: add `mod wal_frame;` + `pub use wal_frame::{FrameLog, WalIndex, FrameRef};`.
- `axiomdb-wal/src/lib.rs`: remove the `wal_frame` module + its re-exports. If
  `axiomdb-wal` references these types (subphase 5 will), it imports them from
  `axiomdb_storage`.
- The 6 unit tests in the `tests` module move with the file and must still pass.

### New `MmapStorage` internal state (no public-API change)

```rust
// New fields on MmapStorage:
/// Page-image redo log. `None` ⇒ write-ahead disabled ⇒ behaves exactly as today.
/// NOT wrapped in a Mutex: `FrameLog::append` is lock-free (see Concurrency).
frame_log: Option<FrameLog>,
/// Live page → latest-appended-frame index (superset of committed; updated on
/// every append within this session). Internally sharded (16-way, like
/// `buffer_pool`); `record`/`latest` take `&self`. Empty when frame_log is None.
wal_index: WalIndex,
/// Monotonic LSN STAMPED INTO each page's header (PageHeader.lsn). Starts at 1
/// (0 = "older than any frame", the unmaintained legacy value). On open,
/// initialized past the highest LSN already in the frame log. Distinct from the
/// frame log's internal `write_offset` atomic (file placement).
frame_lsn: AtomicU64,
```

The frame log is enabled via a constructor variant or method (exact name decided in
`/plan-task`, e.g. `MmapStorage::create_with_redo(path)` / `open_with_redo(path)` or
an `enable_redo_log(&self)`), used by new tests in this subphase. The frame-log file
path is derived from the db path (e.g. `<db>.wf`), mirroring
`DoublewriteBuffer::for_db`.

### Concurrency / scalability (multi-writer — first-class, not retrofitted)

AxiomDB is multi-writer (Phase 40). The frame log MUST NOT serialize writers behind
a global lock. It mirrors the proven models already in this codebase
(`ConcurrentWalWriter`: lock-free `AtomicU64` LSN + ~1µs submission + group-commit
leader; `BufferPool`: 16-shard) and PostgreSQL/MySQL-8:

- **`FrameLog::append(&self)` is lock-free.** The file offset is reserved with
  `write_offset: AtomicU64::fetch_add(FRAME_SIZE)` (like `reserve_lsn`); the frame
  header + page are then written with `write_all_at(offset)` — `pwrite(2)` is
  thread-safe for disjoint regions, so concurrent writers never contend. No
  `Mutex<FrameLog>`. (The file header + salt are immutable after create.)
- **`WalIndex` is internally sharded 16-way** (same structure as `BufferPool`):
  `record`/`latest` lock only the page's shard. No global `RwLock`.
- **Deferred to subphase 4 (when the frame log becomes durability):** the
  *contiguous durable prefix*. Concurrent lock-free appends can leave a transient
  hole (writer A reserves offset N+1 and finishes before writer B finishes N); a
  crash then leaves N torn but N+1 intact. Recovery's `scan()` must stop at the
  highest *contiguously* written frame. This is MySQL 8.0's `link_buf`
  (`recent_written`) / PostgreSQL's wait-on-`WALInsertLock` pattern, added with the
  commit boundary in subphase 4. In subphase 3 it is unnecessary: dual-write keeps
  the main file authoritative and recovery does not consult frames until subphase 5.

Reference: `research/postgres/.../xlog.c` (`ReserveXLogInsertLocation`,
`WALInsertLocks`), MySQL 8.0 redo log (`log_sys`, `link_buf`).

### `WalIndex` — new live-update method (sharded)

`build_index()` (existing) reconstructs from disk excluding the uncommitted tail —
used by recovery (subphase 5). Subphase 3 makes `WalIndex` internally sharded and
adds a **live insert** taking `&self` (interior mutability per shard):

```rust
impl WalIndex {
    /// Records that `page_id`'s latest version is now this frame (live append path).
    /// Locks only `page_id`'s shard. `latest(&self)` likewise.
    pub fn record(&self, frame: FrameRef);
}
```

### `read_page` ordering (SQLite `walFindFrame` model)

```
1. page_id >= page_count          → PageNotFound        (unchanged)
2. buffer_pool.get(page_id)        → hit: return cached  (unchanged hot path)
3. if frame_log is None            → mmap read (today's cold path)   ← additive escape
4. wal_index.read().latest(page_id):
     Some(frame) → FrameLog::read_page_at(frame.offset), verify checksum,
                   insert into buffer_pool, return
     None        → mmap read (today's cold path)
```

SQLite parallel: `if (iLast==0) return early` — when the index is empty the lookup is
skipped, so an enabled-but-empty log behaves like today. The buffer-pool hit (step 2)
is checked **before** the index, so warm reads never pay the index lookup.

### `write_page` (and `write_page_under_page_lock`) when frame log enabled

```
1. lsn = frame_lsn.fetch_add(1)                          (Relaxed; monotonic)
2. copy page bytes into a mutable [u8; PAGE_SIZE] buffer
3. stamp lsn at header offset 24 (8 bytes LE)            ← does NOT touch the
   page checksum (checksum at offset 12 covers only body [64..16384])
4. pwrite the stamped buffer to the main file            ← dual-write (subphase 3)
5. frame_log.append(page_id, lsn, commit_marker = 0, &buffer)   (no fsync here)
6. wal_index.record(FrameRef { page_id, lsn, commit_marker: 0, offset })
7. buffer_pool.insert(page_id, Arc<Page>::from(buffer))  ← serve read-after-write
8. dirty.mark(page_id)                                   (unchanged; flush still works)
```

When frame log is `None`: steps 1–3,5–7 are skipped; the method is exactly today's
`write_page_inner` (pwrite + buffer_pool.invalidate + dirty.mark).

`commit_marker = 0` for all subphase-3 writes (no commit boundary yet); the live
index is updated regardless of marker, so in-session reads see the writes. The marker
on the last frame of a txn is subphase 4.

### Semantics

- **Precondition:** `page` has a valid body checksum (callers already ensure this).
- **Postcondition (enabled):** the main file AND the frame log both contain the
  lsn-stamped page; `wal_index.latest(page_id)` points to the appended frame;
  `buffer_pool.get(page_id)` returns the new bytes.
- **Postcondition (disabled):** identical to today.
- **Invariant (subphase 3):** the main file remains a complete authoritative copy
  (dual-write) ⇒ any page not in the live index reads correctly from mmap, and
  `flush()`/durability are unchanged.
- **Invariant:** `frame_lsn` is strictly monotonic; a frame's lsn is stamped into its
  page header so a later checkpoint/recovery can apply `frame.lsn > on_disk.lsn`.

### Error cases

| Input | Expected error | Notes |
|-------|----------------|-------|
| `read_page` of a frame whose CRC fails | `DbError::ChecksumMismatch` | frame corruption surfaced like a page checksum failure |
| `write_page` with `page_id >= page_count` | `DbError::PageNotFound { page_id }` | unchanged guard, before any frame append |
| frame log append I/O failure | `DbError` (`classify_io`) | propagated; main pwrite already done — dual-write means data is still durable in main (subphase-3 safety) |

## Edge cases

- [ ] Frame log **disabled** (None): all existing storage/index/sql tests pass
      byte-for-byte unchanged.
- [ ] Enabled, **empty index**: reads fall through to mmap (= today).
- [ ] Enabled, **write then read same page in-session**: read served from the frame
      (verify bytes match what was written).
- [ ] Enabled, **two writes to the same page**: index points to the latest frame;
      read returns the latest bytes.
- [ ] Enabled, **buffer-pool hit after write**: warm read does not consult the index.
- [ ] Enabled, **read a never-written page**: not in index → mmap.
- [ ] **lsn stamping** does not change the page's body checksum (verify the page
      still verifies after stamping).
- [ ] **Concurrent** writes to different pages (existing concurrency test) still pass
      with the frame log enabled (frame_log Mutex held only during append).
- [ ] **alloc_page** of a fresh page then read: in subphase 3, `alloc_page` keeps its
      direct pwrite (page not in index) → read falls through to mmap (correct via
      dual-write). (Capturing alloc into frames is subphase 4.)

## On-disk format

No new format. The frame layout is subphase 2's
(`page_id(8) lsn(8) commit_marker(4) salt(8) frame_crc(4)` + 16 KB page). Subphase 3
starts **maintaining `PageHeader.lsn`** (offset 24, 8 bytes LE). Because the page
checksum (offset 12) covers only `[HEADER_SIZE..PAGE_SIZE]`, stamping the lsn does not
require recomputing it. Existing pages keep `lsn = 0` (treated as older than any
frame), which is correct for the subphase-5/6 idempotence guard.

## Performance budget

| Operation | Target | Notes |
|-----------|--------|-------|
| `read_page` warm (pool hit) | unchanged | index checked only on miss |
| `read_page` disabled | unchanged | `None` short-circuits before any index work |
| `read_page` enabled miss + in index | +1 `RwLock` read + 1 HashMap lookup + frame `pread` | comparable to a mmap cold read |
| `write_page` enabled | today + 1 page copy + frame append (no fsync) | dual-write cost is transient (frame-only lands in subphase 4) |

No read regression is the hard requirement: `cargo nextest -p axiomdb-storage` and the
buffer-pool tests must be green and show no behavior change with the log disabled.

## Dependencies

- Depends on: subphase 2 (`FrameLog`/`WalIndex`/`FrameRef`), already committed
  (`7d9e2319`).
- Blocks: subphase 4 (commit boundary + frame-only + drop flush), which flips the
  default to enabled and removes the dual-write.

## Open questions

- [ ] Constructor surface for enabling the log: a `*_with_redo` variant vs an
      `enable_redo_log(&self)` method. (Decide in `/plan-task`; does not affect the
      durability contract.)
- [ ] Frame-log file path suffix (`<db>.wf` vs `<db>-wal`). (Cosmetic; pick in plan.)

## Done criteria

- [ ] `FrameLog`/`WalIndex`/`FrameRef` live in `axiomdb-storage`; `axiomdb-wal` no
      longer declares the module; both crates build.
- [ ] `FrameLog::append` is **lock-free** (`AtomicU64` offset reservation + `pwrite`);
      `WalIndex` is **16-way sharded**; a concurrency test (N threads appending /
      recording in parallel) yields N intact, CRC-valid frames and a correct index.
- [ ] `MmapStorage` gains the three fields (no `Mutex<FrameLog>`, no global
      `RwLock<WalIndex>`); `read_page`/`write_page` behave as specified for both
      enabled and disabled states.
- [ ] With the log **disabled**, the full existing `axiomdb-storage` suite is green
      and unchanged (no new frames written).
- [ ] New tests cover every edge case above (enabled-path round-trips, lsn-stamp
      checksum invariance, latest-frame-wins, pool-hit-skips-index).
- [ ] `./tools/vm.sh test -p axiomdb-storage` and `-p axiomdb-wal` green (the moved
      6 frame tests pass in their new home).
- [ ] `./tools/vm.sh clippy -p axiomdb-storage -p axiomdb-wal -- -D warnings` clean.
- [ ] `./tools/vm.sh fmt-check` clean.
- [ ] T0 still RED (`#[ignore]`d) — proves we did NOT accidentally implement recovery.
- [ ] rustdoc on every new public item; `docs-site/src/internals/wal.md` updated with
      the read/write-path-aware section.

## References

- Architecture: `specs/fase-redo-recovery/spec-redo-recovery.md` (Option A).
- Subphase 2 plan: `specs/fase-redo-recovery/plan-wal-frame-format.md`.
- External: `research/sqlite/src/wal.c` — `walFindFrame` (read path, the
  `iLast==0` early-out), `walFrames` (write path, `nTruncate` commit marker).
- Hot paths touched: `crates/axiomdb-storage/src/mmap.rs` (`read_page` L611,
  `write_page_inner` L442), `crates/axiomdb-storage/src/buffer_pool.rs`,
  `crates/axiomdb-storage/src/wal_frame.rs` (moved).
