# Plan: 3.8c — Doublewrite Buffer (Torn Page Repair)

## Files to create/modify

| File | Action | Purpose |
|------|--------|---------|
| `crates/axiomdb-storage/src/doublewrite.rs` | **Create** | `DoublewriteBuffer` struct, DW file format, write/recover/validate |
| `crates/axiomdb-storage/src/lib.rs` | Modify | Add `pub mod doublewrite;` and re-export |
| `crates/axiomdb-storage/src/mmap.rs` | Modify | Integrate DW into `flush()` and `open()` |
| `docs-site/src/internals/storage.md` | Modify | Add Doublewrite Buffer section |
| `docs/progreso.md` | Modify | Mark 3.8c completed |

---

## Constants

```rust
// doublewrite.rs

/// DW file magic: "AXMDBLWR" as u64 LE
const DW_MAGIC: [u8; 8] = *b"AXMDBLWR";

/// DW file format version
const DW_VERSION: u32 = 1;

/// Footer sentinel — written last, validates completeness
const DW_SENTINEL: u32 = 0xDEAD_BEEF;

/// Header size: magic(8) + version(4) + slot_count(4) = 16 bytes
const DW_HEADER_SIZE: usize = 16;

/// Slot size: page_id(8) + page_data(PAGE_SIZE) = 16392 bytes
const DW_SLOT_SIZE: usize = 8 + PAGE_SIZE;

/// Footer size: file_crc(4) + sentinel(4) = 8 bytes
const DW_FOOTER_SIZE: usize = 8;
```

---

## Data Structures

```rust
/// Manages the doublewrite buffer file for torn page repair.
///
/// The DW file (`.db.dw`) holds a snapshot of all pages being fsynced in
/// the current flush. If the main file fsync is interrupted by a crash,
/// the DW file provides the committed-state page images for repair.
pub struct DoublewriteBuffer {
    path: PathBuf,
}
```

The struct is intentionally lightweight — no open file handle. The `.dw` file is
opened, written, fsynced, and closed in a single `write_and_sync` call. This avoids
holding extra file descriptors during normal operation.

---

## Algorithm / Pseudocode

### 1. `DoublewriteBuffer::write_and_sync`

```rust
pub fn write_and_sync(&self, pages: &[(u64, &Page)]) -> Result<(), DbError> {
    if pages.is_empty() {
        return Ok(());
    }

    let slot_count = pages.len() as u32;
    let total_size = DW_HEADER_SIZE + pages.len() * DW_SLOT_SIZE + DW_FOOTER_SIZE;

    // Allocate single buffer for sequential write
    let mut buf = vec![0u8; total_size];

    // Header
    buf[0..8].copy_from_slice(&DW_MAGIC);
    buf[8..12].copy_from_slice(&DW_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&slot_count.to_le_bytes());

    // Slots
    let mut offset = DW_HEADER_SIZE;
    for &(page_id, page) in pages {
        buf[offset..offset + 8].copy_from_slice(&page_id.to_le_bytes());
        buf[offset + 8..offset + 8 + PAGE_SIZE].copy_from_slice(page.as_bytes());
        offset += DW_SLOT_SIZE;
    }

    // Footer: CRC32c over header + all slots (everything before footer)
    let file_crc = crc32c::crc32c(&buf[..offset]);
    buf[offset..offset + 4].copy_from_slice(&file_crc.to_le_bytes());
    buf[offset + 4..offset + 8].copy_from_slice(&DW_SENTINEL.to_le_bytes());

    // Write entire buffer + fsync
    let file = File::create(&self.path)?;       // truncate if exists
    file.write_all(&buf)?;                       // single write call
    file.sync_all()?;                            // durable
    Ok(())
}
```

**Key design choice:** Single `write_all` call with a pre-built buffer. This
maximizes sequential I/O throughput and minimizes syscalls. The buffer is
transient (freed after `write_and_sync` returns).

### 2. `DoublewriteBuffer::recover`

```rust
pub fn recover(&self, file: &File) -> Result<usize, DbError> {
    if !self.exists() {
        return Ok(0);
    }

    let data = fs::read(&self.path)?;

    if !Self::validate(&data) {
        warn!(path = %self.path.display(), "invalid DW file, ignoring");
        return Ok(0);
    }

    let slot_count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let mut repaired = 0usize;

    for i in 0..slot_count {
        let slot_offset = DW_HEADER_SIZE + i * DW_SLOT_SIZE;
        let page_id = u64::from_le_bytes(
            data[slot_offset..slot_offset + 8].try_into().unwrap()
        );
        let page_data = &data[slot_offset + 8..slot_offset + 8 + PAGE_SIZE];

        // Read current page from main file
        let file_offset = page_id * PAGE_SIZE as u64;
        let mut current = [0u8; PAGE_SIZE];
        file.read_exact_at(&mut current, file_offset)?;

        // Check if page is torn (CRC mismatch)
        let expected_crc = u32::from_le_bytes(
            current[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].try_into().unwrap()
        );
        let actual_crc = crc32c::crc32c(&current[HEADER_SIZE..PAGE_SIZE]);

        if expected_crc != actual_crc {
            // Torn page detected — restore from DW
            info!(page_id, "repairing torn page from doublewrite buffer");
            file.write_all_at(page_data, file_offset)?;
            repaired += 1;
        }
    }

    if repaired > 0 {
        file.sync_all()?;  // make repairs durable
    }

    Ok(repaired)
}
```

### 3. `DoublewriteBuffer::validate` (private)

```rust
fn validate(data: &[u8]) -> bool {
    // Minimum: header + footer (no slots)
    if data.len() < DW_HEADER_SIZE + DW_FOOTER_SIZE {
        return false;
    }

    // Check magic
    if &data[0..8] != &DW_MAGIC {
        return false;
    }

    // Check version
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if version != DW_VERSION {
        return false;
    }

    // Check size consistency
    let slot_count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let expected_size = DW_HEADER_SIZE + slot_count * DW_SLOT_SIZE + DW_FOOTER_SIZE;
    if data.len() != expected_size {
        return false;
    }

    // Check sentinel
    let sentinel_offset = expected_size - 4;
    let sentinel = u32::from_le_bytes(
        data[sentinel_offset..sentinel_offset + 4].try_into().unwrap()
    );
    if sentinel != DW_SENTINEL {
        return false;
    }

    // Check CRC (covers header + all slots, not the footer)
    let payload_end = expected_size - DW_FOOTER_SIZE;
    let expected_crc = u32::from_le_bytes(
        data[payload_end..payload_end + 4].try_into().unwrap()
    );
    let actual_crc = crc32c::crc32c(&data[..payload_end]);

    expected_crc == actual_crc
}
```

### 4. Modified `MmapStorage::flush()`

```rust
fn flush(&mut self) -> Result<(), DbError> {
    // Step 1: release deferred frees (unchanged)
    self.release_deferred_frees(u64::MAX)?;

    // Step 2: collect pages for DW
    let has_changes = !self.dirty.is_empty() || self.freelist_dirty;

    if has_changes {
        let mut dw_pages: Vec<(u64, Page)> = Vec::new();

        // 2a: page 0 (meta) — always include; may have been modified by grow()
        let meta_page = self.copy_page_from_mmap(0)?;
        dw_pages.push((0, meta_page));

        // 2b: page 1 (freelist) — build from current in-memory state
        let freelist_page = self.build_freelist_page();
        dw_pages.push((1, freelist_page));

        // 2c: dirty data/index/overflow pages
        for page_id in self.dirty.sorted_ids() {
            if page_id <= 1 { continue; }  // already included
            let page = self.copy_page_from_mmap(page_id)?;
            dw_pages.push((page_id, page));
        }

        // Step 3: write DW file and fsync
        let dw_refs: Vec<(u64, &Page)> = dw_pages.iter()
            .map(|(id, p)| (*id, p))
            .collect();
        self.dw_buffer.write_and_sync(&dw_refs)?;
    }

    // Step 4: write freelist to main file (unchanged)
    if self.freelist_dirty {
        self.pwrite_freelist()?;
    }

    // Step 5: fsync main file (unchanged)
    self.file.sync_all()
        .map_err(|e| classify_io(e, "storage fsync"))?;

    // Step 6: cleanup DW file (non-fatal on failure)
    if has_changes {
        if let Err(e) = self.dw_buffer.cleanup() {
            warn!(error = %e, "failed to remove doublewrite file");
        }
    }

    // Step 7: clear tracking (unchanged)
    self.freelist_dirty = false;
    self.dirty.clear();
    Ok(())
}
```

### 5. Modified `MmapStorage::open()`

Insert DW recovery BEFORE the mmap:

```rust
pub fn open(path: &Path) -> Result<Self, DbError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    file.try_lock_exclusive().map_err(|_| DbError::FileLocked { ... })?;

    // ─── NEW: DW recovery ───
    let dw_buffer = DoublewriteBuffer::for_db(path);
    if dw_buffer.exists() {
        match dw_buffer.recover(&file) {
            Ok(n) if n > 0 => {
                info!(repaired = n, "doublewrite recovery completed");
            }
            Ok(_) => {
                debug!("doublewrite file found but no repairs needed");
            }
            Err(e) => {
                warn!(error = %e, "doublewrite recovery failed, continuing");
            }
        }
        let _ = dw_buffer.cleanup();
    }
    // ─── END NEW ───

    let mmap = unsafe { Mmap::map(&file)? };
    // ... rest of validation unchanged

    // Store dw_buffer in struct
    Ok(MmapStorage {
        mmap,
        file,
        freelist,
        freelist_dirty: false,
        dirty: PageDirtyTracker::new(),
        deferred_frees: Vec::new(),
        current_snapshot_id: 0,
        dw_buffer,  // NEW field
    })
}
```

### 6. Modified `MmapStorage::create()`

Also store the DW buffer (no recovery needed for new databases):

```rust
pub fn create(path: &Path) -> Result<Self, DbError> {
    // ... existing create logic ...

    let dw_buffer = DoublewriteBuffer::for_db(path);

    Ok(MmapStorage {
        // ... existing fields ...
        dw_buffer,  // NEW field
    })
}
```

### 7. New helper: `copy_page_from_mmap`

```rust
/// Copies raw page bytes from the mmap without CRC verification.
/// Used by flush() to collect DW pages. The CRC was already verified
/// when the page was first read via read_page() or write_page().
fn copy_page_from_mmap(&self, page_id: u64) -> Result<Page, DbError> {
    let offset = page_id as usize * PAGE_SIZE;
    if offset + PAGE_SIZE > self.mmap.len() {
        return Err(DbError::PageNotFound { page_id });
    }
    let mut bytes = [0u8; PAGE_SIZE];
    bytes.copy_from_slice(&self.mmap[offset..offset + PAGE_SIZE]);
    Ok(Page::from_raw_bytes(bytes))
}
```

### 8. New helper: `build_freelist_page`

```rust
/// Builds the freelist bitmap page (page 1) from the current in-memory
/// FreeList without writing it to disk. Used by flush() to include
/// the freelist in the DW buffer before the main pwrite_freelist().
fn build_freelist_page(&self) -> Page {
    let mut page = Page::new(PageType::Free, 1);
    self.freelist.to_bytes(page.body_mut());
    page.update_checksum();
    page
}
```

---

## Implementation Phases

### Phase 1: `DoublewriteBuffer` struct and DW file I/O

1. Create `crates/axiomdb-storage/src/doublewrite.rs`
2. Define constants: `DW_MAGIC`, `DW_VERSION`, `DW_SENTINEL`, sizes
3. Implement `DoublewriteBuffer` struct: `for_db()`, `exists()`, `cleanup()`
4. Implement `write_and_sync()` — serialize pages to DW format, write, fsync
5. Implement `validate()` — check magic, version, size, CRC, sentinel
6. Implement `recover()` — read DW, validate, repair torn pages
7. Add `pub mod doublewrite;` to `lib.rs`

### Phase 2: Integrate into MmapStorage

8. Add `dw_buffer: DoublewriteBuffer` field to `MmapStorage`
9. Add helper `copy_page_from_mmap()` — raw page copy without CRC check
10. Add helper `build_freelist_page()` — build freelist page in memory
11. Modify `create()` — initialize `dw_buffer` field
12. Modify `open()` — run DW recovery before mmap
13. Modify `flush()` — collect pages, write DW, fsync DW, then proceed as before
14. Add `Page::from_raw_bytes()` if not already available — constructs Page
    from raw byte array without validation (caller knows bytes are valid)

### Phase 3: Tests

15. Unit test: `validate()` with valid DW data → true
16. Unit test: `validate()` with wrong magic → false
17. Unit test: `validate()` with wrong CRC → false
18. Unit test: `validate()` with wrong sentinel → false
19. Unit test: `validate()` with wrong size → false
20. Unit test: `validate()` with truncated data → false
21. Unit test: `write_and_sync()` → read back → `validate()` → true
22. Integration test: write pages → `write_and_sync()` → corrupt one page
    in main file → `recover()` → verify page restored
23. Integration test: `recover()` with valid DW but all main pages OK → 0 repaired
24. Integration test: `recover()` twice (idempotence) → second returns 0
25. Integration test: `recover()` with invalid DW → returns 0, no damage
26. Integration test: `flush()` with dirty pages → DW file appears then disappears
27. Integration test: `open()` with leftover DW file → recovery runs automatically
28. Integration test: empty flush → no DW file created

### Phase 4: Documentation

29. Update `docs-site/src/internals/storage.md` — add Doublewrite Buffer section
30. Update `docs/progreso.md` — mark 3.8c `[x] ✅`

---

## Tests to Write

### Unit tests (in `doublewrite.rs`)

```rust
#[cfg(test)]
mod tests {
    // validate_* tests
    test_validate_empty_dw                    // empty file → false
    test_validate_truncated_header            // < 16 bytes → false
    test_validate_wrong_magic                 // bad magic → false
    test_validate_wrong_version               // version != 1 → false
    test_validate_wrong_slot_count            // size mismatch → false
    test_validate_wrong_crc                   // CRC tampered → false
    test_validate_wrong_sentinel              // sentinel tampered → false
    test_validate_valid_zero_slots            // 0 slots + correct footer → true
    test_validate_valid_one_slot              // 1 slot + correct footer → true
    test_validate_valid_many_slots            // 10 slots + correct footer → true

    // write + recover roundtrip
    test_write_and_recover_repairs_torn_page
    test_write_and_recover_skips_valid_pages
    test_recover_idempotent
    test_recover_invalid_dw_returns_zero
    test_cleanup_removes_file
    test_cleanup_noop_if_missing
    test_write_empty_is_noop
}
```

### Integration tests (in `mmap.rs` tests or `tests/`)

```rust
test_flush_creates_and_removes_dw_file
test_open_with_dw_file_runs_recovery
test_full_crash_simulation              // write → leave DW → corrupt → open → verify
```

---

## Anti-patterns to Avoid

- **DO NOT** keep the `.dw` file open between calls. Open → write → fsync → close
  in `write_and_sync`. Keeping it open wastes an fd and complicates error handling.

- **DO NOT** buffer page copies in `MmapStorage` between `write_page` and `flush`.
  Reading from the mmap at flush time is correct (MAP_SHARED coherence) and avoids
  per-write memory allocation.

- **DO NOT** use `mmap` for the DW file. It's a write-once sequential file. Plain
  `File::create` + `write_all` + `sync_all` is simpler and correct.

- **DO NOT** check for `Page::from_raw_bytes` validity. The mmap contains valid
  pages at flush time (all pwrite calls completed). CRC verification in the DW
  collection path would be wasted work.

- **DO NOT** make `cleanup()` failure a hard error. The DW file being left on disk
  after a successful flush is harmless — the next `open()` will see all pages
  valid and delete it. Log a warning and continue.

- **DO NOT** add the DW buffer to `MemoryStorage`. It's only meaningful for disk-
  backed storage where crash-atomicity matters. MemoryStorage (tests, in-memory
  mode) does not need it.

---

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Large transactions produce large DW files | Memory spike during flush (N × 16KB buffer) | Acceptable for alpha. Future: chunked DW writes. |
| Extra fsync per flush adds latency | ~1-2ms per flush on SSD | Same cost as InnoDB/PostgreSQL. Measured at review. |
| DW file on same disk as main file | Both files compete for bandwidth | Future: configurable DW path on separate disk. |
| `copy_page_from_mmap` reads stale data after concurrent remap | Corrupt DW | Impossible: flush requires `&mut self` via RwLock. |
| `fs::read` of DW file fails (permissions, disk error) | Recovery not possible | Log warning, continue with normal open (same as without 3.8c). |
| CRC collision: torn page has accidental valid CRC | Undetected corruption | Probability: ~1/2³² per torn page. Acceptable. Same as PostgreSQL/InnoDB. |
