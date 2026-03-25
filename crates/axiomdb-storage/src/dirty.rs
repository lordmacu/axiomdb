//! Page dirty tracker — in-memory set of page IDs written since the last flush.
//!
//! [`PageDirtyTracker`] is embedded in [`MmapStorage`] and updated on every
//! `write_page` and `alloc_page` call. After `flush()`, the set is cleared.
//!
//! ## Purpose
//!
//! Knowing which pages are dirty enables:
//! - Monitoring: `dirty_page_count()` reports pending writes.
//! - Targeted flush: `contiguous_runs()` coalesces dirty page IDs into byte
//!   ranges for `flush_range` calls instead of a full-file `msync`.
//!
//! [`MmapStorage`]: crate::mmap::MmapStorage

use std::collections::HashSet;

// ── Free functions ─────────────────────────────────────────────────────────────

/// Coalesces a pre-sorted list of page IDs into contiguous runs.
///
/// Input must be sorted ascending; duplicates are tolerated (ignored).
/// Returns `(start_page_id, run_length_in_pages)` pairs.
///
/// This is a pure function and does not touch any [`PageDirtyTracker`] state.
pub fn coalesce_page_ids(sorted_ids: &[u64]) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &id in sorted_ids {
        match runs.last_mut() {
            Some((start, len)) if *start + *len == id => *len += 1,
            Some((start, len)) if *start + *len > id => { /* duplicate — skip */ }
            _ => runs.push((id, 1)),
        }
    }
    runs
}

/// In-memory set of page IDs that have been written since the last flush.
#[derive(Debug, Default)]
pub struct PageDirtyTracker {
    dirty: HashSet<u64>,
}

impl PageDirtyTracker {
    /// Creates an empty tracker.
    pub fn new() -> Self {
        Self {
            dirty: HashSet::new(),
        }
    }

    /// Marks `page_id` as dirty (written, pending flush).
    #[inline]
    pub fn mark(&mut self, page_id: u64) {
        self.dirty.insert(page_id);
    }

    /// Returns `true` if `page_id` has been written since the last flush.
    #[inline]
    pub fn contains(&self, page_id: u64) -> bool {
        self.dirty.contains(&page_id)
    }

    /// Number of dirty pages currently tracked.
    #[inline]
    pub fn count(&self) -> usize {
        self.dirty.len()
    }

    /// Returns `true` if no pages are dirty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }

    /// Clears all dirty marks. Called by `MmapStorage::flush()` after sync.
    pub fn clear(&mut self) {
        self.dirty.clear();
    }

    /// Returns dirty page IDs sorted ascending.
    ///
    /// Useful for deterministic output (logs, tests, and targeted `flush_range`).
    pub fn sorted_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Returns the dirty pages coalesced into contiguous runs `(start_page, len)`.
    ///
    /// Used by [`MmapStorage::flush`] to issue targeted `flush_range` calls
    /// instead of flushing the entire file.
    ///
    /// [`MmapStorage::flush`]: crate::mmap::MmapStorage
    pub fn contiguous_runs(&self) -> Vec<(u64, u64)> {
        coalesce_page_ids(&self.sorted_ids())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let t = PageDirtyTracker::new();
        assert!(t.is_empty());
        assert_eq!(t.count(), 0);
    }

    #[test]
    fn test_mark_and_contains() {
        let mut t = PageDirtyTracker::new();
        t.mark(5);
        assert!(t.contains(5));
        assert!(!t.contains(6));
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn test_mark_idempotent() {
        let mut t = PageDirtyTracker::new();
        t.mark(3);
        t.mark(3);
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn test_clear_resets() {
        let mut t = PageDirtyTracker::new();
        t.mark(1);
        t.mark(2);
        t.mark(3);
        t.clear();
        assert!(t.is_empty());
        assert!(!t.contains(1));
    }

    #[test]
    fn test_sorted_ids_ascending() {
        let mut t = PageDirtyTracker::new();
        for id in [10, 2, 7, 1, 99] {
            t.mark(id);
        }
        assert_eq!(t.sorted_ids(), vec![1, 2, 7, 10, 99]);
    }

    #[test]
    fn test_sorted_ids_empty() {
        let t = PageDirtyTracker::new();
        assert!(t.sorted_ids().is_empty());
    }

    // ── coalesce_page_ids unit tests ───────────────────────────────────────────

    #[test]
    fn test_coalesce_empty() {
        assert!(coalesce_page_ids(&[]).is_empty());
    }

    #[test]
    fn test_coalesce_single_page() {
        assert_eq!(coalesce_page_ids(&[5]), vec![(5, 1)]);
    }

    #[test]
    fn test_coalesce_contiguous_pages() {
        // Pages 3, 4, 5 → one run of length 3
        assert_eq!(coalesce_page_ids(&[3, 4, 5]), vec![(3, 3)]);
    }

    #[test]
    fn test_coalesce_split_runs() {
        // Pages 1, 2 then gap then 5, 6, 7 → two runs
        assert_eq!(coalesce_page_ids(&[1, 2, 5, 6, 7]), vec![(1, 2), (5, 3)]);
    }

    #[test]
    fn test_coalesce_non_adjacent_singles() {
        assert_eq!(coalesce_page_ids(&[0, 2, 4]), vec![(0, 1), (2, 1), (4, 1)]);
    }

    #[test]
    fn test_coalesce_duplicate_ids_ignored() {
        // Input: [3, 3, 4] — duplicate 3 must not inflate the run
        assert_eq!(coalesce_page_ids(&[3, 3, 4]), vec![(3, 2)]);
    }

    // ── contiguous_runs method tests ───────────────────────────────────────────

    #[test]
    fn test_contiguous_runs_empty_tracker() {
        let t = PageDirtyTracker::new();
        assert!(t.contiguous_runs().is_empty());
    }

    #[test]
    fn test_contiguous_runs_single_page() {
        let mut t = PageDirtyTracker::new();
        t.mark(7);
        assert_eq!(t.contiguous_runs(), vec![(7, 1)]);
    }

    #[test]
    fn test_contiguous_runs_contiguous() {
        let mut t = PageDirtyTracker::new();
        for id in [10, 11, 12] {
            t.mark(id);
        }
        assert_eq!(t.contiguous_runs(), vec![(10, 3)]);
    }

    #[test]
    fn test_contiguous_runs_two_separated_runs() {
        let mut t = PageDirtyTracker::new();
        for id in [2, 3, 7, 8, 9] {
            t.mark(id);
        }
        assert_eq!(t.contiguous_runs(), vec![(2, 2), (7, 3)]);
    }
}
