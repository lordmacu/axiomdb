//! Per-chain insert hint for concurrent heap operations.
//!
//! Shared across all transactions inserting into the same table. Caches the
//! last page known to have free space, avoiding O(chain_length) walks to find
//! the tail page on every insert.
//!
//! Inspired by PostgreSQL's `BulkInsertState` (hio.c) which caches a target
//! block per session, and InnoDB's `FSP_HEADER` last-used-page hint.
//!
//! For AxiomDB (typically <10K pages per table), a single AtomicU64 hint is
//! sufficient. PostgreSQL's full Free Space Map (B-tree per relation) would be
//! overkill at this scale.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Shared hint for where to insert the next row in a heap chain.
///
/// Thread-safe (all fields are atomic). Multiple transactions can read/update
/// concurrently without locking. Stale hints are harmless — the inserter
/// falls back to chain walking if the hinted page is full.
#[derive(Debug)]
pub struct HeapInsertHint {
    /// Last page_id known to have free space. 0 = unknown.
    last_page: AtomicU64,
    /// Estimated free bytes on `last_page`. Approximate — not updated on
    /// every insert. Used to skip hint when a large row definitely won't fit.
    estimated_free: AtomicU32,
}

impl HeapInsertHint {
    /// Creates a hint with no cached page.
    pub fn new() -> Self {
        Self {
            last_page: AtomicU64::new(0),
            estimated_free: AtomicU32::new(0),
        }
    }

    /// Creates a hint pointing to a known page.
    pub fn with_page(page_id: u64, free_bytes: u32) -> Self {
        Self {
            last_page: AtomicU64::new(page_id),
            estimated_free: AtomicU32::new(free_bytes),
        }
    }

    /// Returns the hinted page_id, or `None` if no hint is available.
    ///
    /// The caller should try inserting on this page first. If the page is
    /// full (HeapPageFull), fall back to chain walking and call `update()`
    /// with the new tail page.
    pub fn suggest_page(&self, needed_bytes: u32) -> Option<u64> {
        let page_id = self.last_page.load(Ordering::Relaxed);
        if page_id == 0 {
            return None;
        }
        let est = self.estimated_free.load(Ordering::Relaxed);
        if est > 0 && est < needed_bytes {
            return None; // hint page definitely too small
        }
        Some(page_id)
    }

    /// Updates the hint after a successful insert or page allocation.
    pub fn update(&self, page_id: u64, free_bytes: u32) {
        self.last_page.store(page_id, Ordering::Relaxed);
        self.estimated_free.store(free_bytes, Ordering::Relaxed);
    }

    /// Invalidates the hint (e.g., after DROP TABLE or TRUNCATE).
    pub fn invalidate(&self) {
        self.last_page.store(0, Ordering::Relaxed);
        self.estimated_free.store(0, Ordering::Relaxed);
    }

    /// Returns the current hinted page_id (0 = none). For diagnostics.
    pub fn current_page(&self) -> u64 {
        self.last_page.load(Ordering::Relaxed)
    }
}

impl Default for HeapInsertHint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_hint_suggests_none() {
        let h = HeapInsertHint::new();
        assert!(h.suggest_page(100).is_none());
    }

    #[test]
    fn update_and_suggest() {
        let h = HeapInsertHint::new();
        h.update(42, 8000);
        assert_eq!(h.suggest_page(100), Some(42));
    }

    #[test]
    fn skip_if_too_small() {
        let h = HeapInsertHint::with_page(42, 50);
        assert!(h.suggest_page(100).is_none()); // 50 < 100
        assert_eq!(h.suggest_page(30), Some(42)); // 50 >= 30
    }

    #[test]
    fn invalidate_clears() {
        let h = HeapInsertHint::with_page(42, 8000);
        h.invalidate();
        assert!(h.suggest_page(100).is_none());
    }
}
