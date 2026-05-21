# Plan: subphase 2 — WAL page-frame format + writer/reader + wal-index

Phase: redo-recovery (project B, Option A — SQLite-WAL write-ahead)
Spec: specs/fase-redo-recovery/spec-redo-recovery.md
Status: in progress · Branch: fase-redo-recovery · Effort: high

## Goal

A self-contained, unit-tested `wal_frame` module: a page-frame log we can append to,
read back (stopping at a torn tail), and index (page_id → latest committed frame).
**Isolated** — it does NOT touch the live write/read path yet (subphase 3 wires it in).

## Format (borrowed from SQLite `wal.c`, adapted to our LSN model)

**File header (32 bytes)** — written by `create`, checked by `open`:
`magic:u64 | version:u32 | page_size:u32 | salt:u64 | header_crc:u32 | _pad:u32`

**Per-frame: 32-byte header + PAGE_SIZE bytes:**
`page_id:u64 | lsn:u64 | commit_marker:u32 | salt:u64 | frame_crc:u32`
- `commit_marker`: 0 = non-commit frame; nonzero = the LAST frame of a committed txn
  (SQLite's `nTruncate` idea; store the committing `txn_id` so we can correlate).
- `salt`: copied from the file header — frames whose salt ≠ the run's salt are stale
  (from a previous WAL run / pre-reset) → treated as end-of-valid.
- `frame_crc`: crc32c over (frame header with crc field = 0) ++ page bytes. A torn /
  partially-written frame fails crc → marks the end of the valid prefix (append-only
  ⇒ damage is always at the tail).

## Public API (`crates/axiomdb-wal/src/wal_frame.rs`)

```rust
pub struct FrameLog { /* File, salt, write offset */ }
pub struct FrameRef { pub page_id: u64, pub lsn: u64, pub commit_marker: u32, pub offset: u64 }
pub struct WalIndex { /* page_id -> FrameRef, last_commit_offset, last_commit_lsn */ }

impl FrameLog {
    pub fn create(path: &Path) -> Result<Self, DbError>;         // fresh salt + file header
    pub fn open(path: &Path) -> Result<Self, DbError>;           // validate header, load salt
    pub fn append(&mut self, page_id: u64, lsn: u64, commit_marker: u32,
                  page: &[u8; PAGE_SIZE]) -> Result<u64, DbError>; // returns frame offset
    pub fn sync(&self) -> Result<(), DbError>;                   // fdatasync
    pub fn read_page_at(&self, offset: u64) -> Result<[u8; PAGE_SIZE], DbError>;
    pub fn scan(&self) -> Result<Vec<FrameRef>, DbError>;        // valid frames only (crc+salt), stop at torn tail
    pub fn build_index(&self) -> Result<WalIndex, DbError>;      // latest frame per page, bounded by last commit
}
impl WalIndex {
    pub fn latest(&self, page_id: u64) -> Option<&FrameRef>;     // committed only
    pub fn last_commit_lsn(&self) -> u64;
}
```

## Steps (TDD)

1. **Format + (de)serialize** — constants (FILE_HDR=32, FRAME_HDR=32, MAGIC, VERSION),
   `FrameHeader` encode/decode (LE), crc32c helper. Unit: header round-trip.
2. **`FrameLog::create/open/append/sync/read_page_at`** — append writes header+page,
   tracks offset; open validates magic/version/page_size + loads salt. Unit:
   create→append N→read_page_at returns exact bytes.
3. **`scan`** — read frames sequentially; for each, verify salt == run salt AND
   frame_crc; stop at first failure (torn/stale tail). Unit: full scan; truncated
   last frame (manually corrupt) → scan returns the valid prefix; salt mismatch → stops.
4. **`build_index`** — fold scan into `page_id → latest FrameRef`, but only include
   frames at/below the **last `commit_marker != 0`** offset (frames after the last
   commit are uncommitted → excluded). Unit: two txns where the 2nd is uncommitted →
   index reflects only the 1st; latest-wins per page across commits.
5. **Verify on Lima** (`./tools/vm.sh test -p axiomdb-wal wal_frame`), clippy, fmt.
6. **Doc** `internals/wal.md` (the frame format) + commit on the branch.

## Notes / non-goals
- NOT wired into `write_page`/`read_page` (subphase 3).
- Per-frame crc (not SQLite's cumulative chain) — sufficient for append-only torn-tail
  detection; salt handles run identity; LSN handles ordering.
- Single-writer assumption for now (matches the WAL writer); concurrency hardening
  rides the subphase-3 integration.
