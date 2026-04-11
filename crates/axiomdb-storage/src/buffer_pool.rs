//! User-space LRU buffer pool for page caching (Phase 11.8).
//!
//! Layers on top of mmap: captures `PageRef` copies from `read_page()` and
//! serves subsequent reads from the cache. 16-shard partitioned LRU reduces
//! lock contention to match CPU cache line parallelism.
//!
//! ## InnoDB reference (buf0buf.cc)
//!
//! InnoDB uses a hash table + doubly-linked LRU with young/old split (25%
//! boundary). New pages start "old"; promoted to "young" on second access
//! within a time window. This prevents sequential scans from evicting the
//! working set.
//!
//! ## PostgreSQL reference (bufmgr.c)
//!
//! PostgreSQL uses clock-sweep replacement: a linear buffer array with
//! `usage_count` per slot. Clock hand decrements counts; evicts first slot
//! reaching zero. Simpler than LRU linked-list.
//!
//! ## AxiomDB design
//!
//! Simple LRU with 16 shards. No young/old split in Phase 11.8 (add in
//! future if scan pollution observed). Pin count prevents eviction of
//! in-flight pages.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::page_ref::PageRef;

const NUM_SHARDS: usize = 16;
const DEFAULT_CAPACITY: usize = 1024; // pages (16 MB at 16 KB/page)

/// Per-shard cached page entry.
struct CacheEntry {
    page_ref: Arc<PageRef>,
    pin_count: u32,
}

/// One shard of the partitioned buffer pool.
struct CacheShard {
    entries: HashMap<u64, CacheEntry>,
    lru_order: VecDeque<u64>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl CacheShard {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            lru_order: VecDeque::with_capacity(capacity),
            capacity,
            hits: 0,
            misses: 0,
        }
    }

    /// Looks up a page. Returns Some on hit (moves to MRU end).
    fn get(&mut self, page_id: u64) -> Option<Arc<PageRef>> {
        if let Some(entry) = self.entries.get_mut(&page_id) {
            self.hits += 1;
            entry.pin_count += 1;
            // Move to MRU end.
            if let Some(pos) = self.lru_order.iter().position(|&id| id == page_id) {
                self.lru_order.remove(pos);
            }
            self.lru_order.push_back(page_id);
            Some(Arc::clone(&entry.page_ref))
        } else {
            self.misses += 1;
            None
        }
    }

    /// Inserts a page into the cache. Evicts LRU if over capacity.
    fn insert(&mut self, page_id: u64, page_ref: PageRef) -> Arc<PageRef> {
        let arc = Arc::new(page_ref);
        self.entries.insert(
            page_id,
            CacheEntry {
                page_ref: Arc::clone(&arc),
                pin_count: 1,
            },
        );
        self.lru_order.push_back(page_id);
        self.evict_if_needed();
        arc
    }

    /// Decrements pin count for a page.
    fn unpin(&mut self, page_id: u64) {
        if let Some(entry) = self.entries.get_mut(&page_id) {
            entry.pin_count = entry.pin_count.saturating_sub(1);
        }
    }

    /// Invalidates a page (remove from cache). Called after write_page
    /// to ensure next read sees fresh data.
    fn invalidate(&mut self, page_id: u64) {
        self.entries.remove(&page_id);
        if let Some(pos) = self.lru_order.iter().position(|&id| id == page_id) {
            self.lru_order.remove(pos);
        }
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > self.capacity {
            let candidates = self.lru_order.len();
            let mut scanned = 0usize;
            let mut evicted = false;

            // A shard can temporarily exceed capacity when every candidate is
            // pinned. Scan the current LRU set once, then stop instead of
            // cycling pinned pages forever.
            if let Some(victim_id) = self.lru_order.pop_front() {
                if let Some(entry) = self.entries.get(&victim_id) {
                    if entry.pin_count == 0 {
                        self.entries.remove(&victim_id);
                        evicted = true;
                    } else {
                        self.lru_order.push_back(victim_id);
                    }
                }
                scanned += 1;
            }

            while !evicted && scanned < candidates {
                let Some(victim_id) = self.lru_order.pop_front() else {
                    break;
                };
                if let Some(entry) = self.entries.get(&victim_id) {
                    if entry.pin_count == 0 {
                        self.entries.remove(&victim_id);
                        evicted = true;
                    } else {
                        self.lru_order.push_back(victim_id);
                    }
                }
                scanned += 1;
            }

            if !evicted {
                break;
            }
        }
    }
}

/// 16-shard partitioned LRU buffer pool.
///
/// Thread-safe: each shard has its own `Mutex`. Two threads accessing
/// different shards (different page_ids) never contend.
pub struct BufferPool {
    shards: Box<[Mutex<CacheShard>]>,
}

impl BufferPool {
    /// Creates a buffer pool with default capacity (1024 pages = 16 MB).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates a buffer pool with the given total capacity (in pages).
    pub fn with_capacity(total_pages: usize) -> Self {
        let per_shard = (total_pages / NUM_SHARDS).max(1);
        let shards: Vec<_> = (0..NUM_SHARDS)
            .map(|_| Mutex::new(CacheShard::new(per_shard)))
            .collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    #[inline]
    fn shard_idx(page_id: u64) -> usize {
        (page_id as usize) & (NUM_SHARDS - 1)
    }

    /// Looks up a cached page. Returns None on miss.
    pub fn get(&self, page_id: u64) -> Option<Arc<PageRef>> {
        let mut shard = self.shards[Self::shard_idx(page_id)].lock().unwrap();
        shard.get(page_id)
    }

    /// Inserts a page into the cache. Returns an Arc for the caller to use.
    pub fn insert(&self, page_id: u64, page_ref: PageRef) -> Arc<PageRef> {
        let mut shard = self.shards[Self::shard_idx(page_id)].lock().unwrap();
        shard.insert(page_id, page_ref)
    }

    /// Decrements pin count. Call when done reading the page.
    pub fn unpin(&self, page_id: u64) {
        let mut shard = self.shards[Self::shard_idx(page_id)].lock().unwrap();
        shard.unpin(page_id);
    }

    /// Invalidates a cached page (e.g., after write_page).
    pub fn invalidate(&self, page_id: u64) {
        let mut shard = self.shards[Self::shard_idx(page_id)].lock().unwrap();
        shard.invalidate(page_id);
    }

    /// Returns (total_hits, total_misses) across all shards.
    pub fn stats(&self) -> (u64, u64) {
        let mut hits = 0u64;
        let mut misses = 0u64;
        for shard in self.shards.iter() {
            let s = shard.lock().unwrap();
            hits += s.hits;
            misses += s.misses;
        }
        (hits, misses)
    }
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Page, PageType};

    fn make_page(page_id: u64) -> PageRef {
        let page = Page::new(PageType::Data, page_id);
        PageRef::new(Box::new(page))
    }

    #[test]
    fn test_cache_hit() {
        let pool = BufferPool::with_capacity(16);
        let pr = make_page(42);
        pool.insert(42, pr);

        let cached = pool.get(42);
        assert!(cached.is_some());

        let (hits, misses) = pool.stats();
        assert_eq!(hits, 1);
        assert_eq!(misses, 0);
    }

    #[test]
    fn test_cache_miss() {
        let pool = BufferPool::with_capacity(16);
        let cached = pool.get(99);
        assert!(cached.is_none());

        let (_, misses) = pool.stats();
        assert_eq!(misses, 1);
    }

    #[test]
    fn test_eviction() {
        // Capacity 2 per shard. Insert 3 pages to same shard → 1 evicted.
        let pool = BufferPool::with_capacity(NUM_SHARDS * 2);

        // Pages 0, 16, 32 all map to shard 0 (page_id % 16 == 0).
        pool.insert(0, make_page(0));
        pool.unpin(0);
        pool.insert(16, make_page(16));
        pool.unpin(16);
        pool.insert(32, make_page(32));
        pool.unpin(32);

        // Page 0 should have been evicted (LRU).
        assert!(pool.get(0).is_none());
        assert!(pool.get(16).is_some());
        assert!(pool.get(32).is_some());
    }

    #[test]
    fn test_pinned_not_evicted() {
        let pool = BufferPool::with_capacity(NUM_SHARDS);

        // Insert page 0 (pinned by default).
        pool.insert(0, make_page(0));
        // DON'T unpin — it's still pinned.

        // Insert page 16 (same shard, should try to evict 0 but can't).
        pool.insert(16, make_page(16));

        // Both should be present (0 is pinned, 16 is newest).
        assert!(pool.get(0).is_some());
        assert!(pool.get(16).is_some());
    }

    #[test]
    fn test_invalidate() {
        let pool = BufferPool::with_capacity(16);
        pool.insert(42, make_page(42));
        assert!(pool.get(42).is_some());

        pool.invalidate(42);
        assert!(pool.get(42).is_none());
    }
}
