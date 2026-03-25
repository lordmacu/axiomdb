# Plan: 3.15b — Targeted `flush_range` For Dirty mmap Pages

## Files to create / modify

- `crates/axiomdb-storage/src/dirty.rs` — add deterministic contiguous-run coalescing helper
- `crates/axiomdb-storage/src/mmap.rs` — replace whole-mmap flush with targeted range flushing
- `crates/axiomdb-storage/tests/integration_storage.rs` — success-path integration coverage

## Algorithm / Data structure

### 1. Coalesce dirty page IDs into runs

Add a pure helper on `PageDirtyTracker`:

```rust
pub fn contiguous_runs(&self) -> Vec<(u64, u64)> {
    // returns (start_page_id, run_len_pages)
}
```

Rules:
- page IDs are sorted ascending first
- adjacent pages merge into one run
- non-adjacent pages start a new run

### 2. Build the effective flush set

Before flushing:
- if `freelist_dirty` is true, serialize page 1 into the mmap first
- include page 1 in the effective dirty set even if the tracker did not contain it
- page 0 already participates naturally because meta writes go through `write_page(0, ...)`

### 3. Flush only those byte ranges

For each run `(start_page, len_pages)`:

```rust
let offset = start_page as usize * PAGE_SIZE;
let len = len_pages as usize * PAGE_SIZE;
self.mmap.flush_range(offset, len)?;
```

Only after **all** runs succeed:
- clear `freelist_dirty`
- clear the dirty tracker

### 4. Make failure testable

Introduce a small internal helper in `mmap.rs`:

```rust
fn flush_runs<F>(runs: &[(usize, usize)], mut flusher: F) -> std::io::Result<()>
where
    F: FnMut(usize, usize) -> std::io::Result<()>;
```

Production code passes `|off, len| self.mmap.flush_range(off, len)`.
Tests can pass a fake flusher that fails on the Nth call to verify that dirty
state is preserved on partial failure.

## Implementation phases

1. Add pure dirty-run coalescing helper in `dirty.rs` with unit tests.
2. Add `flush_runs(...)` helper in `mmap.rs` so the failure path is testable.
3. Change `MmapStorage::flush()` to serialize freelist first, build effective runs, and flush only those ranges.
4. Add integration coverage for empty, contiguous, split, and freelist-only flushes.

## Tests to write

- unit:
  - `contiguous_runs()` on empty input
  - `contiguous_runs()` on one page
  - `contiguous_runs()` on contiguous pages
  - `contiguous_runs()` on split runs
  - `flush_runs()` preserves dirty state on injected failure
- integration:
  - write a page then `flush()` clears dirty state
  - write multiple contiguous pages then `flush()` clears dirty state
  - freelist-only change flushes successfully
- bench:
  - compare whole-file flush vs dirty-run flush on sparse dirty pages

## Anti-patterns to avoid

- Do **not** clear dirty tracking before all range flushes succeed.
- Do **not** fall back to whole-file flush on every call.
- Do **not** duplicate range-merging logic in `mmap.rs`; keep it as one pure helper.
- Do **not** forget page 1 when `freelist_dirty` is set.

## Risks

- Many tiny dirty runs could increase syscall count.
  Mitigation: accepted in this subphase; benchmark before adding heuristics.
- Failure-path testing is hard if `flush_range` is called directly.
  Mitigation: explicit `flush_runs(...)` helper with injected fake flusher.
