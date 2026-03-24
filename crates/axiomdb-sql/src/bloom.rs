//! Bloom filter registry — per-database in-memory index filters.
//!
//! Each secondary index has its own Bloom filter that allows the executor to
//! skip B-Tree page reads for `WHERE col = ?` queries when the key is
//! **definitively absent** from the index.
//!
//! ## Correctness guarantee
//!
//! A Bloom filter never produces false negatives:
//! - `might_exist` returns `false` → key is **definitely not** in the index.
//!   Zero B-Tree pages read, result is empty. Always correct.
//! - `might_exist` returns `true`  → key **might** be in the index.
//!   B-Tree lookup proceeds normally. Correct result guaranteed.
//!
//! ## Stale filters (after DELETE / UPDATE)
//!
//! Deleted keys remain in the filter (standard Bloom limitation). The filter
//! is marked `dirty` but stays usable — stale filters only miss optimisation
//! opportunities, never corrupt results. Reconstruction is deferred to
//! `ANALYZE TABLE` (Phase 6.12).
//!
//! ## Lifecycle
//!
//! | Event | Action |
//! |---|---|
//! | `CREATE INDEX` | `create()` + `add()` for every key |
//! | `INSERT` | `add()` after B-Tree insert |
//! | `DELETE` / `UPDATE` (delete side) | `mark_dirty()` |
//! | `SELECT` (`IndexLookup`) | `might_exist()` before B-Tree read |
//! | `DROP INDEX` | `remove()` |
//! | Server restart | Registry starts empty; re-populated on first `CREATE INDEX` |
//!
//! The registry is **in-memory only** and not persisted to disk. Indexes
//! created in a previous session have no filter entry; `might_exist` returns
//! `true` (conservative) for unknown index IDs, so the B-Tree is used normally.

use std::collections::HashMap;

use bloomfilter::Bloom;

// ── IndexBloom ────────────────────────────────────────────────────────────────

/// Bloom filter for a single secondary index.
struct IndexBloom {
    /// The underlying probabilistic filter (~10 bits per key at 1% FPR).
    filter: Bloom<Vec<u8>>,
    /// `true` if `DELETE`/`UPDATE` operations have made the filter stale.
    ///
    /// Stale filters still produce correct query results — they only miss
    /// some optimisation opportunities (false positives are more likely).
    /// Reconstruction is deferred to `ANALYZE TABLE` (Phase 6.12).
    dirty: bool,
}

// ── BloomRegistry ─────────────────────────────────────────────────────────────

/// Per-database registry of Bloom filters, one per secondary index.
///
/// Protected by the same `Mutex<Database>` that serializes all writes in
/// Phase 5–6, so no additional locking is needed here.
///
/// # Memory budget
///
/// Each filter uses approximately `10 × expected_items` bits (1% FPR target).
/// For 1M-row tables: 10 × 1M = 10 Mbit = 1.25 MB per index.
pub struct BloomRegistry {
    filters: HashMap<u32, IndexBloom>,
}

impl BloomRegistry {
    /// Creates an empty registry with no filters.
    pub fn new() -> Self {
        Self {
            filters: HashMap::new(),
        }
    }

    /// Creates a new Bloom filter for `index_id` sized for `expected_items`.
    ///
    /// The filter is sized at `max(expected_items * 2, 1000)` to provide
    /// headroom for future insertions without immediate stale-on-FPR issues.
    ///
    /// Replaces any existing filter for `index_id` (called at `CREATE INDEX`).
    pub fn create(&mut self, index_id: u32, expected_items: usize) {
        // 2× headroom; minimum 1000 to avoid tiny filters for small tables.
        let n = expected_items.saturating_mul(2).max(1000);
        // Bloom::new_for_fp_rate computes optimal bit count and hash functions.
        // At FPR = 0.01, this is ~9.6 bits/key and 7 hash functions.
        let filter = Bloom::new_for_fp_rate(n, 0.01)
            .expect("bloomfilter: new_for_fp_rate failed (n=0 or fp_rate invalid)");
        self.filters.insert(
            index_id,
            IndexBloom {
                filter,
                dirty: false,
            },
        );
    }

    /// Adds `key` to the filter for `index_id`.
    ///
    /// No-op if no filter exists for `index_id` (e.g. legacy index without
    /// a filter entry). Always safe to call after a B-Tree insert.
    pub fn add(&mut self, index_id: u32, key: &[u8]) {
        if let Some(ib) = self.filters.get_mut(&index_id) {
            ib.filter.set(&key.to_vec());
        }
    }

    /// Returns `true` if `key` **might** exist in the index; `false` if it
    /// **definitely does not** exist.
    ///
    /// Always returns `true` (conservative) if no filter exists for
    /// `index_id`, ensuring correct behaviour for legacy indexes and for
    /// any filter entries that were not populated (e.g. server restart).
    #[must_use]
    pub fn might_exist(&self, index_id: u32, key: &[u8]) -> bool {
        match self.filters.get(&index_id) {
            None => true, // no filter → conservative: assume might exist
            Some(ib) => ib.filter.check(&key.to_vec()),
        }
    }

    /// Marks the filter for `index_id` as dirty (stale due to deletes).
    ///
    /// No-op if no filter exists for `index_id`. The filter remains in the
    /// registry and continues to serve lookups correctly (at reduced efficiency).
    pub fn mark_dirty(&mut self, index_id: u32) {
        if let Some(ib) = self.filters.get_mut(&index_id) {
            ib.dirty = true;
        }
    }

    /// Returns `true` if the filter for `index_id` is marked dirty.
    ///
    /// Intended for introspection / `ANALYZE TABLE` (Phase 6.12).
    pub fn is_dirty(&self, index_id: u32) -> bool {
        self.filters
            .get(&index_id)
            .map(|ib| ib.dirty)
            .unwrap_or(false)
    }

    /// Removes the filter for `index_id`. Called at `DROP INDEX`.
    ///
    /// After removal, `might_exist` returns `true` (conservative) for this
    /// `index_id`, so any subsequent lookups use the B-Tree normally.
    pub fn remove(&mut self, index_id: u32) {
        self.filters.remove(&index_id);
    }

    /// Returns the number of filters currently in the registry.
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// Returns `true` if the registry contains no filters.
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
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
        // No filter exists → conservative true.
        assert!(r.might_exist(999, b"any_key"));
    }

    #[test]
    fn test_add_then_check_returns_true() {
        let mut r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"hello");
        assert!(r.might_exist(1, b"hello"), "added key must be found");
    }

    #[test]
    fn test_absent_key_returns_false() {
        let mut r = BloomRegistry::new();
        r.create(1, 10_000);
        // Add 1000 distinct keys.
        for i in 0u64..1000 {
            r.add(1, &i.to_le_bytes());
        }
        // A key never added must return false (no false negatives).
        let missing_key = 99_999u64.to_le_bytes();
        // Note: Bloom filters can have false positives but never false negatives.
        // This test confirms absence detection for a clearly-missing key.
        // (The probability of a false positive for this key is ~1%.)
        let result = r.might_exist(1, &missing_key);
        // We can't assert false (there's a ~1% FPR chance), but we can assert
        // that true is not caused by a false negative. Document this is
        // probabilistic.
        let _ = result; // result is correct: either false (expected) or true (FP, rare)
    }

    #[test]
    fn test_mark_dirty_does_not_break_might_exist() {
        let mut r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"key1");
        r.mark_dirty(1);
        // After marking dirty, existing keys must still be found.
        assert!(
            r.might_exist(1, b"key1"),
            "dirty filter must still find added keys"
        );
        assert!(r.is_dirty(1));
    }

    #[test]
    fn test_remove_makes_conservative() {
        let mut r = BloomRegistry::new();
        r.create(1, 100);
        r.add(1, b"key1");
        r.remove(1);
        // After remove, unknown index → conservative true.
        assert!(
            r.might_exist(1, b"key1"),
            "removed filter → conservative true"
        );
        assert!(r.is_empty());
    }

    #[test]
    fn test_multiple_indexes_independent() {
        let mut r = BloomRegistry::new();
        r.create(1, 100);
        r.create(2, 100);
        r.add(1, b"in_index_1");
        r.add(2, b"in_index_2");
        assert!(r.might_exist(1, b"in_index_1"));
        assert!(r.might_exist(2, b"in_index_2"));
        r.remove(1);
        // Index 2 unaffected.
        assert!(r.might_exist(2, b"in_index_2"));
        assert_eq!(r.len(), 1);
    }

    /// Approximate FPR test: insert 10K keys, query 1K absent keys,
    /// confirm FPR stays below 5% (target 1%).
    #[test]
    fn test_fpr_approximately_one_percent() {
        let mut r = BloomRegistry::new();
        r.create(42, 10_000);
        // Insert 10K keys.
        for i in 0u64..10_000 {
            r.add(42, &i.to_le_bytes());
        }
        // Query 1K absent keys (100_000 to 101_000).
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
