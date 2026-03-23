//! Post-recovery integrity checker for heap pages.
//!
//! Verifies structural consistency and MVCC correctness of data pages after
//! crash recovery, before the database accepts connections.
//!
//! ## Scope (Phase 3.9)
//!
//! Without the catalog (Phase 3.12+), index-vs-heap cross-checks are not possible
//! because we don't know which B+ Trees exist or which pages belong to which table.
//! This module provides:
//!
//! - **Structural checks**: slot array and tuple area consistency per page.
//! - **MVCC post-recovery checks**: no alive row has `txn_id_created > max_committed`.
//! - **Stale-delete detection**: alive rows whose deleter already committed (warning).
//!
//! In Phase 3.12 the heuristic page-type detection will be replaced by
//! `catalog.page_type(page_id)`.

use nexusdb_core::{error::DbError, TxnId};

use crate::{
    engine::StorageEngine,
    page::{Page, HEADER_SIZE, PAGE_SIZE},
};

// ── Severity ──────────────────────────────────────────────────────────────────

/// Whether a violation blocks database startup or is informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Must be resolved before accepting connections.
    Error,
    /// Logged but does not block startup (e.g. pre-VACUUM stale deletes).
    Warning,
}

// ── IntegrityViolation ────────────────────────────────────────────────────────

/// A single integrity problem found on a heap page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityViolation {
    /// Alive slot with `txn_id_created > max_committed`.
    ///
    /// Indicates crash recovery failed to undo an uncommitted INSERT.
    UncommittedAliveRow {
        page_id: u64,
        slot_id: u16,
        txn_id_created: u64,
        max_committed: u64,
    },

    /// Alive slot with `txn_id_deleted > 0` and `txn_id_deleted <= max_committed`.
    ///
    /// The deleting transaction committed, so the row should be invisible to all
    /// future snapshots. This is the normal pre-VACUUM state and does not indicate
    /// data loss — logged as a warning.
    StaleDeletedRow {
        page_id: u64,
        slot_id: u16,
        txn_id_deleted: u64,
        max_committed: u64,
    },

    /// Two alive slots have overlapping byte ranges in the page body.
    ///
    /// Indicates structural corruption of the slot array or tuple packing.
    SlotAreaOverlap {
        page_id: u64,
        slot_a: u16,
        slot_b: u16,
    },

    /// `free_start >= free_end` or the slot array extends past `free_start`.
    ///
    /// Indicates the page header's free-space bookkeeping is corrupt.
    FreeSpaceInconsistent {
        page_id: u64,
        free_start: u16,
        free_end: u16,
    },

    /// A slot's offset points into the page header region (`< HEADER_SIZE`)
    /// or past the end of the page, which is structurally impossible.
    InvalidSlotOffset {
        page_id: u64,
        slot_id: u16,
        offset: u16,
    },
}

impl IntegrityViolation {
    /// Severity of this violation.
    pub fn severity(&self) -> Severity {
        match self {
            Self::StaleDeletedRow { .. } => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// Page where the violation was found.
    pub fn page_id(&self) -> u64 {
        match self {
            Self::UncommittedAliveRow { page_id, .. }
            | Self::StaleDeletedRow { page_id, .. }
            | Self::SlotAreaOverlap { page_id, .. }
            | Self::FreeSpaceInconsistent { page_id, .. }
            | Self::InvalidSlotOffset { page_id, .. } => *page_id,
        }
    }
}

// ── IntegrityReport ───────────────────────────────────────────────────────────

/// Aggregated result of an integrity check run.
#[derive(Debug, Default)]
pub struct IntegrityReport {
    /// Number of data pages inspected.
    pub pages_checked: u64,
    /// Total slot entries inspected (live + dead).
    pub slots_checked: u64,
    /// Violations with [`Severity::Error`] — block database startup.
    pub errors: Vec<IntegrityViolation>,
    /// Violations with [`Severity::Warning`] — logged but do not block startup.
    pub warnings: Vec<IntegrityViolation>,
}

impl IntegrityReport {
    /// Returns `true` if no errors were found (warnings are acceptable).
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Adds a violation to the appropriate list.
    pub fn push(&mut self, v: IntegrityViolation) {
        match v.severity() {
            Severity::Error => self.errors.push(v),
            Severity::Warning => self.warnings.push(v),
        }
    }

    /// Merges `other` into `self`.
    pub fn merge(&mut self, other: IntegrityReport) {
        self.pages_checked += other.pages_checked;
        self.slots_checked += other.slots_checked;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    /// One-line human-readable summary.
    pub fn summary(&self) -> String {
        if self.is_clean() {
            format!(
                "OK: {} page(s), {} slot(s) checked, {} warning(s)",
                self.pages_checked,
                self.slots_checked,
                self.warnings.len()
            )
        } else {
            format!(
                "ERRORS: {} error(s), {} warning(s) — {} page(s), {} slot(s) checked",
                self.errors.len(),
                self.warnings.len(),
                self.pages_checked,
                self.slots_checked,
            )
        }
    }
}

// ── IntegrityChecker ──────────────────────────────────────────────────────────

/// Stateless integrity checker for heap pages.
pub struct IntegrityChecker;

impl IntegrityChecker {
    /// Checks a single page for structural and MVCC violations.
    ///
    /// Pure function — no I/O, no mutation.
    ///
    /// `max_committed` is the highest committed `TxnId` at the time of the check.
    /// Pass `0` to skip MVCC checks (e.g. on a fresh database).
    pub fn check_page(page: &Page, max_committed: TxnId) -> Vec<IntegrityViolation> {
        let mut violations = Vec::new();
        let page_id = page.header().page_id;
        let h = page.header();
        let raw = page.as_bytes();

        // ── Structural check 1: free-space pointers ───────────────────────────
        let free_start = h.free_start as usize;
        let free_end = h.free_end as usize;

        if free_start >= free_end {
            violations.push(IntegrityViolation::FreeSpaceInconsistent {
                page_id,
                free_start: h.free_start,
                free_end: h.free_end,
            });
            // Cannot trust the slot array — stop here.
            return violations;
        }

        let num_slots = h.item_count as usize;
        let slot_array_end = HEADER_SIZE + num_slots * 4; // 4 bytes per SlotEntry

        // ── Structural check 2: slot array fits before free_start ────────────
        if slot_array_end > free_start {
            violations.push(IntegrityViolation::FreeSpaceInconsistent {
                page_id,
                free_start: h.free_start,
                free_end: h.free_end,
            });
            return violations;
        }

        // ── Per-slot checks ────────────────────────────────────────────────────
        // (slot_id, offset, exclusive_end) for alive slots — used for overlap check.
        let mut alive_ranges: Vec<(u16, usize, usize)> = Vec::new();

        for slot_idx in 0..num_slots {
            let slot_id = slot_idx as u16;
            let slot_entry_base = HEADER_SIZE + slot_idx * 4;

            let slot_offset =
                u16::from_le_bytes([raw[slot_entry_base], raw[slot_entry_base + 1]]) as usize;
            let slot_length =
                u16::from_le_bytes([raw[slot_entry_base + 2], raw[slot_entry_base + 3]]) as usize;

            // Dead slot (offset=0, length=0) — skip all per-slot MVCC checks.
            if slot_offset == 0 && slot_length == 0 {
                continue;
            }

            // ── Check 3: offset must be in the body (>= HEADER_SIZE) ──────────
            if slot_offset < HEADER_SIZE {
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id,
                    slot_id,
                    offset: slot_offset as u16,
                });
                continue;
            }

            // ── Check 4: slot byte range must fit within the page ─────────────
            let slot_end = slot_offset + slot_length;
            if slot_end > PAGE_SIZE {
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id,
                    slot_id,
                    offset: slot_offset as u16,
                });
                continue;
            }

            alive_ranges.push((slot_id, slot_offset, slot_end));

            // ── Check 5: tuple must be large enough to hold a RowHeader ────────
            // RowHeader = [txn_id_created:8][txn_id_deleted:8][row_version:4][_flags:4] = 24B
            const ROW_HEADER_SIZE: usize = 24;
            if slot_length < ROW_HEADER_SIZE {
                violations.push(IntegrityViolation::InvalidSlotOffset {
                    page_id,
                    slot_id,
                    offset: slot_offset as u16,
                });
                continue;
            }

            // ── Check 6: MVCC fields ───────────────────────────────────────────
            let b = slot_offset;
            let txn_id_created = u64::from_le_bytes([
                raw[b],
                raw[b + 1],
                raw[b + 2],
                raw[b + 3],
                raw[b + 4],
                raw[b + 5],
                raw[b + 6],
                raw[b + 7],
            ]);
            let txn_id_deleted = u64::from_le_bytes([
                raw[b + 8],
                raw[b + 9],
                raw[b + 10],
                raw[b + 11],
                raw[b + 12],
                raw[b + 13],
                raw[b + 14],
                raw[b + 15],
            ]);

            if txn_id_deleted == 0 {
                // Alive row: creator must have committed.
                // Skip when max_committed == 0 (fresh DB, no committed txns yet).
                if max_committed > 0 && txn_id_created > max_committed {
                    violations.push(IntegrityViolation::UncommittedAliveRow {
                        page_id,
                        slot_id,
                        txn_id_created,
                        max_committed,
                    });
                }
            } else {
                // Row marked for deletion: if the deleter committed, it's stale.
                if txn_id_deleted <= max_committed {
                    violations.push(IntegrityViolation::StaleDeletedRow {
                        page_id,
                        slot_id,
                        txn_id_deleted,
                        max_committed,
                    });
                }
            }
        }

        // ── Check 7: no overlapping alive slot ranges ─────────────────────────
        alive_ranges.sort_by_key(|&(_, off, _)| off);
        for w in alive_ranges.windows(2) {
            let (slot_a, _, end_a) = w[0];
            let (slot_b, off_b, _) = w[1];
            if end_a > off_b {
                violations.push(IntegrityViolation::SlotAreaOverlap {
                    page_id,
                    slot_a,
                    slot_b,
                });
            }
        }

        violations
    }

    /// Checks all data pages in storage for structural and MVCC violations.
    ///
    /// Skips page 0 (meta) and page 1 (freelist). Uses a heuristic to skip
    /// B+ Tree pages: if `body()[0]` is 0 (internal node) or 1 (leaf node),
    /// the page is treated as a B+ Tree node. This will be replaced by
    /// catalog-driven page type lookup in Phase 3.12.
    pub fn check_all_data_pages(
        storage: &dyn StorageEngine,
        max_committed: TxnId,
    ) -> Result<IntegrityReport, DbError> {
        let mut report = IntegrityReport::default();
        let count = storage.page_count();

        for page_id in 2..count {
            let page = match storage.read_page(page_id) {
                Ok(p) => p,
                Err(DbError::PageNotFound { .. }) => continue,
                Err(e) => return Err(e),
            };

            // Heuristic: B+ Tree nodes have body()[0] == 0 (internal) or 1 (leaf).
            // Data (heap) pages have their first body byte as part of the slot array
            // (slot count bytes), which is typically not 0 or 1 for a non-empty page,
            // or is 0 for an empty data page (no slots).
            // This is best-effort without catalog.
            let first_body = page.body()[0];
            if first_body == 0 || first_body == 1 {
                // Likely a B+ Tree node — skip.
                // Also skip if free_start == HEADER_SIZE and free_end == PAGE_SIZE
                // (completely empty page, possibly unallocated).
                let h = page.header();
                if h.free_start as usize == HEADER_SIZE && h.free_end as usize == PAGE_SIZE {
                    continue;
                }
                if first_body == 0 || first_body == 1 {
                    continue;
                }
            }

            report.pages_checked += 1;
            report.slots_checked += page.header().item_count as u64;

            for v in Self::check_page(page, max_committed) {
                report.push(v);
            }
        }

        Ok(report)
    }

    /// Convenience wrapper: checks all data pages using `max_committed` from
    /// crash recovery. Intended to be called immediately after
    /// [`CrashRecovery::recover`].
    pub fn post_recovery_check(
        storage: &dyn StorageEngine,
        max_committed: TxnId,
    ) -> Result<IntegrityReport, DbError> {
        Self::check_all_data_pages(storage, max_committed)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        heap::{insert_tuple, RowHeader},
        MemoryStorage, PageType,
    };

    fn fresh_heap_page(page_id: u64) -> Page {
        Page::new(PageType::Data, page_id)
    }

    fn insert_committed(page: &mut Page, data: &[u8], txn_id: u64) -> u16 {
        insert_tuple(page, data, txn_id).unwrap()
    }

    // ── clean page ────────────────────────────────────────────────────────────

    #[test]
    fn test_clean_page_no_violations() {
        let mut page = fresh_heap_page(42);
        insert_committed(&mut page, b"row1", 1);
        insert_committed(&mut page, b"row2", 2);

        let violations = IntegrityChecker::check_page(&page, 2);
        assert!(
            violations.is_empty(),
            "clean page must have no violations: {violations:?}"
        );
    }

    #[test]
    fn test_empty_page_no_violations() {
        let page = fresh_heap_page(1);
        let violations = IntegrityChecker::check_page(&page, 0);
        assert!(violations.is_empty());
    }

    // ── UncommittedAliveRow ───────────────────────────────────────────────────

    #[test]
    fn test_uncommitted_alive_row_detected() {
        let mut page = fresh_heap_page(10);
        // Insert row with txn_id=5, but max_committed=3.
        insert_committed(&mut page, b"uncommitted", 5);

        let violations = IntegrityChecker::check_page(&page, 3);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0],
            IntegrityViolation::UncommittedAliveRow {
                page_id: 10,
                slot_id: 0,
                txn_id_created: 5,
                max_committed: 3,
            }
        ));
        assert_eq!(violations[0].severity(), Severity::Error);
    }

    #[test]
    fn test_max_committed_zero_skips_mvcc() {
        // max_committed=0 means fresh DB — skip MVCC checks.
        let mut page = fresh_heap_page(10);
        insert_committed(&mut page, b"row", 99);

        let violations = IntegrityChecker::check_page(&page, 0);
        assert!(
            violations.is_empty(),
            "max_committed=0 must skip MVCC checks"
        );
    }

    // ── StaleDeletedRow ───────────────────────────────────────────────────────

    #[test]
    fn test_stale_deleted_row_is_warning() {
        let mut page = fresh_heap_page(20);
        insert_committed(&mut page, b"row", 1);

        // Mark as deleted by txn 2 (committed before max_committed=3).
        // We write txn_id_deleted directly into the page body.
        let slot_entry_off = HEADER_SIZE; // slot 0 entry
        let slot_offset = {
            let raw = page.as_bytes();
            u16::from_le_bytes([raw[slot_entry_off], raw[slot_entry_off + 1]]) as usize
        };
        // txn_id_deleted is at bytes [8..16] of the RowHeader.
        page.as_bytes_mut()[slot_offset + 8..slot_offset + 16].copy_from_slice(&2u64.to_le_bytes());
        page.update_checksum();

        let violations = IntegrityChecker::check_page(&page, 3);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0],
            IntegrityViolation::StaleDeletedRow {
                txn_id_deleted: 2,
                max_committed: 3,
                ..
            }
        ));
        assert_eq!(violations[0].severity(), Severity::Warning);
    }

    // ── FreeSpaceInconsistent ─────────────────────────────────────────────────

    #[test]
    fn test_free_space_inconsistent() {
        let mut page = fresh_heap_page(30);
        // Corrupt free_start to be >= free_end.
        page.header_mut().free_start = 16384;
        page.header_mut().free_end = 64;
        page.update_checksum();

        let violations = IntegrityChecker::check_page(&page, 0);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0],
            IntegrityViolation::FreeSpaceInconsistent { .. }
        ));
        assert_eq!(violations[0].severity(), Severity::Error);
    }

    // ── InvalidSlotOffset ─────────────────────────────────────────────────────

    #[test]
    fn test_invalid_slot_offset_in_header() {
        let mut page = fresh_heap_page(40);
        // Manually inject a slot entry with offset < HEADER_SIZE.
        page.header_mut().item_count = 1;
        page.header_mut().free_start = (HEADER_SIZE + 4) as u16;
        // Write SlotEntry at body[0..4]: offset=10 (in header region), length=24.
        page.as_bytes_mut()[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&10u16.to_le_bytes());
        page.as_bytes_mut()[HEADER_SIZE + 2..HEADER_SIZE + 4].copy_from_slice(&24u16.to_le_bytes());
        page.update_checksum();

        let violations = IntegrityChecker::check_page(&page, 0);
        let has_invalid = violations.iter().any(|v| {
            matches!(
                v,
                IntegrityViolation::InvalidSlotOffset {
                    slot_id: 0,
                    offset: 10,
                    ..
                }
            )
        });
        assert!(has_invalid, "must detect slot offset in header region");
    }

    // ── IntegrityReport ────────────────────────────────────────────────────────

    #[test]
    fn test_is_clean_errors_vs_warnings() {
        let mut report = IntegrityReport::default();
        assert!(report.is_clean()); // empty = clean

        // Add a warning — still clean.
        report.push(IntegrityViolation::StaleDeletedRow {
            page_id: 1,
            slot_id: 0,
            txn_id_deleted: 1,
            max_committed: 2,
        });
        assert!(report.is_clean(), "warnings alone must not fail is_clean");

        // Add an error — no longer clean.
        report.push(IntegrityViolation::UncommittedAliveRow {
            page_id: 1,
            slot_id: 1,
            txn_id_created: 5,
            max_committed: 3,
        });
        assert!(!report.is_clean());
    }

    #[test]
    fn test_summary_clean_and_error() {
        let mut clean = IntegrityReport::default();
        clean.pages_checked = 5;
        clean.slots_checked = 20;
        let s = clean.summary();
        assert!(
            s.starts_with("OK:"),
            "clean summary must start with OK: {s}"
        );

        let mut err = IntegrityReport::default();
        err.push(IntegrityViolation::FreeSpaceInconsistent {
            page_id: 1,
            free_start: 100,
            free_end: 50,
        });
        let s = err.summary();
        assert!(
            s.starts_with("ERRORS:"),
            "error summary must start with ERRORS: {s}"
        );
    }

    // ── check_all_data_pages ──────────────────────────────────────────────────

    #[test]
    fn test_check_all_skips_meta_and_freelist() {
        let mut storage = MemoryStorage::new();
        // Allocate a data page and insert a committed row.
        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        insert_committed(&mut page, b"data", 1);
        storage.write_page(page_id, &page).unwrap();

        // max_committed=1: row committed, no violations.
        let report = IntegrityChecker::check_all_data_pages(&storage, 1).unwrap();
        assert!(report.is_clean(), "should be clean: {:?}", report.errors);
        // Must have checked at least the data page.
        assert!(report.pages_checked >= 1);
    }

    // ── post_recovery_check integration ──────────────────────────────────────

    #[test]
    fn test_post_recovery_clean() {
        let mut storage = MemoryStorage::new();
        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        insert_committed(&mut page, b"committed row", 1);
        storage.write_page(page_id, &page).unwrap();

        let report = IntegrityChecker::post_recovery_check(&storage, 1).unwrap();
        assert!(report.is_clean(), "{}", report.summary());
    }

    #[test]
    fn test_post_recovery_detects_uncommitted_row() {
        // Simulate: recovery missed undoing an uncommitted insert.
        // Row has txn_id_created=5 but max_committed=1.
        let mut storage = MemoryStorage::new();
        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let page_bytes = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(page_bytes).unwrap();
        insert_committed(&mut page, b"orphan", 5); // txn 5 never committed
        storage.write_page(page_id, &page).unwrap();

        let report = IntegrityChecker::post_recovery_check(&storage, 1).unwrap();
        assert!(!report.is_clean(), "uncommitted row must trigger error");
        assert_eq!(report.errors.len(), 1);
        assert!(matches!(
            report.errors[0],
            IntegrityViolation::UncommittedAliveRow {
                txn_id_created: 5,
                ..
            }
        ));
    }
}
