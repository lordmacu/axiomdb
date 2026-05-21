//! User-space clock-sweep buffer pool for page caching (Phase 11.8, wired into
//! `read_page` in perf-sqlite-gap).
//!
//! Layers on top of mmap: `read_page` populates the pool on a cold miss and
//! serves every subsequent read as an `Arc::clone` of the cached `Arc<Page>` —
//! no mmap lock, no 16 KB copy, no CRC re-check (verify-once-then-serve). A
//! 16-shard partition keeps lock contention near CPU-cache-line parallelism.
//!
//! ## Replacement: clock-sweep (PostgreSQL `bufmgr.c` reference)
//!
//! Each shard is a fixed array of slots with a circulating "hand". A slot has a
//! reference bit set on every hit. On insertion the hand advances: a slot with
//! its reference bit set gets the bit cleared and a second chance; the first
//! slot with a clear bit **and** no external owner is evicted. This makes both
//! `get` and `insert` O(1) (amortized) — unlike a `VecDeque` LRU whose
//! move-to-MRU is O(capacity) per hit and would regress sequential scans.
//!
//! ## Liveness via `Arc::strong_count` (no manual pin/unpin)
//!
//! A page is "in use" iff some reader still holds a `PageRef`/`Arc<Page>` clone,
//! i.e. `Arc::strong_count > 1` (the pool holds one reference). The sweep never
//! evicts such a slot — it skips it. This replaces the old explicit pin/unpin
//! API, which was error-prone across the many `read_page` call sites.
//!
//! ## InnoDB / PostgreSQL note
//!
//! InnoDB uses a hash table + young/old LRU split to resist scan pollution;
//! PostgreSQL uses this same clock-sweep. We start without a young/old split;
//! add midpoint admission only if a scan-heavy regression is observed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::page::Page;

const NUM_SHARDS: usize = 16;
const DEFAULT_CAPACITY: usize = 1024; // pages (16 MB at 16 KB/page)

/// One cached page plus its clock reference bit.
struct Slot {
    page_id: u64,
    page: Arc<Page>,
    referenced: bool,
}

/// What the clock hand should do with the slot under inspection.
enum SweepAction {
    /// Empty slot — reuse it directly.
    Free,
    /// Reference bit set — clear it and give a second chance.
    ClearAndAdvance,
    /// Externally referenced (`strong_count > 1`) — skip, never evict.
    Skip,
    /// Unreferenced and unpinned — evict it.
    Evict,
}

/// One shard: a fixed slot array with a circulating clock hand.
struct CacheShard {
    /// Slot array; grows only transiently when every slot is externally pinned.
    slots: Vec<Option<Slot>>,
    /// page_id → slot index.
    index: HashMap<u64, usize>,
    /// Clock hand position into `slots`.
    hand: usize,
    hits: u64,
    misses: u64,
}

impl CacheShard {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: (0..capacity).map(|_| None).collect(),
            index: HashMap::with_capacity(capacity),
            hand: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Hit → set reference bit and clone the `Arc`; miss → `None`.
    fn get(&mut self, page_id: u64) -> Option<Arc<Page>> {
        if let Some(&idx) = self.index.get(&page_id) {
            let slot = self.slots[idx].as_mut().expect("indexed slot present");
            slot.referenced = true;
            self.hits += 1;
            Some(Arc::clone(&slot.page))
        } else {
            self.misses += 1;
            None
        }
    }

    /// Inserts (or refreshes) a page; returns an `Arc` clone for the caller.
    fn insert(&mut self, page_id: u64, page: Arc<Page>) -> Arc<Page> {
        let arc = Arc::clone(&page);
        if let Some(&idx) = self.index.get(&page_id) {
            // Already resident (cold-miss race or post-write reload): refresh.
            self.slots[idx] = Some(Slot {
                page_id,
                page,
                referenced: false,
            });
            return arc;
        }
        let idx = self.free_slot();
        self.slots[idx] = Some(Slot {
            page_id,
            page,
            referenced: false,
        });
        self.index.insert(page_id, idx);
        self.hand = idx + 1; // resume the sweep past the freshly filled slot
        arc
    }

    /// Drops a cached entry. Idempotent.
    fn invalidate(&mut self, page_id: u64) {
        if let Some(idx) = self.index.remove(&page_id) {
            self.slots[idx] = None;
        }
    }

    /// Returns the index of a usable slot, evicting via clock-sweep if needed.
    ///
    /// Bounds the walk to two revolutions: one to clear reference bits, one to
    /// evict. If every slot is externally pinned, grows by one slot rather than
    /// block or evict an in-use page (transient over-capacity).
    fn free_slot(&mut self) -> usize {
        let max_steps = self.slots.len().saturating_mul(2).saturating_add(1);
        for _ in 0..=max_steps {
            if self.slots.is_empty() {
                break;
            }
            let i = self.hand % self.slots.len();
            let action = match &self.slots[i] {
                None => SweepAction::Free,
                Some(s) if s.referenced => SweepAction::ClearAndAdvance,
                Some(s) if Arc::strong_count(&s.page) > 1 => SweepAction::Skip,
                Some(_) => SweepAction::Evict,
            };
            match action {
                SweepAction::Free => return i,
                SweepAction::ClearAndAdvance => {
                    self.slots[i].as_mut().expect("some").referenced = false;
                    self.hand = i + 1;
                }
                SweepAction::Skip => {
                    self.hand = i + 1;
                }
                SweepAction::Evict => {
                    let pid = self.slots[i].as_ref().expect("some").page_id;
                    self.index.remove(&pid);
                    self.slots[i] = None;
                    return i;
                }
            }
        }
        // Everything pinned/referenced — grow rather than evict an in-use page.
        self.slots.push(None);
        self.slots.len() - 1
    }
}

/// 16-shard partitioned clock-sweep buffer pool.
///
/// Thread-safe: each shard has its own `Mutex`. Two threads touching different
/// shards (different page_ids) never contend.
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

    /// Looks up a cached page. `None` on miss.
    pub fn get(&self, page_id: u64) -> Option<Arc<Page>> {
        let mut shard = self.shards[Self::shard_idx(page_id)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.get(page_id)
    }

    /// Inserts a freshly-loaded (already CRC-verified) page. Returns an `Arc`
    /// clone for the caller to use.
    pub fn insert(&self, page_id: u64, page: Arc<Page>) -> Arc<Page> {
        let mut shard = self.shards[Self::shard_idx(page_id)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.insert(page_id, page)
    }

    /// Invalidates a cached page (after `write_page` or page free/reuse).
    pub fn invalidate(&self, page_id: u64) {
        let mut shard = self.shards[Self::shard_idx(page_id)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.invalidate(page_id);
    }

    /// Returns (total_hits, total_misses) across all shards.
    pub fn stats(&self) -> (u64, u64) {
        let mut hits = 0u64;
        let mut misses = 0u64;
        for shard in self.shards.iter() {
            let s = shard.lock().unwrap_or_else(|e| e.into_inner());
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

    fn make_page(page_id: u64) -> Page {
        Page::new(PageType::Data, page_id)
    }

    #[test]
    fn hit_returns_shared_page() {
        let pool = BufferPool::with_capacity(16);
        pool.insert(42, Arc::new(make_page(42)));
        let got = pool.get(42).expect("hit");
        assert_eq!(got.header().page_id, 42);
        assert_eq!(pool.stats(), (1, 0));
    }

    #[test]
    fn miss_counts() {
        let pool = BufferPool::with_capacity(16);
        assert!(pool.get(99).is_none());
        assert_eq!(pool.stats(), (0, 1));
    }

    #[test]
    fn clock_sweep_evicts_unreferenced_only() {
        // capacity 2 per shard; pages 0, 16, 32 all map to shard 0.
        let pool = BufferPool::with_capacity(NUM_SHARDS * 2);
        pool.insert(0, Arc::new(make_page(0)));
        pool.insert(16, Arc::new(make_page(16)));
        let _keep = pool.get(0); // reference bit on 0 → second chance
        pool.insert(32, Arc::new(make_page(32)));
        assert!(pool.get(0).is_some(), "referenced page survived");
        assert!(pool.get(32).is_some(), "newest present");
        assert!(pool.get(16).is_none(), "unreferenced page evicted");
    }

    #[test]
    fn never_evicts_externally_referenced() {
        // 1 slot per shard; the held Arc keeps page 0 from being evicted.
        let pool = BufferPool::with_capacity(NUM_SHARDS);
        let live = pool.insert(0, Arc::new(make_page(0))); // strong_count == 2
        pool.insert(16, Arc::new(make_page(16))); // would evict 0, but it's pinned
        assert!(pool.get(0).is_some(), "pinned page not evicted");
        assert!(pool.get(16).is_some(), "new page admitted via growth");
        drop(live);
    }

    #[test]
    fn invalidate_removes() {
        let pool = BufferPool::with_capacity(16);
        pool.insert(7, Arc::new(make_page(7)));
        pool.invalidate(7);
        assert!(pool.get(7).is_none());
        pool.invalidate(7); // idempotent
    }

    #[test]
    fn fills_capacity_without_premature_eviction() {
        // capacity 4 in one shard: 4 distinct same-shard ids all fit.
        let pool = BufferPool::with_capacity(NUM_SHARDS * 4);
        for k in 0..4u64 {
            pool.insert(k * 16, Arc::new(make_page(k * 16)));
        }
        for k in 0..4u64 {
            assert!(pool.get(k * 16).is_some(), "page {} present", k * 16);
        }
    }

    #[test]
    fn reload_after_invalidate_serves_fresh() {
        let pool = BufferPool::with_capacity(16);
        pool.insert(5, Arc::new(make_page(5)));
        pool.invalidate(5);
        // Re-insert with a fresh page; get returns it.
        pool.insert(5, Arc::new(make_page(5)));
        assert!(pool.get(5).is_some());
    }
}
