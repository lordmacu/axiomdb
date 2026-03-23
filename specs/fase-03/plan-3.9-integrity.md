# Plan: 3.9 — Post-recovery Integrity Check

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/nexusdb-storage/src/integrity.rs` | create | `IntegrityViolation`, `IntegrityReport`, `IntegrityChecker` |
| `crates/nexusdb-storage/src/lib.rs` | modify | Export integrity types |

## Step 1 — IntegrityViolation + Severity

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityViolation {
    UncommittedAliveRow { page_id: u64, slot_id: u16, txn_id_created: u64, max_committed: u64 },
    StaleDeletedRow     { page_id: u64, slot_id: u16, txn_id_deleted: u64, max_committed: u64 },
    SlotAreaOverlap     { page_id: u64, slot_a: u16, slot_b: u16 },
    FreeSpaceInconsistent { page_id: u64, free_start: u16, free_end: u16 },
    InvalidSlotOffset   { page_id: u64, slot_id: u16, offset: u16 },
}

impl IntegrityViolation {
    pub fn severity(&self) -> Severity {
        match self {
            Self::StaleDeletedRow { .. } => Severity::Warning,
            _ => Severity::Error,
        }
    }
    pub fn page_id(&self) -> u64 { /* extract from each variant */ }
}
```

## Step 2 — IntegrityReport

```rust
#[derive(Debug, Default)]
pub struct IntegrityReport {
    pub pages_checked: u64,
    pub slots_checked: u64,
    pub errors: Vec<IntegrityViolation>,
    pub warnings: Vec<IntegrityViolation>,
}

impl IntegrityReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn push(&mut self, v: IntegrityViolation) {
        match v.severity() {
            Severity::Error   => self.errors.push(v),
            Severity::Warning => self.warnings.push(v),
        }
    }

    pub fn merge(&mut self, other: IntegrityReport) {
        self.pages_checked += other.pages_checked;
        self.slots_checked += other.slots_checked;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    pub fn summary(&self) -> String {
        if self.is_clean() {
            format!(
                "OK: {} pages, {} slots checked, {} warnings",
                self.pages_checked, self.slots_checked, self.warnings.len()
            )
        } else {
            format!(
                "ERRORS: {} errors, {} warnings — {} pages, {} slots checked",
                self.errors.len(), self.warnings.len(),
                self.pages_checked, self.slots_checked
            )
        }
    }
}
```

## Step 3 — IntegrityChecker::check_page

Pure function — no I/O, no heap allocator beyond the returned Vec.

```rust
pub struct IntegrityChecker;

impl IntegrityChecker {
    pub fn check_page(page: &Page, max_committed: TxnId) -> Vec<IntegrityViolation> {
        let mut violations = Vec::new();
        let page_id = page.header().page_id;
        let h = page.header();

        // ── Structural check 1: free space pointers ───────────────────────────
        if h.free_start as usize >= h.free_end as usize {
            violations.push(IntegrityViolation::FreeSpaceInconsistent {
                page_id, free_start: h.free_start, free_end: h.free_end,
            });
            return violations; // structural corruption — other checks unreliable
        }

        let num_slots = h.item_count as usize;
        let slot_array_end = HEADER_SIZE + num_slots * 4; // 4 bytes per SlotEntry

        // ── Structural check 2: slot array fits in page ───────────────────────
        if slot_array_end > h.free_start as usize {
            violations.push(IntegrityViolation::FreeSpaceInconsistent {
                page_id, free_start: h.free_start, free_end: h.free_end,
            });
            return violations;
        }

        // ── Per-slot checks ────────────────────────────────────────────────────
        let raw = page.as_bytes();
        let mut alive_ranges: Vec<(u16, u16, u16)> = Vec::new(); // (slot_id, offset, end)

        for slot_id in 0..num_slots as u16 {
            let off = HEADER_SIZE + slot_id as usize * 4;
            let slot_offset = u16::from_le_bytes([raw[off], raw[off + 1]]);
            let slot_length = u16::from_le_bytes([raw[off + 2], raw[off + 3]]);

            // Dead slot (offset=0, length=0) — skip all per-slot checks.
            if slot_offset == 0 && slot_length == 0 {
                continue;
            }

            // ── Check 3: slot offset must be in body (>= HEADER_SIZE) ──────────
            if (slot_offset as usize) < HEADER_SIZE {
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id, slot_id, offset: slot_offset,
                });
                continue;
            }

            // ── Check 4: slot byte range must be within page ──────────────────
            let slot_end = slot_offset as usize + slot_length as usize;
            if slot_end > PAGE_SIZE {
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id, slot_id, offset: slot_offset,
                });
                continue;
            }

            alive_ranges.push((slot_id, slot_offset, slot_end as u16));

            // ── Check 5: RowHeader MVCC fields ────────────────────────────────
            // RowHeader = [txn_id_created:8][txn_id_deleted:8][row_version:4][_flags:4]
            if slot_length < 24 {
                // Tuple too small to hold a RowHeader — structural corruption.
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id, slot_id, offset: slot_offset,
                });
                continue;
            }

            let hdr_base = slot_offset as usize;
            let txn_id_created = u64::from_le_bytes([
                raw[hdr_base],   raw[hdr_base+1], raw[hdr_base+2], raw[hdr_base+3],
                raw[hdr_base+4], raw[hdr_base+5], raw[hdr_base+6], raw[hdr_base+7],
            ]);
            let txn_id_deleted = u64::from_le_bytes([
                raw[hdr_base+8],  raw[hdr_base+9],  raw[hdr_base+10], raw[hdr_base+11],
                raw[hdr_base+12], raw[hdr_base+13], raw[hdr_base+14], raw[hdr_base+15],
            ]);

            if txn_id_deleted == 0 {
                // Alive slot: txn_id_created must be <= max_committed.
                if txn_id_created > max_committed && max_committed > 0 {
                    violations.push(IntegrityViolation::UncommittedAliveRow {
                        page_id, slot_id, txn_id_created, max_committed,
                    });
                }
            } else {
                // Deleted slot (still visible via txn_id_deleted).
                // If the deleter committed, the row should be invisible — warning.
                if txn_id_deleted <= max_committed {
                    violations.push(IntegrityViolation::StaleDeletedRow {
                        page_id, slot_id, txn_id_deleted, max_committed,
                    });
                }
            }
        }

        // ── Check 6: no overlapping alive slot ranges ─────────────────────────
        // Sort by offset, check consecutive pairs.
        alive_ranges.sort_by_key(|&(_, off, _)| off);
        for w in alive_ranges.windows(2) {
            let (slot_a, _, end_a) = w[0];
            let (slot_b, off_b, _) = w[1];
            if end_a > off_b {
                violations.push(IntegrityViolation::SlotAreaOverlap {
                    page_id, slot_a, slot_b,
                });
            }
        }

        violations
    }
```

## Step 4 — IntegrityChecker::check_all_data_pages

```rust
    pub fn check_all_data_pages(
        storage: &dyn StorageEngine,
        max_committed: TxnId,
    ) -> Result<IntegrityReport, DbError> {
        let mut report = IntegrityReport::default();
        let count = storage.page_count();

        for page_id in 2..count {  // skip page 0 (meta) and page 1 (freelist)
            let page = match storage.read_page(page_id) {
                Ok(p) => p,
                Err(DbError::PageNotFound { .. }) => continue, // freed page
                Err(e) => return Err(e),
            };

            // Heuristic: skip if the first body byte is 0 or 1 (B+ Tree is_leaf byte).
            // Without catalog we can't reliably distinguish page types.
            // In Phase 3.12 this will use catalog.page_type(page_id).
            let first_body = page.body()[0];
            if first_body == 0 || first_body == 1 {
                continue; // likely B+ Tree internal or leaf node
            }

            report.pages_checked += 1;
            report.slots_checked += page.header().item_count as u64;

            for v in Self::check_page(page, max_committed) {
                report.push(v);
            }
        }

        Ok(report)
    }
```

Note: `free_list.is_allocated(page_id)` would be cleaner but FreeList isn't
exposed publicly. The `read_page` returning `PageNotFound` for freed pages handles
this in MmapStorage (since freed pages are beyond `page_count`).

## Step 5 — IntegrityChecker::post_recovery_check

```rust
    pub fn post_recovery_check(
        storage: &dyn StorageEngine,
        max_committed: TxnId,
    ) -> Result<IntegrityReport, DbError> {
        Self::check_all_data_pages(storage, max_committed)
    }
}
```

## Step 6 — Tests

**Unit tests in integrity.rs** (`#[cfg(test)]`):

```rust
// Clean page: no violations
fn test_clean_page_no_violations()

// UncommittedAliveRow: alive slot with txn_id_created > max_committed
fn test_uncommitted_alive_row_detected()

// StaleDeletedRow (warning): alive slot with txn_id_deleted <= max_committed
fn test_stale_deleted_row_warning()

// FreeSpaceInconsistent: free_start >= free_end
fn test_free_space_inconsistent()

// InvalidSlotOffset: slot.offset < HEADER_SIZE
fn test_invalid_slot_offset()

// SlotAreaOverlap: two alive slots with overlapping ranges (craft manually)
fn test_slot_area_overlap()

// is_clean: true for clean, false for errors, true for warnings-only
fn test_is_clean_errors_vs_warnings()

// summary: correct strings
fn test_summary_clean_and_error()

// check_all_data_pages: skip page 0 and 1, only check data pages
fn test_check_all_skips_meta_and_freelist()

// post_recovery_check after CrashRecovery: returns clean for recovered-clean DB
fn test_post_recovery_clean()

// post_recovery_check detects uncommitted row that survived recovery
fn test_post_recovery_detects_uncommitted_row()
```

## Anti-patterns to avoid

- **NO** mutating the page during integrity check — read-only, report only
- **NO** panicking on malformed data — return violations instead
- **NO** scanning B+ Tree pages (heuristic skip for is_leaf = 0 or 1)
- **NO** calling `read_tuple` from heap.rs — use raw byte reads for check_page
  (avoids AlreadyDeleted errors on genuinely corrupted data)
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| Heuristic page type detection may misidentify B+ Tree pages as heap | False negatives (missed violations) not false positives; acceptable for Phase 3 |
| `StaleDeletedRow` false positives for rows deleted before checkpoint | These are legitimate pre-VACUUM state; treated as warnings not errors |
| `free_start/free_end` in PageHeader initialized to HEADER_SIZE/PAGE_SIZE in Page::new | Fresh pages pass check trivially (no slots = nothing to violate) |

## Implementation order

```
1. integrity.rs: Severity + IntegrityViolation
2. integrity.rs: IntegrityReport (push, merge, is_clean, summary)
3. integrity.rs: IntegrityChecker::check_page (structural checks)
4. integrity.rs: IntegrityChecker::check_all_data_pages
5. integrity.rs: IntegrityChecker::post_recovery_check
6. lib.rs: export integrity types
7. Tests: 11 unit tests
8. cargo test --workspace + clippy + fmt
```
