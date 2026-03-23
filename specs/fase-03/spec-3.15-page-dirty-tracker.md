# Spec: 3.15 — Page Dirty Tracker

## What to build

An in-memory set that tracks which pages have been written since the last
`flush()`. Integrated into `MmapStorage` so the checkpoint and monitoring code
can know exactly which pages need flushing, without scanning all pages.

## PageDirtyTracker

```rust
// nexusdb-storage/src/dirty.rs
pub struct PageDirtyTracker {
    dirty: HashSet<u64>,
}
impl PageDirtyTracker {
    pub fn new() -> Self
    pub fn mark(&mut self, page_id: u64)
    pub fn contains(&self, page_id: u64) -> bool
    pub fn count(&self) -> usize
    pub fn is_empty(&self) -> bool
    pub fn clear(&mut self)
    pub fn sorted_ids(&self) -> Vec<u64>  // sorted for deterministic output
}
```

## MmapStorage integration

- Add `dirty: PageDirtyTracker` field.
- In `write_page`: call `self.dirty.mark(page_id)`.
- In `alloc_page`: call `self.dirty.mark(page_id)`.
- In `flush()`: after `mmap.flush()`, call `self.dirty.clear()`.
- Expose `pub fn dirty_page_count(&self) -> usize`.

## Acceptance criteria

- [ ] `PageDirtyTracker::new()` starts empty
- [ ] `mark` + `contains` roundtrip works
- [ ] `sorted_ids()` returns page IDs in ascending order
- [ ] `clear()` resets to empty
- [ ] After `MmapStorage::write_page(id)`, `dirty_page_count()` includes that page
- [ ] After `MmapStorage::flush()`, `dirty_page_count()` is 0
- [ ] `MmapStorage::alloc_page` marks the new page dirty
- [ ] No `unwrap()` in `src/`

## ⚠️ DEFERRED

- Per-page `msync` (flush_range) instead of full mmap flush → depends on profiling
- Integration with Checkpointer to skip clean pages → Phase 5 (checkpoint frequency)
