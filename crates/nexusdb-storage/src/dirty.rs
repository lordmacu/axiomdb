//! Page dirty tracker — in-memory set of page IDs written since the last flush.
//!
//! [`PageDirtyTracker`] is embedded in [`MmapStorage`] and updated on every
//! `write_page` and `alloc_page` call. After `flush()`, the set is cleared.
//!
//! ## Purpose
//!
//! Knowing which pages are dirty enables:
//! - Monitoring: `dirty_page_count()` reports pending writes.
//! - Future optimization: per-page `msync` (flush_range) instead of full mmap
//!   flush — currently deferred pending profiling data.
//!
//! [`MmapStorage`]: crate::mmap::MmapStorage

use std::collections::HashSet;

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
    /// Useful for deterministic output (logs, tests, future per-page msync).
    pub fn sorted_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        ids
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
}
