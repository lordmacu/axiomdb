# Plan: subphase 3 — write-ahead integration

Phase: redo-recovery (project B) — subphase 3
Task: wire `MmapStorage` read/write to the page-frame log, additively
Spec: specs/fase-redo-recovery/spec-subfase-3-write-ahead.md
Status: done

## Summary

Six ordered steps, each leaving the tree green. Step A is the mechanical crate
move. Step A2 makes the frame log **scalable for multi-writer** (lock-free append +
sharded index) — done now because it is cheap on new code and expensive to retrofit.
Step B adds the dormant scaffolding (fields + enable hook) with the log defaulting
OFF, so all existing tests are unchanged. Step C makes `write_page` dual-write +
append a frame when enabled. Step D makes `read_page` frame-aware (consume what C
produces). Step E verifies against the spec and updates docs. Write path before read
path so the read tests can populate the index with real writes. All builds/tests run
on the Lima VM.

## Dependencies

Must be done first:
- [x] spec-subfase-3-write-ahead approved
- [x] subphase 2 committed (`7d9e2319`) — `FrameLog`/`WalIndex`/`FrameRef` exist

Blocks (until done):
- [ ] subphase 4 (commit boundary, frame-only write, drop per-commit flush)

## Affected files

Moved:
- `crates/axiomdb-wal/src/wal_frame.rs` → `crates/axiomdb-storage/src/wal_frame.rs`

Modified:
- `crates/axiomdb-storage/src/lib.rs` — add `pub mod wal_frame;` + re-export
- `crates/axiomdb-wal/src/lib.rs` — remove the `wal_frame` module + re-export (lines 19, 34)
- `crates/axiomdb-storage/src/wal_frame.rs` — `use axiomdb_storage::PAGE_SIZE` → `use crate::page::PAGE_SIZE`; lock-free `append(&self)` (`AtomicU64` offset); 16-way sharded `WalIndex` + `record(&self)`
- `crates/axiomdb-storage/src/mmap.rs` — new fields, enable hook, `read_page`/`write_page_inner`
- `docs-site/src/internals/wal.md` — read/write-path-aware section (Step E)

---

## Step A — Move the frame log into axiomdb-storage

**Goal:** `FrameLog`/`WalIndex`/`FrameRef` live in `axiomdb-storage`; both crates build; the 6 frame tests pass in their new home.
**Files:** the move + both `lib.rs` + the `use` fix.
**Approach:** mechanical; the existing 6 tests in `wal_frame.rs` are the test coverage (no new test needed — they must stay green after the move).

### Changes
- `git mv crates/axiomdb-wal/src/wal_frame.rs crates/axiomdb-storage/src/wal_frame.rs`.
- In the moved file: `use axiomdb_storage::PAGE_SIZE;` → `use crate::page::PAGE_SIZE;`
  (the `axiomdb_core::error` import is unchanged; storage already depends on core +
  crc32c + tempfile dev-dep).
- `axiomdb-storage/src/lib.rs`: add `pub mod wal_frame;` (alpha order, after `page_ref`)
  and `pub use wal_frame::{FrameLog, FrameRef, WalIndex};`.
- `axiomdb-wal/src/lib.rs`: delete line 19 (`pub mod wal_frame;`) and line 34
  (`pub use wal_frame::{FrameLog, FrameRef, WalIndex};`).

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage wal_frame      # the moved 6 tests
./tools/vm.sh test -p axiomdb-wal                    # wal still builds without the module
./tools/vm.sh clippy -p axiomdb-storage -p axiomdb-wal -- -D warnings
```

### Commit
```
refactor(redo-recovery): move FrameLog/WalIndex to axiomdb-storage (subphase 3 step A)

- breaks the crate cycle: storage owns the page-frame log directly
- no behavior change; the 6 frame tests pass in their new home
```

---

## Step A2 — FrameLog lock-free append + WalIndex sharded (multi-writer)

**Goal:** make the frame log scale under concurrent writers, matching the existing
`ConcurrentWalWriter` (atomic LSN + group commit) and `BufferPool` (16-shard) models.
No `Mutex<FrameLog>`, no global `RwLock<WalIndex>`.
**Files:** `crates/axiomdb-storage/src/wal_frame.rs`.

### Tests to add
```rust
#[test]
fn append_is_lock_free_under_concurrency() {
    // Arc<FrameLog>; N=8 threads each append M frames via &self.
    // After join: scan() returns N*M frames, all CRC-valid, offsets unique.
}
#[test]
fn index_record_latest_are_sharded_and_correct() {
    // concurrent record() of different + same page_ids; latest() returns the
    // highest-lsn frame per page.
}
```

### Implementation outline
```rust
pub struct FrameLog {
    file: File,
    salt: u64,
    write_offset: AtomicU64,      // was u64; reserve via fetch_add
}
impl FrameLog {
    pub fn append(&self, page_id, lsn, commit_marker, page) -> Result<u64, DbError> {
        let offset = self.write_offset.fetch_add(FRAME_SIZE, Ordering::Relaxed);
        // build header (salt immutable), crc; two write_all_at at `offset` — no lock
    }
}
// WalIndex: Box<[Mutex<HashMap<u64, FrameRef>>]> (16 shards) + AtomicU64
// last_commit_lsn; record(&self)/latest(&self) hit one shard; build_index builds it.
```
Update the 6 moved tests: `let log` (not `let mut log`); `append` now takes `&self`.

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage wal_frame
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit
```
feat(redo-recovery): lock-free FrameLog append + sharded WalIndex (subphase 3 step A2)

- atomic offset reservation + pwrite (no global mutex); 16-way sharded index
- mirrors ConcurrentWalWriter + BufferPool; ready for multi-writer
```

---

## Step B — Dormant scaffolding in MmapStorage (log OFF by default)

**Goal:** add the three fields + `WalIndex::record` + an enable hook, with the log
defaulting to `None` so every existing test is byte-for-byte unchanged.
**Files:** `wal_frame.rs` (record), `mmap.rs` (fields, ctor wiring, enable hook).

### Tests to add
```rust
// wal_frame.rs unit test
#[test]
fn record_updates_latest_for_page() {
    let mut idx = WalIndex::default();
    idx.record(FrameRef { page_id: 5, lsn: 1, commit_marker: 0, offset: 32 });
    idx.record(FrameRef { page_id: 5, lsn: 2, commit_marker: 0, offset: 96 });
    assert_eq!(idx.latest(5).unwrap().lsn, 2); // latest wins
}

// mmap.rs test: enabling the log does not change page_count / open semantics
#[test]
fn enable_redo_log_is_additive() {
    let s = MmapStorage::create(&tmp_path()).unwrap();
    s.enable_redo_log().unwrap();          // exact surface TBD in this step
    assert!(s.redo_enabled());
    // a plain read of an unwritten page still works (empty index → mmap)
    let pid = s.alloc_page(PageType::Data).unwrap();
    let _ = s.read_page(pid).unwrap();
}
```

### Implementation outline
```rust
// wal_frame.rs
impl WalIndex {
    pub fn record(&mut self, frame: FrameRef) { self.map.insert(frame.page_id, frame); }
}

// mmap.rs — new fields
frame_log: Option<Mutex<FrameLog>>,
wal_index: RwLock<WalIndex>,        // empty until a write appends
frame_lsn: AtomicU64,               // 0 until enabled; first write uses 1
// create/open initialize: frame_log = None, wal_index = default, frame_lsn = 0.
// enable hook: open/create the `<db>.wf` frame log, rebuild frame_lsn past the
// highest lsn already present (scan().map(|f| f.lsn).max()+1), store Some(...).
fn redo_enabled(&self) -> bool { self.frame_log.is_some() }
```
Decide the enable surface here: prefer `enable_redo_log(&self)` returning
`Result<(), DbError>` (keeps `create`/`open` signatures untouched — most additive).
Frame-log path: `<db_path>.wf`.

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage          # full storage suite unchanged + 2 new
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit
```
feat(redo-recovery): dormant frame-log scaffolding in MmapStorage (subphase 3 step B)

- frame_log Option (default None), wal_index, frame_lsn fields + enable hook
- WalIndex::record for the live append path; log OFF ⇒ behavior unchanged
```

---

## Step C — write_page dual-writes + appends a frame (when enabled)

**Goal:** when the log is enabled, `write_page`/`write_page_under_page_lock` stamp the
lsn, pwrite the main file (dual-write), append a frame (marker=0), record in the live
index, and insert into the buffer pool. Disabled ⇒ exactly today.
**Files:** `mmap.rs` (`write_page_inner`), small test-only accessor.

### Tests to add
```rust
#[test]
fn write_appends_frame_and_records_index_when_enabled() {
    let s = MmapStorage::create(&tmp_path()).unwrap();
    s.enable_redo_log().unwrap();
    let pid = s.alloc_page(PageType::Data).unwrap();
    let mut p = Page::new(PageType::Data, pid);
    p.body_mut()[0] = 0x7C; p.update_checksum();
    s.write_page(pid, &p).unwrap();
    assert!(s.frame_index_contains(pid));          // test accessor
    assert_eq!(s.read_page(pid).unwrap().body()[0], 0x7C); // via pool (C) / frame (D)
}

#[test]
fn lsn_stamp_does_not_break_page_checksum() {
    // write with log enabled, then read raw from MAIN file and verify_checksum() ok,
    // and header lsn (offset 24) is non-zero.
}

#[test]
fn write_disabled_is_byte_identical() {
    // log OFF: no .wf file created, no frames; existing behavior.
}
```

### Implementation outline
```rust
fn write_page_inner(&self, page_id, page) -> Result<(), DbError> {
    bounds check (unchanged);
    if let Some(fl) = &self.frame_log {
        let lsn = self.frame_lsn.fetch_add(1, Ordering::Relaxed); // first write → 1
        let mut buf: [u8; PAGE_SIZE] = *page bytes;               // one copy
        buf[24..32].copy_from_slice(&lsn.to_le_bytes());          // stamp; checksum
                                                                  // (offset 12) untouched
        self.pwrite_bytes(page_id * PAGE_SIZE, &buf)?;            // dual-write to main
        let offset = fl.append(page_id, lsn, 0, &buf)?;           // lock-free; marker 0
        self.wal_index.record(FrameRef{page_id,lsn,commit_marker:0,offset}); // sharded
        self.buffer_pool.insert(page_id, Arc::new(Page::from(buf)));
    } else {
        self.pwrite_page(page_id, page)?;            // today's path
        self.buffer_pool.invalidate(page_id);
    }
    self.dirty.mark(page_id);
    Ok(())
}
```
Add a `#[cfg(test)]`/diagnostic accessor `frame_index_contains(page_id)` and/or
`frame_log_len()` (mirrors `buffer_pool_stats`).

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit
```
feat(redo-recovery): write_page dual-writes + appends a page frame (subphase 3 step C)

- enabled: stamp PageHeader.lsn (offset 24, no checksum recompute) + pwrite main
  + frame append (marker 0) + live index record + buffer-pool insert
- disabled: unchanged
```

---

## Step D — read_page consults the wal-index on a miss

**Goal:** on a buffer-pool miss, when the log is enabled and the page has a frame,
read it from the frame log (verify CRC, cache it); otherwise mmap. SQLite
`walFindFrame` ordering (pool hit → index → main).
**Files:** `mmap.rs` (`read_page`).

### Tests to add
```rust
#[test]
fn read_after_write_served_from_frame() {
    // enable; write page with marker bytes; INVALIDATE the pool entry to force the
    // miss path; read → must return the written bytes via the frame (not stale mmap).
}
#[test]
fn pool_hit_skips_index() {
    // warm read: hits counter increments, index not consulted (still correct bytes).
}
#[test]
fn enabled_but_unwritten_page_reads_from_mmap() {
    // alloc a page, no write_page → not in index → mmap path.
}
```

### Implementation outline
```rust
fn read_page(&self, page_id) -> Result<PageRef, DbError> {
    bounds check;                                   // unchanged
    if let Some(p) = self.buffer_pool.get(page_id) { return Ok(from_arc(p)); } // hot
    if let Some(fl) = &self.frame_log {
        if let Some(fref) = self.wal_index.latest(page_id) {   // sharded; returns Copy
            let bytes = fl.read_page_at(fref.offset)?;         // &self; pread
            let arc = Arc::new(Page::from_bytes(*bytes)?);   // verifies CRC
            let arc = self.buffer_pool.insert(page_id, arc);
            return Ok(from_arc(arc));
        }
    }
    // cold mmap path (unchanged)
}
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage
./tools/vm.sh clippy -p axiomdb-storage -- -D warnings
```

### Commit
```
feat(redo-recovery): read_page is frame-aware on miss (subphase 3 step D)

- pool hit → index (enabled) → mmap; empty index/disabled ⇒ today's behavior
```

---

## Step E — Verify against spec, docs, workspace

**Goal:** confirm every spec done-criterion; update docs; full workspace green.

### Verification against spec done-criteria
- [ ] Types live in storage; wal builds without the module
- [ ] read/write behave per spec (enabled + disabled)
- [ ] disabled suite unchanged (no `.wf` created)
- [ ] every edge case has a test (round-trips, lsn-stamp invariance, latest-wins,
      pool-hit-skips-index, unwritten→mmap, concurrent writes with log enabled)
- [ ] T0 still RED (`#[ignore]`d) — we did NOT implement recovery

```bash
./tools/vm.sh test --workspace          # closing: full suite
./tools/vm.sh clippy --workspace -- -D warnings
./tools/vm.sh fmt-check
./tools/vm.sh test -p axiomdb-wal --run-ignored all t0_committed_heap_insert_survives_power_loss  # expect RED
```

### Docs
- `docs-site/src/internals/wal.md`: add the "read/write path is frame-aware" section
  (dual-write in subphase 3; frame-only deferred to subphase 4). Add a
  `callout-design` noting the SQLite `walFindFrame` ordering borrowed.

### Final commit
```
feat(redo-recovery): complete subphase 3 write-ahead integration

Implements specs/fase-redo-recovery/spec-subfase-3-write-ahead.md
Plan: specs/fase-redo-recovery/plan-subfase-3-write-ahead.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Crate move breaks an import | low | grep showed only wal/lib.rs references it; nothing external uses the types |
| Read-path perf regression | low | `None` short-circuit + pool-first; index only on enabled miss |
| pool/frame/main divergence (dual-write bug) | medium | Step C/D tests assert read-from-frame == written bytes; lsn-stamp checksum-invariance test |
| `Page::from_bytes` double-verifies CRC (frame_crc + page checksum) | low | acceptable; both cover corruption; measure only if Step E shows regression |
| Accidentally enabling recovery | low | T0 stays RED is an explicit done-criterion |
| Concurrent lock-free appends leave a transient hole on crash | n/a here | subphase-3 dual-write keeps the main file authoritative; contiguous-prefix tracking (MySQL-8 `link_buf` / PG insert-locks) lands in subphase 4 when frames become durability |

## Rollback plan

Each step is its own commit. To abandon: `git reset --hard <commit before Step A>`
(branch `fase-redo-recovery`). No main-file format change was made, so existing dbs
are unaffected (the `.wf` file is only created when the log is enabled).

## Estimated effort

Total: ~1.5 days. A: 20min · A2: 2h · B: 1.5h · C: 2h · D: 1.5h · E: 1.5h (+ docs).
Overall effort level **max** (data-loss surface + the concurrency design), even though
individual steps are small.
