//! Adaptive Hash Index — fast-path cache for B-Tree point lookups (Gap Closure Opt 4).
//!
//! Inspired by MariaDB/InnoDB's AHI (`btr0sea.cc`): a CRC-32C hash of the
//! search key maps directly to a `RecordId`, bypassing B-Tree traversal.
//!
//! MariaDB builds AHI entries after 100 consecutive lookups on the same leaf
//! page (`BTR_SEARCH_BUILD_LIMIT`). Our simplified version caches every
//! successful lookup result immediately — simpler, lower memory, and effective
//! for the repeated PK lookup pattern in OLTP benchmarks.
//!
//! ## Thread safety
//!
//! Uses `std::cell::RefCell` (thread-local). Each connection thread has its
//! own AHI cache — no locking, no contention (MariaDB uses 512 partitioned
//! latches for the same reason).
//!
//! ## Invalidation
//!
//! Callers must call `ahi_invalidate(root_pid)` when the index is modified
//! (INSERT, UPDATE, DELETE). This clears all entries for that index.
//! Stale entries are also detected at lookup time via heap visibility check.

use std::cell::RefCell;
use std::collections::HashMap;

use axiomdb_core::RecordId;

/// Maximum entries per thread-local AHI cache. When exceeded, the cache is
/// cleared entirely (simple eviction — MariaDB uses per-partition LRU).
const AHI_MAX_ENTRIES: usize = 4096;

// Thread-local AHI cache: hash(root_pid, key) → RecordId.
thread_local! {
    static AHI_CACHE: RefCell<HashMap<u64, RecordId>> = RefCell::new(HashMap::new());
}

/// Computes the AHI hash for a (root_pid, key) pair.
/// Uses CRC-32C (same as MariaDB's `rec_fold`) extended with the root_pid
/// to distinguish entries from different indexes.
#[inline]
fn ahi_hash(root_pid: u64, key: &[u8]) -> u64 {
    let h1 = crc32c::crc32c(&root_pid.to_le_bytes());
    let h2 = crc32c::crc32c_append(h1, key);
    h2 as u64
}

/// Looks up a key in the AHI cache. Returns `Some(RecordId)` on hit.
///
/// Called before `BTree::lookup_in()` — if hit, the caller can skip the
/// entire B-Tree traversal and go directly to the heap page.
#[inline]
pub fn ahi_lookup(root_pid: u64, key: &[u8]) -> Option<RecordId> {
    let hash = ahi_hash(root_pid, key);
    AHI_CACHE.with(|cache| cache.borrow().get(&hash).copied())
}

/// Inserts a successful lookup result into the AHI cache.
///
/// Called after `BTree::lookup_in()` returns `Some(rid)`. Future lookups
/// for the same key will hit the cache and skip B-Tree traversal.
#[inline]
pub fn ahi_insert(root_pid: u64, key: &[u8], rid: RecordId) {
    let hash = ahi_hash(root_pid, key);
    AHI_CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        if c.len() >= AHI_MAX_ENTRIES {
            c.clear(); // Simple eviction — matches MariaDB's batch cleanup
        }
        c.insert(hash, rid);
    });
}

/// Removes a specific key from the AHI cache.
///
/// Called on INSERT, UPDATE, DELETE that touches this key in the index.
/// More targeted than clearing the entire cache.
#[inline]
pub fn ahi_remove(root_pid: u64, key: &[u8]) {
    let hash = ahi_hash(root_pid, key);
    AHI_CACHE.with(|cache| {
        cache.borrow_mut().remove(&hash);
    });
}

/// Invalidates all AHI entries (conservative — used on DDL or bulk operations).
#[inline]
pub fn ahi_invalidate_all() {
    AHI_CACHE.with(|cache| cache.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ahi_insert_lookup() {
        let rid = RecordId {
            page_id: 42,
            slot_id: 7,
        };
        ahi_insert(100, b"key1", rid);
        assert_eq!(ahi_lookup(100, b"key1"), Some(rid));
        assert_eq!(ahi_lookup(100, b"key2"), None);
    }

    #[test]
    fn test_ahi_different_roots() {
        let rid1 = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        let rid2 = RecordId {
            page_id: 2,
            slot_id: 0,
        };
        ahi_insert(100, b"key", rid1);
        ahi_insert(200, b"key", rid2);
        assert_eq!(ahi_lookup(100, b"key"), Some(rid1));
        assert_eq!(ahi_lookup(200, b"key"), Some(rid2));
    }

    #[test]
    fn test_ahi_invalidate() {
        let rid = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        ahi_insert(100, b"key", rid);
        assert_eq!(ahi_lookup(100, b"key"), Some(rid));
        ahi_invalidate_all();
        assert_eq!(ahi_lookup(100, b"key"), None);
    }

    #[test]
    fn test_ahi_eviction_on_overflow() {
        // Fill cache beyond AHI_MAX_ENTRIES
        for i in 0..AHI_MAX_ENTRIES + 10 {
            ahi_insert(
                1,
                format!("key_{i}").as_bytes(),
                RecordId {
                    page_id: i as u64,
                    slot_id: 0,
                },
            );
        }
        // After overflow, cache was cleared and only recent entries exist
        // (the exact behavior depends on clear-on-overflow)
        // Just verify no panic and cache is functional
        assert!(ahi_lookup(1, b"key_0").is_none()); // cleared by overflow
    }
}
