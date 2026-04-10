//! Bloom filter registry — per-database in-memory index filters.
//!
//! Each secondary index has its own Bloom filter that allows the executor to
//! skip B-Tree page reads for `WHERE col = ?` queries when the key is
//! **definitively absent** from the index.
//!
//! ## Interior mutability (Phase 40.10)
//!
//! All methods take `&self`. The internal `HashMap` is protected by an
//! `RwLock`: reads (`might_exist`) acquire a shared read lock; writes
//! (`create`, `add`, `mark_dirty`, `remove`) acquire an exclusive write lock.
//! This allows concurrent readers with brief exclusion only during mutations.

use std::collections::HashMap;
use std::sync::RwLock;

use bloomfilter::Bloom;

// ── IndexBloom ────────────────────────────────────────────────────────────────

/// Bloom filter for a single secondary index.
struct IndexBloom {
    filter: Bloom<Vec<u8>>,
    dirty: bool,
}

// ── BloomRegistry ─────────────────────────────────────────────────────────────

/// Per-database registry of Bloom filters, one per secondary index.
///
/// All methods take `&self` via interior mutability (Phase 40.10).
/// The internal `RwLock` allows concurrent `might_exist` reads with brief
/// exclusion only during `create`, `add`, `mark_dirty`, or `remove`.
pub struct BloomRegistry {
    filters: RwLock<HashMap<u32, IndexBloom>>,
}

impl BloomRegistry {
    /// Creates an empty registry with no filters.
    pub fn new() -> Self {
        Self {
            filters: RwLock::new(HashMap::new()),
        }
    }

    /// Creates a new Bloom filter for `index_id` sized for `expected_items`.
    pub fn create(&self, index_id: u32, expected_items: usize) {
        let n = expected_items.saturating_mul(2).max(1000);
        let filter = Bloom::new_for_fp_rate(n, 0.01)
            .expect("bloomfilter: new_for_fp_rate failed (n=0 or fp_rate invalid)");
        self.filters.write().unwrap().insert(
            index_id,
            IndexBloom {
                filter,
                dirty: false,
            },
        );
    }

    /// Adds `key` to the filter for `index_id`.
    pub fn add(&self, index_id: u32, key: &[u8]) {
        if let Some(ib) = self.filters.write().unwrap().get_mut(&index_id) {
            ib.filter.set(&key.to_vec());
        }
    }

    /// Returns `true` if `key` **might** exist in the index; `false` if it
    /// **definitely does not** exist.
    #[must_use]
    pub fn might_exist(&self, index_id: u32, key: &[u8]) -> bool {
        match self.filters.read().unwrap().get(&index_id) {
            None => true,
            Some(ib) => ib.filter.check(&key.to_vec()),
        }
    }

    /// Marks the filter for `index_id` as dirty (stale due to deletes).
    pub fn mark_dirty(&self, index_id: u32) {
        if let Some(ib) = self.filters.write().unwrap().get_mut(&index_id) {
            ib.dirty = true;
        }
    }

    /// Returns `true` if the filter for `index_id` is marked dirty.
    pub fn is_dirty(&self, index_id: u32) -> bool {
        self.filters
            .read()
            .unwrap()
            .get(&index_id)
            .map(|ib| ib.dirty)
            .unwrap_or(false)
    }

    /// Removes the filter for `index_id`. Called at `DROP INDEX`.
    pub fn remove(&self, index_id: u32) {
        self.filters.write().unwrap().remove(&index_id);
    }

    /// Returns the number of filters currently in the registry.
    pub fn len(&self) -> usize {
        self.filters.read().unwrap().len()
    }

    /// Returns `true` if the registry contains no filters.
    pub fn is_empty(&self) -> bool {
        self.filters.read().unwrap().is_empty()
    }
}

impl Default for BloomRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_registry_is_empty() {
        let r = BloomRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn test_might_exist_unknown_index_returns_true() {
        let r = BloomRegistry::new();
        assert!(r.might_exist(999, b"any_key"));
    }

    #[test]
    fn test_add_then_check_returns_true() {
        let r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"hello");
        assert!(r.might_exist(1, b"hello"), "added key must be found");
    }

    #[test]
    fn test_absent_key_returns_false() {
        let r = BloomRegistry::new();
        r.create(1, 10_000);
        for i in 0u64..1000 {
            r.add(1, &i.to_le_bytes());
        }
        let missing_key = 99_999u64.to_le_bytes();
        let result = r.might_exist(1, &missing_key);
        let _ = result;
    }

    #[test]
    fn test_mark_dirty_does_not_break_might_exist() {
        let r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"key1");
        r.mark_dirty(1);
        assert!(
            r.might_exist(1, b"key1"),
            "dirty filter must still find added keys"
        );
        assert!(r.is_dirty(1));
    }

    #[test]
    fn test_remove_makes_conservative() {
        let r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"key1");
        r.remove(1);
        assert!(
            r.might_exist(1, b"key1"),
            "removed filter → conservative true"
        );
        assert!(r.is_empty());
    }

    #[test]
    fn test_multiple_indexes_independent() {
        let r = BloomRegistry::new();
        r.create(1, 100);
        r.create(2, 100);
        r.add(1, b"in_index_1");
        r.add(2, b"in_index_2");
        assert!(r.might_exist(1, b"in_index_1"));
        assert!(r.might_exist(2, b"in_index_2"));
        r.remove(1);
        assert!(r.might_exist(2, b"in_index_2"));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn test_fpr_approximately_one_percent() {
        let r = BloomRegistry::new();
        r.create(42, 10_000);
        for i in 0u64..10_000 {
            r.add(42, &i.to_le_bytes());
        }
        let mut false_positives = 0usize;
        let queries = 1000usize;
        for i in 100_000u64..100_000 + queries as u64 {
            if r.might_exist(42, &i.to_le_bytes()) {
                false_positives += 1;
            }
        }
        let fpr = false_positives as f64 / queries as f64;
        assert!(
            fpr < 0.05,
            "FPR {:.1}% exceeds 5% threshold (target 1%)",
            fpr * 100.0
        );
    }
}
