# Spec: 3.8c — Doublewrite Buffer (Torn Page Repair)

## Context

AxiomDB uses `pwrite()` to write 16 KB pages to disk. On all modern filesystems
(APFS, ext4, XFS, ZFS) that use 4 KB internal blocks, a 16 KB `pwrite()` is NOT
crash-atomic. A power failure or kernel panic mid-write leaves a **torn page**:
the first N×4 KB contain new data, the remainder contains the previous state.
CRC32c detects this corruption on startup but there is currently no repair path —
the database refuses to open.

This spec defines the **doublewrite (DW) buffer**: a separate `.dw` file that holds
a durable copy of every page about to be fsynced, providing a repair source if any
page is found torn on the next startup.

**Reference:** MySQL 8.0.20+ moved InnoDB's doublewrite buffer from an embedded
system tablespace zone to a standalone `#ib_*.dblwr` file for better sequential I/O
and zero impact on the main tablespace format. AxiomDB adopts the same design.

---

## What to build (not how)

A `DoublewriteBuffer` type that wraps a `.dw` file alongside the main `.db` file.
Before every `flush()` commits dirty pages to durable storage, the DW buffer:

1. Captures a snapshot of all pages about to be written (dirty pages + structural
   pages 0 and 1).
2. Writes them to the `.dw` file and fsyncs it.
3. Lets `flush()` proceed to fsync the main file.
4. Removes the `.dw` file after the main fsync completes.

On startup (`MmapStorage::open`), if a `.dw` file exists, AxiomDB repairs any
torn pages from it before proceeding with the normal validation scan.

---

## Inputs / Outputs

### `DoublewriteBuffer::write_and_sync`
- **Input:** slice of `(page_id: u64, &Page)` pairs — pages to protect
- **Output:** `Ok(())` when the `.dw` file is fully written and fsynced
- **Errors:** `DbError::Io` on any write/sync failure

### `DoublewriteBuffer::recover`
- **Input:** `&File` — the open main database file (for repair writes)
- **Output:** `Ok(repaired_count: usize)` — number of pages restored from DW
- **Errors:** `DbError::Io` on write/sync failure during repair

### `DoublewriteBuffer::cleanup`
- **Input:** nothing
- **Output:** `Ok(())` — removes the `.dw` file (ignores "not found" error)
- **Errors:** `DbError::Io` on unexpected filesystem error

---

## DW File Format

```
Offset   Size       Field          Description
──────── ────────── ────────────── ───────────────────────────────────────────
0        8          magic          b"AXMDBLWR" — identifies this as a DW file
8        4          version        u32 LE = 1
12       4          slot_count     u32 LE — number of page slots that follow

[Header: 16 bytes]

16                  --- slot 0 ---
16       8          page_id        u64 LE
24       16384      page_data      [u8; PAGE_SIZE] — full page image

[Each slot: 16392 bytes. Slots 0..slot_count follow contiguously.]

16 + slot_count × 16392 + 0    4    file_crc    CRC32c(header || all slots)
16 + slot_count × 16392 + 4    4    sentinel    u32 LE = 0xDEAD_BEEF

[Footer: 8 bytes]

Total size = 16 + slot_count × 16392 + 8
```

**Magic constant:** `DW_MAGIC = b"AXMDBLWR"` (u64 LE)
**Sentinel constant:** `DW_SENTINEL: u32 = 0xDEAD_BEEF`
**Version constant:** `DW_VERSION: u32 = 1`

The DW file is valid for recovery **only if all three conditions hold:**
1. `footer.sentinel == DW_SENTINEL`
2. `CRC32c(header || all slots) == footer.file_crc`
3. `file.len() == 16 + slot_count × 16392 + 8`

If any condition fails, the DW file is corrupt or incomplete (crash before DW fsync)
and MUST be ignored (deleted) — the main file is at least as good as before the
attempted flush.

---

## Modified `flush()` Protocol

```
flush():
  // Step 1: update in-memory state (unchanged)
  release_deferred_frees(u64::MAX)

  // Step 2: collect all pages that will be fsynced
  let mut dw_pages: Vec<(u64, Page)> = Vec::new()

  // 2a: structural pages 0 and 1 — always included
  //     page 0 may have been modified by grow() without dirty tracking
  //     page 1 will be overwritten if freelist_dirty; always protected either way
  dw_pages.push((0, read_page_from_mmap(0)))   // meta page
  dw_pages.push((1, build_freelist_page()))    // freelist page (current in-memory state)

  // 2b: dirty data/index/overflow pages (already pwritten to main file)
  for page_id in dirty.iter() {
      dw_pages.push((page_id, read_page_from_mmap(page_id)))
  }

  // Step 3: write DW file and fsync (only if there are changes)
  if !dw_pages.is_empty() {
      dw_buffer.write_and_sync(&dw_pages)?
  }

  // Step 4: write freelist to main file (same as today)
  if freelist_dirty {
      pwrite_freelist()?
  }

  // Step 5: fsync main file (same as today)
  file.sync_all()?

  // Step 6: remove DW file (cleanup; failure is non-fatal and logged)
  let _ = dw_buffer.cleanup()

  // Step 7: clear tracking (same as today)
  freelist_dirty = false
  dirty.clear()
```

**Why always include pages 0 and 1:** `grow()` modifies page 0 (via
`update_page_count_in_mmap`) and page 1 (via `pwrite_freelist`) but does not add
them to the `dirty` tracker and does not fsync. Their pwrite'd content sits in the
kernel page cache until the next `flush()`. Including them in the DW ensures that
even a torn grow()-related write is recoverable.

**Why read dirty pages from mmap:** At the time `flush()` is called, all
`write_page()` calls have already completed via `pwrite()`. Under `MAP_SHARED`,
the mmap reflects the kernel page cache and therefore contains the exact bytes
that were pwritten. Reading from the mmap at this point yields the committed state
with zero extra allocation (no write buffer needed).

---

## Recovery Protocol in `MmapStorage::open`

```
open(path):
  // Step 1: open file and acquire lock (unchanged)
  file = open + try_lock_exclusive

  // Step 2: recover from DW if present (NEW)
  let dw = DoublewriteBuffer::for_db(path)
  if dw.exists() {
      let repaired = dw.recover(&file)?  // may repair torn pages
      if repaired > 0 {
          info!("doublewrite recovery: {} page(s) repaired", repaired)
      }
      dw.cleanup()?
  }

  // Step 3: mmap, validate pages 0 and 1, load freelist, scan all pages
  //         (unchanged — recovery above ensures pages are valid before scan)
  let mmap = Mmap::map(&file)
  ... (existing validation logic)
```

### `recover()` detail

```
recover(file):
  data = fs::read(dw_path)
  if !validate_dw_file(data):
      log warning "DW file invalid/incomplete, ignoring"
      return Ok(0)

  repaired = 0
  for (page_id, page_data) in parse_slots(data):
      offset = page_id * PAGE_SIZE
      let mut current = [0u8; PAGE_SIZE]
      file.read_exact_at(&mut current, offset as u64)?
      if crc32c(current[HEADER_SIZE..]) != read_u32_le(&current[CHECKSUM_OFFSET..]):
          // Torn page detected → restore from DW
          file.write_all_at(&page_data, offset as u64)?
          repaired += 1

  if repaired > 0:
      file.sync_all()?    // make repairs durable before startup scan

  return Ok(repaired)
```

**Idempotence:** If `recover()` is interrupted by a crash, the DW file still exists
and still passes validation. The next startup reruns recovery. Pages already repaired
have valid CRC → skipped. Pages not yet repaired → restored again. Perfectly safe.

---

## DW File Path Convention

```
main file:   /path/to/database.db
DW file:     /path/to/database.db.dw
```

Derived by appending `.dw` to the main file path. This matches MySQL's convention
(`database.ibd` → `#ib_16384_0.dblwr`) — a separate file, visible in the directory.

---

## Use Cases

### 1. Normal operation (no crash)
- Dirty pages pwritten during transaction
- `flush()`: DW written + fsynced → freelist pwritten → main fsynced → DW deleted
- Startup: DW file absent → skip recovery → normal scan

### 2. Crash between DW fsync and main fsync
- DW file exists and is valid
- Some pages in main file may be torn
- Startup: DW valid → repair torn pages → fsync → delete DW → normal scan succeeds

### 3. Crash before DW fsync (during DW write)
- DW file exists but footer sentinel or CRC is wrong
- Main file is in the same state as before the attempted flush (no new fsync happened)
- Startup: DW invalid → delete DW → normal scan (same outcome as before 3.8c)

### 4. Crash during recovery (between repair writes)
- DW file still exists after partial recovery
- Next startup: reruns recovery → idempotent → completes

### 5. Empty flush (no dirty pages, no freelist changes)
- `dw_pages` is empty → `write_and_sync` is skipped entirely
- No `.dw` file created → no overhead

### 6. Large transaction with many dirty pages
- All N dirty pages included in DW
- DW file size: `16 + (N+2) × 16392 + 8` bytes (includes pages 0, 1, and N dirty pages)
- Sequential write to `.dw` → good I/O pattern

---

## Acceptance Criteria

- [ ] `DoublewriteBuffer::write_and_sync` writes the DW file with correct format
      (magic, version, slot_count, slots, footer CRC, sentinel) and fsyncs.
- [ ] `DoublewriteBuffer::recover` detects and repairs torn pages from a valid DW file.
- [ ] `DoublewriteBuffer::recover` ignores an invalid or incomplete DW file (wrong
      CRC, wrong sentinel, wrong size) and returns `Ok(0)`.
- [ ] `MmapStorage::flush()` calls `write_and_sync` before the main `sync_all()`.
- [ ] `MmapStorage::open()` calls `recover()` before the mmap and validation scan.
- [ ] Simulation test: write pages → crash-simulate (leave DW, corrupt main page) →
      open → verify page data is correct.
- [ ] Idempotence test: call `recover()` twice on the same DW file → second call
      repairs nothing (all CRCs already valid) → `Ok(0)`.
- [ ] Invalid DW test: truncated DW file, wrong sentinel, wrong CRC → `recover`
      ignores and deletes the file, open proceeds normally.
- [ ] Empty flush test: `flush()` with no dirty pages → no `.dw` file created.
- [ ] Unit tests for `validate_dw_file` and `parse_slots` covering all invalid cases.
- [ ] `cargo test -p axiomdb-storage` passes clean.
- [ ] `cargo clippy -p axiomdb-storage -- -D warnings` passes clean.
- [ ] `docs-site/src/internals/storage.md` updated with DW section.

---

## Out of Scope

- **Batch-size limit / sliding window:** All dirty pages go into one DW file per
  flush. No chunking by max slot count. Simplest correct design.
- **DW compression:** Page data is stored uncompressed. Adding compression (PGLZ,
  LZ4) would reduce DW I/O but complicates recovery. Deferred to future optimization.
- **DW encryption:** Not in scope. Will follow whatever at-rest encryption strategy
  is adopted for the main file.
- **`grow()` internal fsync:** `grow()` already modifies pages 0 and 1 and leaves
  them in the kernel cache until the next `flush()`. This spec protects them via
  the "always include pages 0 and 1" rule. Making `grow()` itself DW-safe is not
  in scope (grow happens within a write transaction, always followed by a flush).
- **WAL full-page images:** This spec does NOT add physical page images to the WAL.
  The WAL remains a logical-redo log. The DW buffer is the physical repair mechanism.
- **Windows support:** `write_all_at` uses `pwrite()` (Unix). Windows uses
  `seek_write` internally; recovery logic is identical. No platform-specific
  branching needed in this spec.

---

## Dependencies

- `crates/axiomdb-storage` — all changes are internal to this crate
- `crc32c` crate — already used for page checksums; reused for DW footer CRC
- `std::os::unix::fs::FileExt` — already used for `write_all_at` / `read_exact_at`
- No changes required in any other crate
- No WAL format changes
- No changes to the main `.db` file format or version number

---

## ⚠️ DEFERRED

- **Configurable DW path:** The `.dw` file is always co-located with `.db`. A
  separate `dw_path` config option (to place DW on a faster disk) is not in scope.
- **DW for `grow()` atomicity:** If `grow()` is interrupted mid-write (before the
  next flush), the repair depends on the OS partially applying the `set_len` +
  `update_page_count_in_mmap` + `pwrite_freelist` sequence. Full atomicity of grow
  requires a separate mechanism; deferred post-3.8c.
